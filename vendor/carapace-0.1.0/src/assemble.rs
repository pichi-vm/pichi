// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Assemble read-only mappings with open files as their lifetime anchors.

use crate::snapshot::{ValidatedSnapshotHeader, SNAPSHOT_HEADER_SIZE};
use crate::util::hex_lower;
use crate::verity::superblock::VERITY_SUPERBLOCK_SIZE;
use crate::verity::ValidatedVeritySuperblock;
use crate::{Chain, Scute};
use devmap_core::{BlockSize, Span};
use devmap_linux::records::Records;
use devmap_linux::targets::{Target, Zero};
use devmap_linux::{Control, DevId, Status, Table};
use devmap_snapshot::{
    Discard, Parameters as SnapshotParameters, Persistence, Persistent, Snapshot,
};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, FileTypeExt};

use crate::ZERO_COUNT_SECTORS;

#[derive(Debug)]
struct PreparedScute<'a> {
    data: &'a File,
    hash: &'a File,
    superblock: ValidatedVeritySuperblock,
    root: Box<[u8]>,
}

impl<'a> PreparedScute<'a> {
    fn read(scute: &'a Scute, root: &[u8], index: usize) -> io::Result<Self> {
        let mut record = [0; VERITY_SUPERBLOCK_SIZE];
        scute.hash.read_exact_at(&mut record, 0)?;
        let superblock = ValidatedVeritySuperblock::parse(&record, index)?;
        Ok(Self {
            data: &scute.data,
            hash: &scute.hash,
            superblock,
            root: root.into(),
        })
    }
}

impl Chain {
    /// Independently validate the retained inputs before creating mappings.
    fn prepare(&self) -> io::Result<Vec<PreparedScute<'_>>> {
        if self.scutes.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a chain needs at least one scute",
            ));
        }
        let mut root = self.root.clone();
        let mut algorithm = None;
        let mut prepared = Vec::with_capacity(self.scutes.len());
        for (index, scute) in self.scutes.iter().enumerate().rev() {
            let scute = PreparedScute::read(scute, &root, index)?;
            let sb = &scute.superblock;
            if algorithm.is_some_and(|expected| sb.algorithm != expected) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "chain algorithm mismatch",
                ));
            }
            if root.len() != sb.algorithm.digest_size() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "trusted root length must match the hash algorithm",
                ));
            }
            algorithm = Some(sb.algorithm);
            if sb.is_base() != (index == 0) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("scute {index}: invalid base salt sentinel"),
                ));
            }
            root = sb.full_salt()[..sb.algorithm.digest_size()].into();
            prepared.push(scute);
        }
        prepared.reverse();
        Ok(prepared)
    }

    pub(crate) fn assemble_sync(&self) -> io::Result<File> {
        let prepared = self.prepare()?;
        let assembler = Assembler {
            control: Control::open()?,
        };
        let mut origin = assembler.activate("CARAPACE-z", ZERO_COUNT_SECTORS, Zero)?;
        for (index, scute) in prepared.iter().enumerate() {
            origin = assembler
                .scute(scute, origin, index)
                .map_err(|error| io::Error::new(error.kind(), format!("scute {index}: {error}")))?;
        }
        Ok(origin)
    }
}

#[derive(Debug)]
struct Assembler {
    control: Control,
}

/// Rolls a freshly created device back with an immediate remove on drop.
///
/// Mirrors the pre-refactor `Device::guard()` contract: stay armed through
/// load/resume/open, then disarm only after deferred removal is scheduled
/// behind an open file that pins the mapping.
struct Rollback {
    control: Control,
    id: Option<DevId>,
}

impl Rollback {
    fn new(control: &Control, id: DevId) -> Self {
        Self {
            control: control.clone(),
            id: Some(id),
        }
    }

    fn disarm(mut self) {
        self.id = None;
    }

    fn fail(mut self, error: io::Error) -> io::Error {
        let Some(id) = self.id.take() else {
            return error;
        };
        // `take` disarms Drop so cleanup runs once here, and any cleanup
        // error is composed into the returned value.
        match self.control.remove(id, false) {
            Ok(()) => error,
            Err(cleanup) => io::Error::new(
                error.kind(),
                format!("{error}; immediate cleanup also failed: {cleanup}"),
            ),
        }
    }
}

impl Drop for Rollback {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            let _ = self.control.remove(id, false);
        }
    }
}

