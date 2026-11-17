// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! One writable block device. Formatting and mounting belong to the image.

use super::requirements::SlotArgument as _;
use super::run::DevicePath as _;
use anyhow::{Context, Result, ensure};
use conglobate::artifact::{Recommendation, Slot, Storage};
use std::{
    fs::File,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

const MIB: u64 = 1024 * 1024;

/// Own a fresh build disk until it is retained for monitor-driven cleanup.
#[derive(Debug)]
pub(super) struct WritableStorage {
    path: PathBuf,
    lock: File,
    temporary: TempDir,
}

impl WritableStorage {
    pub(super) fn prepare(
        requirement: &Storage,
        explicit_mib: Option<u64>,
        directory: &Path,
    ) -> Result<Self> {
        std::fs::create_dir_all(directory)?;
        let bytes = Self::capacity(requirement, explicit_mib, directory)?;
        let temporary = TempDir::new_in(directory)?;
        let path = temporary.path().join("storage.raw");
        let mut options = File::options();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options.open(&path).context("opening writable storage")?;
        let path = path.canonicalize().context("resolving storage path")?;
        ensure!(path.to_str().is_some(), "storage path must be UTF-8");
        #[cfg(windows)]
        {
            let output = std::process::Command::new("fsutil")
                .args(["sparse", "setflag"])
                .arg(&path)
                .output()
                .context("marking writable storage sparse")?;
            ensure!(
                output.status.success(),
                "marking writable storage sparse: {output:?}"
            );
        }
        file.set_len(bytes).context("sizing writable storage")?;
        // The lock is inherited by both monitor and VMM. Windows sharing
        // restrictions follow the open file object across those processes.
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            options.share_mode(0);
        }
        let lock = options.open(temporary.path().join("storage.raw.lock"))?;
        #[cfg(unix)]
        lock.try_lock()?;
        Ok(Self {
            path,
            lock,
            temporary,
        })
    }

    /// Resolve a capacity in bytes for a disk created in `directory`: an
    /// explicit size, else the image recommendation (`max` is the free
    /// space there), else its required minimum.
    pub(super) fn capacity(
        requirement: &Storage,
        explicit_mib: Option<u64>,
        directory: &Path,
    ) -> Result<u64> {
        let required = requirement
            .capacity
            .required()
            .map(std::num::NonZeroU64::get);
        let recommended = requirement
            .capacity
            .recommended()
            .map(|r| r.map(std::num::NonZeroU64::get));
        let available = fs4::available_space(directory)?;
        let mib = match (explicit_mib, recommended) {
            (Some(value), _) | (None, Some(Recommendation::Value(value))) => value,
            (None, Some(Recommendation::Max)) => available / MIB,
            (None, None) => required.context("supply --storage or an image storage capacity")?,
        };
        ensure!(
            mib != 0 && required.is_none_or(|floor| mib >= floor),
            "storage capacity ({mib} MiB) is below the image's required minimum ({} MiB)",
            required.unwrap_or(1)
        );
        if explicit_mib.is_some()
            && let Some(Recommendation::Value(rec)) = recommended
            && mib < rec
        {
            log::warn!("storage ({mib} MiB) is below the recommended {rec} MiB");
        }
        let bytes = mib
            .checked_mul(MIB)
            .context("storage capacity overflows bytes")?;
        ensure!(
            bytes <= available,
            "insufficient host space for storage allocation"
        );
        Ok(bytes)
    }

    pub(super) fn argument(&self, slot: Slot) -> String {
        format!(
            "path={},bus={}",
            self.path.key_value_arg(),
            slot.as_argument()
        )
    }

    /// Retain temporary backing and transfer the lock to monitor and VMM.
    /// Either process may outlive the other.
    pub(super) fn into_boot(self) -> Result<(File, File)> {
        let inherited = self.lock.try_clone()?;
        let _ = self.temporary.keep();
        Ok((self.lock, inherited))
    }
}

/// An instance's persistent disk: the qcow2 profile Dillo admits (v3,
/// 64 KiB clusters, 16-bit refcounts, no backing file, encryption or
/// snapshots). Allocation grows on write; nothing here repairs an image.
#[derive(Debug)]
pub(crate) struct InstanceDisk;

impl InstanceDisk {
    pub(crate) fn create(file: File, bytes: u64) -> Result<()> {
        use imago::{FormatCreateBuilder as _, StorageOpenOptions, qcow2::Qcow2};
        let storage = imago::file::File::from_open_file(
            file.try_clone()?,
            StorageOpenOptions::new().write(true),
        )?;
        Qcow2::<imago::file::File>::create_builder(storage)
            .size(bytes)
            .create()?;
        file.sync_all()?;
        Ok(())
    }

    /// Check read-only admission and capacity, without scanning all disk data.
    pub(crate) fn check(file: File, bytes: u64) -> Result<()> {
        use imago::{StorageOpenOptions, format::drivers::FormatDriverInstance as _, qcow2::Qcow2};
        let storage = imago::file::File::from_open_file(file, StorageOpenOptions::new())?;
        let image = Qcow2::<imago::file::File>::open_image(storage, false)?;
        ensure!(
            image.size() == bytes,
            "disk capacity {} differs from the recorded {bytes} bytes",
            image.size()
        );
        Ok(())
    }

