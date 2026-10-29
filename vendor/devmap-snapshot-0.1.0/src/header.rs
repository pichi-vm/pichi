// SPDX-License-Identifier: Apache-2.0

use crate::Constraint;
use devmap_core::BlockSize;
use std::{io, num::NonZero};

/// The on-disk header of a version-one persistent snapshot store.
///
/// The 16-byte record occupies the start of the copy-on-write (COW) store's
/// first chunk. The remaining chunks interleave exception metadata and data;
/// use [`Self::metadata_chunk_index`] to locate metadata chunks.
/// Encoding and decoding operate on bytes without performing I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Whether the snapshot is marked valid on disk.
    ///
    /// A false value marks an invalidated snapshot, whose contents must not be
    /// trusted. Setting this to true does not repair its contents, and a true
    /// value alone does not establish the integrity of the store.
    pub valid: bool,
    /// The chunk size in bytes, encoded on disk in 512-byte sectors.
    pub chunk_size: BlockSize<Constraint>,
}

impl Header {
    /// Decodes a 16-byte little-endian header record.
    ///
    /// Invalidated snapshots decode successfully with [`Self::valid`] set to
    /// false. Successful decoding validates only the header, not the exception
    /// store or the remaining bytes of the header chunk.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidData`] for an incorrect magic or version,
    /// a validity flag other than zero or one, or a chunk size outside
    /// [`Constraint`]'s supported powers of two.
    pub fn decode(bytes: [u8; 16]) -> io::Result<Self> {
        let word = |i| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid snapshot header");
        if word(0) != 0x7041_6e53 || word(8) != 1 || word(4) > 1 {
            return Err(invalid());
        }
        let chunk_size = word(12)
            .checked_mul(512)
            .and_then(NonZero::new)
            .and_then(BlockSize::from_bytes)
            .ok_or_else(invalid)?;
        Ok(Self {
            valid: word(4) != 0,
            chunk_size,
        })
    }

    /// Encodes a 16-byte little-endian header record, preserving [`Self::valid`].
    ///
    /// The caller supplies padding through the end of the header chunk.
    pub fn encode(&self) -> [u8; 16] {
        let mut bytes = [0; 16];
        for (field, value) in bytes.chunks_exact_mut(4).zip([
            0x7041_6e53,
            u32::from(self.valid),
            1,
            self.chunk_size.bytes().get() / 512,
        ]) {
            field.copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    /// Returns the number of 16-byte [`crate::Exception`] records that fit in
    /// one metadata chunk, including any end-of-list record.
    pub fn exceptions_per_chunk(&self) -> u64 {
        u64::from(self.chunk_size.bytes().get()) / 16
    }

    /// Returns the COW-store chunk index of the `index`th metadata chunk,
    /// numbered from zero.
    ///
    /// The result is a chunk index, not a byte offset. This calculation does not
    /// check whether the metadata chunk exists in the store.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] if the chunk index overflows `u64`.
    ///
    /// # Examples
    ///
    /// With 4 KiB chunks, each metadata chunk holds 256 exception records.
    /// The first metadata chunk follows the header; the next follows 256 data
    /// chunks.
    ///
    /// ```
    /// use devmap_snapshot::Header;
    ///
    /// let header = Header {
    ///     valid: true,
    ///     chunk_size: Default::default(), // 4 KiB.
    /// };
    /// assert_eq!(header.exceptions_per_chunk(), 256);
    /// assert_eq!(header.metadata_chunk_index(0)?, 1);
    /// assert_eq!(header.metadata_chunk_index(1)?, 258);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn metadata_chunk_index(&self, index: u64) -> io::Result<u64> {
        index
            .checked_mul(self.exceptions_per_chunk() + 1)
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "snapshot layout overflow"))
    }
}
