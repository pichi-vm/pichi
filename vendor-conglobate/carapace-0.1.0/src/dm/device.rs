// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `DmDevice`: RAII handle over a created `/dev/mapper/<name>`.
//! Construction does `DM_DEV_CREATE`, drop does `DM_DEV_REMOVE`
//! (best-effort, opt-out via [`DmDevice::forget`]).
//!
//! Free helper `remove_by_name` (no-handle removal) lives here too —
//! all dm-ioctl-bearing code in one place.

use super::header::DmHeader;
use super::table::{DmTable, DmTableBuf};
use super::uapi::{
    DM_BUFFER_FULL_FLAG, DM_DEV_CREATE, DM_DEV_REMOVE, DM_DEV_SUSPEND, DM_IOCTL_VERSION_MAJOR,
    DM_LIST_DEVICES, DM_TABLE_LOAD, DM_TABLE_STATUS, DM_TARGET_SPEC_SIZE,
};
use crate::dm::DmError;
use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use zerocopy::{FromBytes, IntoBytes};

/// Open `/dev/mapper/control` — the dm subsystem's ioctl entry point.
/// One open is enough for an entire activation; pass `&mut File` to
/// each `DmDevice` method. `DmDevice::Drop` falls back to its own open
/// if it has to fire (rare; only on rollback).
pub fn open_dm_control() -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/mapper/control")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmCreateMode {
    ReadOnly,
    ReadWrite,
}

/// Decode Linux dev_t into (major, minor) per `<linux/kdev_t.h>`.
///
/// Layout (32-bit dev_t in `dm_ioctl.dev`'s lower half — kernel zero-
/// extends the upper 32 bits):
/// ```text
///   bit 31 ........... 20 19 ......... 8 7 ........ 0
///       minor[19:8]        major[11:0]    minor[7:0]
/// ```
/// `MAJOR_BITS = 12`, `MINOR_LOW_BITS = 8`. The split mirrors
/// glibc's `gnu_dev_major`/`gnu_dev_minor` for compatibility.
#[inline]
fn split_dev(dev: u64) -> (u32, u32) {
    /// Mask for `minor[7:0]` — extracts the bottom 8 bits of dev_t.
    const MINOR_LOW_MASK: u32 = 0x0000_00ff;
    /// Mask for `major[11:0]` after a `>> 8` — 12 bits of major.
    const MAJOR_MASK: u32 = 0x0000_0fff;
    /// Mask for `minor[19:8]` after a `>> 12` — leaves the high 12
    /// bits of the 20-bit minor in their original positions.
    const MINOR_HIGH_MASK: u32 = 0x000f_ff00;

    let dev = dev as u32;
    let major = (dev >> 8) & MAJOR_MASK;
    let minor = (dev & MINOR_LOW_MASK) | ((dev >> 12) & MINOR_HIGH_MASK);
    (major, minor)
}

/// RAII handle over a configured `/dev/mapper/<name>` device. Drop
/// calls `DM_DEV_REMOVE` best-effort; use [`DmDevice::forget`] to opt
/// out (the device persists past this handle's drop).
///
/// The control fd (`/dev/mapper/control`) is NOT stored — the
/// orchestrator (`crate::assemble`) opens one for the entire
/// activation and passes `&mut File` to each method. This eliminates
/// 3N+1 redundant opens per attach (one per ioctl call) and removes
/// the `RefCell<File>` interior-mutability dance the `&self` ioctl
/// methods previously needed. `Drop` opens its own fd inline (the
/// rollback path runs at most once per device — the cost is amortized
/// across the activation).
#[derive(Debug)]
pub struct DmDevice {
    name: String,
    remove_on_drop: bool,
    mode: DmCreateMode,
    /// dev_t returned synchronously by DM_DEV_CREATE.
    dev_t: u64,
}

impl DmDevice {
    pub fn create(control: &mut File, name: &str, mode: DmCreateMode) -> Result<Self, DmError> {
        let mut header = DmHeader::new(name)?;
        if matches!(mode, DmCreateMode::ReadOnly) {
            header = header.with_readonly();
        }
        match DM_DEV_CREATE.ioctl(control, &mut header) {
            Ok(_) => {}
            Err(source)
                if matches!(
                    source.kind(),
                    std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::ResourceBusy
                ) =>
            {
                return Err(DmError::NameConflict { name: name.into() });
            }
            Err(source) => {
                return Err(DmError::DmIoctl {
                    op: "DM_DEV_CREATE",
                    source,
                    table_line: None,
                });
            }
        }
        check_version(&header, "DM_DEV_CREATE")?;
        Ok(Self {
            name: name.into(),
            remove_on_drop: true,
            mode,
            dev_t: header.dev(),
        })
    }

