//! carapace assembler — read-side only.
//!
//! Given a trusted chain root, walk the partitions visible to the
//! kernel via `/sys/class/block/*/uevent` (PARTUUID lookup), validate
//! every scute's parameters against the RDP whitelist, and build the
//! dm-verity + dm-snapshot stack as `/dev/mapper/<name>`.
//!
//! The producer (chain authoring) lives in a separate project. This
//! crate is read-only at the operator surface and contains no write
//! paths into scute partitions.

// `deny` not `forbid` — src/dm/uapi.rs needs module-level
// `#![allow(unsafe_code)]` for iocuddle const constructors. `forbid`
// cannot be overridden (rustc E0453). The CI grep gate at
// ci/grep-unsafe.sh is the authoritative enforcement and is honoured
// even if a future contributor flips a module attribute.
#![deny(unsafe_code)]
#![cfg(target_os = "linux")]

mod assemble;
mod chain;
mod cli;
mod dm;
mod error;
mod partition;
mod snapshot;
mod util;
mod verity;

use error::CarapaceError;
use std::process::ExitCode;

fn main() -> ExitCode {
    cli::run()
}
