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
    DM_LIST_DEVICES, DM_TABLE_LOAD,
};
use crate::CarapaceError;
use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use zerocopy::{FromBytes, IntoBytes};

/// Open `/dev/mapper/control` — the dm subsystem's ioctl entry point.
/// One open is enough for an entire activation; pass `&mut File` to
/// each `DmDevice` method. `DmDevice::Drop` falls back to its own open
/// if it has to fire (rare; only on rollback).
pub(crate) fn open_dm_control() -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/mapper/control")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DmCreateMode {
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
pub(crate) struct DmDevice {
    name: String,
    remove_on_drop: bool,
    mode: DmCreateMode,
    /// dev_t returned synchronously by DM_DEV_CREATE.
    dev_t: u64,
}

impl DmDevice {
    pub(crate) fn create(
        control: &mut File,
        name: &str,
        mode: DmCreateMode,
    ) -> Result<Self, CarapaceError> {
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
                return Err(CarapaceError::NameConflict { name: name.into() });
            }
            Err(source) => {
                return Err(CarapaceError::DmIoctl {
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
    pub(crate) fn dev_ref(&self) -> (u32, u32) {
        split_dev(self.dev_t)
    }

    /// `/dev/dm-<minor>` path. Created synchronously by the dm-mapper
    /// kernel module at DM_DEV_CREATE time (no udev). Use for direct
    /// I/O (e.g. chunk_size read through dm-verity).
    pub(crate) fn dev_node(&self) -> PathBuf {
        let (_, minor) = split_dev(self.dev_t);
        PathBuf::from(format!("/dev/dm-{minor}"))
    }

    /// Submit a DM_TABLE_LOAD. ERR-04: on failure, the rendered table
    /// is attached to the error.
    ///
    /// `render_all()` is deferred to the error path — the success path
    /// pays nothing for it. Cuts one String allocation per `load_table`
    /// (~3N+1 per attach).
    pub(crate) fn load_table(
        &self,
        control: &mut File,
        table: &DmTable,
    ) -> Result<(), CarapaceError> {
        let mut buf = DmTableBuf::build(&self.name, table)?;
        // Replay create-time RO flag. dm-verity's verity_ctr rejects RW
        // at DM_TABLE_LOAD time with "Device must be readonly".
        if matches!(self.mode, DmCreateMode::ReadOnly) {
            buf.header_mut().add_readonly();
        }
        DM_TABLE_LOAD
            .ioctl(control, buf.header_mut())
            .map_err(|source| CarapaceError::DmIoctl {
                op: "DM_TABLE_LOAD",
                source,
                table_line: Some(table.render_all()),
            })?;
        let header = buf.header_mut();
        if header.major_version() != DM_IOCTL_VERSION_MAJOR {
            return Err(CarapaceError::DmIoctl {
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
    pub(crate) fn resume(&self, control: &mut File) -> Result<(), CarapaceError> {
        let mut header = DmHeader::new(&self.name)?;
        if matches!(self.mode, DmCreateMode::ReadOnly) {
            header.add_readonly();
        }
        header.set_suspend(false);
        DM_DEV_SUSPEND
            .ioctl(control, &mut header)
            .map_err(|source| CarapaceError::DmIoctl {
                op: "DM_DEV_RESUME",
                source,
                table_line: None,
            })?;
        check_version(&header, "DM_DEV_RESUME")?;
        Ok(())
    }

    /// Opt out of `Drop = DM_DEV_REMOVE`.
    pub(crate) fn forget(mut self) {
        self.remove_on_drop = false;
    }

    fn remove_inner(&self, control: &mut File) -> Result<(), CarapaceError> {
        let mut header = DmHeader::new(&self.name)?;
        DM_DEV_REMOVE
            .ioctl(control, &mut header)
            .map_err(|source| CarapaceError::DmIoctl {
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
            .map_err(CarapaceError::from)
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
pub(crate) fn list_devices_with_prefix(base: &str) -> Result<Vec<String>, CarapaceError> {
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
        .map_err(|source| CarapaceError::DmIoctl {
            op: "DM_LIST_DEVICES",
            source,
            table_line: None,
        })?;
    check_version(header_mut, "DM_LIST_DEVICES")?;

    if header_mut.flags() & DM_BUFFER_FULL_FLAG != 0 {
        return Err(CarapaceError::DmIoctl {
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
            .map(|n| name_start + n)
            .unwrap_or(data_end);
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
pub(crate) fn remove_by_name(name: &str) -> Result<(), CarapaceError> {
    let mut control = open_dm_control()?;
    let mut header = DmHeader::new(name)?;
    DM_DEV_REMOVE
        .ioctl(&mut control, &mut header)
        .map_err(|source| CarapaceError::DmIoctl {
            op: "DM_DEV_REMOVE",
            source,
            table_line: None,
        })?;
    check_version(&header, "DM_DEV_REMOVE")?;
    Ok(())
}

fn check_version(header: &DmHeader, op: &'static str) -> Result<(), CarapaceError> {
    if header.major_version() != DM_IOCTL_VERSION_MAJOR {
        let v = header.version();
        return Err(CarapaceError::DmIoctl {
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
}