    /// `(major, minor)` for use as the `<maj>:<min>` device argument in
    /// a dm-table line. Returns the raw pair so the dm-table renderer
    /// can format on demand without an intermediate `PathBuf`
    /// allocation. Avoids the udev wait for internal layers.
    pub fn dev_ref(&self) -> (u32, u32) {
        split_dev(self.dev_t)
    }

    /// `/dev/dm-<minor>` path. Created synchronously by the dm-mapper
    /// kernel module at DM_DEV_CREATE time (no udev). Use for direct
    /// I/O (e.g. chunk_size read through dm-verity).
    pub fn dev_node(&self) -> PathBuf {
        let (_, minor) = split_dev(self.dev_t);
        PathBuf::from(format!("/dev/dm-{minor}"))
    }

    /// Submit a DM_TABLE_LOAD. ERR-04: on failure, the rendered table
    /// is attached to the error.
    ///
    /// `render_all()` is deferred to the error path — the success path
    /// pays nothing for it. Cuts one String allocation per `load_table`
    /// (~3N+1 per attach).
    pub fn load_table(&self, control: &mut File, table: &DmTable) -> Result<(), DmError> {
        let mut buf = DmTableBuf::build(&self.name, table)?;
        // Replay create-time RO flag. dm-verity's verity_ctr rejects RW
        // at DM_TABLE_LOAD time with "Device must be readonly".
        if matches!(self.mode, DmCreateMode::ReadOnly) {
            buf.header_mut().add_readonly();
        }
        DM_TABLE_LOAD
            .ioctl(control, buf.header_mut())
            .map_err(|source| DmError::DmIoctl {
                op: "DM_TABLE_LOAD",
                source,
                table_line: Some(table.render_all()),
            })?;
        let header = buf.header_mut();
        if header.major_version() != DM_IOCTL_VERSION_MAJOR {
            return Err(DmError::DmIoctl {
                op: "DM_TABLE_LOAD",
                source: std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    format!(
                        "kernel returned dm-ioctl version major {}; require {}",
                        header.major_version(),
                        DM_IOCTL_VERSION_MAJOR
                    ),
                ),
                table_line: Some(table.render_all()),
            });
        }
        Ok(())
    }

    /// DM_DEV_RESUME — toggle the device from "loaded" to "active",
    /// publishing the loaded table. Read-side activation never has
    /// reason to re-suspend the device, so no public `suspend` exists.
    pub fn resume(&self, control: &mut File) -> Result<(), DmError> {
        let mut header = DmHeader::new(&self.name)?;
        if matches!(self.mode, DmCreateMode::ReadOnly) {
            header.add_readonly();
        }
        header.set_suspend(false);
        DM_DEV_SUSPEND
            .ioctl(control, &mut header)
            .map_err(|source| DmError::DmIoctl {
                op: "DM_DEV_RESUME",
                source,
                table_line: None,
            })?;
        check_version(&header, "DM_DEV_RESUME")?;
        Ok(())
    }

    /// dm-snapshot COW usage from `DM_TABLE_STATUS`. Returns
    /// `(allocated_sectors, total_sectors)` in 512-byte sectors, parsed
    /// from the snapshot status line `"<allocated>/<total> <metadata>"`.
    /// Errors if the device is not a (valid) snapshot or status is
    /// `"Invalid"` / `"Overflow"`.
    ///
    /// conglobate uses this to read back ONLY the allocated extent of a
    /// large sparse loop-backed COW exception store, rather than copying
    /// the whole (mostly-unwritten) device.
    ///
    /// `DM_TABLE_STATUS` mirrors `DM_TABLE_LOAD`'s buffer shape but in
    /// the read direction: the kernel fills a trailing variable-length
    /// region after the `dm_ioctl` header with, per target, a
    /// `dm_target_spec` followed by a NUL-terminated status string. We
    /// size the buffer generously and re-issue with a larger one if the
    /// kernel sets `DM_BUFFER_FULL_FLAG` (same retry discipline as
    /// `list_devices_with_prefix`).
    pub fn snapshot_allocated(&self, control: &mut File) -> Result<(u64, u64), DmError> {
        /// Initial trailing-buffer size. A snapshot status line is a few
        /// dozen bytes (spec + "alloc/total meta"); 4 KiB is ample, and
        /// the retry below covers any pathological case.
        const INITIAL_CAP: usize = 4 * 1024;
        /// Hard ceiling on the retry growth — a single snapshot target's
        /// status can never approach this; the cap guards a misbehaving
        /// kernel from driving an unbounded allocation.
        const MAX_CAP: usize = 256 * 1024;

        let mut cap = INITIAL_CAP;
        loop {
            let total = DmHeader::SIZE + cap;
            let mut bytes = vec![0u8; total];

            let mut header = DmHeader::new(&self.name)?;
            header.set_data_size(total as u32);
            bytes[..DmHeader::SIZE].copy_from_slice(header.as_bytes());

            let header_mut = DmHeader::mut_from_prefix(&mut bytes)
                .expect("DmHeader::SIZE bytes were just written")
                .0;

            DM_TABLE_STATUS
                .ioctl(control, header_mut)
                .map_err(|source| DmError::DmIoctl {
                    op: "DM_TABLE_STATUS",
                    source,
                    table_line: None,
                })?;
            check_version(header_mut, "DM_TABLE_STATUS")?;

            // Buffer too small: grow and retry, like list_devices.
            if header_mut.flags() & DM_BUFFER_FULL_FLAG != 0 {
                if cap >= MAX_CAP {
                    return Err(DmError::DmIoctl {
                        op: "DM_TABLE_STATUS",
                        source: std::io::Error::new(
                            std::io::ErrorKind::OutOfMemory,
                            format!("snapshot status exceeded {MAX_CAP}-byte buffer"),
                        ),
                        table_line: None,
                    });
                }
                cap = (cap * 2).min(MAX_CAP);
                continue;
            }

            let target_count = header_mut.target_count();
            let data_start = header_mut.data_start() as usize;
            let data_end = (header_mut.data_size() as usize).min(bytes.len());

            return parse_snapshot_status(&bytes, target_count, data_start, data_end);
        }
    }

    /// Opt out of `Drop = DM_DEV_REMOVE`.
    pub fn forget(mut self) {
        self.remove_on_drop = false;
    }

    fn remove_inner(&self, control: &mut File) -> Result<(), DmError> {
        let mut header = DmHeader::new(&self.name)?;
        DM_DEV_REMOVE
            .ioctl(control, &mut header)
            .map_err(|source| DmError::DmIoctl {
                op: "DM_DEV_REMOVE",
                source,
                table_line: None,
            })?;
        check_version(&header, "DM_DEV_REMOVE")?;
        Ok(())
    }
}

