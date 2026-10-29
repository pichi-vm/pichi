// SPDX-License-Identifier: Apache-2.0

use std::io;

use zerocopy::FromBytes;

use super::Records;
use crate::{DevId, abi::Command};

/// Owns the reply so names can be borrowed without allocating.
pub(crate) struct Devices<const N: usize = 4096> {
    command: Command<N>,
    offset: Option<usize>,
}

impl<const N: usize> Devices<N> {
    pub(crate) fn new(command: Command<N>) -> io::Result<Self> {
        command.payload()?;
        Ok(Self {
            command,
            offset: Some(0),
        })
    }
}

impl<const N: usize> Records for Devices<N> {
    type Item<'a> = (&'a str, DevId);

    fn next(&mut self) -> Option<io::Result<Self::Item<'_>>> {
        let start = self.offset.take()?;
        let payload = match self.command.payload() {
            Ok(payload) => payload,
            Err(error) => {
                return Some(Err(io::Error::other(format!(
                    "validated reply became invalid: {error}"
                ))));
            }
        };
        if start >= payload.len() {
            return None;
        }
        // Stop after malformed data rather than repeating the same error.
        let record = &payload[start..];
        let Ok((device, fields)) = u64::read_from_prefix(record) else {
            return Some(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated devices record",
            )));
        };
        let Ok((next, _)) = u32::read_from_prefix(fields) else {
            return Some(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated devices record",
            )));
        };
        let value = DevId::from_kernel(device);
        let next = next as usize;
        let end = if next == 0 { record.len() } else { next };
        if end <= 12 || end > record.len() {
            return Some(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid devices record offset",
            )));
        }
        let name = &record[12..end];
        let Some(nul) = name.iter().position(|&byte| byte == 0) else {
            return Some(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unterminated devices name",
            )));
        };
        let name = match std::str::from_utf8(&name[..nul]) {
            Ok(name) => name,
            Err(error) => return Some(Err(io::Error::new(io::ErrorKind::InvalidData, error))),
        };
        if next != 0 {
            self.offset = Some(start + next);
        }
        Some(Ok((name, value)))
    }
}

