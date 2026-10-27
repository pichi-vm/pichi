// SPDX-License-Identifier: Apache-2.0

use std::io;

use devmap_core::Span;
use zerocopy::byteorder::native_endian::{I32, U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

const DM_MAX_TYPE_NAME: usize = 16;

/// The fixed part of a target row, borrowed directly from an ioctl buffer.
///
/// On load, `next` is relative to this record. On status, it is relative
/// to the first record. Traversal belongs to the corresponding operation.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
pub(crate) struct TargetSpec {
    sector_start: U64,
    length: U64,
    status: I32,
    next: U32,
    target_type: [u8; DM_MAX_TYPE_NAME],
}

const _: () = {
    use core::mem::{offset_of, size_of};
    assert!(size_of::<TargetSpec>() == 40);
    assert!(offset_of!(TargetSpec, sector_start) == 0);
    assert!(offset_of!(TargetSpec, length) == 8);
    assert!(offset_of!(TargetSpec, status) == 16);
    assert!(offset_of!(TargetSpec, next) == 20);
    assert!(offset_of!(TargetSpec, target_type) == 24);
};

impl TargetSpec {
    pub(crate) const SIZE: usize = core::mem::size_of::<Self>();

    pub(crate) fn new(span: Span<u64>, name: &str) -> io::Result<Self> {
        if name.is_empty()
            || name.len() >= DM_MAX_TYPE_NAME
            || name
                .bytes()
                .any(|byte| byte == 0 || byte.is_ascii_whitespace())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid dm target type name: {name:?}"),
            ));
        }
        let mut target_type = [0; DM_MAX_TYPE_NAME];
        target_type[..name.len()].copy_from_slice(name.as_bytes());
        Ok(Self {
            sector_start: span.start.into(),
            length: span.length.into(),
            status: I32::ZERO,
            next: U32::ZERO,
            target_type,
        })
    }

    pub(crate) fn span(&self) -> Span<u64> {
        Span::new(self.sector_start.get(), self.length.get())
    }

    pub(crate) fn name(&self) -> io::Result<&str> {
        let end = self
            .target_type
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(self.target_type.len());
        std::str::from_utf8(&self.target_type[..end])
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    pub(crate) fn next(&self) -> u32 {
        self.next.get()
    }

    #[must_use]
    pub(crate) fn with_next(mut self, next: u32) -> Self {
        self.next.set(next);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_layout_and_unaligned_borrow() {
        let spec = TargetSpec::new(Span::new(123, 456), "linear")
            .unwrap()
            .with_next(48);
        let bytes = spec.as_bytes();
        assert_eq!(&bytes[..8], &123u64.to_ne_bytes());
        assert_eq!(&bytes[8..16], &456u64.to_ne_bytes());
        assert_eq!(&bytes[16..20], &[0; 4]);
        assert_eq!(&bytes[20..24], &48u32.to_ne_bytes());
        assert_eq!(&bytes[24..], b"linear\0\0\0\0\0\0\0\0\0\0");

        let mut storage = [0u64; 6];
        let unaligned = &mut storage.as_mut_bytes()[1..41];
        unaligned.copy_from_slice(bytes);
        let borrowed = TargetSpec::mut_from_bytes(unaligned).unwrap();
        assert_eq!(borrowed.span(), Span::new(123, 456));
        assert_eq!(borrowed.name().unwrap(), "linear");
        assert_eq!(borrowed.next(), 48);
        assert_eq!(&unaligned[20..24], &48u32.to_ne_bytes());
    }

    #[test]
    fn invalid_names_are_rejected_on_write_and_invalid_utf8_on_read() {
        for name in ["", "bad name", "bad\0name", "0123456789abcdef"] {
            assert!(TargetSpec::new(Span::new(0, 1), name).is_err());
        }
        let mut bytes = [0; TargetSpec::SIZE];
        bytes[24] = 0xff;
        let spec = TargetSpec::ref_from_bytes(&bytes).unwrap();
        assert_eq!(spec.name().unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}
