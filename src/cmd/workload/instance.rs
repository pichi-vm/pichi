// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Durable instances: frozen configuration, one private qcow2 disk and a
//! lifetime lock held by every process of the current boot.

use super::record::{Entry, Records, Run};
use crate::cmd::writable_storage::InstanceDisk;
use anyhow::{Context, Result, bail, ensure};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use conglobate::artifact::Digest;
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};
use uuid::Uuid;

const VERSION: u16 = 2;
const IDENTITY: &str = "identity.json";
const LOCK: &str = "boot.lock";
const DISK: &str = "storage.qcow2";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    version: u16,
    id: Uuid,
    root: Digest,
    manifest: Digest,
    cpus: u32,
    memory_mib: u32,
    storage_bytes: u64,
    attempt: Option<Attempt>,
}

/// The selected boot.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Attempt {
    id: Uuid,
    endpoint: PathBuf,
    started: bool,
}

/// Configuration fixed at creation.
#[derive(Debug)]
pub(crate) struct Spec {
    pub(crate) root: Digest,
    pub(crate) manifest: Digest,
    pub(crate) cpus: u32,
    pub(crate) memory_mib: u32,
    pub(crate) storage_bytes: u64,
}

#[derive(Debug)]
pub(crate) struct Instance {
    identity: Identity,
    path: PathBuf,
    directory: Dir,
}

/// Derived, never stored.
#[derive(Debug)]
pub(crate) enum State {
    Created,
    Running,
    Exited,

    /// Startup or final result cannot be confirmed.
    Unknown,
}

/// Lifetime ownership of one boot, transferred to its monitor and VMM.
#[derive(Debug)]
pub(crate) struct Lease(File);

/// Owns the operation lock until boot selection and acknowledgment finish.
#[derive(Debug)]
pub(crate) struct Gate {
    _file: File,
}

impl Instance {
    /// Publish a new instance. Caller holds the image-index lock, which
    /// serializes image pins and transient directories.
    pub(crate) fn create(records: &Records, spec: Spec) -> Result<Self> {
        ensure!(
            records.path().to_str().is_some(),
            "instance storage path must be UTF-8"
        );
        let id = Uuid::new_v4();
        let staging = format!(".creating-{id}");
        records
            .directory
            .create_dir_with(&staging, &Records::private_builder())?;
        let identity = Identity {
            version: VERSION,
            id,
            root: spec.root,
            manifest: spec.manifest,
            cpus: spec.cpus,
            memory_mib: spec.memory_mib,
            storage_bytes: spec.storage_bytes,
            attempt: None,
        };
        let built: Result<()> = (|| {
            let directory = records.directory.open_dir_nofollow(&staging)?;
            let new = || {
                let mut options = OpenOptions::new();
                options.read(true).write(true).create_new(true);
                options
            };
            let disk = directory.open_with(DISK, &new())?.into_std();
            let bytes = identity.storage_bytes;
            // Keep disk creation inside publication ownership. A detached
            // worker could keep writing after cancellation releases the index lock.
            InstanceDisk::create(disk, bytes).context("creating instance disk")?;
            directory.open_with("operation.lock", &new())?.sync_all()?;
            let mut file = directory.open_with(IDENTITY, &new())?;
            serde_json::to_writer(&mut file, &identity)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            #[cfg(unix)]
            directory.open(".")?.sync_all()?;
            // Windows cannot rename a directory while handles inside it are open.
            drop((file, directory));
            records
                .directory
                .rename(&staging, &records.directory, id.to_string())?;
            Ok(())
        })();
        if let Err(error) = built {
            if let Err(cleanup) = records.reap(&staging) {
                log::warn!("removing unpublished {staging}: {cleanup:#}");
            }
            return Err(error);
        }
        // Published: a later failure must not hide the instance's ID.
        let directory = (|| {
            #[cfg(unix)]
            records.directory.open(".")?.sync_all()?;
            Ok::<_, anyhow::Error>(records.directory.open_dir_nofollow(id.to_string())?)
        })()
        .with_context(|| format!("instance {id} was published but cannot be reopened"))?;
        Ok(Self {
            identity,
            path: records.path.join(id.to_string()),
            directory,
        })
    }

    pub(super) fn read(bytes: &[u8], id: Uuid, path: PathBuf, directory: Dir) -> Result<Self> {
        let identity: Identity =
            serde_json::from_slice(bytes).context("invalid instance record")?;
        ensure!(
            identity.version == VERSION,
            "unsupported workload record version {}; upgrade pichi",
            identity.version
        );
        ensure!(
            identity.id == id
                && identity.cpus != 0
                && identity.memory_mib != 0
                && identity.storage_bytes != 0,
            "invalid instance record"
        );
        Ok(Self {
            identity,
            path,
            directory,
        })
    }

