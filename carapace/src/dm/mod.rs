//! Device-mapper wrapper. Split into focused submodules:
//!
//!   * [`uapi`]   — kernel UAPI mirrors + iocuddle ioctl-number
//!                  declarations. The ONLY file with
//!                  `#![allow(unsafe_code)]`. ~80 LOC.
//!   * [`header`] — `DmHeader`: safe `#[repr(transparent)]` newtype
//!                  over `dm_ioctl_raw`. The type iocuddle's typed
//!                  ioctl declarations reference.
//!   * [`table`]  — `TargetSpec` / `TableLine` / `DmTable` (operator
//!                  model) + `DmTableBuf` (kernel-ABI byte buffer
//!                  for `DM_TABLE_LOAD`) + `DmDeviceArg` (uniform
//!                  device-arg type for the renderer).
//!   * [`device`] — `DmDevice` RAII handle (create / load_table /
//!                  resume / drop=remove) + `remove_by_name` +
//!                  `list_devices_with_prefix` + `split_dev`.
//!
//! `dm/` is **chain-agnostic**: it knows kernel ABI, dm-table
//! rendering, and per-device RAII. It does NOT know what a "scute"
//! is. The orchestrator that bridges `chain::ValidatedChain` to
//! these primitives lives at `crate::assemble`, not here.
//!
//! Iocuddle paradigm (preserved across the split): the kernel UAPI
//! structs (`dm_ioctl_raw` / `dm_target_spec_raw`) are `pub(super)`
//! to `uapi` — visible to dm/* siblings so they can wrap them,
//! invisible to the rest of the crate. `unsafe { Group::write_read(N) }`
//! declarations live in `uapi` and reference newtypes in `header` /
//! `table`, which uphold their invariants by construction.

mod device;
mod header;
mod table;
mod uapi;

// Re-exports for items that cross out of dm/:
//
//   - cli/detach.rs uses: remove_by_name, list_devices_with_prefix
//   - assemble.rs uses: open_dm_control, DmCreateMode, DmDevice,
//                       DmTable, TableLine, TargetSpec
//
// dm/-internal types (DmHeader, DmTableBuf, dm_target_spec_raw, the
// ioctl const declarations) stay `pub(super)` and never leave dm/.
pub(crate) use device::{
    list_devices_with_prefix, open_dm_control, remove_by_name, DmCreateMode, DmDevice,
};
pub(crate) use table::{DmTable, TableLine, TargetSpec};
