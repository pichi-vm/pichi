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
    path: PathBuf,
    directory: Dir,
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
            match record.directory.open_dir_nofollow("output") {
                Ok(_) => {
                    if !matches!(record.completion()?, Some(result) if result.cleanup == CleanupResult::Complete)
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
        // Keep Unix socket names within native limits even for long image
        // cache paths. Both peers authenticate the invoking UID.
        let endpoint =
            PathBuf::from("/tmp").join(format!("pichi-{}-{id}.sock", nix::unistd::geteuid()));
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
        let options = cap_std::fs::DirBuilder::new();
        #[cfg(unix)]
        let options = {
            use cap_std::fs::DirBuilderExt;
            let mut options = options;
            options.mode(0o700);
            options
        };
        self.directory.create_dir_with(&name, &options)?;
        let directory = self.directory.open_dir_nofollow(&name)?;
        let identity = Identity {
            version: 1,
            id,
            images,
            endpoint,
        };
        let mut file = directory.open_with(
            "identity.json",
            OpenOptions::new().write(true).create_new(true),
        )?;
        serde_json::to_writer(&mut file, &identity)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            directory.open(".")?.sync_all()?;
            self.directory.open(".")?.sync_all()?;
        }
        Ok(Record {
            identity,
            path: self.path.join(name),
            directory,
        })
    }

    pub(super) fn get(&self, id: Uuid) -> Result<Record> {
        let name = id.to_string();
        let directory = self.directory.open_dir_nofollow(&name)?;
        let identity: Identity = Record::read(&directory, "identity.json", 1024 * 1024)?
            .context("missing workload identity")?;
        if identity.version != 1
            || identity.id != id
            || identity.images.is_empty()
            || identity.images.len() > 1024
        {
            bail!("invalid workload identity");
        }
        Ok(Record {
            identity,
            path: self.path.join(name),
            directory,
        })
    }

    pub(super) fn list(&self) -> Result<Vec<Record>> {
        let mut records = Vec::new();
        for entry in self.directory.entries()? {
            let entry = entry?;
            let name = entry.file_name();
            let id = name
                .to_str()
                .context("invalid workload directory name")?
                .parse::<Uuid>()?;
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
            if !matches!(record.completion()?, Some(result) if result.cleanup == CleanupResult::Complete)
            {
                pins.extend(record.identity.images);
            }
        }
        Ok(pins)
    }
}

impl Record {
    pub(crate) fn id(&self) -> Uuid {
        self.identity.id
    }
    pub(super) fn images(&self) -> &[Digest] {
        &self.identity.images
    }
    pub(super) fn endpoint(&self) -> &Path {
        &self.identity.endpoint
    }
    pub(super) fn state(&self) -> PathBuf {
        self.path.join("monitor")
    }

    pub(crate) fn create_build_output(&self) -> Result<PathBuf> {
        self.directory.create_dir("output")?;
        Ok(self.path.join("output"))
    }

    pub(crate) fn release_build_output(&self) -> Result<()> {
        if matches!(self.completion()?, Some(result) if result.cleanup == CleanupResult::Complete) {
            match self.directory.remove_dir_all("output") {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    pub(crate) fn completion(&self) -> Result<Option<Completion>> {
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
        let mut bytes = Vec::new();
        file.take(limit + 1).read_to_end(&mut bytes)?;
        if u64::try_from(bytes.len())? > limit {
            bail!("workload record exceeds size limit");
        }
        Ok(Some(serde_json::from_slice(&bytes)?))
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
            std::fs::create_dir(record.state())?;
            for cleanup in [
                CleanupResult::Unknown,
                CleanupResult::Incomplete {
                    errors: vec!["helper still running".into()],
                },
            ] {
                result.cleanup = cleanup;
                std::fs::write(
                    record.state().join("result.json"),
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
                record.state().join("result.json"),
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
        symlink(outside.path(), record.state())?;
        assert!(record.completion().is_err());
        Ok(())
    }
}
