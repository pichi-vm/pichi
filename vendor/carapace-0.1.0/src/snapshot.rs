// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! dm-snapshot persistent on-disk header parser. Read through the
//! activated dm-verity device — never directly from the data partition,
//! per the specification’s “Consumer Verification and Setup” section.
//!
//! Per kernel `drivers/md/dm-snap-persistent.c` (and the v1.0 spec
//! reference), the header is the first 16 bytes of the data file:
//!   magic (u32 LE) | valid (u32 LE) | version (u32 LE) | chunk_size (u32 LE)
//! All numeric fields are little-endian.

use std::io;
/// "SnAp" little-endian.
#[cfg(test)]
pub(crate) const SNAPSHOT_MAGIC: u32 = 0x7041_6e53;
pub(crate) const SNAPSHOT_HEADER_SIZE: usize = 16;
/// The specification’s Parameter Whitelist requires chunk_size to equal 8
/// (sectors; equivalent to 4096 bytes).
const RDP_CHUNK_SIZE: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ValidatedSnapshotHeader {
    /// Snapshot exception-store chunk size, in 512-byte sectors.
    pub chunk_size: u32,
}

impl ValidatedSnapshotHeader {
    /// Parse 16 bytes read through the activated dm-verity device for
    /// the scute at `scute_index` (BASE → TOP). All four invariant
    /// fields (magic / valid / version / chunk_size) are checked.
    pub(crate) fn parse(bytes: &[u8], scute_index: usize) -> io::Result<Self> {
        let invalid = |reason: String| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("scute {scute_index}: snapshot header: {reason}"),
            )
        };
        if bytes.len() < SNAPSHOT_HEADER_SIZE {
            return Err(invalid(format!(
                "need {SNAPSHOT_HEADER_SIZE} bytes, got {}",
                bytes.len()
            )));
        }
        let record = bytes[..SNAPSHOT_HEADER_SIZE]
            .try_into()
            .map_err(|_| invalid("record size".into()))?;
        let header =
            devmap_snapshot::Header::decode(record).map_err(|error| invalid(error.to_string()))?;
        if !header.valid {
            return Err(invalid("snapshot is invalidated".into()));
        }
        let chunk_size = header.chunk_size.bytes().get() / 512;
        if chunk_size != RDP_CHUNK_SIZE {
            return Err(invalid(format!(
                "chunk_size = {chunk_size}, expected {RDP_CHUNK_SIZE} (spec Parameter Whitelist)"
            )));
        }
        Ok(Self { chunk_size })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_bytes(magic: u32, valid: u32, version: u32, chunk_size: u32) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0..4].copy_from_slice(&magic.to_le_bytes());
        b[4..8].copy_from_slice(&valid.to_le_bytes());
        b[8..12].copy_from_slice(&version.to_le_bytes());
        b[12..16].copy_from_slice(&chunk_size.to_le_bytes());
        b
    }

    #[test]
    fn parses_canonical_header() {
        let b = header_bytes(SNAPSHOT_MAGIC, 1, 1, 8);
        let h = ValidatedSnapshotHeader::parse(&b, 0).unwrap();
        assert_eq!(h.chunk_size, 8);
    }

    #[test]
    fn rejects_wrong_magic() {
        let b = header_bytes(0xdead_beef, 1, 1, 8);
        let e = ValidatedSnapshotHeader::parse(&b, 0).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_invalid_flag() {
        let b = header_bytes(SNAPSHOT_MAGIC, 0, 1, 8);
        let e = ValidatedSnapshotHeader::parse(&b, 0).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_unknown_version() {
        let b = header_bytes(SNAPSHOT_MAGIC, 1, 2, 8);
        let e = ValidatedSnapshotHeader::parse(&b, 0).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_chunk_size_not_equal_to_eight() {
        for cs in [0u32, 1, 2, 3, 4, 5, 7, 9, 16, 32, 64, 4096] {
            let b = header_bytes(SNAPSHOT_MAGIC, 1, 1, cs);
            let e = ValidatedSnapshotHeader::parse(&b, 0).unwrap_err();
            assert_eq!(
                e.kind(),
                io::ErrorKind::InvalidData,
                "chunk_size = {cs} should fail"
            );
        }
    }

    #[test]
    fn rejects_short_buffer() {
        let e = ValidatedSnapshotHeader::parse(&[0u8; 10], 0).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn error_carries_scute_index() {
        let b = header_bytes(0xdead_beef, 1, 1, 8);
        let err = ValidatedSnapshotHeader::parse(&b, 7).unwrap_err();
        assert!(
            err.to_string().contains("scute 7"),
            "expected scute index in error: {err}"
        );
    }
}
