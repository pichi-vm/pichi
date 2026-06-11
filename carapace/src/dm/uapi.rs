//! Kernel UAPI mirrors + iocuddle ioctl-number declarations. This is
//! the ONLY file in the crate that needs `#![allow(unsafe_code)]` — all
//! 4 unsafe blocks are iocuddle const constructors.
//!
//! Raw structs (`dm_ioctl_raw`, `dm_target_spec_raw`) are `pub(super)`
//! so the safe wrappers in `dm::mod` can use them, but they remain
//! invisible to the rest of the crate. The wrappers (`DmHeader`,
//! `DmTableBuf`) uphold every invariant iocuddle requires of the
//! ioctl-argument types.

#![allow(unsafe_code)]

use iocuddle::{Group, Ioctl, WriteRead};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

pub(super) const DM_NAME_LEN: usize = 128;
pub(super) const DM_UUID_LEN: usize = 129;
pub(super) const DM_MAX_TYPE_NAME: usize = 16;

pub(super) const DM_IOCTL_VERSION_MAJOR: u32 = 4;

/// Mirror of `struct dm_ioctl` from `<linux/dm-ioctl.h>`. Visible only
/// to the parent `dm` module (raw types do NOT cross the dm boundary).
/// Sizeof locked at 312 bytes by the unit test in `dm::mod`.
///
/// Field order is byte-for-byte identical to the kernel UAPI; `name` /
/// `uuid` / `data` are `[u8; N]` rather than `[c_char; N]` because we
/// target Linux only (carapace's IMP-05 floor).
#[repr(C)]
#[derive(Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable)]
#[allow(non_camel_case_types)]
pub(super) struct dm_ioctl_raw {
    pub version: [u32; 3],
    pub data_size: u32,
    pub data_start: u32,
    pub target_count: u32,
    pub open_count: i32,
    pub flags: u32,
    pub event_nr: u32,
    pub padding: u32,
    pub dev: u64,
    pub name: [u8; DM_NAME_LEN],
    pub uuid: [u8; DM_UUID_LEN],
    pub data: [u8; 7],
}

const _: () = assert!(core::mem::size_of::<dm_ioctl_raw>() == 312);

/// Mirror of `struct dm_target_spec` from `<linux/dm-ioctl.h>`.
/// Sizeof locked at 40 bytes.
#[repr(C)]
#[derive(Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable)]
#[allow(non_camel_case_types)]
pub(super) struct dm_target_spec_raw {
    pub sector_start: u64,
    pub length: u64,
    pub status: i32,
    pub next: u32,
    pub target_type: [u8; DM_MAX_TYPE_NAME],
}

const _: () = assert!(core::mem::size_of::<dm_target_spec_raw>() == 40);

pub(super) mod dm_flags {
    pub const READONLY: u32 = 1 << 0;
    pub const SUSPEND: u32 = 1 << 1;
}

const DM_IOCTL_GROUP: Group = Group::new(0xfd);

// SAFETY: every dm-ioctl is `_IOWR(0xfd, N, struct dm_ioctl)` per
// `<linux/dm-ioctl.h>`. We declare against `&DmHeader` (defined in
// dm::mod) which is `#[repr(transparent)]` over `dm_ioctl_raw` — same
// memory layout, but the newtype confines mutation to its safe
// constructors. This satisfies iocuddle's "T provides safe wrappers
// around its raw contents" contract.
pub(super) const DM_DEV_CREATE: Ioctl<WriteRead, &super::header::DmHeader> =
    unsafe { DM_IOCTL_GROUP.write_read(3) };
pub(super) const DM_DEV_REMOVE: Ioctl<WriteRead, &super::header::DmHeader> =
    unsafe { DM_IOCTL_GROUP.write_read(4) };
pub(super) const DM_DEV_SUSPEND: Ioctl<WriteRead, &super::header::DmHeader> =
    unsafe { DM_IOCTL_GROUP.write_read(6) };
pub(super) const DM_TABLE_LOAD: Ioctl<WriteRead, &super::header::DmHeader> =
    unsafe { DM_IOCTL_GROUP.write_read(9) };
pub(super) const DM_LIST_DEVICES: Ioctl<WriteRead, &super::header::DmHeader> =
    unsafe { DM_IOCTL_GROUP.write_read(2) };

/// `DM_BUFFER_FULL_FLAG` from `<linux/dm-ioctl.h>`. Set in
/// `dm_ioctl.flags` by the kernel when our supplied payload buffer
/// for `DM_LIST_DEVICES` was too small to hold the full reply.
pub(super) const DM_BUFFER_FULL_FLAG: u32 = 1 << 8;