    /// Re-read the record, for example after acquiring the lease.
    pub(crate) fn reread(self) -> Result<Self> {
        let bytes = Run::read_bytes(&self.directory, IDENTITY, 1024 * 1024)?.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "missing instance record")
        })?;
        Self::read(&bytes, self.identity.id, self.path, self.directory)
    }

    fn save(&self) -> Result<()> {
        let temporary = format!("{IDENTITY}.tmp");
        let mut file = self.directory.open_with(
            &temporary,
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .follow(FollowSymlinks::No),
        )?;
        serde_json::to_writer(&mut file, &self.identity)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        self.directory
            .rename(&temporary, &self.directory, IDENTITY)?;
        #[cfg(unix)]
        self.directory.open(".")?.sync_all()?;
        Ok(())
    }

    pub(crate) fn id(&self) -> Uuid {
        self.identity.id
    }
    pub(crate) fn root(&self) -> &Digest {
        &self.identity.root
    }
    pub(crate) fn manifest(&self) -> &Digest {
        &self.identity.manifest
    }
    pub(crate) fn cpus(&self) -> u32 {
        self.identity.cpus
    }
    pub(crate) fn memory_mib(&self) -> u32 {
        self.identity.memory_mib
    }
    pub(crate) fn attempt(&self) -> Option<Uuid> {
        self.identity.attempt.as_ref().map(|attempt| attempt.id)
    }
    pub(crate) fn disk(&self) -> PathBuf {
        self.path.join(DISK)
    }

    /// The selected boot, if the instance was ever started.
    pub(crate) fn run(&self) -> Result<Option<Run>> {
        let Some(attempt) = &self.identity.attempt else {
            return Ok(None);
        };
        self.run_of(attempt.id, attempt.endpoint.clone()).map(Some)
    }

    /// Reopen the boot an observer selected, even after another boot starts.
    pub(super) fn run_of(&self, id: Uuid, endpoint: PathBuf) -> Result<Run> {
        let name = id.to_string();
        let directory = self
            .directory
            .open_dir_nofollow("runs")?
            .open_dir_nofollow(&name)?;
        Ok(Run::new(
            self.path.join("runs").join(name),
            directory,
            endpoint,
        ))
    }

    pub(crate) fn state(&self) -> Result<State> {
        let Some(run) = self.run()? else {
            return Ok(State::Created);
        };
        if self.live()? {
            return Ok(if self.started() {
                State::Running
            } else {
                State::Unknown
            });
        }
        // The lock is free, so no process of this boot remains to change it.
        Ok(match run.completion() {
            Ok(Some(_)) => State::Exited,
            _ => State::Unknown,
        })
    }

    /// Serialize selection without keeping a gate for the lifetime of a boot.
    pub(crate) fn gate(&self) -> Result<Gate> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).follow(FollowSymlinks::No);
        let gate = self
            .directory
            .open_with("operation.lock", &options)?
            .into_std();
        ensure!(gate.metadata()?.is_file(), "operation lock must be a file");
        gate.try_lock().context("instance is busy")?;
        Ok(Gate { _file: gate })
    }

    /// Whether any process of the selected boot remains. Never waits.
    pub(crate) fn live(&self) -> Result<bool> {
        match self.run()? {
            Some(run) => Lease::live(&run.directory),
            None => Ok(false),
        }
    }

    pub(crate) fn started(&self) -> bool {
        self.identity
            .attempt
            .as_ref()
            .is_some_and(|run| run.started)
    }

    /// Record the acknowledgment while the caller still holds the launch gate.
    pub(crate) fn confirm_started(&mut self, _gate: &Gate) -> Result<()> {
        self.identity
            .attempt
            .as_mut()
            .context("no selected boot")?
            .started = true;
        self.save()
            .context("boot started but its acknowledgment could not be saved")
    }

    /// Reopen the disk without repair before a boot may write to it.
    pub(crate) fn check_disk(&self, _gate: &Gate) -> Result<()> {
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let file = self.directory.open_with(DISK, &options)?.into_std();
        ensure!(file.metadata()?.is_file(), "instance disk must be a file");
        let bytes = self.identity.storage_bytes;
        InstanceDisk::check(file, bytes).with_context(|| {
            format!(
                "instance {} disk {} failed its consistency check; it was not repaired",
                self.identity.id,
                self.disk().display()
            )
        })
    }

    /// Create a fresh boot's run directory. It is not the selected boot
    /// until `select_attempt`.
    pub(crate) fn prepare_attempt(&self, _gate: &Gate) -> Result<(Run, Attempt, Lease)> {
        ensure!(!self.live()?, "instance is running or busy");
        let id = Uuid::new_v4();
        let endpoint = Run::endpoint_for(id)?;
        match self
            .directory
            .create_dir_with("runs", &Records::private_builder())
        {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let runs = self.directory.open_dir_nofollow("runs")?;
        runs.create_dir_with(id.to_string(), &Records::private_builder())?;
        let name = id.to_string();
        let directory = runs.open_dir_nofollow(&name)?;
        let lease = Lease::create(&directory)?;
        #[cfg(unix)]
        {
            directory.open(".")?.sync_all()?;
            runs.open(".")?.sync_all()?;
        }
        let run = Run::new(
            self.path.join("runs").join(&name),
            directory,
            endpoint.clone(),
        );
        Ok((
            run,
            Attempt {
                id,
                endpoint,
                started: false,
            },
            lease,
        ))
    }

    /// Select the boot, once its monitor listens.
    pub(crate) fn select_attempt(&mut self, _gate: &Gate, attempt: Attempt) -> Result<()> {
        // Preparation proved the previous boot ended under this launch gate.
        // Remove any resize socket left by that boot.
        if let Some(previous) = self.run()? {
            previous.tidy();
        }
        self.identity.attempt = Some(attempt);
        self.save()
    }

    /// Exit callback for one boot: remove its stale resize socket after
    /// confirmed cleanup. Never touches the disk, record or lock.
    pub(super) fn release(path: &Path) -> Result<()> {
        let attempt: Uuid = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("run directory must name an attempt")?
            .parse()?;
        let instance = path
            .parent()
            .and_then(Path::parent)
            .context("run directory must be inside an instance")?;
        let id = instance
            .file_name()
            .and_then(|name| name.to_str())
            .context("instance directory must name an ID")?
            .parse()?;
        let records = Records::at(instance.parent().context("instance parent")?)?;
        let Entry::Instance(instance) = records.get(id)? else {
            bail!("expected an instance record");
        };
        if instance.attempt() != Some(attempt) {
            return Ok(());
        }
        let run = instance.run()?.context("missing selected run")?;
        if run
            .completion()?
            .is_some_and(|result| result.cleanup == madrillo::CleanupResult::Complete)
        {
            run.release_endpoint()?;
        }
        Ok(())
    }
}

