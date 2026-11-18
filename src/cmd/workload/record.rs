// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Retained caller identity. Missing monitor results remain unknown and pinned.

use anyhow::{Context, Result, bail};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use conglobate::artifact::Digest;
use madrillo::{CleanupResult, Completion, PROTOCOL_VERSION};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
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

/// One boot's monitor state, diagnostics and control endpoint.
#[derive(Debug)]
pub(crate) struct Run {
    path: PathBuf,
    directory: Dir,
    endpoint: PathBuf,
}

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
        if let Err(error) = self.reap_abandoned() {
            log::warn!("listing abandoned workload creations: {error:#}");
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

    /// The caller holds the image-index lock, excluding live creators.
    fn reap_abandoned(&self) -> Result<()> {
        for entry in self.directory.entries()? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.strip_prefix(".creating-").is_some_and(|id| {
                id.parse::<Uuid>()
                    .is_ok_and(|parsed| parsed.to_string() == id)
            }) {
                continue;
            }
            let removed: Result<()> = (|| {
                let directory = self.directory.open_dir_nofollow(name)?;
                for child in directory.entries()? {
                    let child = child?;
                    if child.file_name() != "identity.json" || !child.file_type()?.is_file() {
                        bail!("unexpected file in unpublished workload");
                    }
                }
                // Only unlink the recognized artifact through this handle.
                // A late unfamiliar child must make the final rmdir fail.
                match directory.remove_file("identity.json") {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
                drop(directory);
                self.directory.remove_dir(name)?;
                Ok(())
            })();
            if let Err(error) = removed {
                log::warn!("removing abandoned {name}: {error:#}");
            }
        }
        Ok(())
    }

    pub(super) fn get(&self, id: Uuid) -> Result<Record> {
        let name = id.to_string();
        let directory = self.directory.open_dir_nofollow(&name)?;
        let identity: Identity = Run::read(&directory, "identity.json", 1024 * 1024)?
            .context("missing workload identity")?;
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
            (None, _) => bail!("no workload matches ID prefix {selector:?}"),
        }
    }

    pub(super) fn list(&self) -> Result<Vec<Record>> {
        let mut records = Vec::new();
        for entry in self.directory.entries()? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_str().context("invalid workload directory name")?;
            if name.starts_with(".creating-") {
                continue;
            }
            let id = name.parse::<Uuid>()?;
            if !entry.file_type()?.is_dir() {
                bail!("workload identity must be a directory");
            }
            records.push(self.get(id)?);
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
        if matches!(self.run.completion()?, Some(result) if result.cleanup == CleanupResult::Complete)
        {
            // Concurrent Windows removals can observe a delete-pending file as
            // access denied. Keep the cleanup lock outside the removed tree.
            let lock = self.run.directory.open_with(
                "cleanup.lock",
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
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
    }

    pub(crate) fn id(&self) -> Uuid {
        self.identity.id
    }
    pub(super) fn images(&self) -> &[Digest] {
        &self.identity.images
    }
    pub(crate) fn create_build_output(&self) -> Result<PathBuf> {
        self.run.directory.create_dir("output")?;
        Ok(self.run.path.join("output"))
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
    fn creation_reclaims_only_recognized_unpublished_files() -> Result<()> {
        let root = tempfile::tempdir()?;
        let cache = root.path().join("cache");
        std::fs::create_dir(&cache)?;
        let records = Records::open(&cache)?;
        let retained = records.reserve(vec![Digest::Sha256([1; 32])])?;
        let retained_path = retained.run.path().join("identity.json");
        let identity = std::fs::read(&retained_path)?;
        let mut abandoned = Vec::new();
        for partial in [false, true] {
            let path = records.path.join(format!(".creating-{}", Uuid::new_v4()));
            std::fs::create_dir(&path)?;
            if partial {
                std::fs::write(path.join("identity.json"), b"interrupted write")?;
            }
            abandoned.push(path);
        }
        let foreign = records.path.join(format!(".creating-{}", Uuid::new_v4()));
        std::fs::create_dir(&foreign)?;
        std::fs::write(foreign.join("identity.json"), b"keep partial identity")?;
        std::fs::write(foreign.join("foreign"), b"keep foreign file")?;
        let unnamed = records.path.join(".creating-not-a-workload");
        std::fs::create_dir(&unnamed)?;
        std::fs::write(unnamed.join("identity.json"), b"keep unrelated directory")?;
        let noncanonical = records
            .path
            .join(format!(".creating-{}", Uuid::new_v4().simple()));
        std::fs::create_dir(&noncanonical)?;
        std::fs::write(noncanonical.join("identity.json"), b"not a producer name")?;
        let next = records.reserve(vec![Digest::Sha256([2; 32])])?;
        assert!(
            next.run.path().is_dir(),
            "a refused leftover blocked creation"
        );
        assert!(abandoned.iter().all(|path| !path.exists()));
        assert_eq!(std::fs::read(retained_path)?, identity);
        assert_eq!(
            std::fs::read(foreign.join("identity.json"))?,
            b"keep partial identity"
        );
        assert_eq!(
            std::fs::read(foreign.join("foreign"))?,
            b"keep foreign file"
        );
        assert_eq!(
            std::fs::read(unnamed.join("identity.json"))?,
            b"keep unrelated directory"
        );
        assert_eq!(
            std::fs::read(noncanonical.join("identity.json"))?,
            b"not a producer name"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn creation_preserves_staging_symlinks_and_their_targets() -> Result<()> {
        let root = tempfile::tempdir()?;
        let cache = root.path().join("cache");
        std::fs::create_dir(&cache)?;
        let records = Records::open(&cache)?;
        let outside = root.path().join("outside");
        std::fs::create_dir(&outside)?;
        std::fs::write(outside.join("identity.json"), b"external data")?;
        let alias = records.path.join(format!(".creating-{}", Uuid::new_v4()));
        std::os::unix::fs::symlink(&outside, &alias)?;
        let staging = records.path.join(format!(".creating-{}", Uuid::new_v4()));
        std::fs::create_dir(&staging)?;
        std::os::unix::fs::symlink(outside.join("identity.json"), staging.join("identity.json"))?;
        records.reserve(vec![Digest::Sha256([1; 32])])?;
        assert!(alias.symlink_metadata()?.file_type().is_symlink());
        assert!(
            staging
                .join("identity.json")
                .symlink_metadata()?
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read(outside.join("identity.json"))?,
            b"external data"
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
