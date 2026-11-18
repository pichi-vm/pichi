// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Retained caller identity. Missing monitor results remain unknown and pinned.

use anyhow::{Context, Result, bail, ensure};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use conglobate::artifact::Digest;
use madrillo::{CleanupResult, Completion, PROTOCOL_VERSION};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    version: u16,
    id: Uuid,
    images: Vec<Digest>,
    endpoint: PathBuf,
}

#[derive(Debug)]
pub(crate) struct Records {
    path: PathBuf,
    directory: Dir,
}

#[derive(Debug)]
pub(crate) struct Record {
    identity: Identity,
    pub(crate) run: Run,
}

/// Host ownership of output spans import and any uncancelled snapshot worker.
#[derive(Clone)]
pub(crate) struct BuildOutput {
    path: PathBuf,
    _owner: Arc<std::fs::File>,
}

impl AsRef<Path> for BuildOutput {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

/// One boot's monitor state, diagnostics and control endpoint.
#[derive(Debug)]
pub(crate) struct Run {
    path: PathBuf,
    directory: Dir,
    endpoint: PathBuf,
}

/// A selector has no retained record, rather than unreadable retained state.
#[derive(Debug)]
pub(super) struct NoSuchWorkload(String);

impl std::fmt::Display for NoSuchWorkload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no workload matches {:?}", self.0)
    }
}

impl std::error::Error for NoSuchWorkload {}