impl Lease {
    fn create(directory: &Dir) -> Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(windows)]
        {
            use cap_std::fs::OpenOptionsExt as _;
            // Sharing restrictions follow inherited handles; byte-range locks do not.
            options.share_mode(0);
        }
        let file = directory.open_with(LOCK, &options)?.into_std();
        #[cfg(unix)]
        file.try_lock()?;
        file.sync_all()?;
        Ok(Self(file))
    }

    fn live(directory: &Dir) -> Result<bool> {
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        #[cfg(windows)]
        {
            use cap_std::fs::OpenOptionsExt as _;
            options.share_mode(7);
        }
        let file = match directory.open_with(LOCK, &options) {
            Ok(file) => file.into_std(),
            #[cfg(windows)]
            Err(error) if error.raw_os_error() == Some(32) => return Ok(true),
            Err(error) => return Err(error.into()),
        };
        ensure!(file.metadata()?.is_file(), "boot lock must be a file");
        #[cfg(unix)]
        match file.try_lock_shared() {
            Ok(()) => Ok(false),
            Err(std::fs::TryLockError::WouldBlock) => Ok(true),
            Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
        }
        #[cfg(windows)]
        Ok(false)
    }

    /// One handle for the monitor to hold until cleanup and one for the VMM,
    /// so the lock outlives every process of the boot.
    pub(crate) fn clone_boot(&self) -> Result<(File, File)> {
        Ok((self.0.try_clone()?, self.0.try_clone()?))
    }
}

