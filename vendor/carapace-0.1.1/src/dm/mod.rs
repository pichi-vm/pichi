// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Device-mapper wrapper, carved out of the `carapace` crate so producers
//! other than the read-only assembler (notably `conglobate`) can reuse it.
//!
//! * [`uapi`] — kernel UAPI mirrors + iocuddle ioctl-number declarations. The
//!   ONLY module with `#![allow(unsafe_code)]`.
//! * [`header`] — `DmHeader`: safe `#[repr(transparent)]` newtype over
//!   `dm_ioctl_raw`.
//! * [`table`] — `TargetSpec` / `TableLine` / `DmTable` (operator model) +
//!   `DmTableBuf` (kernel-ABI byte buffer for `DM_TABLE_LOAD`).
//! * [`device`] — `DmDevice` RAII handle (create / load_table / resume /
//!   drop=remove) + `remove_by_name` + `list_devices_with_prefix`.
//!
//! This crate is **chain-agnostic**: it knows kernel ABI, dm-table
//! rendering, and per-device RAII. It does NOT know what a "scute" is —
//! the verity target type takes the hash algorithm as a plain name
//! (`"sha256"`), not a carapace `Algorithm`. The orchestrator that
//! bridges a validated chain to these primitives lives in `carapace`
//! (`carapace::assemble`).
//!
//! Iocuddle paradigm: the kernel UAPI structs (`dm_ioctl_raw` /
//! `dm_target_spec_raw`) are `pub(super)` to `uapi` — visible to the
//! sibling modules that wrap them, invisible outside the crate. The
//! `unsafe { Group::write_read(N) }` declarations live in `uapi` and
//! reference newtypes in `header` / `table`, which uphold their
//! invariants by construction.

// Linux-only: every primitive here is a `/dev/mapper/control` ioctl.
// Mirrors carapace's own `#![cfg(target_os = "linux")]` floor.

mod device;
mod header;
mod table;
mod uapi;

pub use device::{
    list_devices_with_prefix, open_dm_control, remove_by_name, DmCreateMode, DmDevice,
};
pub use table::{DmTable, TableLine, TargetSpec};

use thiserror::Error;

/// Errors from the device-mapper layer. Operational failures only — this
/// crate validates nothing chain-related (no superblocks, no salts). The
/// `carapace` crate maps these into its richer `CarapaceError` via
/// `From<DmError>`. Not `#[non_exhaustive]`: the set is closed, and an
/// exhaustive `From` in `carapace` should fail to compile if it grows.
#[derive(Debug, Error)]
pub enum DmError {
    #[error("usage: {0}")]
    Usage(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("dm ioctl {op} failed: {source}{}", table_line.as_deref().map(|s| format!(" (table: {s})")).unwrap_or_default())]
    DmIoctl {
        op: &'static str,
        #[source]
        source: std::io::Error,
        /// Operator-facing table line when the failure can be attributed
        /// to a specific dm target (e.g. `DM_TABLE_LOAD`). `None` for ops
        /// that produce no table line (`DM_DEV_CREATE`, etc.).
        table_line: Option<String>,
    },

    #[error("dm device name conflict: /dev/mapper/{name} already exists")]
    NameConflict { name: String },
}
