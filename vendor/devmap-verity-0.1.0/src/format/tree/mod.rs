// SPDX-License-Identifier: Apache-2.0

use std::io;

use crate::Parameters;
use state::State;

#[cfg(feature = "tokio")]
mod r#async;
mod state;
mod sync;

/// Writes a dm-verity hash tree to a seekable output.
///
/// Write exactly the number of complete data blocks declared by the
/// shape. Input slices may have any length; only fragments are copied
/// into the internal block buffer. Complete aligned blocks are hashed directly.
///
/// The synchronous `Write` implementation drains available hash output on
/// `flush`, without padding or sealing incomplete input. Call [`finish`](Self::finish)
/// after the exact input length has been written and flushed. The asynchronous
/// path accepts chunks with `write_all_async` and drains and flushes them with
/// `finish_async`. Dropping the writer does not flush it.
/// The writer cannot be used after an output error.
#[allow(missing_debug_implementations)]
#[must_use = "dropping a tree writer does not finish the hash tree"]
pub(crate) struct TreeWriter<W> {
    output: W,
    tree: State,
    output_range: Option<OutputRange>,
    phase: Phase,
}

impl<W> TreeWriter<W> {
    /// Creates a tree writer at the output's current position.
    ///
    /// This writes only the hash tree. Write the encoded
    /// header and its block padding first. Creating the
    /// writer performs no I/O.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::Unsupported`] if the feature for the selected
    /// hash algorithm is not enabled.
    pub(crate) fn new(output: W, parameters: &Parameters) -> io::Result<Self> {
        let hasher = parameters.algorithm.hasher().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "hash algorithm support is not enabled",
            )
        })?;

        Ok(Self {
            output,
            tree: State::new(parameters, hasher)?,
            output_range: None,
            phase: Phase::Open,
        })
    }

    pub(crate) const fn output_mut(&mut self) -> &mut W {
        &mut self.output
    }

    /// Returns the root only after a successful final flush.
    pub(crate) fn finish(self) -> io::Result<Box<[u8]>> {
        match self.phase {
            Phase::Complete(digest) => Ok(digest),
            Phase::Open => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "verity input has not reached its final data block",
            )),
            Phase::Failed => Err(io::Error::other(
                "cannot use a tree writer after an output failure",
            )),
            Phase::Sealed => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "verity tree must be flushed before its digest is available",
            )),
        }
    }

    fn ensure_writable(&self) -> io::Result<()> {
        match self.phase {
            Phase::Open => Ok(()),
            Phase::Failed => Err(io::Error::other(
                "cannot use a tree writer after an output failure",
            )),
            Phase::Sealed | Phase::Complete(_) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "verity input is already complete",
            )),
        }
    }

    fn seal_if_complete(&mut self) {
        if self.tree.reached_end() {
            self.phase = Phase::Sealed;
        }
    }
}

struct OutputRange {
    start: u64,
    end: u64,
}

enum Phase {
    Open,
    Sealed,
    Complete(Box<[u8]>),
    Failed,
}

#[derive(Clone, Copy)]
enum Drain {
    Full,
    All,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FlushMode {
    Intermediate,
    Final,
}

impl FlushMode {
    const fn drain(self) -> Drain {
        match self {
            Self::Intermediate => Drain::Full,
            Self::Final => Drain::All,
        }
    }
}

enum Failure {
    Recoverable(io::Error),
    Fatal(io::Error),
}

impl Failure {
    fn into_error(self) -> io::Error {
        match self {
            Self::Recoverable(error) | Self::Fatal(error) => error,
        }
    }
}