/// Run a last-owner test without unrelated process creation.
#[cfg(test)]
pub(crate) fn isolated(test: &str) -> Result<bool> {
    const CHILD: &str = "PICHI_TEST_INSTANCE_CASE";
    if std::env::var(CHILD).as_deref() == Ok(test) {
        return Ok(false);
    }
    // Parallel tests spawn children; a fork briefly inherits CLOEXEC locks.
    // Last-owner assertions need a process without those unrelated children.
    let output = std::process::Command::new(std::env::current_exe()?)
        .args(["--exact", test, "--nocapture"])
        .env(CHILD, test)
        .output()?;
    ensure!(
        output.status.success()
            && String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
        "isolated {test} failed or did not run: {output:?}"
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    impl Spec {
        fn fixture() -> Self {
            Self {
                root: Digest::Sha256([1; 32]),
                manifest: Digest::Sha256([2; 32]),
                cpus: 1,
                memory_mib: 128,
                storage_bytes: 2 * 1024 * 1024,
            }
        }
    }

    // APFS cannot create the invalid-UTF-8 filename this regression needs.
    #[cfg(target_os = "linux")]
    #[test]
    fn rejects_non_utf8_disk_path_before_instance_publication() -> Result<()> {
        use std::os::unix::{ffi::OsStringExt as _, fs::symlink};
        let root = tempfile::tempdir()?;
        let actual = root
            .path()
            .join(std::ffi::OsString::from_vec(b"cache-\xff".to_vec()));
        std::fs::create_dir(&actual)?;
        let alias = root.path().join("cache");
        symlink(&actual, &alias)?;
        let records = Records::open(&alias)?;
        let error = Instance::create(&records, Spec::fixture()).unwrap_err();
        assert!(error.to_string().contains("UTF-8"), "{error:#}");
        assert!(records.directory.entries()?.next().is_none());
        Ok(())
    }

    #[tokio::test]
    async fn operation_gate_refuses_contention_and_never_recreates_a_missing_lock() -> Result<()> {
        let root = tempfile::tempdir()?;
        let graphroot = root.path().join("cache");
        std::fs::create_dir(&graphroot)?;
        let records = Records::open(&graphroot)?;
        let instance = Instance::create(&records, Spec::fixture())?;
        let first = instance.gate()?;
        assert!(instance.gate().is_err());
        drop(first);
        drop(instance.gate()?);
        let lock = instance.path.join("operation.lock");
        std::fs::remove_file(&lock)?;
        assert!(instance.gate().is_err());
        assert!(!lock.exists());
        Ok(())
    }

    #[tokio::test]
    async fn disk_and_pins_outlive_every_boot_handle() -> Result<()> {
        if isolated("cmd::workload::instance::tests::disk_and_pins_outlive_every_boot_handle")? {
            return Ok(());
        }
        let root = tempfile::tempdir()?;
        let graphroot = root.path().join("cache");
        std::fs::create_dir(&graphroot)?;
        let records = Records::open(&graphroot)?;
        let mut instance = Instance::create(&records, Spec::fixture())?;
        assert!(matches!(instance.state()?, State::Created));
        assert_eq!(records.pins()?, vec![Digest::Sha256([1; 32])]);
        assert!(instance.disk().is_file());
        assert!(matches!(records.get(instance.id())?, Entry::Instance(_)));
        let gate = instance.gate()?;
        instance.check_disk(&gate)?;
        let (run, attempt, lease) = instance.prepare_attempt(&gate)?;
        assert!(
            instance.attempt().is_none(),
            "preparation must not select a boot"
        );
        instance.select_attempt(&gate, attempt)?;
        let selected = instance.attempt();
        let (monitor, vm) = lease.clone_boot()?;
        drop(lease);
        assert!(instance.live()?);
        drop(monitor);
        assert!(
            instance.live()?,
            "a VM must retain exclusion after its monitor dies"
        );
        drop(vm);
        assert!(!instance.live()?);
        assert!(
            matches!(instance.state()?, State::Unknown),
            "a lost monitor must not fabricate an exit status"
        );
        assert_eq!(records.pins()?, vec![Digest::Sha256([1; 32])]);
        assert!(instance.disk().is_file());
        let instance = instance.reread()?;
        assert_eq!(instance.attempt(), selected);
        assert_eq!(
            instance.run()?.context("selected boot")?.endpoint(),
            run.endpoint()
        );
        instance.check_disk(&gate)?;
        let lock = run.path().join(LOCK);
        std::fs::remove_file(&lock)?;
        assert!(instance.live().is_err());
        assert!(instance.prepare_attempt(&gate).is_err());
        assert!(!lock.exists(), "a missing boot lock was recreated");
        Ok(())
    }

    #[tokio::test]
    async fn disconnected_waiter_reads_its_original_boot_after_restart() -> Result<()> {
        if isolated(
            "cmd::workload::instance::tests::disconnected_waiter_reads_its_original_boot_after_restart",
        )? {
            return Ok(());
        }
        use super::super::Selection;
        use interprocess::local_socket::{GenericFilePath, ListenerOptions, tokio::prelude::*};
        use madrillo::{CleanupResult, Completion, CompletionCause, LeaderResult, OomEvidence};

        #[cfg(target_os = "linux")]
        {
            use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
            setrlimit(
                Resource::Nofile,
                Rlimit {
                    current: Some(64),
                    ..getrlimit(Resource::Nofile)
                },
            )?;
        }

        let root = tempfile::tempdir()?;
        let graphroot = root.path().join("cache");
        std::fs::create_dir(&graphroot)?;
        let config = crate::config::Config {
            storage: crate::config::StorageConfig {
                graphroot: Some(graphroot.clone()),
                runroot: Some(root.path().join("run")),
            },
            ..crate::config::Config::default()
        };
        let records = Records::open(&graphroot)?;
        let mut instance = Instance::create(&records, Spec::fixture())?;
        let created = Selection::open(&instance.id().to_string(), &config)?;
        let gate = instance.gate()?;
        let (first, mut attempt, lease) = instance.prepare_attempt(&gate)?;
        if cfg!(unix) {
            attempt.endpoint = root.path().join("waiter.sock");
        }
        let listener = ListenerOptions::new()
            .name(attempt.endpoint.as_path().to_fs_name::<GenericFilePath>()?)
            .create_tokio()?;
        instance.select_attempt(&gate, attempt)?;
        // Batch selection must not keep one directory handle per operand.
        let queued = (0..256)
            .map(|_| Selection::open(&instance.id().to_string(), &config))
            .collect::<Result<Vec<_>>>()?;
        let observer = super::super::Workload::new(records.get(instance.id())?)?;
        let waiting = observer.wait();
        tokio::pin!(waiting);
        let connection = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                connection = listener.accept() => Ok::<_, anyhow::Error>(connection?),
                result = &mut waiting => bail!("waiter ended before connecting: {result:?}"),
            }
        })
        .await??;

        // Complete the observed boot and select a different one before the
        // waiter sees EOF. Its fallback must still report the first result.
        drop(lease);
        let (second, attempt, _lease) = instance.prepare_attempt(&gate)?;
        for (run, code) in [(&first, 37), (&second, 42)] {
            let result = Completion {
                version: madrillo::PROTOCOL_VERSION,
                leader: LeaderResult::Exit { code },
                cause: CompletionCause::LeaderExit,
                exited_at_unix_ms: 0,
                cleanup: CleanupResult::Complete,
                oom: OomEvidence::NoneObserved,
                log_errors: Vec::new(),
                monitor_errors: Vec::new(),
                monitor_warnings: Vec::new(),
                output_truncated: false,
            };
            std::fs::create_dir(run.state())?;
            std::fs::write(
                run.state().join("result.json"),
                serde_json::to_vec(&result)?,
            )?;
        }
        instance.select_attempt(&gate, attempt)?;
        drop(connection);
        let result = tokio::time::timeout(Duration::from_secs(5), waiting).await??;
        assert_eq!(result.code()?, 37);
        for selected in queued {
            assert_eq!(selected.reopen()?.wait().await?.code()?, 37);
        }
        assert!(created.reopen()?.stop(Duration::ZERO).await?.is_none());
        assert_eq!(
            instance
                .run()?
                .context("selected boot")?
                .completion()?
                .context("selected result")?
                .leader,
            LeaderResult::Exit { code: 42 },
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn selection_never_follows_a_temporary_record_symlink() -> Result<()> {
        let root = tempfile::tempdir()?;
        let graphroot = root.path().join("cache");
        std::fs::create_dir(&graphroot)?;
        let records = Records::open(&graphroot)?;
        let mut instance = Instance::create(&records, Spec::fixture())?;
        let gate = instance.gate()?;
        let before = std::fs::read(instance.disk())?;
        let temporary = instance.path.join(format!("{IDENTITY}.tmp"));
        std::os::unix::fs::symlink(DISK, &temporary)?;
        let (_, attempt, _lease) = instance.prepare_attempt(&gate)?;
        assert!(instance.select_attempt(&gate, attempt).is_err());
        assert_eq!(std::fs::read(instance.disk())?, before);
        let mut instance = instance.reread()?;
        assert!(instance.attempt().is_none());

        // A partial regular file left by an interrupted write is reusable.
        std::fs::remove_file(&temporary)?;
        std::fs::write(&temporary, b"partial identity")?;
        let (_, attempt, _lease) = instance.prepare_attempt(&gate)?;
        let selected = attempt.id;
        instance.select_attempt(&gate, attempt)?;
        assert_eq!(instance.reread()?.attempt(), Some(selected));
        assert!(!temporary.exists());
        Ok(())
    }
}