impl Records {
    /// Build guests select their own inputs from the read-only image cache.
    /// Their retained output directory also marks that whole-cache lifetime;
    /// only confirmed complete workload cleanup releases it for collection.
    pub(crate) fn build_cache_in_use(graphroot: &Path) -> Result<bool> {
        let Some(records) = Self::existing(graphroot)? else {
            return Ok(false);
        };
        for record in records.list()? {
            match record.run.directory.open_dir_nofollow("output") {
                Ok(_) => {
                    if !matches!(record.run.completion()?, Some(result) if result.cleanup == CleanupResult::Complete)
                    {
                        return Ok(true);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(false)
    }

    pub(crate) fn open(graphroot: &Path) -> Result<Self> {
        let path = Self::location(graphroot)?;
        let parent = Dir::open_ambient_dir(
            path.parent().context("workload state parent")?,
            cap_std::ambient_authority(),
        )?;
        #[cfg(windows)]
        Self::private_directory(&parent)?;
        let options = cap_std::fs::DirBuilder::new();
        #[cfg(unix)]
        let options = {
            use cap_std::fs::DirBuilderExt;
            let mut options = options;
            options.mode(0o700);
            options
        };
        match parent.create_dir_with(path.file_name().context("workload state name")?, &options) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        Self::from_parent(path, &parent)?.context("missing workload directory")
    }

    pub(super) fn existing(graphroot: &Path) -> Result<Option<Self>> {
        let path = match Self::location(graphroot) {
            Ok(path) => path,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let parent = match Dir::open_ambient_dir(
            path.parent().context("workload state parent")?,
            cap_std::ambient_authority(),
        ) {
            Ok(parent) => parent,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Self::from_parent(path, &parent)
    }

    // Cache aliases share one identity. Keep state outside the graphroot:
    // build guests mount that image cache read-only and must not see consoles.
    fn location(graphroot: &Path) -> Result<PathBuf> {
        let root = match std::fs::canonicalize(graphroot) {
            Ok(root) => root,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let parent = graphroot
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                std::fs::canonicalize(parent)?.join(
                    graphroot
                        .file_name()
                        .context("cache needs a directory name")?,
                )
            }
            Err(error) => return Err(error.into()),
        };
        let mut name = root
            .file_name()
            .context("cache needs a directory name")?
            .to_os_string();
        name.push(".workloads");
        Ok(root.with_file_name(name))
    }

    /// A read-only lookup: prune must not create or repair workload state.
    pub(crate) fn pins_at(graphroot: &Path) -> Result<Vec<Digest>> {
        match Self::existing(graphroot)? {
            Some(records) => records.pins(),
            None => Ok(Vec::new()),
        }
    }

    fn from_parent(path: PathBuf, parent: &Dir) -> Result<Option<Self>> {
        let directory =
            match parent.open_dir_nofollow(path.file_name().context("workload state name")?) {
                Ok(directory) => directory,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
        Self::private_directory(&directory)?;
        Ok(Some(Self { path, directory }))
    }

    #[cfg(unix)]
    fn private_directory(directory: &Dir) -> Result<()> {
        use cap_std::fs::{MetadataExt, PermissionsExt};
        let metadata = directory.dir_metadata()?;
        if metadata.uid() != nix::unistd::geteuid().as_raw()
            || metadata.permissions().mode() & 0o077 != 0
        {
            bail!("workload directory must be private and owned by the invoking user");
        }
        Ok(())
    }

    #[cfg(windows)]
    fn private_directory(directory: &Dir) -> Result<()> {
        use win_custody::{ObjectSecurityDescriptor, OwnedSid};
        let descriptor = ObjectSecurityDescriptor::query_kernel_object(directory)?;
        let user = OwnedSid::from_string(&win_custody::current_user_sid_string()?)?;
        let system = OwnedSid::from_string("S-1-5-18")?;
        let administrators = OwnedSid::from_string("S-1-5-32-544")?;
        let creator = OwnedSid::from_string("S-1-3-0")?;
        let trusted = [&user, &system, &administrators];
        let owner = descriptor.owner()?;
        let acl = descriptor.dacl()?;
        let private = trusted.iter().any(|sid| owner.matches(sid))
            && acl.allowed_aces().all(|ace| {
                ace.mask() == 0
                    || trusted.iter().any(|sid| ace.trustee_matches(sid))
                    || (ace.trustee_matches(&creator) && ace.flags() & 0x08 != 0)
            })
            && acl.allowed_aces().any(|ace| {
                ace.flags() & 0x07 == 0x03
                    && (ace.mask() & 0x1000_0000 != 0 || ace.mask() & 0x001f_01ff == 0x001f_01ff)
                    && (ace.trustee_matches(&user) || ace.trustee_matches(&creator))
            });
        if !private {
            bail!("workload parent must grant private, inheritable user access");
        }
        Ok(())
    }

    /// Caller holds the image-index lock through resolution and this write.
    pub(crate) fn reserve(&self, images: Vec<Digest>) -> Result<Record> {
        let id = Uuid::new_v4();
        #[cfg(unix)]
        // Respect the caller's native temporary directory, including sandbox
        // restrictions. A compact UUID leaves room for macOS socket paths and
        // the resize suffix. Both peers authenticate the invoking UID.
        let endpoint = std::path::absolute(std::env::temp_dir())?.join(id.simple().to_string());
        #[cfg(windows)]
        let endpoint = PathBuf::from(format!(r"\\.\pipe\pichi-{id}"));
        self.create(images, endpoint, id)
    }

    /// Caller holds the image-index lock until the identity is durably written.
    pub(super) fn create(
        &self,
        images: Vec<Digest>,
        endpoint: PathBuf,
        id: Uuid,
    ) -> Result<Record> {
        if images.is_empty() || images.len() > 1024 {
            bail!("a workload must pin between 1 and 1024 images");
        }
        let name = id.to_string();
        // The image-index lock excludes other creators. Preserve an existing
        // entry even if it is empty, which rename could otherwise replace.
        match self.directory.symlink_metadata(&name) {
            Ok(_) => return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists).into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let options = cap_std::fs::DirBuilder::new();
        #[cfg(unix)]
        let options = {
            use cap_std::fs::DirBuilderExt;
            let mut options = options;
            options.mode(0o700);
            options
        };
        let identity = Identity {
            version: 1,
            id,
            images,
            endpoint,
        };
        // Observers do not hold the image-index lock. Publish only a complete
        // identity; an interrupted creation must never look like a workload.
        let staging = format!(".creating-{id}");
        self.directory.create_dir_with(&staging, &options)?;
        let published: Result<()> = (|| {
            let directory = self.directory.open_dir_nofollow(&staging)?;
            for name in ["publication.lock", "cleanup.lock"] {
                directory
                    .open_with(name, OpenOptions::new().write(true).create_new(true))?
                    .sync_all()?;
            }
            let mut file = directory.open_with(
                "identity.json",
                OpenOptions::new().write(true).create_new(true),
            )?;
            serde_json::to_writer(&mut file, &identity)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            #[cfg(unix)]
            directory.open(".")?.sync_all()?;
            // Windows cannot rename a directory with open handles inside it.
            drop((file, directory));
            self.directory.rename(&staging, &self.directory, &name)?;
            Ok(())
        })();
        if let Err(error) = published {
            if let Err(cleanup) = self.directory.remove_dir_all(&staging) {
                log::warn!("removing unpublished {staging}: {cleanup:#}");
            }
            return Err(error);
        }
        #[cfg(unix)]
        self.directory.open(".")?.sync_all()?;
        let directory = self.directory.open_dir_nofollow(&name)?;
        Ok(Record {
            run: Run {
                path: self.path.join(name),
                directory,
                endpoint: identity.endpoint.clone(),
            },
            identity,
        })
    }

    pub(super) fn get(&self, id: Uuid) -> Result<Record> {
        let name = id.to_string();
        let directory = match self.directory.open_dir_nofollow(&name) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // A dangling link or missing file inside an existing record is
                // corruption, not a nonexistent target.
                match self.directory.symlink_metadata(&name) {
                    Err(absent) if absent.kind() == std::io::ErrorKind::NotFound => {
                        return Err(error).context(NoSuchWorkload(name));
                    }
                    _ => return Err(error.into()),
                }
            }
            Err(error) => return Err(error.into()),
        };
        let identity: Identity = match Run::read(&directory, "identity.json", 1024 * 1024)? {
            Some(identity) => identity,
            None => {
                // Removal may retire and empty an already-open directory on
                // Unix. A missing identity is corruption only while published.
                match self.directory.symlink_metadata(&name) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        return Err(NoSuchWorkload(name).into());
                    }
                    Err(error) => return Err(error.into()),
                    Ok(_) => bail!("missing workload identity"),
                }
            }
        };
        if identity.version != 1
            || identity.id != id
            || identity.images.is_empty()
            || identity.images.len() > 1024
        {
            bail!("invalid workload identity");
        }
        Ok(Record {
            run: Run {
                path: self.path.join(name),
                directory,
                endpoint: identity.endpoint.clone(),
            },
            identity,
        })
    }

    /// Resolve a full UUID or a unique prefix before any workload operation.
    pub(super) fn resolve(&self, selector: &str) -> Result<Record> {
        if let Ok(id) = selector.parse::<Uuid>() {
            return self.get(id);
        }
        let prefix = selector.replace('-', "").to_ascii_lowercase();
        let mut matches = self.list()?.into_iter().filter(|record| {
            !prefix.is_empty() && record.id().simple().to_string().starts_with(&prefix)
        });
        match (matches.next(), matches.next()) {
            (Some(record), None) => Ok(record),
            (Some(_), Some(_)) => bail!("workload prefix {selector:?} matches more than one ID"),
            (None, _) => Err(NoSuchWorkload(selector.to_owned()).into()),
        }
    }

    pub(super) fn list(&self) -> Result<Vec<Record>> {
        let mut records = Vec::new();
        for entry in self.directory.entries()? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_str().context("invalid workload directory name")?;
            if name.starts_with(".creating-") || name.starts_with(".retired-") {
                continue;
            }
            let id = name.parse::<Uuid>()?;
            ensure!(
                name == id.to_string(),
                "noncanonical workload directory name {name:?}"
            );
            match self.get(id) {
                Ok(record) => records.push(record),
                Err(error) if error.is::<NoSuchWorkload>() => {}
                Err(error) => return Err(error),
            }
        }
        records.sort_by_key(Record::id);
        Ok(records)
    }

    pub(super) fn pins(&self) -> Result<Vec<Digest>> {
        let mut pins = Vec::new();
        for record in self.list()? {
            if !matches!(record.run.completion()?, Some(result) if result.cleanup == CleanupResult::Complete)
            {
                pins.extend(record.identity.images);
            }
        }
        Ok(pins)
    }

    pub(super) fn has_retired(&self, id: Uuid) -> Result<bool> {
        match self.directory.symlink_metadata(format!(".retired-{id}")) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Retry metadata deletion even when the identity was already removed.
    /// The caller holds the image-index lock, excluding another remover.
    pub(super) fn remove_retired(&self, id: Uuid) -> Result<()> {
        if !self.has_retired(id)? {
            return Ok(());
        }
        let name = format!(".retired-{id}");
        let result = (|| -> Result<()> {
            let directory = self.directory.open_dir_nofollow(&name)?;
            for entry in directory.entries()? {
                let entry = entry?;
                let kind = entry.file_type()?;
                let known = match entry.file_name().to_str() {
                    Some(
                        "identity.json" | "monitor.err" | "build.lock" | "cleanup.lock"
                        | "publication.lock",
                    ) => kind.is_file(),
                    Some("monitor") => {
                        Self::check_monitor(&directory)?;
                        true
                    }
                    _ => false,
                };
                ensure!(known, "unexpected file in retired workload {id}");
            }
            drop(directory);
            #[cfg(unix)]
            self.directory.open(".")?.sync_all()?;
            self.directory.remove_dir_all(&name)?;
            Ok(())
        })();
        result.with_context(|| {
            format!("removing retired workload metadata; retry with pichi rm {id}")
        })
    }

    fn check_monitor(directory: &Dir) -> Result<()> {
        let monitor = directory.open_dir_nofollow("monitor")?;
        for entry in monitor.entries()? {
            let entry = entry?;
            // Madrillo also retains an OOM sentinel when the kernel reports it.
            ensure!(
                matches!(
                    entry.file_name().to_str(),
                    Some("console.log" | "result.json" | "oom")
                ) && entry.file_type()?.is_file(),
                "unexpected file in workload monitor: {:?}",
                entry.file_name()
            );
        }
        Ok(())
    }

    /// Retire a workload after its payload is gone. Caller holds the index lock
    /// and has dropped all handles inside the record (required on Windows).
    fn retire(&self, id: Uuid) -> Result<()> {
        let name = id.to_string();
        let retired = format!(".retired-{id}");
        let rename = || self.directory.rename(&name, &self.directory, &retired);
        #[cfg(windows)]
        let renamed = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                match rename() {
                    // Completed workloads can still have observers or the exit
                    // callback briefly holding directory handles. cap-std must
                    // deny delete sharing; wait only at this final rename.
                    Err(error)
                        if matches!(error.raw_os_error(), Some(5 | 32))
                            && std::time::Instant::now() < deadline =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    result => break result,
                }
            }
        };
        #[cfg(not(windows))]
        let renamed = rename();
        renamed.with_context(|| format!("retiring workload {id}"))?;
        self.remove_retired(id)
    }
}

