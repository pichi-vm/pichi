//! carapace — assemble carapace block-device chains (read-side only).
//!
//! The composed stack is a cryptographically-bound chain of content-addressable
//! block-device layers ("scutes") presented as a unified, integrity-protected,
//! read-only device validated by a single trust anchor. Given a trusted chain
//! root, this library walks the partitions the kernel exposes via
//! `/sys/class/block/*/uevent` (PARTUUID lookup), validates every scute's
//! parameters against the RDP whitelist, and builds the dm-verity + dm-snapshot
//! stack as `/dev/mapper/<name>`. The on-disk format is specified in `SPEC.md`;
//! producer (chain-authoring) paths live elsewhere — this crate is read-only at
//! the operator surface.
//!
//! Library API: [`attach`], [`detach`], [`validate_dm_name`], and the
//! [`CarapaceError`] type (use [`CarapaceError::is_adversary_rejection`] to
//! distinguish a forged/malformed chain from an operational failure). The
//! `carapace` binary is a thin CLI over this API; in-process consumers (e.g. an
//! initrd probe) call these functions directly.

// `deny` not `forbid` — src/dm/uapi.rs needs module-level
// `#![allow(unsafe_code)]` for iocuddle const constructors. `forbid`
// cannot be overridden (rustc E0453). The CI grep gate at
// ci/grep-unsafe.sh is the authoritative enforcement and is honoured
// even if a future contributor flips a module attribute.
#![deny(unsafe_code)]
#![cfg(target_os = "linux")]

mod assemble;
mod attach;
mod chain;
mod detach;
mod dm;
mod error;
mod name;
mod partition;
mod snapshot;
mod util;
mod verity;

pub use attach::attach;
pub use detach::detach;
pub use error::CarapaceError;
pub use name::validate_dm_name;
