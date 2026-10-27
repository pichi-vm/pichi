// SPDX-License-Identifier: Apache-2.0

use std::io::{self, SeekFrom};

use tokio::io::{AsyncSeek, AsyncSeekExt, AsyncWrite, AsyncWriteExt};

use super::{Drain, Failure, OutputRange, Phase, TreeWriter};

impl<W: AsyncWrite + AsyncSeek + Unpin> TreeWriter<W> {
    async fn initialize_async(&mut self) -> io::Result<()> {
        if self.output_range.is_some() {
            return Ok(());
        }
        let start = self.output.stream_position().await?;
        let end = start.checked_add(self.tree.tree_size()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "hash tree does not fit after the output's current position",
            )
        })?;
        self.output_range = Some(OutputRange { start, end });
        Ok(())
    }

    async fn drain_async(&mut self, mode: Drain) -> io::Result<()> {
        let start = self
            .output_range
            .as_ref()
            .ok_or_else(|| io::Error::other("hash tree output is not initialized"))?
            .start;
        while let Some(block) = self.tree.take_pending(mode) {
            let offset = start.checked_add(block.offset()).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "hash tree output offset overflows",
                )
            })?;
            self.output.seek(SeekFrom::Start(offset)).await?;
            self.output.write_all(block.bytes()).await?;
            self.tree.commit(block)?;
        }
        Ok(())
    }

    /// Accepts the entire chunk, draining completed hash blocks as needed.
    ///
    /// An error or cancellation leaves the writer unusable.
    pub(crate) async fn write_all_async(&mut self, mut input: &[u8]) -> io::Result<()> {
        self.ensure_writable()?;
        if input.is_empty() {
            return Ok(());
        }

        // Restore a usable phase only after every await and state update succeeds.
        self.phase = Phase::Failed;
        self.initialize_async().await?;
        while !input.is_empty() {
            self.drain_async(Drain::Full).await?;
            let copied = self.tree.accept(input).map_err(Failure::into_error)?;
            input = &input[copied..];
        }
        self.phase = Phase::Open;
        self.seal_if_complete();
        Ok(())
    }

    /// Drains the final blocks and flushes output before returning the root.
    pub(crate) async fn finish_async(mut self) -> io::Result<Box<[u8]>> {
        if matches!(self.phase, Phase::Failed) {
            return Err(io::Error::other(
                "cannot use a tree writer after an output failure",
            ));
        }
        if !self.tree.reached_end() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }

        self.initialize_async().await?;
        self.drain_async(Drain::All).await?;
        let digest = self.tree.take_digest().ok_or_else(|| {
            io::Error::other("complete verity input did not produce a root digest")
        })?;
        let end = self
            .output_range
            .as_ref()
            .ok_or_else(|| io::Error::other("hash tree output is not initialized"))?
            .end;
        self.output.seek(SeekFrom::Start(end)).await?;
        self.output.flush().await?;
        Ok(digest)
    }
}