impl Drop for DmDevice {
    fn drop(&mut self) {
        if !self.remove_on_drop {
            return;
        }
        // Open a fresh control fd inline. The orchestrator's shared fd
        // isn't accessible here (Drop can't take parameters); but Drop
        // only fires on rollback (commit() calls .forget() to opt out),
        // so the extra open is paid only when something has already
        // failed — not on the success path.
        let result = open_dm_control()
            .map_err(DmError::from)
            .and_then(|mut control| self.remove_inner(&mut control));
        if let Err(e) = result {
            eprintln!(
                "carapace: best-effort DM_DEV_REMOVE for '{}' failed: {}",
                self.name, e
            );
        }
    }
}

/// Enumerate all dm devices visible to the kernel and return the
/// names that belong to a carapace stack named `base` — i.e., either
/// exactly `base` (the top alias) or `base-<suffix>` (an internal
/// layer). Used by detach to discover the actual surviving devices
/// instead of probing MAX_CHAIN_DEPTH * 2 + 2 = 65 names blindly.
///
/// Critical: bare `str::starts_with(base)` would also match unrelated
/// devices like `<base>X` (e.g. `vault` enumerating `vaultkeeper`),
/// risking collateral removal of someone else's dm stack. We require
/// either an exact match or a `-` immediately after the base name.
///
/// Implementation notes
///
/// `DM_LIST_DEVICES` returns a variable-length payload of
/// `dm_name_list` records right after the standard `dm_ioctl` header.
/// Each record is:
///
/// ```text
///     u64 dev          (8)   Linux dev_t for the device.
///     u32 next         (4)   Offset (from start of THIS record) to the
///                            next record. 0 = no more records.
///     char name[]            NUL-terminated; padded so the next record
///                            begins at an 8-byte alignment.
/// ```
///
/// The header's `data_start` points at the first record; `data_size`
/// is the total payload length the kernel filled. If our buffer was
/// too small for the full reply, the kernel sets `DM_BUFFER_FULL_FLAG`
/// in `flags`; we treat this as an error rather than silently
/// truncating (a 64 KiB buffer holds ~1500 typical-name entries —
/// orders of magnitude beyond any realistic dm-mapper population).
pub fn list_devices_with_prefix(base: &str) -> Result<Vec<String>, DmError> {
    /// Generous payload cap. Each record is ~24 bytes for typical
    /// short names; 64 KiB easily holds thousands of entries.
    const PAYLOAD_CAP: usize = 64 * 1024;

    let mut control = open_dm_control()?;

    let total = DmHeader::SIZE + PAYLOAD_CAP;
    let mut bytes = vec![0u8; total];

    // Header is name-less ("" — DM_LIST_DEVICES doesn't take a name
    // filter; we filter client-side). data_size = total so the kernel
    // knows how much room it has for the reply payload.
    let mut header = DmHeader::new("")?;
    header.set_data_size(total as u32);
    bytes[..DmHeader::SIZE].copy_from_slice(header.as_bytes());

    let header_mut = DmHeader::mut_from_prefix(&mut bytes)
        .expect("DmHeader::SIZE bytes were just written")
        .0;

    DM_LIST_DEVICES
        .ioctl(&mut control, header_mut)
        .map_err(|source| DmError::DmIoctl {
            op: "DM_LIST_DEVICES",
            source,
            table_line: None,
        })?;
    check_version(header_mut, "DM_LIST_DEVICES")?;

    if header_mut.flags() & DM_BUFFER_FULL_FLAG != 0 {
        return Err(DmError::DmIoctl {
            op: "DM_LIST_DEVICES",
            source: std::io::Error::new(
                std::io::ErrorKind::OutOfMemory,
                format!("kernel reply exceeded {PAYLOAD_CAP}-byte buffer"),
            ),
            table_line: None,
        });
    }

    let data_start = header_mut.data_start() as usize;
    let data_end = (header_mut.data_size() as usize).min(bytes.len());

    // Empty payload: kernel reports data_size == data_start when no
    // dm devices exist. Nothing to do.
    if data_start >= data_end {
        return Ok(Vec::new());
    }

    let mut names = Vec::new();
    let mut cursor = data_start;
    loop {
        // Each record header is dev:u64 + next:u32 = 12 bytes minimum.
        if cursor + 12 > data_end {
            break;
        }
        let _dev = u64::from_ne_bytes(bytes[cursor..cursor + 8].try_into().unwrap());
        let next = u32::from_ne_bytes(bytes[cursor + 8..cursor + 12].try_into().unwrap()) as usize;

        // Name is NUL-terminated, immediately after the 12-byte fixed
        // prefix. Bound to data_end as a safety net against a
        // malformed reply (kernel doesn't produce these in practice).
        let name_start = cursor + 12;
        let name_end = bytes[name_start..data_end]
            .iter()
            .position(|&b| b == 0)
            .map_or(data_end, |n| name_start + n);
        if let Ok(name) = std::str::from_utf8(&bytes[name_start..name_end]) {
            // Match `base` exactly OR `base-...` — never `baseX`.
            let is_ours = name == base
                || (name.len() > base.len()
                    && name.starts_with(base)
                    && name.as_bytes()[base.len()] == b'-');
            if is_ours {
                names.push(name.to_string());
            }
        }

        // `next == 0` is the terminator; `next < 12` would loop forever
        // or jump backwards into already-parsed bytes. Kernel doesn't
        // emit either; treat both as end-of-list defensively.
        if next < 12 {
            break;
        }
        cursor += next;
    }

    Ok(names)
}

