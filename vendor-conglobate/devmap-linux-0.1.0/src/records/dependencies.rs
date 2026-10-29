// SPDX-License-Identifier: Apache-2.0

use std::io;

use zerocopy::FromBytes;

use super::Records;
use crate::{DevId, abi::Command};

/// Owned device dependencies, available through [`super::Records`].
pub(crate) struct Dependencies<const N: usize = 4096> {
    command: Command<N>,
    cursor: Option<usize>,
}

impl<const N: usize> Dependencies<N> {
    pub(crate) fn new(command: Command<N>) -> io::Result<Self> {
        let payload = command.payload()?;
        if payload.is_empty() {
            return Ok(Self {
                command,
                cursor: None,
            });
        }
        let ([count, _], data) = <[u32; 2]>::read_from_prefix(payload).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "truncated dependency header")
        })?;
        if count as usize > data.len() / 8 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated dependency array",
            ));
        }
        Ok(Self {
            command,
            cursor: Some(0),
        })
    }
}

impl<const N: usize> Records for Dependencies<N> {
    type Item<'a> = DevId;

    fn next(&mut self) -> Option<io::Result<Self::Item<'_>>> {
        let index = self.cursor.take()?;
        let payload = match self.command.payload() {
            Ok(payload) => payload,
            Err(error) => {
                return Some(Err(io::Error::other(format!(
                    "validated reply became invalid: {error}"
                ))));
            }
        };
        let Ok(([count, _], data)) = <[u32; 2]>::read_from_prefix(payload) else {
            return Some(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated dependency header",
            )));
        };
        if index >= count as usize {
            return None;
        }
        let Some(data) = index.checked_mul(8).and_then(|start| data.get(start..)) else {
            return Some(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid dependency offset",
            )));
        };
        let Ok((device, _)) = u64::read_from_prefix(data) else {
            return Some(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated dependency array",
            )));
        };
        self.cursor = Some(index + 1);
        Some(Ok(DevId::from_kernel(device)))
    }
}

impl<const N: usize> std::fmt::Debug for Dependencies<N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dependencies")
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
        let mut reply = Dependencies::new(Command::<8>::new(header)).unwrap();
        // Only test code can mutate the owned response after validation.
        reply.command.as_mut_bytes()[12..16].copy_from_slice(&0u32.to_ne_bytes());
        assert_eq!(
            reply.next().unwrap().unwrap_err().kind(),
            io::ErrorKind::Other
        );
        assert!(reply.next().is_none());
        assert!(reply.next().is_none());
    }

    #[test]
    fn header_only_reply_has_no_dependencies() {
        let mut header = Header::any();
        header.as_mut_bytes()[12..16].copy_from_slice(&305u32.to_ne_bytes());
        let mut reply = Dependencies::new(Command::<0>::new(header)).unwrap();
        assert!(reply.next().is_none());
        assert!(reply.next().is_none());
    }

    #[test]
    fn dependencies_validate_counts_before_borrowed_iteration() {
        fn collect(reply: &mut impl for<'a> Records<Item<'a> = DevId>) -> Vec<DevId> {
            let mut devices = Vec::new();
            while let Some(device) = reply.next() {
                devices.push(device.unwrap());
            }
            devices
        }
        let mut data = [0; 24];
        data[..4].copy_from_slice(&2u32.to_ne_bytes());
        data[8..16].copy_from_slice(&0xfc01u64.to_ne_bytes());
        data[16..24].copy_from_slice(&0xfc02u64.to_ne_bytes());
        let mut header = Header::any();
        header.as_mut_bytes()[12..16].copy_from_slice(
            &u32::try_from(Header::SIZE + data.len())
                .unwrap()
                .to_ne_bytes(),
        );
        let mut reply = Dependencies::new(Command::from_parts(header, data)).unwrap();
        assert_eq!(
            collect(&mut reply),
            [DevId::new(252, 1).unwrap(), DevId::new(252, 2).unwrap()]
        );
        data[..4].copy_from_slice(&3u32.to_ne_bytes());
        assert_eq!(
            Dependencies::new(Command::from_parts(header, data))
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
}
