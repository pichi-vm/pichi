// SPDX-License-Identifier: Apache-2.0

use std::io::{self, Seek, SeekFrom, Write};

use super::state::PendingBlock;
use super::{Drain, Failure, FlushMode, OutputRange, Phase, TreeWriter};

impl<W: Write + Seek> TreeWriter<W> {
    fn initialize(&mut self) -> Result<(), Failure> {
        if self.output_range.is_some() {
            return Ok(());
        }
        let start = self.output.stream_position().map_err(Failure::Fatal)?;
        let end = start.checked_add(self.tree.tree_size()).ok_or_else(|| {
            Failure::Fatal(io::Error::new(
                io::ErrorKind::InvalidInput,
                "hash tree does not fit after the output's current position",
            ))
        })?;
        self.output_range = Some(OutputRange { start, end });
        Ok(())
    }

    fn write_pending(&mut self, block: PendingBlock) -> Result<(), Failure> {
        let start = self
            .output_range
            .as_ref()
            .ok_or_else(|| Failure::Fatal(io::Error::other("hash tree output is not initialized")))?
            .start;
        let offset = start.checked_add(block.offset()).ok_or_else(|| {
            Failure::Fatal(io::Error::new(
                io::ErrorKind::InvalidInput,
                "hash tree output offset overflows",
            ))
        })?;
        self.output
            .seek(SeekFrom::Start(offset))
            .map_err(Failure::Fatal)?;
        self.output
            .write_all(block.bytes())
            .map_err(Failure::Fatal)?;
        self.tree.commit(block).map_err(Failure::Fatal)
    }

    fn drain(&mut self, mode: Drain) -> Result<(), Failure> {
        while let Some(block) = self.tree.take_pending(mode) {
            self.write_pending(block)?;
        }
        Ok(())
    }

    fn prepare_for_input(&mut self) -> Result<(), Failure> {
        self.initialize()?;
        self.drain(Drain::Full)
    }

    fn flush_inner(&mut self) -> Result<(), Failure> {
        match self.phase {
            Phase::Failed => {
                return Err(Failure::Fatal(io::Error::other(
                    "cannot use a tree writer after an output failure",
                )));
            }
            Phase::Complete(_) => return self.output.flush().map_err(Failure::Fatal),
            Phase::Open | Phase::Sealed => {}
        }

        self.initialize()?;
        let mode = self.tree.begin_flush();
        if mode == FlushMode::Final {
            self.phase = Phase::Sealed;
        }
        self.drain(mode.drain())?;

        let digest = if mode == FlushMode::Final {
            let digest = self.tree.take_digest().ok_or_else(|| {
                Failure::Fatal(io::Error::other(
                    "complete verity input did not produce a root digest",
                ))
            })?;
            let end = self
                .output_range
                .as_ref()
                .ok_or_else(|| {
                    Failure::Fatal(io::Error::other("hash tree output is not initialized"))
                })?
                .end;
            self.output
                .seek(SeekFrom::Start(end))
                .map_err(Failure::Fatal)?;
            Some(digest)
        } else {
            None
        };

        self.output.flush().map_err(Failure::Fatal)?;
        self.phase = match digest {
            Some(digest) => Phase::Complete(digest),
            None => Phase::Open,
        };
        Ok(())
    }
}

impl<W: Write + Seek> Write for TreeWriter<W> {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        self.ensure_writable()?;
        if input.is_empty() {
            return Ok(0);
        }

        if let Err(failure) = self.prepare_for_input() {
            if matches!(failure, Failure::Fatal(_)) {
                self.phase = Phase::Failed;
            }
            return Err(failure.into_error());
        }

        let copied = self.tree.accept(input).map_err(Failure::into_error)?;
        self.seal_if_complete();
        Ok(copied)
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.flush_inner() {
            Ok(()) => Ok(()),
            Err(Failure::Recoverable(error)) => Err(error),
            Err(Failure::Fatal(error)) => {
                self.phase = Phase::Failed;
                Err(error)
            }
        }
    }
}
