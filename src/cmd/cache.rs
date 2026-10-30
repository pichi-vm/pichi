// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Authenticated image metadata and the cache objects reachable from it.

use anyhow::{Context, Result, bail, ensure};
use pichi_artifact::{Digest, Manifest};
use pichi_registry::OCI_IMAGE_INDEX_MEDIA_TYPE;
use pichi_storage::BlobStore;
use sha2::{Digest as _, Sha256};
use std::{collections::HashSet, io};
use tokio::io::AsyncReadExt as _;

pub(super) const METADATA_CAP: u64 = 1024 * 1024;
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

pub(super) trait ImageStore: BlobStore {
    /// Capture and authenticate small metadata; layer payloads are not read here.
    async fn metadata(&self, digest: &Digest) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.open_blob(digest)
            .await?
            .take(METADATA_CAP + 1)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(
            bytes.len() as u64 <= METADATA_CAP,
            "image metadata exceeds size limit"
        );
        let actual = Digest::Sha256(Sha256::digest(&bytes).into());
        ensure!(
            &actual == digest,
            "cached metadata digest mismatch: {digest}"
        );
        Ok(bytes)
    }

    /// The caller holds the cache's index lock through traversal and deletion.
    /// Missing index entries may belong to platforms that were never pulled.
    /// Corrupt metadata aborts collection before any reachable object is deleted.
    async fn reachable(&self, roots: impl IntoIterator<Item = Digest>) -> Result<HashSet<Digest>> {
        let mut pending: Vec<_> = roots.into_iter().map(|digest| (digest, None)).collect();
        let mut visited = HashSet::new();
        let mut live = HashSet::new();
        while let Some((digest, descriptor)) = pending.pop() {
            live.insert(digest.clone());
            let bytes = match self.metadata(&digest).await {
                Err(error)
                    if descriptor.is_some()
                        && error
                            .downcast_ref::<io::Error>()
                            .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
                {
                    continue;
                }
                result => {
                    result.with_context(|| format!("reading live image metadata {digest}"))?
                }
            };
            let value: serde_json::Value = serde_json::from_slice(&bytes)?;
            ensure!(
                value["schemaVersion"] == 2,
                "unsupported image metadata version"
            );
            let media_type = value["mediaType"]
                .as_str()
                .context("image metadata has no media type")?;
            if let Some((size, expected_type)) = descriptor {
                ensure!(
                    bytes.len() as u64 == size,
                    "index entry size mismatch: {digest}"
                );
                ensure!(
                    media_type == expected_type,
                    "index entry media type mismatch: {digest}"
                );
            }
            if !visited.insert(digest) {
                continue;
            }
            match media_type {
                OCI_IMAGE_INDEX_MEDIA_TYPE => {
                    for entry in value["manifests"]
                        .as_array()
                        .context("index has no manifests")?
                    {
                        let digest = entry["digest"]
                            .as_str()
                            .context("index entry has no digest")?
                            .parse()?;
                        let size = entry["size"].as_u64().context("index entry has no size")?;
                        let media_type = entry["mediaType"]
                            .as_str()
                            .context("index entry has no media type")?;
                        ensure!(
                            matches!(media_type, OCI_IMAGE_INDEX_MEDIA_TYPE | MANIFEST_TYPE),
                            "unsupported index entry media type: {media_type}"
                        );
                        pending.push((digest, Some((size, media_type.to_owned()))));
                    }
                }
                MANIFEST_TYPE => {
                    let manifest: Manifest = serde_json::from_slice(&bytes)?;
                    live.insert(manifest.config.digest.parse()?);
                    for layer in &manifest.layers {
                        live.insert(layer.digest_str().parse()?);
                    }
                }
                _ => bail!("unsupported image metadata media type: {media_type}"),
            }
        }
        Ok(live)
    }
}

impl<T: BlobStore + ?Sized> ImageStore for T {}
