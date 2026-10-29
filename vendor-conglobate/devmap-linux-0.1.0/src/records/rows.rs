// SPDX-License-Identifier: Apache-2.0

use std::{fmt, io, marker::PhantomData};

use zerocopy::FromBytes;

use super::{Records, Row, mode::Mode};
use crate::abi::{Command, TargetSpec};

/// Owned target rows in table or info mode, backed by an inline ioctl buffer.
///
/// [`Records::next`] lends one row at a time without allocating or copying parameters.
/// `N` is the payload capacity in bytes, excluding the ioctl header.
///
/// Truncated headers, out-of-bounds intermediate offsets, and invalid UTF-8
/// target names yield `InvalidData` and end iteration. Decoding accepts an
/// absent NUL terminator within a bounded field. A zero next offset ends
/// iteration even if the header reports more rows; the final next offset
/// is ignored because it may include padding beyond the reply.
///
pub(crate) struct Rows<M: Mode, const N: usize = 4096> {
    command: Command<N>,
    cursor: Option<(usize, u32)>,
    mode: PhantomData<M>,
}

impl<M: Mode, const N: usize> Rows<M, N> {
    pub(crate) fn new(command: Command<N>) -> io::Result<Self> {
        command.payload()?;
        Ok(Self {
            command,
            mode: PhantomData,
            cursor: Some((0, 0)),
        })
    }
}

impl<M: Mode, const N: usize> Records for Rows<M, N> {
    type Item<'a> = Row<'a, M>;

    fn next(&mut self) -> Option<io::Result<Self::Item<'_>>> {
        let (offset, index) = self.cursor.take()?;
        let payload = match self.command.payload() {
            Ok(payload) => payload,
            Err(error) => {
                return Some(Err(io::Error::other(format!(
                    "validated reply became invalid: {error}"
                ))));
            }
        };
        let end = payload.len();
        let count = self.command.header().target_count();
        if index >= count {
            return None;
        }
        let remaining = count - index;
        let Some(record) = payload.get(offset..) else {
            return Some(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "dm target spec offset outside response",
            )));
        };
        let Ok((spec, params)) = TargetSpec::ref_from_prefix(record) else {
            return Some(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated dm target spec",
            )));
        };
        let spec_end = end - params.len();
        let next = spec.next();

        // The last row's next offset can include padding beyond data_size.
        let record_end = if next == 0 || remaining == 1 {
            end
        } else {
            let next_offset = next as usize;
            if next_offset < spec_end || next_offset > end {
                return Some(Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "dm target next offset outside response",
                )));
            }
            next_offset
        };

        let type_name = match spec.name() {
            Ok(name) => name,
            Err(error) => {
                return Some(Err(error));
            }
        };

        let params = &payload[spec_end..record_end];
        let params_end = params
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(params.len());

        if next != 0 {
            self.cursor = Some((record_end, index + 1));
        }

        Some(Ok(Row::new(spec.span(), type_name, &params[..params_end])))
    }
}

impl<M: Mode, const N: usize> fmt::Debug for Rows<M, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Rows")
            .field("target_count", &self.command.header().target_count())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)] // test fixtures: sizes are tiny, never near u32::MAX
mod tests {
    use devmap_core::Span;
    use zerocopy::IntoBytes as _;

    use super::super::mode::Table;
    use super::*;
    use crate::abi::Header;

    #[test]
    fn invalidated_reply_returns_one_error_then_stops() {
        let mut header = Header::any();
        header.as_mut_bytes()[12..16]
            .copy_from_slice(&u32::try_from(Header::SIZE + 8).unwrap().to_ne_bytes());
        let mut reply = Rows::<Table, 8>::new(Command::<8>::new(header)).unwrap();
        // Only test code can mutate the owned response after validation.
        reply.command.as_mut_bytes()[12..16].copy_from_slice(&0u32.to_ne_bytes());
        assert_eq!(
            reply.next().unwrap().unwrap_err().kind(),
            io::ErrorKind::Other
        );
        assert!(reply.next().is_none());
        assert!(reply.next().is_none());
    }

    /// Hand-builds a synthetic `DM_TABLE_STATUS`-shaped response buffer:
    /// header + N `dm_target_spec` entries whose `next` fields use the
    /// *read*-direction convention (offset from the *first* spec's start),
    /// each followed by a NUL-terminated status string.
    fn synthetic_table_status_response(entries: &[(&[u8], &str)]) -> Vec<u8> {
        let first = Header::SIZE;
        let payload_len = entries
            .iter()
            .map(|(_, params)| (TargetSpec::SIZE + params.len() + 1).next_multiple_of(8))
            .sum::<usize>();
        let mut bytes = vec![0u8; first + payload_len];
        let header = Header::by_dev(0).with_target_count(entries.len() as u32);
        bytes[..Header::SIZE].copy_from_slice(header.as_bytes());
        let size = u32::try_from(bytes.len()).unwrap();
        bytes[12..16].copy_from_slice(&size.to_ne_bytes());

        let mut abs_offset_from_first = 0usize;
        for (i, (type_name, params)) in entries.iter().enumerate() {
            let abs_offset = first + abs_offset_from_first;
            let aligned_len = (TargetSpec::SIZE + params.len() + 1).next_multiple_of(8);
            let next = if i == entries.len() - 1 {
                0
            } else {
                (abs_offset_from_first + aligned_len) as u32
            };

            bytes[abs_offset..abs_offset + 8].copy_from_slice(&(i as u64 * 10).to_ne_bytes());
            bytes[abs_offset + 8..abs_offset + 16].copy_from_slice(&(i as u64 + 1).to_ne_bytes());
            bytes[abs_offset + 20..abs_offset + 24].copy_from_slice(&next.to_ne_bytes());
            let type_field = &mut bytes[abs_offset + 24..abs_offset + 24 + 16];
            type_field[..type_name.len()].copy_from_slice(type_name);

            let param_start = abs_offset + TargetSpec::SIZE;
            bytes[param_start..param_start + params.len()].copy_from_slice(params.as_bytes());

            abs_offset_from_first += aligned_len;
        }

        bytes
    }

