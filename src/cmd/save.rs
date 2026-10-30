// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `pichi save <ref> -o <dir>` — export a cached artifact to a portable OCI
//! image layout directory (`oci-layout` + `index.json` + `blobs/sha256/`),
//! mirroring `podman save`. The output is self-contained: manifest, config,
//! and every layer blob. `pichi load` reads it back.
//!
//! Used to move per-architecture builds off their native runners so a final
//! job can `load` them into one cache and assemble a multi-arch manifest — the
//! platform is supplied later via `pichi manifest annotate`, not here (podman's
//! save carries no platform either).

use crate::cmd::cache::ImageStore as _;
use anyhow::{Context, Result};
use serde_json::json;
use std::path::Path;

use pichi_artifact::{Digest, MEDIA_TYPE_PICHI_ARTIFACT_V1, Reference};
use pichi_storage::{FilesystemBlobStore, FilesystemTagDb};

use crate::cli::SaveArgs;
use crate::config::Config;

const OCI_IMAGE_INDEX_MEDIA_TYPE: &str = "application/vnd.oci.image.index.v1+json";
const OCI_LAYOUT_MARKER: &str = r#"{"imageLayoutVersion":"1.0.0"}"#;
const REF_NAME_ANNOTATION: &str = "org.opencontainers.image.ref.name";

pub async fn run(args: SaveArgs, config: &Config) -> Result<()> {
    let reference: Reference = args
        .reference
        .parse()
        .with_context(|| format!("invalid reference: {}", args.reference))?;
    let layout = config.resolve_layout()?;
    let blob_store = FilesystemBlobStore::new(&layout.graphroot);
    let tag_db = FilesystemTagDb::open(&layout.graphroot)
        .with_context(|| format!("opening tag db at {}", layout.graphroot.display()))?;

    let tag = reference.to_string();
    let digest = blob_store.resolve(&reference, &tag_db).await?;
    let images = blob_store.export(&digest).await?;
    let root = images.last().context("image has no metadata")?;

    let out = &args.output;
    let blobs_dir = out.join("blobs").join("sha256");
    tokio::fs::create_dir_all(&blobs_dir)
        .await
        .with_context(|| format!("create {}", blobs_dir.display()))?;
    tokio::fs::write(out.join("oci-layout"), OCI_LAYOUT_MARKER).await?;

    let mut to_copy = std::collections::HashSet::new();
    for image in &images {
        tokio::fs::write(blobs_dir.join(image.digest.hex()), &image.bytes).await?;
        if !image.is_index() {
            let manifest = image.manifest()?;
            if !manifest.config.is_empty() {
                to_copy.insert(manifest.config.digest.parse::<Digest>()?);
            }
            for layer in &manifest.layers {
                to_copy.insert(layer.digest_str().parse::<Digest>()?);
            }
        }
    }
    futures_util::future::try_join_all(
        to_copy
            .iter()
            .map(|digest| copy_blob(&blob_store, digest, &blobs_dir)),
    )
    .await?;

    let index = json!({
        "schemaVersion": 2,
        "mediaType": OCI_IMAGE_INDEX_MEDIA_TYPE,
        "manifests": [{
            "mediaType": root.value["mediaType"],
            "artifactType": MEDIA_TYPE_PICHI_ARTIFACT_V1,
            "digest": digest.to_string(),
            "size": root.bytes.len(),
            "annotations": { REF_NAME_ANNOTATION: tag },
        }],
    });
    tokio::fs::write(
        out.join("index.json"),
        serde_json::to_vec_pretty(&index).context("serialise index.json")?,
    )
    .await?;

    println!("saved {tag} -> {}", out.display());
    Ok(())
}

/// Copy a cache blob into the layout's `blobs/sha256/<hex>` (streamed by the OS,
/// no full read into this process).
async fn copy_blob(
    blob_store: &FilesystemBlobStore,
    digest: &Digest,
    blobs_dir: &Path,
) -> Result<()> {
    let src = blob_store.blob_path(digest);
    let dst = blobs_dir.join(digest.hex());
    tokio::fs::copy(&src, &dst).await.with_context(|| {
        format!(
            "copy blob {digest} ({} -> {})",
            src.display(),
            dst.display()
        )
    })?;
    Ok(())
}
