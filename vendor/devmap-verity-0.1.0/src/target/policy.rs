// SPDX-License-Identifier: Apache-2.0

/// Action when a block does not match its expected hash.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CorruptionPolicy {
    /// Fail the read.
    #[default]
    Error,
    /// Allow data to be returned despite a detected hash mismatch.
    Ignore,
    /// Request a kernel restart.
    Restart,
    /// Request a kernel panic.
    Panic,
}

/// Action when the backing storage reports an I/O error.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IoErrorPolicy {
    /// Return the I/O error.
    #[default]
    Error,
    /// Restart the machine.
    Restart,
    /// Panic the kernel.
    Panic,
}
