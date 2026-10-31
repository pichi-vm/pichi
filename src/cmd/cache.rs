// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Authenticated image metadata, platform selection and cache reachability.

use crate::cmd::platform::PlatformExt as _;
use anyhow::{Context, Result, anyhow, ensure};
use conglobate::artifact::{
    Digest, Index, IndexDescriptor, Manifest, Platform, Reference, ReferenceKind,
};
use pichi_registry::{OCI_IMAGE_INDEX_MEDIA_TYPE, pick_pichi_entry_from_index};
use pichi_storage::{BlobStore, TagDb};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    io,
};
use tokio::io::AsyncReadExt as _;

use super::manifest_ext::ManifestExt as _;

pub(super) const METADATA_CAP: u64 = 1024 * 1024;
pub(super) use conglobate::artifact::MEDIA_TYPE_OCI_MANIFEST_V1 as MANIFEST_TYPE;
pub(super) const TARGET_OS: &str = "pichi";
/// Captured bytes retain their original content identity through publication.
pub(super) struct ImageMetadata {
    pub digest: Digest,
    pub bytes: Vec<u8>,
    pub value: Value,
}

impl TryFrom<(Digest, Vec<u8>)> for ImageMetadata {
    type Error = anyhow::Error;

    fn try_from((digest, bytes): (Digest, Vec<u8>)) -> Result<Self> {
        ensure!(
            bytes.len() as u64 <= METADATA_CAP,
            "image metadata exceeds size limit"
        );
        let actual = Digest::Sha256(Sha256::digest(&bytes).into());
        ensure!(actual == digest, "image metadata digest mismatch: {digest}");
        let value: Value = serde_json::from_slice(&bytes)?;
        ensure!(
            value["schemaVersion"] == 2,
            "unsupported image metadata version"
        );
        ensure!(
            matches!(
                value["mediaType"].as_str(),
                Some(OCI_IMAGE_INDEX_MEDIA_TYPE | MANIFEST_TYPE)
            ),
            "unsupported image metadata media type"
        );
        Ok(Self {
            digest,
            bytes,
            value,
        })
    }
}

impl ImageMetadata {
    pub(super) fn is_index(&self) -> bool {
        self.value["mediaType"] == OCI_IMAGE_INDEX_MEDIA_TYPE
    }

    pub(super) fn entries(&self) -> Result<&[Value]> {
        self.value["manifests"]
            .as_array()
            .map(Vec::as_slice)
            .context("index has no manifests")
    }

    pub(super) fn selected_entry(&self) -> Result<&Value> {
        let selected =
            pick_pichi_entry_from_index(&self.bytes, TARGET_OS, Platform::host_architecture())?;
        self.entries()?
            .iter()
            .find(|entry| entry["digest"] == selected)
            .context("selected index entry is missing")
    }

    pub(super) fn check_descriptor(&self, descriptor: &Value) -> Result<()> {
        ensure!(
            descriptor["digest"].as_str() == Some(self.digest.to_string().as_str()),
            "index entry digest mismatch"
        );
        ensure!(
            descriptor["size"].as_u64() == Some(self.bytes.len() as u64),
            "index entry size mismatch: {}",
            self.digest
        );
        ensure!(
            descriptor["mediaType"] == self.value["mediaType"],
            "index entry media type mismatch: {}",
            self.digest
        );
        Ok(())
    }

    pub(super) fn manifest(&self) -> Result<Manifest> {
        ensure!(
            self.value["artifactType"] == conglobate::artifact::MEDIA_TYPE_PICHI_ARTIFACT_V1,
            "expected {}",
            conglobate::artifact::MEDIA_TYPE_PICHI_ARTIFACT_V1
        );
        Manifest::decode_json(self.bytes.as_slice()).context("invalid image manifest")
    }
}

enum Visit {
    Read(Digest, Option<Value>),
    Finish(ImageMetadata),
}

pub(super) trait ImageStore: BlobStore {
    /// Give a locally produced manifest a publishable platform index. An
    /// existing index retains its exact bytes, including all platform entries.
    async fn local_index(&self, digest: &Digest, platform: Option<Platform>) -> Result<Digest> {
        let metadata = self.metadata(digest).await?;
        if metadata.is_index() {
            return Ok(metadata.digest);
        }
        metadata.manifest()?;
        let index = Index {
            manifests: vec![IndexDescriptor {
                media_type: MANIFEST_TYPE.into(),
                artifact_type: Some(conglobate::artifact::MEDIA_TYPE_PICHI_ARTIFACT_V1.into()),
                digest: metadata.digest.to_string(),
                size: metadata.bytes.len() as u64,
                platform: Some(platform.unwrap_or_else(|| Platform {
                    os: TARGET_OS.into(),
                    architecture: Platform::host_architecture().into(),
                    variant: None,
                })),
                annotations: BTreeMap::new(),
            }],
            ..Index::default()
        };
        let bytes = serde_json::to_vec(&index)?;
        let digest = Digest::Sha256(Sha256::digest(&bytes).into());
        self.put_blob(&digest, &bytes).await?;
        Ok(digest)
    }