/// Remove a dm device by name without holding a [`DmDevice`] handle.
pub fn remove_by_name(name: &str) -> Result<(), DmError> {
    let mut control = open_dm_control()?;
    let mut header = DmHeader::new(name)?;
    DM_DEV_REMOVE
        .ioctl(&mut control, &mut header)
        .map_err(|source| DmError::DmIoctl {
            op: "DM_DEV_REMOVE",
            source,
            table_line: None,
        })?;
    check_version(&header, "DM_DEV_REMOVE")?;
    Ok(())
}

fn check_version(header: &DmHeader, op: &'static str) -> Result<(), DmError> {
    if header.major_version() != DM_IOCTL_VERSION_MAJOR {
        let v = header.version();
        return Err(DmError::DmIoctl {
            op,
            source: std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!(
                    "kernel returned dm-ioctl version {}.{}.{}; we require major == {}",
                    v[0], v[1], v[2], DM_IOCTL_VERSION_MAJOR
                ),
            ),
            table_line: None,
        });
    }
    Ok(())
}

/// Extract the first target's status string from a `DM_TABLE_STATUS`
/// reply buffer and parse it as a dm-snapshot status line.
///
/// The trailing payload, starting at `data_start`, is a sequence of
/// `dm_target_spec` (40 bytes) each followed by a NUL-terminated status
/// string. We only need the first target. `target_count == 0` means the
/// device has no live table (never activated / wrong device).
fn parse_snapshot_status(
    bytes: &[u8],
    target_count: u32,
    data_start: usize,
    data_end: usize,
) -> Result<(u64, u64), DmError> {
    if target_count == 0 {
        return Err(DmError::Usage(
            "DM_TABLE_STATUS returned no targets (device has no live table)".into(),
        ));
    }

    // The status string begins just past the first target's fixed
    // dm_target_spec record.
    let str_start = data_start
        .checked_add(DM_TARGET_SPEC_SIZE)
        .filter(|&s| s <= data_end)
        .ok_or_else(|| {
            DmError::Usage("DM_TABLE_STATUS reply truncated before status string".into())
        })?;

    let str_end = bytes[str_start..data_end]
        .iter()
        .position(|&b| b == 0)
        .map_or(data_end, |n| str_start + n);

    let status = std::str::from_utf8(&bytes[str_start..str_end])
        .map_err(|_| DmError::Usage("DM_TABLE_STATUS status string is not UTF-8".into()))?;

    parse_snapshot_status_line(status)
}