    pub(crate) fn argument(path: &Path, slot: Slot) -> Result<String> {
        ensure!(
            path.to_str().is_some(),
            "instance storage path must be UTF-8"
        );
        Ok(format!(
            "path={},format=qcow2,bus={}",
            path.key_value_arg(),
            slot.as_argument()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};

    #[test]
    fn unwritten_capacity_does_not_allocate_disk_blocks() {
        let directory = tempfile::tempdir().unwrap();
        let size = 256 * MIB;
        let storage =
            WritableStorage::prepare(&Storage::default(), Some(256), directory.path()).unwrap();
        let mut file = File::options()
            .read(true)
            .write(true)
            .open(&storage.path)
            .unwrap();
        file.seek(SeekFrom::End(-4)).unwrap();
        file.write_all(b"data").unwrap();
        file.sync_all().unwrap();
        assert_eq!(file.metadata().unwrap().len(), size);
        file.rewind().unwrap();
        let mut bytes = [1; 4];
        file.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes, [0; 4]);
        file.seek(SeekFrom::End(-4)).unwrap();
        file.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"data");
        let allocated = fs4::FileExt::allocated_size(&file).unwrap();
        assert!(
            allocated < size / 2,
            "allocated {allocated} of {size} bytes"
        );
    }

    #[test]
    fn disposable_storage_is_private_sized_and_removed() {
        let directory = tempfile::tempdir().unwrap();
        let requirement =
            serde_json::from_value(serde_json::json!({"required":1,"recommended":2,"slot":"mmio"}))
                .unwrap();
        let storage = WritableStorage::prepare(&requirement, None, directory.path()).unwrap();
        let path = storage.path.clone();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 2 * MIB);
        assert!(
            storage
                .argument(requirement.slot.unwrap_or_default())
                .ends_with(",bus=mmio")
        );
        let other = WritableStorage::prepare(&requirement, None, directory.path()).unwrap();
        assert_ne!(path, other.path, "build disks must never share a backing");
        drop(other);
        drop(storage);
        assert!(!path.exists());
    }

    #[test]
    fn floors_and_overflow_are_rejected_before_sizing() {
        let directory = tempfile::tempdir().unwrap();
        let requirement = serde_json::from_value(serde_json::json!({"required":2})).unwrap();
        for size in [0, 1, u64::MAX] {
            assert!(WritableStorage::prepare(&requirement, Some(size), directory.path()).is_err());
        }
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        let storage =
            WritableStorage::prepare(&Storage::default(), None, directory.path()).unwrap();
        assert_eq!(std::fs::metadata(&storage.path).unwrap().len(), 1024 * MIB);
    }

    #[test]
    fn instance_disk_is_sparse_persistent_and_refuses_invalid_headers() {
        use imago::{StorageOpenOptions, qcow2::Qcow2};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("storage.qcow2");
        let capacity = 1024 * MIB;
        let file = File::create_new(&path).unwrap();
        InstanceDisk::create(file, capacity).unwrap();
        let header = std::fs::read(&path).unwrap();
        assert_eq!(&header[..8], b"QFI\xfb\0\0\0\x03", "qcow2 version 3");
        assert_eq!(u32::from_be_bytes(header[20..24].try_into().unwrap()), 16);
        assert_eq!(
            u64::from_be_bytes(header[24..32].try_into().unwrap()),
            capacity
        );
        assert!(header.len() < MIB as usize, "{} bytes", header.len());
        InstanceDisk::check(File::open(&path).unwrap(), capacity).unwrap();
        assert!(InstanceDisk::check(File::open(&path).unwrap(), capacity / 2).is_err());
        // Data persists through writable image sessions and reopening.
        for round in 0..2_u8 {
            let storage = imago::file::File::from_open_file(
                File::options().read(true).write(true).open(&path).unwrap(),
                StorageOpenOptions::new().write(true),
            )
            .unwrap();
            let image = Qcow2::<imago::file::File>::open_image(storage, true).unwrap();
            let access = imago::FormatAccess::new(image);
            let mut data = [0xff_u8; 4096];
            access.read(&mut data[..], 512 * MIB).unwrap();
            assert_eq!(data, [round; 4096]);
            access.write(&[round + 1; 4096][..], 512 * MIB).unwrap();
            access.flush().unwrap();
        }
        InstanceDisk::check(File::open(&path).unwrap(), capacity).unwrap();
        // Admission must neither repair damaged headers nor accept features
        // it cannot interpret. Each refusal must leave every byte unchanged.
        let clean = std::fs::read(&path).unwrap();
        for (name, offset, bit) in [
            ("dirty", 0x4f, 1),
            ("bad magic", 0, 0xff),
            ("unknown incompatible feature", 0x48, 0x80),
        ] {
            let mut bytes = clean.clone();
            bytes[offset] ^= bit;
            std::fs::write(&path, &bytes).unwrap();
            assert!(
                InstanceDisk::check(File::open(&path).unwrap(), capacity).is_err(),
                "accepted {name}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), bytes, "modified {name}");
        }
    }

    #[test]
    fn instance_disk_argument_round_trips_through_dillo_config() {
        let argument =
            InstanceDisk::argument(Path::new("/a, [b]/storage.qcow2"), Slot::Pci).unwrap();
        let spec: dillo_config::BlkSpec = serde_keyvalue::from_key_values(&argument).unwrap();
        assert_eq!(spec.format, dillo_config::BlockFormat::Qcow2);
        assert_eq!(spec.path, Path::new("/a, [b]/storage.qcow2"));
        assert_eq!(spec.bus, Some(dillo_config::Bus::Pci));
        assert!(!spec.readonly);
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt as _;
            let path = PathBuf::from(std::ffi::OsString::from_vec(b"/disk-\xff".to_vec()));
            assert!(InstanceDisk::argument(&path, Slot::Pci).is_err());
        }
    }
}