    async fn metadata(&self, digest: &Digest) -> Result<ImageMetadata> {
        let mut bytes = Vec::new();
        self.open_blob(digest)
            .await?
            .take(METADATA_CAP + 1)
            .read_to_end(&mut bytes)
            .await?;
        (digest.clone(), bytes).try_into()
    }

    async fn resolve(&self, reference: &Reference, tags: &dyn TagDb) -> Result<Digest> {
        match &reference.kind {
            ReferenceKind::Digest(digest) => Ok(digest.clone()),
            ReferenceKind::Tag(_) => {
                tags.resolve_tag(&reference.to_string())
                    .await?
                    .ok_or_else(|| {
                        anyhow!("ref not in cache: {reference}\n  hint: pichi pull {reference}")
                    })
            }
        }
    }

    async fn cached(&self, reference: &Reference, tags: &dyn TagDb) -> Result<bool> {
        let digest = match &reference.kind {
            ReferenceKind::Digest(digest) => Some(digest.clone()),
            ReferenceKind::Tag(_) => tags.resolve_tag(&reference.to_string()).await?,
        };
        let Some(digest) = digest else {
            return Ok(false);
        };
        match self.manifest(&digest).await {
            Ok(_) => Ok(true),
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    async fn manifest(&self, digest: &Digest) -> Result<(ImageMetadata, Manifest)> {
        let mut metadata = self.metadata(digest).await?;
        if metadata.is_index() {
            let entry = metadata.selected_entry()?;
            let selected = self
                .metadata(
                    &entry["digest"]
                        .as_str()
                        .context("index entry has no digest")?
                        .parse()?,
                )
                .await?;
            selected.check_descriptor(entry)?;
            metadata = selected;
        }
        let manifest = metadata.manifest()?;
        Ok((metadata, manifest))
    }

    /// Visit authenticated metadata in dependency order, once per digest.
    /// Collection allows uncached platform entries; publication requires all.
    async fn walk(
        &self,
        roots: impl IntoIterator<Item = Digest>,
        allow_missing: bool,
        mut visit: impl FnMut(ImageMetadata) -> Result<()>,
    ) -> Result<()> {
        let mut pending: Vec<_> = roots
            .into_iter()
            .map(|digest| Visit::Read(digest, None))
            .collect();
        let mut active = HashSet::new();
        let mut finished = HashSet::new();
        while let Some(next) = pending.pop() {
            match next {
                Visit::Finish(metadata) => {
                    active.remove(&metadata.digest);
                    finished.insert(metadata.digest.clone());
                    visit(metadata)?;
                }
                Visit::Read(digest, descriptor) => {
                    let metadata = match self.metadata(&digest).await {
                        Err(error)
                            if allow_missing
                                && descriptor.is_some()
                                && error.downcast_ref::<io::Error>().is_some_and(|error| {
                                    error.kind() == io::ErrorKind::NotFound
                                }) =>
                        {
                            continue;
                        }
                        result => {
                            result.with_context(|| format!("reading image metadata {digest}"))?
                        }
                    };
                    if let Some(descriptor) = descriptor {
                        metadata.check_descriptor(&descriptor)?;
                    }
                    if finished.contains(&digest) {
                        continue;
                    }
                    ensure!(active.insert(digest), "cyclic image index");
                    let entries = if metadata.is_index() {
                        metadata.entries()?.to_vec()
                    } else {
                        Vec::new()
                    };
                    pending.push(Visit::Finish(metadata));
                    for entry in entries.into_iter().rev() {
                        ensure!(
                            matches!(
                                entry["mediaType"].as_str(),
                                Some(OCI_IMAGE_INDEX_MEDIA_TYPE | MANIFEST_TYPE)
                            ),
                            "unsupported index entry media type"
                        );
                        let digest = entry["digest"]
                            .as_str()
                            .context("index entry has no digest")?
                            .parse()?;
                        ensure!(entry["size"].as_u64().is_some(), "index entry has no size");
                        pending.push(Visit::Read(digest, Some(entry)));
                    }
                }
            }
        }
        Ok(())
    }

    /// The caller holds the cache's index lock through traversal and deletion.
    async fn reachable(&self, roots: impl IntoIterator<Item = Digest>) -> Result<HashSet<Digest>> {
        let mut live = HashSet::new();
        self.walk(roots, true, |metadata| {
            live.insert(metadata.digest.clone());
            if metadata.is_index() {
                for entry in metadata.entries()? {
                    live.insert(
                        entry["digest"]
                            .as_str()
                            .context("index entry has no digest")?
                            .parse()?,
                    );
                }
            } else {
                let manifest: Manifest = serde_json::from_slice(&metadata.bytes)?;
                live.insert(manifest.config.digest.parse()?);
                for layer in &manifest.layers {
                    live.insert(layer.digest_str().parse()?);
                }
            }
            Ok(())
        })
        .await?;
        Ok(live)
    }

    async fn export(&self, root: &Digest) -> Result<Vec<ImageMetadata>> {
        let mut metadata = Vec::new();
        self.walk([root.clone()], false, |image| {
            metadata.push(image);
            Ok(())
        })
        .await
        .context("all referenced platforms must be cached for export")?;
        Ok(metadata)
    }
}

impl<T: BlobStore + ?Sized> ImageStore for T {}