impl Assembler {
    /// Resolve an already-open input, retaining its block handle until the
    /// verity table holds it. Loop handles are ordinary files with autoclear.
    fn resolve(file: &File) -> io::Result<File> {
        let metadata = file.metadata()?;
        if metadata.file_type().is_block_device() {
            return file.try_clone();
        }
        if metadata.is_file() {
            #[cfg(feature = "loop")]
            {
                let config = lodown::Configurable {
                    read_only: true,
                    writable: lodown::Writable {
                        autoclear: true,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let device = lodown::Control::open()?.attach(file, 0, config)?;
                return Ok(device.into());
            }
            #[cfg(not(feature = "loop"))]
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "file-backed scutes need feature `loop`",
            ));
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "scute is not a block device or regular file",
        ))
    }

    fn scute(&self, scute: &PreparedScute<'_>, origin: File, index: usize) -> io::Result<File> {
        let data = Self::resolve(scute.data)?;
        let hash = Self::resolve(scute.hash)?;
        let data_id = DevId::try_from(data.metadata()?)?;
        let hash_id = DevId::try_from(hash.metadata()?)?;
        let origin_id = DevId::try_from(origin.metadata()?)?;
        let verity_target = scute
            .superblock
            .header
            .builder(data_id, hash_id, scute.root.clone())?
            .build();
        // Bind names to both content and backing devices. Different loop
        // attachments of identical content can coexist without replacing a
        // live table; identical block-device inputs still share mappings.
        let name = format!(
            "CARAPACE-v{}-{data_id}-{hash_id}",
            hex_lower(&scute.root[..16])
        );
        let verity = self.activate(&name, verity_target.data_sectors(), verity_target)?;
        let mut header = [0; SNAPSHOT_HEADER_SIZE];
        verity.read_exact_at(&mut header, 0)?;
        ValidatedSnapshotHeader::parse(&header, index)?;
        let verity_id = DevId::try_from(verity.metadata()?)?;
        let name = format!(
            "CARAPACE-s{}-{origin_id}-{verity_id}",
            hex_lower(&scute.root[16..32])
        );
        let snapshot = Snapshot::from(SnapshotParameters {
            origin: origin_id,
            cow: verity_id,
            persistence: Persistence::Persistent(Persistent::Standard),
            chunk_size: BlockSize::default(),
            discard: Discard::Disabled,
        });
        // The upper mapping holds origin/verity before their files close.
        self.activate(&name, ZERO_COUNT_SECTORS, snapshot)
    }

    /// New mappings get rollback guards. Existing mappings are pinned and
    /// checked, never reloaded or put under an immediate-removal guard.
    fn activate<T>(&self, name: &str, sectors: u64, target: T) -> io::Result<File>
    where
        T: Target<Table = T> + Clone + PartialEq,
    {
        self.activate_inner(
            name,
            sectors,
            target,
            #[cfg(test)]
            &mut |_| Ok(()),
        )
    }

    fn activate_inner<T>(
        &self,
        name: &str,
        sectors: u64,
        target: T,
        #[cfg(test)] observe: &mut impl FnMut(ActivationStage) -> io::Result<()>,
    ) -> io::Result<File>
    where
        T: Target<Table = T> + Clone + PartialEq,
    {
        // An independent open file description is required for flock exclusion
        // between assemblers in the same process as well as across processes.
        // Only publication is serialized; scute I/O and attachment happen outside.
        // Returned files pin each mapping across the unlocked gaps between scutes.
        let lock = File::open("/dev/mapper/control")?;
        lock.lock()?;
        for _ in 0..8 {
            match self.control.create(name) {
                Ok(id) => {
                    let rollback = Rollback::new(&self.control, id);
                    #[cfg(test)]
                    observe(ActivationStage::Created)?;
                    let version = self.control.version(T::NAME)?;
                    let table =
                        Table::<4096>::default().add(Span::new(0, sectors), &target, version)?;
                    self.control.load(id, table, true)?;
                    self.control.suspend(id, false)?;
                    #[cfg(test)]
                    observe(ActivationStage::Resumed)?;
                    // Keep exclusion until the file pins the mapping and the
                    // rollback guard is disarmed. On error, guard cleanup runs
                    // before the earlier-declared lock file closes.
                    let file = match OpenOptions::new()
                        .read(true)
                        .open(format!("/dev/dm-{}", id.minor()))
                    {
                        Ok(file) => file,
                        Err(error) => return Err(rollback.fail(error)),
                    };
                    if let Err(error) = self.control.remove(id, true) {
                        drop(file);
                        return Err(rollback.fail(error));
                    }
                    rollback.disarm();
                    return Ok(file);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::AlreadyExists | io::ErrorKind::ResourceBusy
                    ) => {}
                Err(error) => return Err(error),
            }
            // Deferred removal can race lookup/open even under the setup lock:
            // existing consumers close their files without acquiring it.
            let existing = self.control.by_name(name).and_then(|(id, _)| {
                #[cfg(test)]
                observe(ActivationStage::LookedUp)?;
                let file = File::open(format!("/dev/dm-{}", id.minor()))?;
                Ok((id, file))
            });
            let (id, file) = match existing {
                Ok(existing) => existing,
                Err(error) if matches!(error.raw_os_error(), Some(2 | 6 | 19)) => continue,
                Err(error) => return Err(error),
            };
            // The open file prevents deferred teardown. Cooperating assemblers
            // never modify an existing table, so validation needs no lock.
            drop(lock);
            #[cfg(test)]
            observe(ActivationStage::Pinned)?;
            let status = self.control.status(id)?;
            let required = Status::READ_ONLY | Status::ACTIVE_TABLE;
            if status.flags & required != required
                || status.flags & (Status::SUSPENDED | Status::INACTIVE_TABLE) != 0
                || DevId::try_from(file.metadata()?)? != id
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("existing mapping {name} is not an active read-only device"),
                ));
            }
            let version = self.control.version(T::NAME)?;
            let mut rows = self.control.table::<4096>(id)?;
            let matches = match rows.next() {
                Some(row) => {
                    let row = row?;
                    row.span() == Span::new(0, sectors) && row.parse::<T>(version)? == target
                }
                None => false,
            };
            if !matches || rows.next().is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("existing mapping {name} has a different table"),
                ));
            }
            drop(rows);
            self.control.remove(id, true)?;
            return Ok(file);
        }
        Err(io::Error::new(
            io::ErrorKind::ResourceBusy,
            format!("mapping {name} is being removed; retry assembly"),
        ))
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActivationStage {
    Created,
    Resumed,
    LookedUp,
    Pinned,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn kernel_assembler() -> Option<Assembler> {
        match Control::open() {
            Ok(control) => Some(Assembler { control }),
            Err(error) => {
                assert!(
                    std::env::var_os("CARAPACE_REQUIRE_DM_TESTS").is_none(),
                    "{error}"
                );
                eprintln!("skip: device-mapper access required: {error}");
                None
            }
        }
    }

    fn publication_lock_available() -> bool {
        match File::open("/dev/mapper/control").unwrap().try_lock() {
            Ok(()) => true,
            Err(std::fs::TryLockError::WouldBlock) => false,
            Err(error) => panic!("probe publication lock: {error}"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn kernel_publication_holds_lock_and_rolls_back_before_unlocking() {
        let Some(assembler) = kernel_assembler() else {
            return;
        };
        let name = format!("CARAPACE-lock-rollback-{}", std::process::id());
        let mut stages = Vec::new();
        let error = assembler
            .activate_inner(&name, 8, Zero, &mut |stage| {
                stages.push(stage);
                assert!(!publication_lock_available());
                if stage == ActivationStage::Resumed {
                    return Err(io::Error::other("injected publication failure"));
                }
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "injected publication failure");
        assert_eq!(stages, [ActivationStage::Created, ActivationStage::Resumed]);
        assert!(publication_lock_available());
        assert!(assembler.control.by_name(&name).is_err());
        // The same name can be published successfully after rollback.
        drop(assembler.activate(&name, 8, Zero).unwrap());
    }

    #[test]
    #[serial_test::serial]
    fn kernel_reuse_releases_lock_after_pinning() {
        let Some(assembler) = kernel_assembler() else {
            return;
        };
        let name = format!("CARAPACE-lock-reuse-{}", std::process::id());
        let mut first = Some(assembler.activate(&name, 8, Zero).unwrap());
        let id = DevId::try_from(first.as_ref().unwrap().metadata().unwrap()).unwrap();
        let mut stages = Vec::new();
        let second = assembler
            .activate_inner(&name, 8, Zero, &mut |stage| {
                stages.push(stage);
                assert_eq!(
                    publication_lock_available(),
                    stage == ActivationStage::Pinned
                );
                if stage == ActivationStage::Pinned {
                    // Only the reuser's file now holds the deferred mapping.
                    // Subsequent status/table validation must still succeed.
                    drop(first.take());
                    assert_eq!(assembler.control.by_name(&name)?.0, id);
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(stages, [ActivationStage::LookedUp, ActivationStage::Pinned]);
        assert_eq!(id, DevId::try_from(second.metadata().unwrap()).unwrap());
    }

    #[test]
    #[serial_test::serial]
    fn kernel_competitor_waits_for_publication_or_rollback() {
        let Some(assembler) = kernel_assembler() else {
            return;
        };
        for fail in [false, true] {
            let name = format!("CARAPACE-lock-contender-{}-{fail}", std::process::id());
            std::thread::scope(|scope| {
                let (start_tx, start_rx) = std::sync::mpsc::channel();
                let (attempt_tx, attempt_rx) = std::sync::mpsc::channel();
                let (done_tx, done_rx) = std::sync::mpsc::channel();
                let name = &name;
                let assembler = &assembler;
                let waiter = scope.spawn(move || {
                    start_rx.recv().unwrap();
                    let lock = File::open("/dev/mapper/control").unwrap();
                    assert!(matches!(
                        lock.try_lock(),
                        Err(std::fs::TryLockError::WouldBlock)
                    ));
                    attempt_tx.send(()).unwrap();
                    let file = if fail {
                        lock.lock().unwrap();
                        // Observe state at lock acquisition, not after the
                        // initiating thread has returned from activation.
                        assert!(assembler.control.by_name(name).is_err());
                        None
                    } else {
                        Some(assembler.activate(name, 8, Zero).unwrap())
                    };
                    done_tx.send(()).unwrap();
                    file
                });
                let result = assembler.activate_inner(name, 8, Zero, &mut |stage| {
                    if stage == ActivationStage::Resumed {
                        start_tx.send(()).unwrap();
                        attempt_rx
                            .recv_timeout(std::time::Duration::from_secs(5))
                            .unwrap();
                        assert!(matches!(
                            done_rx.recv_timeout(std::time::Duration::from_millis(50)),
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                        ));
                        if fail {
                            return Err(io::Error::other("injected publication failure"));
                        }
                    }
                    Ok(())
                });
                // Also unblock the waiter if activation failed before Resumed.
                drop(start_tx);
                let other = waiter.join().unwrap();
                if fail {
                    assert_eq!(
                        result.unwrap_err().to_string(),
                        "injected publication failure"
                    );
                } else {
                    let file = result.unwrap();
                    assert_eq!(
                        DevId::try_from(file.metadata().unwrap()).unwrap(),
                        DevId::try_from(other.unwrap().metadata().unwrap()).unwrap()
                    );
                }
            });
        }
    }

    #[test]
    #[serial_test::serial]
    fn kernel_removal_between_lookup_and_open_retries_creation() {
        let Some(assembler) = kernel_assembler() else {
            return;
        };
        let name = format!("CARAPACE-lock-disappear-{}", std::process::id());
        let mut first = Some(assembler.activate(&name, 8, Zero).unwrap());
        let mut stages = Vec::new();
        let replacement = assembler
            .activate_inner(&name, 8, Zero, &mut |stage| {
                stages.push(stage);
                assert!(!publication_lock_available());
                if stage == ActivationStage::LookedUp {
                    drop(first.take().expect("only one existing mapping"));
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                    while assembler.control.by_name(&name).is_ok() {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "deferred removal stalled"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(
            stages,
            [
                ActivationStage::LookedUp,
                ActivationStage::Created,
                ActivationStage::Resumed
            ]
        );
        assert!(publication_lock_available());
        drop(replacement);
    }

    fn scute(dir: &Path, index: usize, algorithm: &str, salt: &[u8]) -> Scute {
        let data = dir.join(format!("data-{index}"));
        let hash = dir.join(format!("hash-{index}"));
        let mut bytes = [0; VERITY_SUPERBLOCK_SIZE];
        bytes[..8].copy_from_slice(b"verity\0\0");
        bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
        bytes[12..16].copy_from_slice(&1u32.to_le_bytes());
        bytes[32..32 + algorithm.len()].copy_from_slice(algorithm.as_bytes());
        bytes[64..68].copy_from_slice(&4096u32.to_le_bytes());
        bytes[68..72].copy_from_slice(&4096u32.to_le_bytes());
        bytes[72..80].copy_from_slice(&1u64.to_le_bytes());
        bytes[80..82].copy_from_slice(&(salt.len() as u16).to_le_bytes());
        bytes[88..88 + salt.len()].copy_from_slice(salt);
        std::fs::write(&data, [0; 4096]).unwrap();
        std::fs::write(&hash, bytes).unwrap();
        Scute {
            data: File::open(data).unwrap(),
            hash: File::open(hash).unwrap(),
        }
    }

    #[test]
    fn preparation_uses_retained_files_and_preserves_offsets() {
        use std::io::{Seek, SeekFrom};
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let mut input = scute(dir.path(), 0, "sha256", &[0; 32]);
        input.data.seek(SeekFrom::Start(19)).unwrap();
        input.hash.seek(SeekFrom::Start(37)).unwrap();
        let data_inode = input.data.metadata().unwrap().ino();
        let hash_inode = input.hash.metadata().unwrap().ino();
        // New files at the old names must not redirect assembly's inputs.
        for name in ["data-0", "hash-0"] {
            std::fs::remove_file(dir.path().join(name)).unwrap();
            std::fs::write(dir.path().join(name), b"replacement").unwrap();
        }
        let mut chain = Chain {
            root: vec![1; 32].into(),
            scutes: vec![input],
        };
        std::thread::scope(|scope| {
            let jobs: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        let prepared = chain.prepare().unwrap();
                        assert_eq!(prepared[0].data.metadata().unwrap().ino(), data_inode);
                        assert_eq!(prepared[0].hash.metadata().unwrap().ino(), hash_inode);
                    })
                })
                .collect();
            for job in jobs {
                job.join().unwrap();
            }
        });
        assert_eq!(chain.scutes[0].data.stream_position().unwrap(), 19);
        assert_eq!(chain.scutes[0].hash.stream_position().unwrap(), 37);
    }

    #[test]
    fn preparation_revalidates_changed_contents() {
        let dir = tempfile::tempdir().unwrap();
        let chain = Chain {
            root: vec![1; 32].into(),
            scutes: vec![scute(dir.path(), 0, "sha256", &[0; 32])],
        };
        chain.prepare().unwrap();
        // An open read-only handle does not prevent another writer changing it.
        let writer = OpenOptions::new()
            .write(true)
            .open(dir.path().join("hash-0"))
            .unwrap();
        writer.write_all_at(b"badmagic", 0).unwrap();
        assert_eq!(
            chain.prepare().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn direct_chains_reject_empty_before_io() {
        let chain = Chain {
            root: vec![1; 32].into(),
            scutes: vec![],
        };
        assert_eq!(
            chain.prepare().unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn prepares_chains_longer_than_32_scutes() {
        let dir = tempfile::tempdir().unwrap();
        let chain = Chain {
            root: vec![64; 32].into(),
            scutes: (0..64)
                .map(|i| scute(dir.path(), i, "sha256", &[i as u8; 32]))
                .collect(),
        };
        let prepared = chain.prepare().unwrap();
        assert_eq!(prepared.len(), 64);
        for (i, scute) in prepared.iter().enumerate() {
            assert_eq!(&*scute.root, &[(i + 1) as u8; 32]);
        }
    }

    #[test]
    fn direct_chains_require_exact_root_length_and_base_sentinel() {
        let dir = tempfile::tempdir().unwrap();
        for length in [0, 31, 33, 64, 65] {
            let chain = Chain {
                root: vec![1; length].into(),
                scutes: vec![scute(dir.path(), 0, "sha256", &[0; 32])],
            };
            assert_eq!(
                chain.prepare().unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
        let chain = Chain {
            root: vec![1; 32].into(),
            scutes: vec![scute(dir.path(), 0, "sha256", &[1; 32])],
        };
        let error = chain.prepare().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("sentinel"));
    }

    #[test]
    fn direct_chains_validate_algorithm_and_derive_parent_roots() {
        let dir = tempfile::tempdir().unwrap();
        let mut chain = Chain {
            root: vec![2; 32].into(),
            scutes: vec![
                scute(dir.path(), 0, "sha256", &[0; 32]),
                scute(dir.path(), 1, "sha256", &[1; 32]),
            ],
        };
        let prepared = chain.prepare().unwrap();
        assert_eq!(&*prepared[0].root, &[1; 32]);
        assert_eq!(&*prepared[1].root, &[2; 32]);
        chain.scutes[0] = scute(dir.path(), 0, "sha512", &[0; 64]);
        let error = chain.prepare().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("algorithm mismatch"));
        chain.scutes[0] = scute(dir.path(), 0, "sha256", &[0; 32]);
        chain.scutes[1] = scute(dir.path(), 1, "sha256", &[0; 32]);
        assert!(chain
            .prepare()
            .unwrap_err()
            .to_string()
            .contains("sentinel"));
    }
}
