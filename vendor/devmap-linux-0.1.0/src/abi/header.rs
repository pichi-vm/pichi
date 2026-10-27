// SPDX-License-Identifier: Apache-2.0

//! Fixed device-mapper ioctl header.

use std::io;

use zerocopy::byteorder::native_endian::{I32, U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

const DM_READONLY_FLAG: u32 = 1 << 0;
const DM_SUSPEND_FLAG: u32 = 1 << 1;
const DM_DEFERRED_REMOVE: u32 = 1 << 17;
const DM_STATUS_TABLE_FLAG: u32 = 1 << 4;
const DM_UUID_FLAG: u32 = 1 << 14;

/// Byte-layout mirror of `struct dm_ioctl`, with alignment-one integer fields.
#[repr(C)]
#[derive(Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
pub(crate) struct Header {
    version: [U32; 3],
    data_size: U32,
    data_start: U32,
    target_count: U32,
    open_count: I32,
    flags: U32,
    event_nr: U32,
    padding: U32,
    dev: U64,
    name: [u8; Self::NAME_LEN],
    uuid: [u8; Self::UUID_LEN],
    data: [u8; 7],
}

const _: () = assert!(core::mem::size_of::<Header>() == 312);
// Field offsets are load-bearing for the `struct dm_ioctl` ABI; a
// size-preserving reorder would break it while passing the size assert.
const _: () = {
    use core::mem::offset_of;
    assert!(core::mem::align_of::<Header>() == 1);
    assert!(offset_of!(Header, version) == 0);
    assert!(offset_of!(Header, data_size) == 12);
    assert!(offset_of!(Header, data_start) == 16);
    assert!(offset_of!(Header, target_count) == 20);
    assert!(offset_of!(Header, flags) == 28);
    assert!(offset_of!(Header, event_nr) == 32);
    assert!(offset_of!(Header, dev) == 40);
    assert!(offset_of!(Header, name) == 48);
    assert!(offset_of!(Header, uuid) == 176);
    assert!(offset_of!(Header, data) == 305);
};

impl Header {
    pub(crate) const NAME_LEN: usize = 128;
    pub(crate) const UUID_LEN: usize = 129;
    pub(crate) const VERSION_MAJOR: u32 = 4;
    pub(super) const BUFFER_FULL: u32 = 1 << 8;

    pub(crate) const SIZE: usize = core::mem::size_of::<Header>();
    pub(super) const MIN_SIZE: usize = core::mem::offset_of!(Header, data);

    /// A header with no identification set at all — used for ioctls like
    /// `DM_LIST_DEVICES` that ignore name/uuid/dev and always operate on
    /// every device.
    // `Header::SIZE` is a fixed 312-byte constant, nowhere near u32::MAX.
    #[allow(clippy::cast_possible_truncation)]
    pub(crate) fn any() -> Self {
        Self {
            version: [U32::new(Self::VERSION_MAJOR), U32::ZERO, U32::ZERO],
            data_size: U32::new(Self::SIZE as u32),
            data_start: U32::new(Self::SIZE as u32),
            target_count: 0.into(),
            open_count: 0.into(),
            flags: 0.into(),
            event_nr: 0.into(),
            padding: 0.into(),
            dev: 0.into(),
            name: [0; Self::NAME_LEN],
            uuid: [0; Self::UUID_LEN],
            data: [0; 7],
        }
    }

    /// A header identifying a device by name (used both for `DM_DEV_CREATE`
    /// and for looking up an existing device).
    pub(crate) fn by_name(name: &str) -> io::Result<Self> {
        let mut header = Self::any();
        let bytes = name.as_bytes();
        if bytes.len() >= Self::NAME_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "dm device name too long: {} bytes (max {})",
                    bytes.len(),
                    Self::NAME_LEN - 1
                ),
            ));
        }
        if bytes.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "dm device name contains NUL byte",
            ));
        }
        header.name[..bytes.len()].copy_from_slice(bytes);
        Ok(header)
    }

    /// A header identifying a device by uuid.
    pub(crate) fn by_uuid(uuid: &str) -> io::Result<Self> {
        let mut header = Self::any();
        let bytes = uuid.as_bytes();
        if bytes.len() >= Self::UUID_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "dm uuid too long: {} bytes (max {})",
                    bytes.len(),
                    Self::UUID_LEN - 1
                ),
            ));
        }
        if bytes.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "dm uuid contains NUL byte",
            ));
        }
        header.uuid[..bytes.len()].copy_from_slice(bytes);
        Ok(header)
    }

    /// A header identifying a device by its raw `dev_t` — the kernel's
    /// lookup falls back to this when both `name` and `uuid` are empty.
    pub(crate) fn by_dev(dev_t: u64) -> Self {
        let mut header = Self::any();
        header.dev = dev_t.into();
        header
    }

    /// Request `STATUSTYPE_TABLE` output from `DM_TABLE_STATUS` (the table
    /// params) instead of the default `STATUSTYPE_INFO` (runtime status).
    #[must_use]
    pub(crate) fn with_status_table(mut self, table: bool) -> Self {
        if table {
            self.flags.set(self.flags.get() | DM_STATUS_TABLE_FLAG);
        } else {
            self.flags.set(self.flags.get() & !DM_STATUS_TABLE_FLAG);
        }
        self
    }

    /// Set `DM_SUSPEND_FLAG` to suspend, or clear it to resume.
    #[must_use]
    pub(crate) fn with_suspend(mut self, suspend: bool) -> Self {
        if suspend {
            self.flags.set(self.flags.get() | DM_SUSPEND_FLAG);
        } else {
            self.flags.set(self.flags.get() & !DM_SUSPEND_FLAG);
        }
        self
    }

    /// Select whether `DM_DEV_RENAME` sets a new uuid rather than a new name.
    #[must_use]
    pub(crate) fn with_uuid_flag(mut self, uuid: bool) -> Self {
        if uuid {
            self.flags.set(self.flags.get() | DM_UUID_FLAG);
        } else {
            self.flags.set(self.flags.get() & !DM_UUID_FLAG);
        }
        self
    }

    /// Select deferred removal rather than failing with `EBUSY` when in use.
    #[must_use]
    pub(crate) fn with_deferred_remove(mut self, deferred: bool) -> Self {
        if deferred {
            self.flags.set(self.flags.get() | DM_DEFERRED_REMOVE);
        } else {
            self.flags.set(self.flags.get() & !DM_DEFERRED_REMOVE);
        }
        self
    }

    /// Select whether to load the table read-only. dm-verity refuses a
    /// writable table (`-EINVAL` "Device must be readonly"); read-only
    /// dm-snapshot composition wants it too.
    #[must_use]
    pub(crate) fn with_readonly(mut self, readonly: bool) -> Self {
        if readonly {
            self.flags.set(self.flags.get() | DM_READONLY_FLAG);
        } else {
            self.flags.set(self.flags.get() & !DM_READONLY_FLAG);
        }
        self
    }

    /// Set total buffer size (for variable-length `DM_TABLE_LOAD`,
    /// `DM_LIST_DEVICES`, `DM_TABLE_STATUS`).
    pub(super) fn set_data_size(&mut self, size: u32) {
        self.data_size = size.into();
    }

    #[must_use]
    pub(crate) fn with_target_count(mut self, count: u32) -> Self {
        self.target_count = count.into();
        self
    }

    /// Set the event number `DM_DEV_WAIT` blocks against — the ioctl
    /// returns once the device's event counter has advanced past this.
    #[must_use]
    pub(crate) fn with_event_nr(mut self, event_nr: u32) -> Self {
        self.event_nr = event_nr.into();
        self
    }

    pub(crate) fn open_count(&self) -> i32 {
        self.open_count.get()
    }

    pub(crate) fn event_nr(&self) -> u32 {
        self.event_nr.get()
    }

    /// Kernel-returned `dev_t` — populated synchronously by `DM_DEV_CREATE`
    /// and returned by any lookup ioctl.
    pub(crate) fn dev(&self) -> u64 {
        self.dev.get()
    }

    /// Kernel-returned dm-ioctl major version. Checked by Command after an ioctl succeeds.
    pub(super) fn major_version(&self) -> u32 {
        self.version[0].get()
    }

    pub(crate) fn target_count(&self) -> u32 {
        self.target_count.get()
    }

    pub(crate) fn flags(&self) -> u32 {
        self.flags.get()
    }

    pub(super) fn data_start(&self) -> u32 {
        self.data_start.get()
    }

    pub(super) fn data_size(&self) -> u32 {
        self.data_size.get()
    }
}
