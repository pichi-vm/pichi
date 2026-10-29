// SPDX-License-Identifier: Apache-2.0

/// An on-disk chunk mapping in a version-one persistent snapshot store.
///
/// An exception maps an origin chunk to its snapshot data in the copy-on-write
/// (COW) store. Both indices use the chunk size in [`crate::Header`], not sectors
/// or bytes. A record with [`Self::new_chunk`] set to zero terminates the list;
/// its [`Self::old_chunk`] is ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exception {
    /// Zero-based chunk index in the origin.
    pub old_chunk: u64,
    /// Zero-based chunk index in the COW store, or zero to terminate the list.
    pub new_chunk: u64,
}

impl Exception {
    /// Decodes a 16-byte little-endian exception record.
    ///
    /// All bit patterns decode successfully, including end-of-list records.
    /// The caller must check the terminator and validate nonterminal mappings
    /// against the origin and COW-store sizes and the store's metadata layout.
    pub fn decode(bytes: [u8; 16]) -> Self {
        let mut old = [0; 8];
        let mut new = [0; 8];
        old.copy_from_slice(&bytes[..8]);
        new.copy_from_slice(&bytes[8..]);
        Self {
            old_chunk: u64::from_le_bytes(old),
            new_chunk: u64::from_le_bytes(new),
        }
    }

    /// Encodes a 16-byte little-endian exception record without validating its
    /// chunk indices.
    ///
    /// A zero [`Self::new_chunk`] encodes an end-of-list record.
    pub fn encode(&self) -> [u8; 16] {
        let mut bytes = [0; 16];
        bytes[..8].copy_from_slice(&self.old_chunk.to_le_bytes());
        bytes[8..].copy_from_slice(&self.new_chunk.to_le_bytes());
        bytes
    }
}
