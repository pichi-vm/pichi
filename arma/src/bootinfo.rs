//! `TatuBootInfo` wire format — must byte-match `tatu/src/bootinfo.rs`.
//!
//! Arma writes this 44-byte header into the first bytes of the
//! `.tatu.bootinfo` PE section when it materializes the tatu ELF as
//! PE sections. Tatu reads it at runtime via the `BOOTINFO_PAGE`
//! static.

use zerocopy::{Immutable, IntoBytes, KnownLayout};

/// Magic value identifying a `TatuBootInfo` header. Tatu rejects any
/// other prefix.
pub(crate) const MAGIC: [u8; 8] = *b"TATUBOOT";

/// Size of the header on the wire. `const_assert`-pinned below.
pub(crate) const HEADER_SIZE: usize = 44;

/// Total in-memory size of the `.tatu.bootinfo` section. Pinned at
/// 4 KiB to match PMI's small-section granularity (the PE section's
/// `SizeOfRawData` is a multiple of 4 KiB per
/// `pmi/spec/granularity.md`).
pub(crate) const SECTION_SIZE: usize = 4096;

/// Fixed 44-byte header. Packed so the wire layout has no implicit
/// padding. All fields little-endian.
#[repr(C, packed)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout)]
pub(crate) struct TatuBootInfo {
    pub(crate) magic: [u8; 8],
    pub(crate) base_dtb_gpa: u64,
    pub(crate) host_dtbo_gpa: u64,
    pub(crate) kernel_gpa: u64,
    pub(crate) base_dtb_size: u32,
    pub(crate) host_dtbo_size: u32,
    pub(crate) kernel_size: u32,
}

const _: () = {
    assert!(core::mem::size_of::<TatuBootInfo>() == HEADER_SIZE);
};

impl TatuBootInfo {
    /// Build a header from the per-region GPAs and sizes.
    pub(crate) fn new(
        base_dtb_gpa: u64,
        base_dtb_size: u32,
        host_dtbo_gpa: u64,
        host_dtbo_size: u32,
        kernel_gpa: u64,
        kernel_size: u32,
    ) -> Self {
        Self {
            magic: MAGIC,
            base_dtb_gpa,
            host_dtbo_gpa,
            kernel_gpa,
            base_dtb_size,
            host_dtbo_size,
            kernel_size,
        }
    }

    /// Render the header as a 4 KiB `.tatu.bootinfo` section payload:
    /// 44 bytes of header followed by 4052 bytes of zero padding.
    pub(crate) fn to_section_bytes(self) -> Vec<u8> {
        let mut v = vec![0u8; SECTION_SIZE];
        v[..HEADER_SIZE].copy_from_slice(self.as_bytes());
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_44_bytes() {
        assert_eq!(core::mem::size_of::<TatuBootInfo>(), HEADER_SIZE);
    }

    #[test]
    fn section_bytes_have_correct_size_and_magic() {
        let bi = TatuBootInfo::new(0x1000, 0x100, 0x2000, 0x200, 0x3000, 0x400);
        let bytes = bi.to_section_bytes();
        assert_eq!(bytes.len(), SECTION_SIZE);
        assert_eq!(&bytes[..8], &MAGIC);
        // Trailing pad is zero.
        assert!(bytes[HEADER_SIZE..].iter().all(|&b| b == 0));
    }

    #[test]
    fn field_layout_matches_tatu() {
        let bi = TatuBootInfo::new(
            0x1122_3344_5566_7788,
            0x9999,
            0xAABB_CCDD_EEFF_0011,
            0x2222,
            0x3333_4444_5555_6666,
            0x7777,
        );
        let bytes = bi.as_bytes();
        // Magic at 0..8
        assert_eq!(&bytes[0..8], &MAGIC);
        // base_dtb_gpa at 8..16 LE
        assert_eq!(
            u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            0x1122_3344_5566_7788
        );
        // host_dtbo_gpa at 16..24 LE
        assert_eq!(
            u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
            0xAABB_CCDD_EEFF_0011
        );
        // kernel_gpa at 24..32 LE
        assert_eq!(
            u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
            0x3333_4444_5555_6666
        );
        // base_dtb_size at 32..36
        assert_eq!(
            u32::from_le_bytes(bytes[32..36].try_into().unwrap()),
            0x9999
        );
        // host_dtbo_size at 36..40
        assert_eq!(
            u32::from_le_bytes(bytes[36..40].try_into().unwrap()),
            0x2222
        );
        // kernel_size at 40..44
        assert_eq!(
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
            0x7777
        );
    }
}
