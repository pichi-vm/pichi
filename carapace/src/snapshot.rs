//! dm-snapshot persistent on-disk header parser. Read through the
//! activated dm-verity device — never directly from the cow partition,
//! per spec §279.
//!
//! Per kernel `drivers/md/dm-snap-persistent.c` (and the v1.0 spec
//! reference), the header is the first 16 bytes of the cow:
//!   magic (u32 LE) | valid (u32 LE) | version (u32 LE) | chunk_size (u32 LE)
//! All numeric fields are little-endian.

use crate::CarapaceError;
use zerocopy::little_endian::U32;
use zerocopy::{FromBytes, Immutable, KnownLayout};

/// "SnAp" little-endian.
pub(crate) const SNAPSHOT_MAGIC: u32 = 0x70416e53;
pub(crate) const SNAPSHOT_HEADER_SIZE: usize = 16;
const SNAPSHOT_VALID: u32 = 1;
const SNAPSHOT_VERSION: u32 = 1;
/// Spec §136 (Parameter Whitelist) + §281: chunk_size MUST equal 8
/// (sectors; equivalent to 4096 bytes). Inter-scute equality is
/// necessary but not sufficient — a chain that internally agrees on
/// `chunk_size = 16` everywhere would have passed if we only checked
/// equality, then misaligned against the activation code's hardcoded
/// `SNAPSHOT_CHUNK_SIZE_SECTORS = 8`. Whitelist the literal value here.
const RDP_CHUNK_SIZE: u32 = 8;

#[derive(FromBytes, KnownLayout, Immutable, Debug, Clone, Copy)]
#[repr(C)]
struct RawSnapshotHeader {
    magic: U32,
    valid: U32,
    version: U32,
    chunk_size: U32,
}

const _: () = assert!(core::mem::size_of::<RawSnapshotHeader>() == SNAPSHOT_HEADER_SIZE);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ValidatedSnapshotHeader {
    /// Snapshot exception-store chunk size, in 512-byte sectors.
    pub chunk_size: u32,
}

impl ValidatedSnapshotHeader {
    /// Parse 16 bytes read through the activated dm-verity device for
    /// the scute at `scute_index` (BASE → TOP). All four invariant
    /// fields (magic / valid / version / chunk_size) are checked; any
    /// mismatch returns [`CarapaceError::SnapshotHeaderInvalid`] with
    /// the scute index attached so the operator can localize the
    /// failure.
    pub fn parse(bytes: &[u8], scute_index: usize) -> Result<Self, CarapaceError> {
        let invalid = |reason: String| CarapaceError::SnapshotHeaderInvalid {
            scute_index,
            reason,
        };
        if bytes.len() < SNAPSHOT_HEADER_SIZE {
            return Err(invalid(format!(
                "need {SNAPSHOT_HEADER_SIZE} bytes, got {}",
                bytes.len()
            )));
        }
        let raw = RawSnapshotHeader::ref_from_bytes(&bytes[..SNAPSHOT_HEADER_SIZE])
            .map_err(|_| invalid("alignment / size".into()))?;

        if raw.magic.get() != SNAPSHOT_MAGIC {
            return Err(invalid(format!(
                "magic = {:#x}, expected {:#x}",
                raw.magic.get(),
                SNAPSHOT_MAGIC
            )));
        }
        if raw.valid.get() != SNAPSHOT_VALID {
            return Err(invalid(format!(
                "valid = {}, expected {}",
                raw.valid.get(),
                SNAPSHOT_VALID
            )));
        }
        if raw.version.get() != SNAPSHOT_VERSION {
            return Err(invalid(format!(
                "version = {}, expected {}",
                raw.version.get(),
                SNAPSHOT_VERSION
            )));
        }
        let chunk_size = raw.chunk_size.get();
        if chunk_size != RDP_CHUNK_SIZE {
            return Err(invalid(format!(
                "chunk_size = {chunk_size}, expected {RDP_CHUNK_SIZE} (spec RDP §136)"
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
        let b = header_bytes(0xdeadbeef, 1, 1, 8);
        assert!(ValidatedSnapshotHeader::parse(&b, 0).is_err());
    }

    #[test]
    fn rejects_invalid_flag() {
        let b = header_bytes(SNAPSHOT_MAGIC, 0, 1, 8);
        assert!(ValidatedSnapshotHeader::parse(&b, 0).is_err());
    }

    #[test]
    fn rejects_unknown_version() {
        let b = header_bytes(SNAPSHOT_MAGIC, 1, 2, 8);
        assert!(ValidatedSnapshotHeader::parse(&b, 0).is_err());
    }

    #[test]
    fn rejects_chunk_size_not_equal_to_eight() {
        // Every value other than the spec-mandated 8 must be rejected,
        // including power-of-two values like 4 and 16 that would have
        // passed a "positive power of two" check.
        for cs in [0u32, 1, 2, 3, 4, 5, 7, 9, 16, 32, 64, 4096] {
            let b = header_bytes(SNAPSHOT_MAGIC, 1, 1, cs);
            assert!(
                ValidatedSnapshotHeader::parse(&b, 0).is_err(),
                "chunk_size = {cs} should fail (only 8 is whitelisted)"
            );
        }
    }

    #[test]
    fn rejects_short_buffer() {
        assert!(ValidatedSnapshotHeader::parse(&[0u8; 10], 0).is_err());
    }

    #[test]
    fn error_carries_scute_index() {
        let b = header_bytes(0xdeadbeef, 1, 1, 8);
        let err = ValidatedSnapshotHeader::parse(&b, 7).unwrap_err();
        assert!(
            err.to_string().contains("scute 7"),
            "expected scute index in error: {err}"
        );
    }
}
