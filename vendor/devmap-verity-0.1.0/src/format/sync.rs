// SPDX-License-Identifier: Apache-2.0

//! Standard-library synchronous formatting.

#[cfg(any(
    feature = "sha1",
    feature = "sha2",
    feature = "sha3",
    feature = "ripemd",
    feature = "whirlpool",
    feature = "streebog",
    feature = "sm3",
    feature = "blake2"
))]
use std::io::{self, Read, Seek, Write};

#[cfg(any(
    feature = "sha1",
    feature = "sha2",
    feature = "sha3",
    feature = "ripemd",
    feature = "whirlpool",
    feature = "streebog",
    feature = "sm3",
    feature = "blake2"
))]
use super::tree::TreeWriter;
#[cfg(any(
    feature = "sha1",
    feature = "sha2",
    feature = "sha3",
    feature = "ripemd",
    feature = "whirlpool",
    feature = "streebog",
    feature = "sm3",
    feature = "blake2"
))]
use crate::{Header, header::RECORD_SIZE};

/// Streams protected data into a standard dm-verity hash volume.
#[cfg(any(
    feature = "sha1",
    feature = "sha2",
    feature = "sha3",
    feature = "ripemd",
    feature = "whirlpool",
    feature = "streebog",
    feature = "sm3",
    feature = "blake2"
))]
#[cfg_attr(
    docsrs,
    doc(cfg(any(
        feature = "sha1",
        feature = "sha2",
        feature = "sha3",
        feature = "ripemd",
        feature = "whirlpool",
        feature = "streebog",
        feature = "sm3",
        feature = "blake2"
    )))
)]
pub trait Format: Sized {
    /// Writes the header and hash tree, returning the root digest.
    ///
    /// The operation consumes exactly the number of data bytes declared by
    /// the header, leaving any additional input unread. The header is written
    /// at the hash stream's current position. The caller owns storage geometry
    /// validation and persistence, and should sync durable storage after this
    /// returns. Pass mutable references when the caller needs to retain either
    /// stream afterward.
    ///
    /// The output occupies [`Header::total_size`] bytes: one hash block for the
    /// padded header prefix followed by [`Header::tree_size`] bytes for the
    /// tree.
    ///
    /// # Errors
    ///
    /// Returns `Unsupported` when the selected algorithm's Cargo feature is
    /// disabled, `InvalidInput` when the header cannot describe a valid
    /// layout, `UnexpectedEof` when the input contains fewer bytes than the
    /// declared shape, or an underlying transport error. Once writing begins,
    /// failure can leave both streams advanced and the output partially
    /// written; the operation does not roll back.
    fn format<R, W>(self, data: R, hash: W) -> io::Result<Box<[u8]>>
    where
        R: Read,
        W: Write + Seek;
}

#[cfg(any(
    feature = "sha1",
    feature = "sha2",
    feature = "sha3",
    feature = "ripemd",
    feature = "whirlpool",
    feature = "streebog",
    feature = "sm3",
    feature = "blake2"
))]
impl Format for Header {
    fn format<R, W>(self, data: R, hash: W) -> io::Result<Box<[u8]>>
    where
        R: Read,
        W: Write + Seek,
    {
        let data_size = self.parameters.data_size()?;
        self.total_size()?;
        let encoded = self.encode();
        let padding = self.prefix_size() - RECORD_SIZE as u64;
        let mut tree = TreeWriter::new(hash, &self.parameters)?;
        tree.output_mut().write_all(&encoded)?;
        io::copy(&mut io::repeat(0).take(padding), tree.output_mut())?;
        let copied = io::copy(&mut data.take(data_size), &mut tree)?;
        if copied != data_size {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        tree.flush()?;
        tree.finish()
    }
}
