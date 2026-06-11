//! `DmHeader`: safe `#[repr(transparent)]` newtype over `dm_ioctl_raw`.
//!
//! This is the type iocuddle's typed ioctl declarations reference. The
//! `inner` field is private; every mutator enforces the kernel's
//! coherency invariants (NUL-terminated name within `DM_NAME_LEN`,
//! `version = [4, 0, 0]`, `data_size = SIZE` for fixed-payload calls).

use super::uapi::{dm_flags, dm_ioctl_raw, DM_IOCTL_VERSION_MAJOR, DM_NAME_LEN, DM_UUID_LEN};
use crate::CarapaceError;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// Safe wrapper over `dm_ioctl`. Hides the UAPI; only construction +
/// flagged accessors. Coherency invariants enforced at construction
/// time:
///   * `name` is NUL-terminated and within `DM_NAME_LEN`
///   * `version = [4, 0, 0]`
///   * `data_size = sizeof(dm_ioctl_raw)` initially (caller can grow it
///     for variable-length payloads via `DmTableBuf`)
///
/// `#[repr(transparent)]` guarantees identical layout to
/// `dm_ioctl_raw` — required so iocuddle can pass `&mut DmHeader` as
/// the ioctl argument.
#[repr(transparent)]
#[derive(Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable)]
pub(super) struct DmHeader {
    inner: dm_ioctl_raw,
}

impl DmHeader {
    pub(super) const SIZE: usize = core::mem::size_of::<dm_ioctl_raw>();

    /// Build a fresh header naming `name`. NUL byte or overlong names
    /// are rejected with `Usage`.
    pub(super) fn new(name: &str) -> Result<Self, CarapaceError> {
        let bytes = name.as_bytes();
        if bytes.len() >= DM_NAME_LEN {
            return Err(CarapaceError::Usage(format!(
                "dm device name too long: {} bytes (max {})",
                bytes.len(),
                DM_NAME_LEN - 1
            )));
        }
        if bytes.iter().any(|&b| b == 0) {
            return Err(CarapaceError::Usage(
                "dm device name contains NUL byte".into(),
            ));
        }
        let mut name_buf = [0u8; DM_NAME_LEN];
        name_buf[..bytes.len()].copy_from_slice(bytes);
        Ok(Self {
            inner: dm_ioctl_raw {
                version: [DM_IOCTL_VERSION_MAJOR, 0, 0],
                data_size: Self::SIZE as u32,
                data_start: Self::SIZE as u32,
                target_count: 0,
                open_count: 0,
                flags: 0,
                event_nr: 0,
                padding: 0,
                dev: 0,
                name: name_buf,
                uuid: [0; DM_UUID_LEN],
                data: [0; 7],
            },
        })
    }

    /// Set the READONLY flag (typestate-style; chains).
    pub(super) fn with_readonly(mut self) -> Self {
        self.inner.flags |= dm_flags::READONLY;
        self
    }

    /// Toggle the SUSPEND flag in place.
    pub(super) fn set_suspend(&mut self, suspend: bool) {
        if suspend {
            self.inner.flags |= dm_flags::SUSPEND;
        } else {
            self.inner.flags &= !dm_flags::SUSPEND;
        }
    }

    /// Re-apply READONLY flag onto an existing header (used by every
    /// post-create ioctl on a ReadOnly device — `init_header` zeros
    /// `flags`, but dm-verity's `verity_ctr` requires READONLY at
    /// `DM_TABLE_LOAD` time).
    pub(super) fn add_readonly(&mut self) {
        self.inner.flags |= dm_flags::READONLY;
    }

    /// Kernel-returned dev_t (synchronously populated by `DM_DEV_CREATE`).
    pub(super) fn dev(&self) -> u64 {
        self.inner.dev
    }

    /// Kernel-returned dm-ioctl major version. Validate this == 4 after
    /// every ioctl (HIGH-11).
    pub(super) fn major_version(&self) -> u32 {
        self.inner.version[0]
    }

    pub(super) fn version(&self) -> [u32; 3] {
        self.inner.version
    }

    /// Set total buffer size (for variable-length `DM_TABLE_LOAD` and
    /// `DM_LIST_DEVICES`). Called by `DmTableBuf::build` in
    /// `super::table` and by `super::device::list_devices_with_prefix`.
    pub(super) fn set_data_size(&mut self, size: u32) {
        self.inner.data_size = size;
    }

    pub(super) fn set_target_count(&mut self, count: u32) {
        self.inner.target_count = count;
    }

    /// Kernel-returned offset (from start of buffer) where the
    /// variable-length payload begins. For `DM_LIST_DEVICES`, this is
    /// where the first `dm_name_list` entry sits.
    pub(super) fn data_start(&self) -> u32 {
        self.inner.data_start
    }

    /// Kernel-returned total buffer size, in bytes — equal to the
    /// header offset plus the actual payload size for replies that
    /// fit. Used by `DM_LIST_DEVICES` to bound the parse cursor.
    pub(super) fn data_size(&self) -> u32 {
        self.inner.data_size
    }

    /// Kernel-returned flags. Caller masks with the specific flag
    /// they're interested in (e.g. `DM_BUFFER_FULL_FLAG`).
    pub(super) fn flags(&self) -> u32 {
        self.inner.flags
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizeof_dm_ioctl_raw() {
        assert_eq!(core::mem::size_of::<dm_ioctl_raw>(), 312);
    }

    #[test]
    fn dmheader_is_layout_identical_to_raw() {
        // #[repr(transparent)] guarantees this; assertion is a witness
        // to prevent future drift.
        assert_eq!(
            core::mem::size_of::<DmHeader>(),
            core::mem::size_of::<dm_ioctl_raw>()
        );
        assert_eq!(
            core::mem::align_of::<DmHeader>(),
            core::mem::align_of::<dm_ioctl_raw>()
        );
    }

    #[test]
    fn header_new_zero_pads_name() {
        let h = DmHeader::new("foo").unwrap();
        assert_eq!(&h.inner.name[..3], b"foo");
        assert!(h.inner.name[3..].iter().all(|&b| b == 0));
    }

    #[test]
    fn header_new_sets_version_4_0_0() {
        let h = DmHeader::new("ok").unwrap();
        assert_eq!(h.inner.version, [4, 0, 0]);
    }

    #[test]
    fn header_new_rejects_nul_in_name() {
        assert!(matches!(
            DmHeader::new("foo\0bar"),
            Err(CarapaceError::Usage(_))
        ));
    }

    #[test]
    fn header_new_rejects_overlong_name() {
        let long = "a".repeat(128);
        assert!(matches!(DmHeader::new(&long), Err(CarapaceError::Usage(_))));
    }

    #[test]
    fn header_with_readonly_sets_flag() {
        let h = DmHeader::new("x").unwrap().with_readonly();
        assert_eq!(h.inner.flags & dm_flags::READONLY, dm_flags::READONLY);
    }

    #[test]
    fn header_set_suspend_toggles() {
        let mut h = DmHeader::new("x").unwrap();
        h.set_suspend(true);
        assert_eq!(h.inner.flags & dm_flags::SUSPEND, dm_flags::SUSPEND);
        h.set_suspend(false);
        assert_eq!(h.inner.flags & dm_flags::SUSPEND, 0);
    }
}