impl Record {
    pub(super) fn at(path: &Path) -> Result<Self> {
        let id = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("workload directory must name an ID")?
            .parse()?;
        let records = path.parent().context("workload directory parent")?;
        let parent = Dir::open_ambient_dir(
            records.parent().context("workload root parent")?,
            cap_std::ambient_authority(),
        )?;
        Records::from_parent(records.to_owned(), &parent)?
            .context("missing workload directory")?
            .get(id)
    }

    pub(crate) fn storage_directory(&self) -> PathBuf {
        self.run.path.join("storage")
    }

    pub(crate) fn release_runtime_files(&self) -> Result<()> {
        let result = (|| -> Result<()> {
            if matches!(self.run.completion()?, Some(result) if result.cleanup == CleanupResult::Complete)
            {
                // New records publish their cleanup lock before launch. Never
                // recreate it through a stale handle after metadata retirement.
                let legacy = match self.run.directory.symlink_metadata("publication.lock") {
                    Ok(metadata) => {
                        ensure!(
                            metadata.is_file(),
                            "publication lock must be a regular file"
                        );
                        false
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                    Err(error) => return Err(error.into()),
                };
                if legacy {
                    // Old records still need lazy initialization, but cannot
                    // be retired by rm. A deleted marker in a retired new
                    // record must not be mistaken for this older format.
                    Self::at(self.run.path())?;
                }
                // Concurrent Windows removals can observe a delete-pending file as
                // access denied. Keep the cleanup lock outside the removed tree.
                let lock = self.run.directory.open_with(
                    "cleanup.lock",
                    OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(legacy)
                        .truncate(false)
                        .follow(FollowSymlinks::No),
                )?;
                if !lock.metadata()?.is_file() {
                    bail!("workload cleanup lock must be a regular file");
                }
                let lock = lock.into_std();
                lock.lock().context("locking workload cleanup")?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::{FileTypeExt, MetadataExt};
                    // A forced Dillo exit cannot run its socket-name destructor.
                    // Remove only its owned socket, after confirmed scope cleanup.
                    let endpoint = self.run.resize_endpoint();
                    match std::fs::symlink_metadata(&endpoint) {
                        Ok(metadata)
                            if metadata.file_type().is_socket()
                                && metadata.uid() == nix::unistd::geteuid().as_raw() =>
                        {
                            match std::fs::remove_file(endpoint) {
                                Ok(()) => {}
                                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                                Err(error) => return Err(error.into()),
                            }
                        }
                        Ok(_) => log::warn!(
                            "leaving unexpected file at console resize endpoint {}",
                            endpoint.display()
                        ),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                match self.run.directory.remove_dir_all("storage") {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Ok(())
        })();
        if result.is_err()
            && matches!(Self::at(self.run.path()), Err(error) if error.is::<NoSuchWorkload>())
        {
            // A remover can unlink a directory still held by a Unix observer.
            // Only confirmed disappearance makes its cleanup a no-op.
            return Ok(());
        }
        result
    }

    pub(crate) fn id(&self) -> Uuid {
        self.identity.id
    }
    pub(super) fn images(&self) -> &[Digest] {
        &self.identity.images
    }
    pub(crate) fn create_build_output(&self) -> Result<BuildOutput> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        let owner = self
            .run
            .directory
            .open_with("build.lock", &options)?
            .into_std();
        owner.lock()?;
        self.run.directory.create_dir("output")?;
        Ok(BuildOutput {
            path: self.run.path.join("output"),
            _owner: Arc::new(owner),
        })
    }

    /// Caller holds the image-index lock through host exclusion and retirement.
    pub(super) fn remove(self, records: &Records) -> Result<()> {
        ensure!(
            matches!(self.run.completion()?, Some(result) if result.cleanup == CleanupResult::Complete),
            "workload is running or its cleanup is unknown"
        );
        let publication = self.run.publication_guard().context("monitor publication is still owned, or this older record has no ownership evidence; preserve it until its monitor has stopped")?;
        let owner = Run::open_file(&self.run.directory, "build.lock")
            .context("opening build ownership lock")?
            .map(cap_std::fs::File::into_std);
        if let Some(owner) = &owner {
            owner
                .try_lock()
                .context("build output is still owned by the host importer")?;
        }
        // Preflight before releasing anything, including the old-build case:
        // a result proves guest cleanup, never the old host importer's exit.
        for entry in self
            .run
            .directory
            .entries()
            .context("listing workload files")?
        {
            let entry = entry?;
            let kind = entry.file_type()?;
            let known = match entry.file_name().to_str() {
                Some(
                    "identity.json" | "monitor.err" | "build.lock" | "cleanup.lock"
                    | "publication.lock",
                ) => kind.is_file(),
                Some("monitor") => {
                    Records::check_monitor(&self.run.directory)?;
                    true
                }
                Some("storage") => kind.is_dir(),
                Some("output") => {
                    ensure!(
                        owner.is_some(),
                        "older build has no host ownership lock; preserve its output until its build command has stopped"
                    );
                    kind.is_dir()
                }
                _ => false,
            };
            ensure!(known, "unexpected file in workload {}", self.id());
        }
        self.release_runtime_files()
            .context("removing workload runtime files")?;
        self.release_build_output()
            .context("removing build output")?;
        for name in ["storage", "output"] {
            match self.run.directory.symlink_metadata(name) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
                Ok(_) => bail!("workload {name} remains after cleanup"),
            }
        }
        let id = self.id();
        // No new host can acquire this record; creation and removal share the
        // index lock. Drop handles before renaming on Windows.
        drop((publication, owner, self));
        records.retire(id)
    }

    pub(crate) fn release_build_output(&self) -> Result<()> {
        if matches!(self.run.completion()?, Some(result) if result.cleanup == CleanupResult::Complete)
        {
            match self.run.directory.remove_dir_all("output") {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

impl Run {
    /// Legacy monitors already hold caller-supplied handles until publication.
    pub(crate) fn publication_guard(&self) -> Result<std::fs::File> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).follow(FollowSymlinks::No);
        #[cfg(windows)]
        {
            use cap_std::fs::OpenOptionsExt as _;
            // Sharing restrictions survive transfer to the monitor; Windows
            // byte-range locks belong to the original locking process.
            options.share_mode(0);
        }
        let file = self
            .directory
            .open_with("publication.lock", &options)?
            .into_std();
        ensure!(
            file.metadata()?.is_file(),
            "publication lock must be a file"
        );
        #[cfg(unix)]
        file.try_lock()?;
        Ok(file)
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn endpoint(&self) -> &Path {
        &self.endpoint
    }
    pub(super) fn resize_endpoint(&self) -> PathBuf {
        let mut endpoint = self.endpoint.as_os_str().to_os_string();
        endpoint.push(".resize");
        endpoint.into()
    }
    pub(super) fn state(&self) -> PathBuf {
        self.path.join("monitor")
    }

    pub(super) fn console_log(&self) -> Result<std::fs::File> {
        let monitor = self.directory.open_dir_nofollow("monitor")?;
        Ok(Self::open_file(&monitor, "console.log")?
            .context("workload has no retained console log")?
            .into_std())
    }

    pub(super) fn monitor_diagnostic(&self) -> Result<String> {
        let Some(file) = Self::open_file(&self.directory, "monitor.err")? else {
            return Ok(String::new());
        };
        let mut bytes = Vec::new();
        file.take(8193).read_to_end(&mut bytes)?;
        let truncated = bytes.len() > 8192;
        bytes.truncate(8192);
        let mut text = String::from_utf8_lossy(&bytes).trim().to_owned();
        if truncated {
            text.push_str(" [truncated]");
        }
        Ok(text)
    }

    pub(super) fn completion(&self) -> Result<Option<Completion>> {
        let monitor = match self.directory.open_dir_nofollow("monitor") {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let Some(result): Option<Completion> = Self::read(&monitor, "result.json", 1024 * 1024)?
        else {
            return Ok(None);
        };
        if result.version != PROTOCOL_VERSION {
            bail!("unsupported retained monitor result version");
        }
        Ok(Some(result))
    }

    fn read<T: serde::de::DeserializeOwned>(
        directory: &Dir,
        name: &str,
        limit: u64,
    ) -> Result<Option<T>> {
        let Some(file) = Self::open_file(directory, name)? else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        file.take(limit + 1).read_to_end(&mut bytes)?;
        if u64::try_from(bytes.len())? > limit {
            bail!("workload record exceeds size limit");
        }
        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    fn open_file(directory: &Dir, name: &str) -> Result<Option<cap_std::fs::File>> {
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.custom_flags(nix::libc::O_NONBLOCK);
        }
        let file = match directory.open_with(name, &options) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !file.metadata()?.is_file() {
            bail!("workload record must be a regular file");
        }
        Ok(Some(file))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cli::{PruneArgs, RmiArgs},
        config::{Config, StorageConfig},
    };
    use conglobate::artifact::{
        ConfigDescriptor, Layer, MEDIA_TYPE_PICHI_ARTIFACT_V1, Manifest, PmiDescriptor,
    };
    use madrillo::{CompletionCause, LeaderResult, OomEvidence};
    use pichi_storage::{BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};
    use sha2::{Digest as _, Sha256};

    #[cfg(unix)]
    #[test]
    fn stale_cleanup_never_recreates_retired_metadata() -> Result<()> {
        let root = tempfile::tempdir()?;
        let graphroot = root.path().join("cache");
        std::fs::create_dir(&graphroot)?;
        let records = Records::open(&graphroot)?;
        for remove_publication in [false, true] {
            let record = records.reserve(vec![Digest::Sha256([1; 32])])?;
            std::fs::create_dir(record.run.state())?;
            let result = Completion {
                version: madrillo::PROTOCOL_VERSION,
                leader: LeaderResult::Exit { code: 0 },
                cause: CompletionCause::LeaderExit,
                exited_at_unix_ms: 0,
                cleanup: CleanupResult::Complete,
                oom: OomEvidence::NoneObserved,
                log_errors: Vec::new(),
                monitor_errors: Vec::new(),
                monitor_warnings: Vec::new(),
                output_truncated: false,
            };
            std::fs::write(
                record.run.state().join("result.json"),
                serde_json::to_vec(&result)?,
            )?;
            record.release_runtime_files()?;
            let retired = format!(".retired-{}", record.id());
            records
                .directory
                .rename(record.id().to_string(), &records.directory, &retired)?;
            let path = records.path.join(&retired);
            std::fs::remove_file(path.join("cleanup.lock"))?;
            if remove_publication {
                std::fs::remove_file(path.join("publication.lock"))?;
            }
            // Simulate final deletion having removed the lock while this
            // callback still owns an opened directory and a complete result.
            record.release_runtime_files()?;
            assert!(
                !path.join("cleanup.lock").exists(),
                "stale cleanup recreated retired metadata"
            );
            drop(record);
            records.remove_retired(Uuid::parse_str(retired.trim_start_matches(".retired-"))?)?;
            assert!(!path.exists());
        }
        Ok(())
    }

    #[test]
    fn removal_requires_released_publication_ownership() -> Result<()> {
        if crate::test_support::isolated(
            "cmd::workload::record::tests::removal_requires_released_publication_ownership",
        ) {
            return Ok(());
        }
        let root = tempfile::tempdir()?;
        let graphroot = root.path().join("cache");
        std::fs::create_dir(&graphroot)?;
        let records = Records::open(&graphroot)?;
        let id = Uuid::new_v4();
        let record = records.create(
            vec![Digest::Sha256([1; 32])],
            root.path().join("control"),
            id,
        )?;
        let path = record.run.path().to_owned();
        std::fs::create_dir(record.run.state())?;
        let result = Completion {
            version: madrillo::PROTOCOL_VERSION,
            leader: LeaderResult::Exit { code: 37 },
            cause: CompletionCause::LeaderExit,
            exited_at_unix_ms: 0,
            cleanup: CleanupResult::Complete,
            oom: OomEvidence::NoneObserved,
            log_errors: Vec::new(),
            monitor_errors: Vec::new(),
            monitor_warnings: Vec::new(),
            output_truncated: false,
        };
        std::fs::write(
            path.join("monitor/result.json"),
            serde_json::to_vec(&result)?,
        )?;
        std::fs::remove_file(path.join("cleanup.lock"))?;
        assert!(record.release_runtime_files().is_err());
        std::fs::write(path.join("cleanup.lock"), b"")?;
        let publication = record.run.publication_guard()?;
        drop(record);
        assert!(records.get(id)?.remove(&records).is_err());
        assert_eq!(records.get(id)?.run.completion()?.unwrap(), result);
        drop(publication);
        // Older records have no evidence that a monitor's publication is done.
        std::fs::remove_file(path.join("publication.lock"))?;
        std::fs::remove_file(path.join("cleanup.lock"))?;
        records.get(id)?.release_runtime_files()?;
        assert!(path.join("cleanup.lock").is_file());
        assert!(records.get(id)?.remove(&records).is_err());
        assert!(path.join("identity.json").exists());
        // Restore the marker only in this fixture: its publisher is known gone.
        std::fs::write(path.join("publication.lock"), b"")?;
        records.get(id)?.remove(&records)?;
        assert!(!path.exists());
        Ok(())
    }

    #[test]
    fn noncanonical_stored_ids_cannot_silently_release_pins() -> Result<()> {
        let root = tempfile::tempdir()?;
        let graphroot = root.path().join("cache");
        std::fs::create_dir(&graphroot)?;
        let records = Records::open(&graphroot)?;
        let id = Uuid::new_v4();
        let record = records.create(
            vec![Digest::Sha256([1; 32])],
            root.path().join("control"),
            id,
        )?;
        std::fs::create_dir(record.run.path().join("output"))?;
        drop(record);
        records
            .directory
            .rename(id.to_string(), &records.directory, id.simple().to_string())?;
        assert!(records.list().is_err());
        assert!(records.pins().is_err());
        assert!(Records::build_cache_in_use(&graphroot).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_errors_are_ignored_only_after_confirmed_retirement() -> Result<()> {
        let root = tempfile::tempdir()?;
        let graphroot = root.path().join("cache");
        std::fs::create_dir(&graphroot)?;
        let records = Records::open(&graphroot)?;
        let id = Uuid::new_v4();
        let record = records.create(
            vec![Digest::Sha256([1; 32])],
            root.path().join("control"),
            id,
        )?;
        std::fs::create_dir(record.run.state())?;
        std::fs::write(record.run.state().join("result.json"), b"partial removal")?;
        assert!(record.release_runtime_files().is_err());
        records
            .directory
            .rename(id.to_string(), &records.directory, format!(".retired-{id}"))?;
        record.release_runtime_files()?;
        // A published but corrupt replacement must not be treated as absence.
        records.directory.create_dir(id.to_string())?;
        assert!(record.release_runtime_files().is_err());
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn retirement_waits_for_an_observer_and_preserves_a_blocked_record() -> Result<()> {
        use std::time::Duration;
        let root = tempfile::tempdir()?;
        let graphroot = root.path().join("cache");
        std::fs::create_dir(&graphroot)?;
        let records = Records::open(&graphroot)?;
        for release in [true, false] {
            let id = Uuid::new_v4();
            let record = records.create(
                vec![Digest::Sha256([0; 32])],
                PathBuf::from(format!(r"\\.\pipe\pichi-{id}")),
                id,
            )?;
            let path = record.run.path().to_owned();
            let identity = std::fs::read(path.join("identity.json"))?;
            // Establish the real Windows refusal while this observer owns the
            // directory. No monitor timing or synthetic error is involved.
            let error = records
                .directory
                .rename(id.to_string(), &records.directory, format!(".retired-{id}"))
                .expect_err("an open directory must block Windows retirement");
            assert!(matches!(error.raw_os_error(), Some(5 | 32)), "{error}");
            if !release {
                assert!(records.retire(id).is_err());
                assert_eq!(std::fs::read(path.join("identity.json"))?, identity);
                continue;
            }
            std::thread::scope(|scope| -> Result<()> {
                let (finished, result) = std::sync::mpsc::channel();
                scope.spawn({
                    let records = &records;
                    move || finished.send(records.retire(id))
                });
                assert!(matches!(
                    result.recv_timeout(Duration::from_millis(100)),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                ));
                assert_eq!(std::fs::read(path.join("identity.json"))?, identity);
                drop(record);
                result.recv_timeout(Duration::from_secs(6))??;
                Ok(())
            })?;
            assert!(!path.exists());
            assert!(!records.path.join(format!(".retired-{id}")).exists());
        }
        Ok(())
    }

    #[tokio::test]
    async fn removal_retries_retired_metadata_without_an_identity() -> Result<()> {
        use crate::cmd::workload::Workload;

        let root = tempfile::tempdir()?;
        let cache = root.path().join("cache");
        std::fs::create_dir(&cache)?;
        let config = Config {
            storage: StorageConfig {
                graphroot: Some(cache.clone()),
                runroot: Some(root.path().join("run")),
            },
            ..Config::default()
        };
        let records = Records::open(&cache)?;
        let id = Uuid::new_v4();
        let retired = records.path.join(format!(".retired-{id}"));
        std::fs::create_dir(&retired)?;
        // An interrupted deletion may have removed identity.json already.
        std::fs::create_dir(retired.join("monitor"))?;
        std::fs::write(retired.join("monitor/result.json"), b"remaining metadata")?;
        std::fs::write(retired.join("monitor/oom"), b"")?;
        assert!(records.list()?.is_empty());
        for name in [
            "storage",
            "output",
            "foreign",
            "monitor/foreign",
            "monitor/console.log",
        ] {
            std::fs::create_dir(retired.join(name))?;
            let payload = retired.join(name).join("data");
            std::fs::write(&payload, b"preserve")?;
            assert_eq!(Workload::remove(&id.to_string(), &config).await?, 2);
            assert_eq!(std::fs::read(payload)?, b"preserve");
            std::fs::remove_dir_all(retired.join(name))?;
        }
        #[cfg(unix)]
        {
            let outside = root.path().join("outside");
            std::fs::write(&outside, b"external")?;
            std::os::unix::fs::symlink(&outside, retired.join("monitor.err"))?;
            assert_eq!(Workload::remove(&id.to_string(), &config).await?, 2);
            assert_eq!(std::fs::read(outside)?, b"external");
            std::fs::remove_file(retired.join("monitor.err"))?;
        }
        #[cfg(windows)]
        {
            // A real final deletion error must not become success. The
            // retained directory is still retryable after its holder exits.
            let held = records
                .directory
                .open_dir_nofollow(format!(".retired-{id}"))?;
            let error = records.remove_retired(id).unwrap_err();
            assert!(format!("{error:#}").contains(&format!("pichi rm {id}")));
            assert_eq!(Workload::remove(&id.to_string(), &config).await?, 2);
            assert!(retired.exists());
            drop(held);
        }
        assert_eq!(Workload::remove(&id.to_string(), &config).await?, 0);
        assert!(!retired.exists());
        assert_eq!(Workload::remove(&id.to_string(), &config).await?, 1);
        Ok(())
    }

    #[tokio::test]
    async fn observers_release_directories_and_keep_completion_fallback() -> Result<()> {
        use crate::cmd::workload::Workload;
        use futures_util::{SinkExt, StreamExt};
        use interprocess::local_socket::{GenericFilePath, ListenerOptions, tokio::prelude::*};
        use std::time::Duration;
        use tokio_util::codec::{Framed, LengthDelimitedCodec};

        for operation in ["wait", "stop", "attach"] {
            for removed in [false, true] {
                let root = tempfile::tempdir()?;
                let graphroot = root.path().join("cache");
                std::fs::create_dir(&graphroot)?;
                let records = Records::open(&graphroot)?;
                let id = Uuid::new_v4();
                #[cfg(unix)]
                let endpoint = root.path().join("observer.sock");
                #[cfg(windows)]
                let endpoint = PathBuf::from(format!(r"\\.\pipe\pichi-observer-{id}"));
                let listener = ListenerOptions::new()
                    .name(endpoint.as_path().to_fs_name::<GenericFilePath>()?)
                    .create_tokio()?;
                let record = records.create(vec![Digest::Sha256([0; 32])], endpoint, id)?;
                let path = record.run.path().to_owned();
                let observer = Workload(record);
                let observing = tokio::spawn(async move {
                    match operation {
                        "wait" => observer.wait().await?.code(),
                        "stop" => observer.stop(Duration::ZERO).await?.code(),
                        _ => observer.attach(false).await,
                    }
                });
                tokio::time::timeout(Duration::from_secs(5), async {
                    let mut server =
                        Framed::new(listener.accept().await?, LengthDelimitedCodec::new());
                    let frame = server
                        .next()
                        .await
                        .context("observer disconnected before request")??;
                    let request: madrillo::Request = serde_json::from_slice(&frame)?;
                    assert!(matches!(
                        (operation, request.message),
                        ("wait", madrillo::ClientMessage::Wait)
                            | ("stop", madrillo::ClientMessage::Terminate)
                            | ("attach", madrillo::ClientMessage::Attach { .. })
                    ));
                    let result = Completion {
                        version: madrillo::PROTOCOL_VERSION,
                        leader: LeaderResult::Exit { code: 37 },
                        cause: CompletionCause::LeaderExit,
                        exited_at_unix_ms: 0,
                        cleanup: CleanupResult::Complete,
                        oom: OomEvidence::NoneObserved,
                        log_errors: Vec::new(),
                        monitor_errors: Vec::new(),
                        monitor_warnings: Vec::new(),
                        output_truncated: false,
                    };
                    std::fs::create_dir(path.join("monitor"))?;
                    std::fs::write(
                        path.join("monitor/result.json"),
                        serde_json::to_vec(&result)?,
                    )?;
                    let mut responses = Vec::new();
                    if operation == "attach" {
                        responses.push(madrillo::ServerMessage::Attached { input: false });
                    }
                    if removed {
                        // The observer is blocked on this connection. Windows
                        // refuses removal if it still owns the record directory.
                        records.directory.remove_dir_all(id.to_string())?;
                        assert!(!path.exists());
                        responses.push(madrillo::ServerMessage::Completed { result });
                    }
                    for message in responses {
                        server
                            .send(
                                serde_json::to_vec(&madrillo::Response {
                                    version: madrillo::PROTOCOL_VERSION,
                                    message,
                                })?
                                .into(),
                            )
                            .await?;
                    }
                    // Without completion, the retained result supplies the
                    // fallback; with removal, the confirmed response must stand.
                    if !removed {
                        drop(server);
                    }
                    assert_eq!(observing.await??, 37);
                    Ok::<_, anyhow::Error>(())
                })
                .await??;
            }
        }
        Ok(())
    }

    #[test]
    fn failed_creation_does_not_publish_an_incomplete_record() -> Result<()> {
        let root = tempfile::tempdir()?;
        let cache = root.path().join("cache");
        std::fs::create_dir(&cache)?;
        let records = Records::open(&cache)?;
        #[cfg(unix)]
        let endpoint = {
            use std::os::unix::ffi::OsStringExt;
            std::ffi::OsString::from_vec(vec![0xff])
        };
        #[cfg(windows)]
        let endpoint = {
            use std::os::windows::ffi::OsStringExt;
            std::ffi::OsString::from_wide(&[0xd800])
        };
        // Path serialization fails after identity.json has been opened.
        assert!(
            records
                .create(
                    vec![Digest::Sha256([1; 32])],
                    endpoint.into(),
                    Uuid::new_v4()
                )
                .is_err()
        );
        assert!(records.list()?.is_empty());
        assert!(records.pins()?.is_empty());
        assert_eq!(records.directory.entries()?.count(), 0);
        Ok(())
    }

    #[test]
    fn unpublished_records_are_invisible_and_published_records_are_preserved() -> Result<()> {
        let root = tempfile::tempdir()?;
        let cache = root.path().join("cache");
        std::fs::create_dir(&cache)?;
        let records = Records::open(&cache)?;
        // An interrupted creator never published this directory.
        records
            .directory
            .create_dir(format!(".creating-{}", Uuid::new_v4()))?;
        assert!(records.list()?.is_empty());
        assert!(records.pins()?.is_empty());
        let id = Uuid::new_v4();
        let images = vec![Digest::Sha256([1; 32])];
        let endpoint = root.path().join("control");
        let record = records.create(images.clone(), endpoint.clone(), id)?;
        assert_eq!(record.id(), id);
        assert_eq!(records.list()?.len(), 1);
        assert_eq!(records.pins()?, images);
        assert!(
            records
                .create(vec![Digest::Sha256([2; 32])], endpoint, id)
                .is_err()
        );
        assert_eq!(records.get(id)?.images(), images);
        let empty = Uuid::new_v4();
        records.directory.create_dir(empty.to_string())?;
        assert!(
            records
                .create(images, root.path().join("control"), empty)
                .is_err()
        );
        assert_eq!(
            records
                .directory
                .open_dir_nofollow(empty.to_string())?
                .entries()?
                .count(),
            0
        );
        Ok(())
    }

    #[test]
    fn id_prefixes_are_unique_case_insensitive_and_ignore_hyphens() -> Result<()> {
        let root = tempfile::tempdir()?;
        let cache = root.path().join("cache");
        std::fs::create_dir(&cache)?;
        let records = Records::open(&cache)?;
        let ids = [
            "abcdefab-cdef-4123-8123-123456789abc".parse::<Uuid>()?,
            "abcdefab-cdef-4567-8567-123456789abc".parse::<Uuid>()?,
        ];
        for id in ids {
            records.create(
                vec![Digest::Sha256([1; 32])],
                root.path().join(id.to_string()),
                id,
            )?;
        }
        for selector in [
            ids[0].to_string(),
            ids[0].simple().to_string(),
            "abcdefabcdef41".into(),
            "ABCDEFAB-CDEF-41".into(),
        ] {
            assert_eq!(records.resolve(&selector)?.id(), ids[0]);
        }
        assert!(
            records
                .resolve("abcdefab")
                .unwrap_err()
                .to_string()
                .contains("more than one ID")
        );
        for selector in ["deadbeef", "", "---"] {
            assert!(
                records
                    .resolve(selector)
                    .unwrap_err()
                    .to_string()
                    .contains("no workload matches")
            );
        }
        let absent = "abcdefab-cdef-4999-8999-123456789abc";
        assert!(records.resolve(absent).is_err());
        Ok(())
    }

    #[test]
    fn storage_cleanup_requires_complete_evidence_and_is_idempotent() -> Result<()> {
        let root = tempfile::tempdir()?;
        let cache = root.path().join("cache");
        std::fs::create_dir(&cache)?;
        let record = Records::open(&cache)?.create(
            vec![Digest::Sha256([1; 32])],
            root.path().join("control"),
            Uuid::new_v4(),
        )?;
        #[cfg(unix)]
        let _resize = {
            use interprocess::local_socket::{GenericFilePath, ListenerOptions, prelude::*};
            ListenerOptions::new()
                .name(
                    record
                        .run
                        .resize_endpoint()
                        .to_fs_name::<GenericFilePath>()?,
                )
                .create_sync()?
        };
        std::fs::create_dir(record.storage_directory())?;
        let backing = record.storage_directory().join("storage.raw");
        std::fs::write(&backing, b"guest data")?;
        Record::at(record.run.path())?.release_runtime_files()?;
        assert!(backing.exists(), "missing result must retain storage");
        std::fs::create_dir(record.run.state())?;
        for cleanup in [
            CleanupResult::Unknown,
            CleanupResult::Incomplete {
                errors: vec!["helper still running".into()],
            },
            CleanupResult::Complete,
        ] {
            let result = Completion {
                version: PROTOCOL_VERSION,
                leader: LeaderResult::Exit { code: 0 },
                cause: CompletionCause::LeaderExit,
                exited_at_unix_ms: 0,
                cleanup,
                oom: OomEvidence::NoneObserved,
                log_errors: Vec::new(),
                monitor_errors: Vec::new(),
                monitor_warnings: Vec::new(),
                output_truncated: false,
            };
            std::fs::write(
                record.run.state().join("result.json"),
                serde_json::to_vec(&result)?,
            )?;
            for _ in 0..2 {
                Record::at(record.run.path())?.release_runtime_files()?;
                assert_eq!(backing.exists(), result.cleanup != CleanupResult::Complete);
                #[cfg(unix)]
                assert_eq!(
                    record.run.resize_endpoint().exists(),
                    result.cleanup != CleanupResult::Complete
                );
            }
        }
        #[cfg(unix)]
        {
            drop(_resize);
            let endpoint = record.run.resize_endpoint();
            std::fs::write(&endpoint, b"not a workload socket")?;
            std::fs::create_dir(record.storage_directory())?;
            std::fs::write(&backing, b"guest data")?;
            for _ in 0..2 {
                Record::at(record.run.path())?.release_runtime_files()?;
                assert_eq!(std::fs::read(&endpoint)?, b"not a workload socket");
                assert!(!backing.exists(), "complete cleanup still releases storage");
            }
        }
        // The foreground caller, stop observer and monitor exit command can
        // all release the same completed workload at once.
        for _ in 0..32 {
            std::fs::create_dir(record.storage_directory())?;
            std::fs::write(&backing, b"guest data")?;
            std::fs::write(record.storage_directory().join(".lock"), b"")?;
            let observers = (0..3)
                .map(|_| Record::at(record.run.path()))
                .collect::<Result<Vec<_>>>()?;
            let barrier = std::sync::Barrier::new(observers.len());
            std::thread::scope(|scope| {
                let workers: Vec<_> = observers
                    .into_iter()
                    .map(|observer| {
                        let barrier = &barrier;
                        scope.spawn(move || {
                            barrier.wait();
                            observer.release_runtime_files()
                        })
                    })
                    .collect();
                for worker in workers {
                    worker.join().expect("cleanup worker panicked")?;
                }
                Ok::<_, anyhow::Error>(())
            })?;
            assert!(!record.storage_directory().exists());
            assert!(record.run.completion()?.is_some());
        }
        Ok(())
    }

    #[tokio::test]
    async fn legacy_removal_requires_complete_cleanup_and_released_host_ownership() -> Result<()> {
        if crate::test_support::isolated(
            "cmd::workload::record::tests::legacy_removal_requires_complete_cleanup_and_released_host_ownership",
        ) {
            return Ok(());
        }
        use std::time::Duration;

        // Pause the real blocking snapshot after it has taken ownership.
        struct Snapshot {
            output: BuildOutput,
            entered: Arc<tokio::sync::Notify>,
            release: std::sync::mpsc::Receiver<()>,
        }
        impl AsRef<Path> for Snapshot {
            fn as_ref(&self) -> &Path {
                self.entered.notify_one();
                self.release.recv_timeout(Duration::from_secs(10)).unwrap();
                self.output.as_ref()
            }
        }

        let root = tempfile::tempdir()?;
        let cache = root.path().join("cache");
        std::fs::create_dir(&cache)?;
        let records = Records::open(&cache)?;
        let record = records.create(
            vec![Digest::Sha256([1; 32])],
            root.path().join("control"),
            Uuid::new_v4(),
        )?;
        let path = record.run.path().to_owned();
        std::fs::create_dir(record.storage_directory())?;
        std::fs::write(record.storage_directory().join("storage.raw"), b"disk")?;
        std::fs::create_dir(record.run.state())?;
        drop(record);
        assert_eq!(
            Record::at(&path)?.remove(&records).unwrap_err().to_string(),
            "workload is running or its cleanup is unknown"
        );
        let mut result = Completion {
            version: PROTOCOL_VERSION,
            leader: LeaderResult::Exit { code: 1 },
            cause: CompletionCause::LeaderExit,
            exited_at_unix_ms: 0,
            cleanup: CleanupResult::Unknown,
            oom: OomEvidence::NoneObserved,
            log_errors: Vec::new(),
            monitor_errors: Vec::new(),
            monitor_warnings: Vec::new(),
            output_truncated: false,
        };
        std::fs::write(
            path.join("monitor/result.json"),
            serde_json::to_vec(&result)?,
        )?;
        assert_eq!(
            Record::at(&path)?.remove(&records).unwrap_err().to_string(),
            "workload is running or its cleanup is unknown"
        );
        assert_eq!(std::fs::read(path.join("storage/storage.raw"))?, b"disk");
        // Only after proving the completion gate, introduce the independent
        // host-import lock so it cannot mask a missing completion check.
        result.cleanup = CleanupResult::Complete;
        std::fs::write(
            path.join("monitor/result.json"),
            serde_json::to_vec(&result)?,
        )?;
        let output = Record::at(&path)?.create_build_output()?;
        std::fs::write(output.as_ref().join("partial"), b"build output")?;
        assert!(
            Record::at(&path)?
                .remove(&records)
                .unwrap_err()
                .to_string()
                .contains("still owned by the host importer")
        );
        assert_eq!(std::fs::read(path.join("storage/storage.raw"))?, b"disk");
        let entered = Arc::new(tokio::sync::Notify::new());
        let (release, receiver) = std::sync::mpsc::channel();
        let snapshot = Snapshot {
            output: output.clone(),
            entered: entered.clone(),
            release: receiver,
        };
        let store = FilesystemBlobStore::new(&cache);
        let task =
            tokio::spawn(
                async move { crate::cmd::load::OciLayout::read_build(snapshot, &store).await },
            );
        tokio::time::timeout(Duration::from_secs(10), entered.notified()).await?;
        task.abort();
        assert!(matches!(task.await, Err(error) if error.is_cancelled()));
        drop(output);
        assert!(Record::at(&path)?.remove(&records).is_err());
        assert_eq!(std::fs::read(path.join("output/partial"))?, b"build output");
        release.send(())?;
        // The aborted async task cannot be joined to its blocking worker.
        // Observe the ownership lock becoming available instead.
        let owner = std::fs::File::open(path.join("build.lock"))?;
        tokio::time::timeout(Duration::from_secs(10), async {
            while owner.try_lock().is_err() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        owner.unlock()?;
        drop(owner);
        Record::at(&path)?.remove(&records)?;
        assert!(!path.exists());
        assert_eq!(records.directory.entries()?.count(), 0);
        Ok(())
    }

    #[test]
    fn legacy_removal_preserves_old_build_output_and_unexpected_files() -> Result<()> {
        if crate::test_support::isolated(
            "cmd::workload::record::tests::legacy_removal_preserves_old_build_output_and_unexpected_files",
        ) {
            return Ok(());
        }
        let root = tempfile::tempdir()?;
        let cache = root.path().join("cache");
        std::fs::create_dir(&cache)?;
        let records = Records::open(&cache)?;
        let record = records.create(
            vec![Digest::Sha256([1; 32])],
            root.path().join("control"),
            Uuid::new_v4(),
        )?;
        let path = record.run.path().to_owned();
        let result = Completion {
            version: PROTOCOL_VERSION,
            leader: LeaderResult::Exit { code: 0 },
            cause: CompletionCause::LeaderExit,
            exited_at_unix_ms: 0,
            cleanup: CleanupResult::Complete,
            oom: OomEvidence::NoneObserved,
            log_errors: Vec::new(),
            monitor_errors: Vec::new(),
            monitor_warnings: Vec::new(),
            output_truncated: false,
        };
        std::fs::create_dir(record.run.state())?;
        std::fs::write(
            record.run.state().join("result.json"),
            serde_json::to_vec(&result)?,
        )?;
        drop(record);
        std::fs::create_dir(path.join("output"))?;
        std::fs::write(path.join("output/partial"), b"older build")?;
        let error = Record::at(&path)?.remove(&records).unwrap_err();
        assert!(error.to_string().contains("older build"));
        assert_eq!(std::fs::read(path.join("output/partial"))?, b"older build");
        // The documented migration preserves old output outside the record.
        let preserved = root.path().join("preserved-output");
        std::fs::rename(path.join("output"), &preserved)?;
        for name in ["foreign", "monitor/foreign"] {
            std::fs::write(path.join(name), b"not ours")?;
            assert!(Record::at(&path)?.remove(&records).is_err());
            assert_eq!(std::fs::read(path.join(name))?, b"not ours");
            std::fs::remove_file(path.join(name))?;
        }
        std::fs::write(path.join("monitor/oom"), b"")?;
        // A still-published record without its identity is corrupt, not an
        // already-removed record that the exit callback may silently accept.
        let identity = std::fs::read(path.join("identity.json"))?;
        std::fs::remove_file(path.join("identity.json"))?;
        let error = super::super::Workload::cleanup(&path).unwrap_err();
        assert!(error.to_string().contains("missing workload identity"));
        assert!(!error.is::<NoSuchWorkload>());
        assert!(records.list().is_err());
        std::fs::write(path.join("identity.json"), identity)?;
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&preserved, path.join("storage"))?;
            assert!(Record::at(&path)?.remove(&records).is_err());
            std::fs::remove_file(path.join("storage"))?;
            std::os::unix::fs::symlink(
                preserved.join("partial"),
                path.join("monitor/console.log"),
            )?;
            assert!(Record::at(&path)?.remove(&records).is_err());
            assert!(path.join("monitor/console.log").is_symlink());
            std::fs::remove_file(path.join("monitor/console.log"))?;
        }
        Record::at(&path)?.remove(&records)?;
        assert!(!path.exists());
        super::super::Workload::cleanup(&path)?;
        assert_eq!(std::fs::read(preserved.join("partial"))?, b"older build");
        Ok(())
    }

    #[tokio::test]
    async fn image_gc_preserves_pins_and_guest_selected_build_inputs() -> Result<()> {
        for build in [false, true] {
            let root = tempfile::tempdir()?;
            let graphroot = root.path().join("cache");
            let config = Config {
                storage: StorageConfig {
                    graphroot: Some(graphroot.clone()),
                    ..Default::default()
                },
                ..Default::default()
            };
            let blobs = FilesystemBlobStore::new(&graphroot);
            let tags = FilesystemTagDb::open(&graphroot)?;
            let records = Records::open(&graphroot)?;
            let mut images = Vec::new();
            let mut layers = Vec::new();
            for payload in [b"appliance".as_slice(), b"build source".as_slice()] {
                let layer = Digest::Sha256(Sha256::digest(payload).into());
                blobs.put_blob(&layer, payload).await?;
                let manifest = Manifest {
                    schema_version: 2,
                    media_type: "application/vnd.oci.image.manifest.v1+json".into(),
                    artifact_type: MEDIA_TYPE_PICHI_ARTIFACT_V1.into(),
                    config: ConfigDescriptor::canonical(),
                    layers: vec![Layer::Pmi(PmiDescriptor {
                        digest: layer.to_string(),
                        size: payload.len() as u64,
                    })],
                    annotations: std::collections::BTreeMap::default(),
                };
                let bytes = serde_json::to_vec(&manifest)?;
                let image = Digest::Sha256(Sha256::digest(&bytes).into());
                blobs.put_blob(&image, &bytes).await?;
                images.push(image);
                layers.push(layer);
            }
            tags.set_tag("docker.io/library/appliance:test", &images[0])
                .await?;
            let pins = if build {
                images[..1].to_vec()
            } else {
                images.clone()
            };
            let record = records.create(pins, root.path().join("control"), Uuid::new_v4())?;
            if build {
                record.create_build_output()?;
            }
            assert!(
                !record
                    .run
                    .state()
                    .starts_with(std::fs::canonicalize(&graphroot)?),
                "build's cache mount must not include console state"
            );
            crate::cmd::rmi::run(
                RmiArgs {
                    references: vec!["appliance:test".into()],
                    force: false,
                },
                &config,
            )
            .await?;
            assert_eq!(tags.list_tags().await?, Vec::new());
            for digest in images.iter().chain(&layers) {
                assert!(
                    blobs.blob_exists(digest).await,
                    "rmi removed a workload image: {digest}"
                );
            }
            let mut result = Completion {
                version: PROTOCOL_VERSION,
                leader: LeaderResult::Exit { code: 0 },
                cause: CompletionCause::LeaderExit,
                exited_at_unix_ms: 0,
                cleanup: CleanupResult::Unknown,
                oom: OomEvidence::NoneObserved,
                log_errors: Vec::new(),
                monitor_errors: Vec::new(),
                monitor_warnings: Vec::new(),
                output_truncated: false,
            };
            std::fs::create_dir(record.run.state())?;
            for cleanup in [
                CleanupResult::Unknown,
                CleanupResult::Incomplete {
                    errors: vec!["helper still running".into()],
                },
            ] {
                result.cleanup = cleanup;
                std::fs::write(
                    record.run.state().join("result.json"),
                    serde_json::to_vec(&result)?,
                )?;
                crate::cmd::prune::run(PruneArgs { dry_run: false }, &config).await?;
                for digest in images.iter().chain(&layers) {
                    assert!(
                        blobs.blob_exists(digest).await,
                        "prune removed a workload image: {digest}"
                    );
                }
            }
            result.cleanup = CleanupResult::Complete;
            std::fs::write(
                record.run.state().join("result.json"),
                serde_json::to_vec(&result)?,
            )?;
            crate::cmd::prune::run(PruneArgs { dry_run: false }, &config).await?;
            for digest in images.iter().chain(&layers) {
                assert!(
                    !blobs.blob_exists(digest).await,
                    "completed workload retained {digest}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn missing_records_are_read_only_and_partial_publication_fails_closed() -> Result<()> {
        let root = tempfile::tempdir()?;
        let graphroot = root.path().join("cache");
        assert_eq!(Records::pins_at(&graphroot)?, Vec::new());
        assert!(!Records::location(&graphroot)?.exists());
        std::fs::create_dir(&graphroot)?;
        let records = Records::open(&graphroot)?;
        let partial = Uuid::new_v4().to_string();
        records.directory.create_dir(&partial)?;
        assert!(
            records.pins().is_err(),
            "a crash during identity publication must stop GC"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn replaced_workload_directory_and_completion_links_fail_closed() -> Result<()> {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        let graphroot = root.path().join("cache");
        std::fs::create_dir(&graphroot)?;
        let state = Records::location(&graphroot)?;
        symlink(outside.path(), &state)?;
        assert!(Records::pins_at(&graphroot).is_err());
        std::fs::remove_file(&state)?;
        let records = Records::open(&graphroot)?;
        let image = Digest::Sha256([0; 32]);
        let record = records.create(vec![image], root.path().join("control"), Uuid::new_v4())?;
        symlink(outside.path(), record.run.state())?;
        assert!(record.run.completion().is_err());
        Ok(())
    }
}