    fn command(bytes: &[u8]) -> Command<4096> {
        let mut command = Command::from_parts(Header::by_dev(0), [0; 4096]);
        command.as_mut_bytes()[..bytes.len()].copy_from_slice(bytes);
        command
    }

    #[test]
    fn rows_borrow_an_unaligned_payload_at_the_reported_start() {
        let mut bytes = synthetic_table_status_response(&[(b"zero", ""), (b"linear", "252:5 5")]);
        bytes.insert(Header::SIZE, 0xff);
        let len = u32::try_from(bytes.len()).unwrap();
        bytes[12..16].copy_from_slice(&len.to_ne_bytes());
        bytes[16..20].copy_from_slice(&313u32.to_ne_bytes());
        let mut response = Rows::<Table, 4096>::new(command(&bytes)).unwrap();
        let name_ptr = response.command.payload().unwrap()[24..].as_ptr();
        let first = response.next().unwrap().unwrap();
        assert_eq!(first.name(), "zero");
        assert_eq!(first.name().as_ptr(), name_ptr);
        let second = response.next().unwrap().unwrap();
        assert_eq!(second.span(), Span::new(10, 2));
        assert_eq!(second.name(), "linear");
        assert!(response.next().is_none());
    }

    #[test]
    fn final_row_next_can_extend_past_the_reported_bytes() {
        let mut bytes = synthetic_table_status_response(&[(b"zero", ""), (b"zero", "")]);
        let last = Header::SIZE + 48;
        bytes[last + 20..last + 24].copy_from_slice(&96u32.to_ne_bytes());
        bytes.truncate(last + TargetSpec::SIZE + 1);
        let mut command = command(&bytes);
        command.as_mut_bytes()[12..16]
            .copy_from_slice(&u32::try_from(bytes.len()).unwrap().to_ne_bytes());
        let mut response = Rows::<Table, 4096>::new(command).unwrap();
        assert!(response.next().unwrap().is_ok());
        let row = response.next().unwrap().unwrap();
        assert_eq!(row.name(), "zero");
        assert_eq!(
            row.parse::<crate::targets::Zero>(crate::targets::Version {
                major: 1,
                minor: 0,
                patch: 0
            })
            .unwrap(),
            crate::targets::Zero
        );
    }

    #[test]
    fn malformed_row_boundaries_stop_iteration_safely() {
        // A last-row `next` of zero wins over an excessive target count.
        let mut bytes = synthetic_table_status_response(&[(b"zero", "")]);
        let (header, _) = Header::mut_from_prefix(&mut bytes).unwrap();
        *header = header.with_target_count(3);
        let mut table = Rows::<Table, 4096>::new(command(&bytes)).unwrap();
        assert!(table.next().unwrap().is_ok());
        assert!(table.next().is_none());

        // An unterminated type field stays within its fixed-width field.
        let mut bytes = synthetic_table_status_response(&[(b"zero", "")]);
        let type_off = Header::SIZE + 24;
        bytes[type_off..type_off + 16].copy_from_slice(b"abcdefghijklmnop");
        let mut table = Rows::<Table, 4096>::new(command(&bytes)).unwrap();
        assert_eq!(table.next().unwrap().unwrap().name(), "abcdefghijklmnop");

        // Unterminated parameters are bounded by the reply, not rejected.
        let mut bytes = synthetic_table_status_response(&[(b"zero", "abc")]);
        bytes.truncate(Header::SIZE + TargetSpec::SIZE + 3);
        let len = u32::try_from(bytes.len()).unwrap();
        bytes[12..16].copy_from_slice(&len.to_ne_bytes());
        let mut table = Rows::<Table, 4096>::new(command(&bytes)).unwrap();
        assert!(table.next().unwrap().is_ok());

        // A truncated spec is an explicit error rather than a clean end.
        let mut bytes = synthetic_table_status_response(&[(b"zero", "")]);
        bytes.truncate(Header::SIZE + 8);
        let mut command = command(&bytes);
        command.as_mut_bytes()[12..16]
            .copy_from_slice(&u32::try_from(bytes.len()).unwrap().to_ne_bytes());
        let mut rows = Rows::<Table, 4096>::new(command).unwrap();
        let error = rows.next().unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(rows.next().is_none());
    }
}