impl<const N: usize> std::fmt::Debug for Devices<N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Devices")
            .field("capacity", &N)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use zerocopy::IntoBytes;

    use super::*;
    use crate::abi::Header;

    #[test]
    fn invalidated_reply_returns_one_error_then_stops() {
        let mut header = Header::any();
        header.as_mut_bytes()[12..16]
            .copy_from_slice(&u32::try_from(Header::SIZE + 8).unwrap().to_ne_bytes());
        let mut reply = Devices::new(Command::<8>::new(header)).unwrap();
        // Only test code can mutate the owned response after validation.
        reply.command.as_mut_bytes()[12..16].copy_from_slice(&0u32.to_ne_bytes());
        assert_eq!(
            reply.next().unwrap().unwrap_err().kind(),
            io::ErrorKind::Other
        );
        assert!(reply.next().is_none());
        assert!(reply.next().is_none());
    }

    fn devices(buf: &[u8]) -> Devices<4096> {
        let mut command = Command::new(Header::any());
        command.as_mut_bytes()[..buf.len()].copy_from_slice(buf);
        command.as_mut_bytes()[..4].copy_from_slice(&Header::VERSION_MAJOR.to_ne_bytes());
        Devices::new(command).unwrap()
    }

    // Hand-builds a synthetic `DM_LIST_DEVICES`-shaped response buffer:
    // N `dm_name_list` entries, each `next` relative to *that entry's
    // own* start (the opposite convention from `DM_TABLE_STATUS`'s
    // `dm_target_spec.next` used by table rows). Independent of any real
    // ioctl call, exercising the parser against a
    // kernel-shaped response rather than a round trip.
    // Test-only helper building tiny fixture buffers; lengths never approach u32::MAX.
    #[allow(clippy::cast_possible_truncation)]
    fn synthetic_list_devices_response(entries: &[(u64, &str)]) -> (Vec<u8>, usize, usize) {
        let start = Header::SIZE;
        let mut lens = Vec::with_capacity(entries.len());
        let mut payload_len = 0usize;
        for (_, name) in entries {
            // 8 (dev) + 4 (next) + name + NUL, 8-byte aligned (matches
            // real dm_name_list chaining in practice, though the kernel
            // doesn't strictly require alignment for the *last* entry).
            let len = (12 + name.len() + 1).next_multiple_of(8);
            lens.push(len);
            payload_len += len;
        }
        let mut bytes = vec![0u8; start + payload_len];
        bytes[16..20].copy_from_slice(&(start as u32).to_ne_bytes());
        bytes[12..16].copy_from_slice(&((start + payload_len) as u32).to_ne_bytes());

        let mut offset = start;
        for (i, (dev, name)) in entries.iter().enumerate() {
            let len = lens[i];
            let next = if i == entries.len() - 1 {
                0
            } else {
                len as u32
            };
            bytes[offset..offset + 8].copy_from_slice(&dev.to_ne_bytes());
            bytes[offset + 8..offset + 12].copy_from_slice(&next.to_ne_bytes());
            bytes[offset + 12..offset + 12 + name.len()].copy_from_slice(name.as_bytes());
            offset += len;
        }

        (bytes, start, start + payload_len)
    }

    #[test]
    fn list_devices_iter_follows_next_relative_to_current_entry() {
        let (buf, _, _) = synthetic_list_devices_response(&[
            (DevId::new(252, 5).unwrap().to_kernel(), "first"),
            (
                DevId::new(252, 6).unwrap().to_kernel(),
                "second-longer-name",
            ),
            (DevId::new(252, 7).unwrap().to_kernel(), "third"),
        ]);
        let mut reply = devices(&buf);
        let mut entries = Vec::new();
        while let Some(record) = reply.next() {
            let (name, id) = record.unwrap();
            entries.push((name.to_owned(), id));
        }
        assert_eq!(
            entries,
            [
                ("first".to_string(), DevId::new(252, 5).unwrap()),
                (
                    "second-longer-name".to_string(),
                    DevId::new(252, 6).unwrap()
                ),
                ("third".to_string(), DevId::new(252, 7).unwrap()),
            ]
        );
    }

    #[test]
    fn list_devices_iter_stops_on_truncated_final_record() {
        // The reply contains fewer than 12 bytes for its last record: the
        // 12-byte-header guard must stop cleanly rather than slice OOB.
        let start = Header::SIZE;
        let mut buf = vec![0u8; start + 8]; // only 8 bytes of a 12+-byte record
        buf[12..16].copy_from_slice(&u32::try_from(start + 8).unwrap().to_ne_bytes());
        buf[16..20].copy_from_slice(&u32::try_from(start).unwrap().to_ne_bytes());
        let mut records = devices(&buf);
        assert!(records.next().unwrap().is_err());
        assert!(records.next().is_none());
    }

    #[test]
    fn enumeration_borrows_utf8_names_and_stops_on_invalid_text() {
        let (mut bytes, start, _) =
            synthetic_list_devices_response(&[(0xfc01, "café"), (0xfc02, "b")]);
        let mut reply = devices(&bytes);
        let name_ptr = reply.command.payload().unwrap()[12..].as_ptr();
        let (first, first_id) = reply.next().unwrap().unwrap();
        assert_eq!(first, "café");
        assert_eq!(first.as_ptr(), name_ptr);
        assert_eq!(first_id, DevId::new(252, 1).unwrap());
        assert_eq!(reply.next().unwrap().unwrap().0, "b");
        assert!(reply.next().is_none());

        bytes[start + 12] = 0xff;
        let mut reply = devices(&bytes);
        assert_eq!(
            reply.next().unwrap().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(reply.next().is_none());
        assert!(reply.next().is_none());
    }
}
