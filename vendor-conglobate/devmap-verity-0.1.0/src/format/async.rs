// SPDX-License-Identifier: Apache-2.0

//! Tokio asynchronous formatting.

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
use std::{future::Future, io};

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
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncWrite, AsyncWriteExt};

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

/// Asynchronously streams protected data into a dm-verity hash volume.
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
    doc(cfg(all(
        feature = "tokio",
        any(
            feature = "sha1",
            feature = "sha2",
            feature = "sha3",
            feature = "ripemd",
            feature = "whirlpool",
            feature = "streebog",
            feature = "sm3",
            feature = "blake2"
        )
    )))
)]
pub trait AsyncFormat: Sized {
    /// Writes the header and hash tree, returning the root digest.
    ///
    /// This is the asynchronous form of
    /// [`crate::Format::format`] and has the same sizing and
    /// positioning contract. The caller owns storage geometry validation and
    /// durable synchronization. Cancellation after writing begins can leave
    /// both streams advanced and the output partially written.
    ///
    /// # Errors
    ///
    /// Returns the same error kinds as [`crate::Format::format`].
    fn format<R, W>(self, data: R, hash: W) -> impl Future<Output = io::Result<Box<[u8]>>> + Send
    where
        R: AsyncRead + Unpin + Send,
        W: AsyncWrite + AsyncSeek + Unpin + Send;
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
impl AsyncFormat for Header {
    async fn format<R, W>(self, data: R, hash: W) -> io::Result<Box<[u8]>>
    where
        R: AsyncRead + Unpin + Send,
        W: AsyncWrite + AsyncSeek + Unpin + Send,
    {
        let data_size = self.parameters.data_size()?;
        self.total_size()?;
        let encoded = self.encode();
        let padding = self.prefix_size() - RECORD_SIZE as u64;
        let mut tree = TreeWriter::new(hash, &self.parameters)?;
        tree.output_mut().write_all(&encoded).await?;
        tokio::io::copy(&mut tokio::io::repeat(0).take(padding), tree.output_mut()).await?;
        let mut data = data.take(data_size);
        let block_size = self.parameters.data.size.bytes().get() as usize;
        let mut buffer = vec![0; block_size].into_boxed_slice();
        while data.limit() != 0 {
            data.read_exact(&mut buffer).await?;
            tree.write_all_async(&buffer).await?;
        }
        tree.finish_async().await
    }
}
