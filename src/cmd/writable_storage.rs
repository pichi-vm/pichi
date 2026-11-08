// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! One writable block device. Formatting and mounting belong to the image.

use super::requirements::SlotArgument as _;
use super::run::DevicePath as _;
use anyhow::{Context, Result, bail, ensure};
use conglobate::artifact::{Recommendation, Slot, Storage};
use std::{
    fs::File,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

const MIB: u64 = 1024 * 1024;

/// Own the backing and its exclusive lock until the VM has exited.
#[derive(Debug)]
pub(super) struct WritableStorage {
    path: PathBuf,
    lock: File,
    temporary: Option<TempDir>,
}

impl WritableStorage {
    pub(super) fn prepare(
        requirement: Option<&Storage>,
        explicit_mib: Option<u64>,
        retained: Option<&Path>,
        directory: &Path,
    ) -> Result<Option<Self>> {
        if requirement.is_none() && explicit_mib.is_none() && retained.is_none() {
            return Ok(None);
        }
        let (path, temporary) = if let Some(path) = retained {
            (path.to_owned(), None)
        } else {
            std::fs::create_dir_all(directory)?;
            let temporary = TempDir::new_in(directory)?;
            (temporary.path().join("storage.raw"), Some(temporary))
        };
        let mut options = File::options();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options.open(&path).context("opening writable storage")?;
        ensure!(
            file.metadata()?.is_file(),
            "storage backing must be a regular file"
        );
        let path = path.canonicalize().context("resolving storage path")?;
        ensure!(path.to_str().is_some(), "storage path must be UTF-8");
        // Resolve aliases before choosing the shared lock. Windows byte-range
        // locks on the backing itself would block the VMM's I/O.
        let mut lock_path = path.as_os_str().to_owned();
        lock_path.push(".lock");
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            // Sharing restrictions follow the open file object when its
            // handle is transferred to Madrillo. Byte-range locks belong to
            // the locking process and cannot provide that lifetime.
            options.share_mode(0);
        }
        let lock = options
            .open(Path::new(&lock_path))
            .context("opening storage lock")?;
        #[cfg(unix)]
        lock.try_lock().context("storage is already in use")?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let length = file.metadata()?.len();
        let required = requirement
            .and_then(|r| r.capacity.required())
            .map(std::num::NonZeroU64::get);
        let recommended = requirement
            .and_then(|r| r.capacity.recommended())
            .map(|r| r.map(std::num::NonZeroU64::get));
        let existing = (length != 0).then_some(length / MIB);
        let chosen = explicit_mib.or(existing);
        let available = fs4::available_space(parent)?;
        let mib = match (chosen, recommended) {
            (Some(value), _) | (None, Some(Recommendation::Value(value))) => value,
            (None, Some(Recommendation::Max)) => available / MIB,
            (None, None) => required.context("supply --storage or an image storage capacity")?,
        };
        ensure!(
            mib != 0 && required.is_none_or(|floor| mib >= floor),
            "storage capacity ({mib} MiB) is below the image's required minimum ({} MiB)",
            required.unwrap_or(1)
        );
        if chosen.is_some()
            && let Some(Recommendation::Value(rec)) = recommended
            && mib < rec
        {
            log::warn!("storage ({mib} MiB) is below the recommended {rec} MiB");
        }
        let bytes = mib
            .checked_mul(MIB)
            .context("storage capacity overflows bytes")?;
        if explicit_mib.is_some() && bytes < length {
            bail!("refusing to shrink retained storage");
        }
        if bytes > length {
            ensure!(
                bytes - length <= available,
                "insufficient host space for storage allocation"
            );
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
        }
        Ok(Some(Self {
            path,
            lock,
            temporary,
        }))
    }

    pub(super) fn argument(&self, slot: Slot) -> String {
        format!(
            "path={},bus={}",
            self.path.key_value_arg(),
            slot.as_argument()
        )
    }

    /// Retain temporary backing in the workload record; its exit callback
    /// removes it only after confirmed cleanup. Madrillo owns the reservation.
    pub(super) fn into_monitor(self) -> File {
        if let Some(temporary) = self.temporary {
            let _ = temporary.keep();
        }
        self.lock
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
        let storage = WritableStorage::prepare(None, Some(256), None, directory.path())
            .unwrap()
            .unwrap();
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
    fn disposable_storage_is_sized_locked_and_removed() {
        let directory = tempfile::tempdir().unwrap();
        let requirement =
            serde_json::from_value(serde_json::json!({"required":1,"recommended":2,"slot":"mmio"}))
                .unwrap();
        let storage = WritableStorage::prepare(Some(&requirement), None, None, directory.path())
            .unwrap()
            .unwrap();
        let path = storage.path.clone();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 2 * MIB);
        assert!(
            storage
                .argument(requirement.slot.unwrap_or_default())
                .ends_with(",bus=mmio")
        );
        assert!(
            WritableStorage::prepare(Some(&requirement), None, Some(&path), directory.path())
                .is_err()
        );
        drop(storage);
        assert!(!path.exists());
    }

    #[test]
    fn retained_storage_preserves_contents_and_never_shrinks() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("disk.raw");
        let mut file = File::create(&path).unwrap();
        file.set_len(2 * MIB).unwrap();
        file.write_all(b"existing data").unwrap();
        drop(file);
        let storage = WritableStorage::prepare(None, None, Some(&path), directory.path())
            .unwrap()
            .unwrap();
        drop(storage);
        assert!(WritableStorage::prepare(None, Some(1), Some(&path), directory.path()).is_err());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 2 * MIB);
        let storage = WritableStorage::prepare(None, Some(4), Some(&path), directory.path())
            .unwrap()
            .unwrap();
        drop(storage);
        let mut data = [0; 13];
        File::open(&path).unwrap().read_exact(&mut data).unwrap();
        assert_eq!(&data, b"existing data");
        assert_eq!(std::fs::metadata(path).unwrap().len(), 4 * MIB);
    }

    #[test]
    fn floors_and_overflow_are_rejected_before_sizing() {
        let directory = tempfile::tempdir().unwrap();
        let requirement = serde_json::from_value(serde_json::json!({"required":2})).unwrap();
        for size in [0, 1, u64::MAX] {
            assert!(
                WritableStorage::prepare(Some(&requirement), Some(size), None, directory.path())
                    .is_err()
            );
        }
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        assert!(
            WritableStorage::prepare(None, None, None, directory.path())
                .unwrap()
                .is_none()
        );
    }
}
