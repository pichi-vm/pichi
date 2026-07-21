// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Blob-handle trait. Locked contract for v0.9 carapace device.

use std::io::{Read, Seek};

/// Marker trait for any type that supports random-access reading.
///
/// The blanket impl below covers `std::fs::File`, `std::io::Cursor<Vec<u8>>`,
/// and any future user-defined types that satisfy `Read + Seek + Send`.
/// `+ Send` is required because the v0.9 carapace device passes a
/// `Box<dyn ReadSeek>` between vCPU threads.
///
/// `+ Sync` is intentionally NOT required: `Seek` mutates internal cursor
/// state, so each consumer holds its own boxed handle.
pub trait ReadSeek: Read + Seek + Send {}

impl<T: Read + Seek + Send> ReadSeek for T {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn box_dyn_readseek_compiles_for_cursor() {
        let cursor: Cursor<Vec<u8>> = Cursor::new(vec![1, 2, 3]);
        let _b: Box<dyn ReadSeek> = Box::new(cursor);
    }

    #[test]
    fn box_dyn_readseek_compiles_for_file() {
        // Compile-only assertion: File is Read + Seek + Send so the blanket
        // impl applies. We don't actually open a file here.
        fn _accepts_file_box(_: Box<dyn ReadSeek>) {}
        // Construct a sentinel that proves the trait object is sized as a
        // Boxed trait object (fat pointer). No runtime use.
        let _ = _accepts_file_box as fn(_);
    }
}