/// Parse a dm-snapshot status line — `"<allocated>/<total> <metadata>"`
/// — into `(allocated_sectors, total_sectors)` (512-byte sectors). The
/// kernel reports `"Invalid"` for a broken snapshot and `"Overflow"`
/// when the COW store is full; both are surfaced as errors.
fn parse_snapshot_status_line(status: &str) -> Result<(u64, u64), DmError> {
    let trimmed = status.trim();
    if trimmed == "Invalid" || trimmed == "Overflow" {
        return Err(DmError::Usage(format!(
            "dm-snapshot status is '{trimmed}' (snapshot is not usable)"
        )));
    }

    // First whitespace-separated token is "allocated/total"; the rest is
    // the metadata-sectors token we don't need.
    let usage = trimmed
        .split_whitespace()
        .next()
        .ok_or_else(|| DmError::Usage(format!("dm-snapshot status line is empty: {status:?}")))?;

    let (alloc, total) = usage.split_once('/').ok_or_else(|| {
        DmError::Usage(format!(
            "dm-snapshot status token {usage:?} is not '<allocated>/<total>' \
             (device may not be a snapshot)"
        ))
    })?;

    let allocated = alloc.parse::<u64>().map_err(|_| {
        DmError::Usage(format!(
            "dm-snapshot allocated sectors not an integer: {alloc:?}"
        ))
    })?;
    let total = total.parse::<u64>().map_err(|_| {
        DmError::Usage(format!(
            "dm-snapshot total sectors not an integer: {total:?}"
        ))
    })?;

    Ok((allocated, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_dev_decomposes_kernel_encoding() {
        let major_in = 253u32;
        let minor_in = 15u32;
        let dev = (minor_in & 0xFF) | ((major_in & 0xFFF) << 8) | ((minor_in & 0xFFF00) << 12);
        let (m, n) = split_dev(dev as u64);
        assert_eq!(m, major_in);
        assert_eq!(n, minor_in);
    }

    #[test]
    fn parse_snapshot_status_line_extracts_allocated_total() {
        // Canonical snapshot status: "<allocated>/<total> <metadata>".
        assert_eq!(
            parse_snapshot_status_line("16/40960 8").unwrap(),
            (16, 40960)
        );
    }

    #[test]
    fn parse_snapshot_status_line_ignores_trailing_metadata() {
        // Only the first token matters; metadata sectors are ignored.
        assert_eq!(
            parse_snapshot_status_line("1024/2097152 256").unwrap(),
            (1024, 2_097_152)
        );
    }

    #[test]
    fn parse_snapshot_status_line_rejects_invalid() {
        assert!(matches!(
            parse_snapshot_status_line("Invalid"),
            Err(DmError::Usage(_))
        ));
    }

    #[test]
    fn parse_snapshot_status_line_rejects_overflow() {
        assert!(matches!(
            parse_snapshot_status_line("Overflow"),
            Err(DmError::Usage(_))
        ));
    }

    #[test]
    fn parse_snapshot_status_line_rejects_non_snapshot() {
        // A non-snapshot target (e.g. "linear") has no "<a>/<b>" token.
        assert!(matches!(
            parse_snapshot_status_line("0 2048 linear"),
            Err(DmError::Usage(_))
        ));
    }

    #[test]
    fn parse_snapshot_status_zero_targets_errors() {
        assert!(matches!(
            parse_snapshot_status(&[0u8; 64], 0, 0, 64),
            Err(DmError::Usage(_))
        ));
    }
}
