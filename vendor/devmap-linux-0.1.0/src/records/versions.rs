// SPDX-License-Identifier: Apache-2.0

use std::io;

use zerocopy::FromBytes;

use super::Records;
use crate::{abi::Command, targets::Version};

/// Owns the reply so names can be borrowed without allocating.
pub(crate) struct Versions<const N: usize = 4096> {
    command: Command<N>,
    offset: Option<usize>,
}

impl<const N: usize> Versions<N> {
    pub(crate) fn new(command: Command<N>) -> io::Result<Self> {
        command.payload()?;
        Ok(Self {
            command,
            offset: Some(0),
        })
    }
}

impl<const N: usize> Records for Versions<N> {
    type Item<'a> = (&'a str, Version);

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
        let Ok((next, fields)) = u32::read_from_prefix(record) else {
            return Some(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated versions record",
            )));
        };
        let Ok(([major, minor, patch], _)) = <[u32; 3]>::read_from_prefix(fields) else {
            return Some(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated versions record",
            )));
        };
        let value = Version {
            major,
            minor,
            patch,
        };
        let next = next as usize;
        let end = if next == 0 { record.len() } else { next };
        if end <= 16 || end > record.len() {
            return Some(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid versions record offset",
            )));
        }
        let name = &record[16..end];
        let Some(nul) = name.iter().position(|&byte| byte == 0) else {
            return Some(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unterminated versions name",
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

impl<const N: usize> std::fmt::Debug for Versions<N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Versions")
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
        let mut reply = Versions::new(Command::<8>::new(header)).unwrap();
        // Only test code can mutate the owned response after validation.
        reply.command.as_mut_bytes()[12..16].copy_from_slice(&0u32.to_ne_bytes());
        assert_eq!(
            reply.next().unwrap().unwrap_err().kind(),
            io::ErrorKind::Other
        );
        assert!(reply.next().is_none());
        assert!(reply.next().is_none());
    }

    fn versions(buf: &[u8]) -> Versions<4096> {
        let mut command = Command::new(Header::any());
        command.as_mut_bytes()[..buf.len()].copy_from_slice(buf);
        command.as_mut_bytes()[..4].copy_from_slice(&Header::VERSION_MAJOR.to_ne_bytes());
        Versions::new(command).unwrap()
    }

    /// Hand-builds a synthetic `DM_LIST_VERSIONS` response: a chain of
    /// `dm_target_versions` records whose `next` is relative to *that
    /// record's own* start (the same convention as `dm_name_list`).
    #[allow(clippy::cast_possible_truncation)] // test fixture, sizes are tiny
    fn synthetic_versions_response(entries: &[(&str, [u32; 3])]) -> Vec<u8> {
        let start = Header::SIZE;
        let lens: Vec<usize> = entries
            .iter()
            .map(|(name, _)| (16 + name.len() + 1).next_multiple_of(8))
            .collect();
        let payload_len: usize = lens.iter().sum();
        let mut buf = vec![0u8; start + payload_len];
        buf[16..20].copy_from_slice(&(start as u32).to_ne_bytes());
        buf[12..16].copy_from_slice(&((start + payload_len) as u32).to_ne_bytes());

        let mut offset = start;
        for (i, (name, version)) in entries.iter().enumerate() {
            let next = if i == entries.len() - 1 {
                0
            } else {
                lens[i] as u32
            };
            buf[offset..offset + 4].copy_from_slice(&next.to_ne_bytes());
            for (v, word) in version.iter().enumerate() {
                let off = offset + 4 + v * 4;
                buf[off..off + 4].copy_from_slice(&word.to_ne_bytes());
            }
            buf[offset + 16..offset + 16 + name.len()].copy_from_slice(name.as_bytes());
            offset += lens[i];
        }
        buf
    }

    #[test]
    fn target_iter_follows_next_relative_to_current_record() {
        let buf = synthetic_versions_response(&[
            ("linear", [1, 4, 0]),
            ("striped", [1, 6, 0]),
            ("thin-pool", [1, 23, 0]),
        ]);
        let mut reply = versions(&buf);
        let mut got = Vec::new();
        while let Some(record) = reply.next() {
            let (name, version) = record.unwrap();
            got.push((name.to_owned(), version));
        }
        assert_eq!(
            got,
            [
                (
                    "linear".to_string(),
                    Version {
                        major: 1,
                        minor: 4,
                        patch: 0
                    }
                ),
                (
                    "striped".to_string(),
                    Version {
                        major: 1,
                        minor: 6,
                        patch: 0
                    }
                ),
                (
                    "thin-pool".to_string(),
                    Version {
                        major: 1,
                        minor: 23,
                        patch: 0
                    }
                ),
            ]
        );
    }

    #[test]
    fn enumeration_borrows_utf8_names_and_stops_on_invalid_text() {
        let mut bytes = synthetic_versions_response(&[("café", [1, 0, 0]), ("zero", [1, 0, 0])]);
        let mut reply = versions(&bytes);
        let name_ptr = reply.command.payload().unwrap()[16..].as_ptr();
        let (name, _) = reply.next().unwrap().unwrap();
        assert_eq!(name, "café");
        assert_eq!(name.as_ptr(), name_ptr);
        assert_eq!(reply.next().unwrap().unwrap().0, "zero");
        assert!(reply.next().is_none());

        bytes[Header::SIZE + 16] = 0xff;
        let mut reply = versions(&bytes);
        assert_eq!(
            reply.next().unwrap().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(reply.next().is_none());
        assert!(reply.next().is_none());
    }

    #[test]
    fn target_iter_stops_on_a_truncated_final_record() {
        // The reply contains fewer than 16 bytes for its last record: the
        // 16-byte-header guard must stop rather than slice out of bounds.
        let mut buf = vec![0u8; Header::SIZE + 8];
        #[allow(clippy::cast_possible_truncation)]
        {
            buf[16..20].copy_from_slice(&(Header::SIZE as u32).to_ne_bytes());
            buf[12..16].copy_from_slice(&((Header::SIZE + 8) as u32).to_ne_bytes());
        }
        assert!(versions(&buf).next().unwrap().is_err());
    }
}
