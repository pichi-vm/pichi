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

use anyhow::{Context, Result, anyhow};
use serde_json::json;
use std::fs;
use std::path::Path;

use pichi_artifact::{Digest, MEDIA_TYPE_PICHI_ARTIFACT_V1, Manifest, Reference};
use pichi_storage::{BlobStore, CacheLayout, FilesystemBlobStore, FilesystemTagDb, TagDb};

use crate::cli::SaveArgs;
use crate::config::Config;

const OCI_IMAGE_MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_IMAGE_INDEX_MEDIA_TYPE: &str = "application/vnd.oci.image.index.v1+json";
const OCI_LAYOUT_MARKER: &str = r#"{"imageLayoutVersion":"1.0.0"}"#;
const REF_NAME_ANNOTATION: &str = "org.opencontainers.image.ref.name";

pub fn run(args: SaveArgs, config: &Config) -> Result<()> {
    let reference: Reference = args
        .reference
        .parse()
        .with_context(|| format!("invalid reference: {}", args.reference))?;
    let layout = resolve_layout(config)?;
    let blob_store = FilesystemBlobStore::new(&layout.graphroot);
    let tag_db = FilesystemTagDb::open(&layout.graphroot)
        .with_context(|| format!("opening tag db at {}", layout.graphroot.display()))?;

    let tag = reference.to_string();
    let manifest_digest = tag_db
        .resolve_tag(&tag)?
        .ok_or_else(|| anyhow!("ref not in cache: {tag}"))?;
    let raw_manifest = blob_store
        .get_blob(&manifest_digest)
        .with_context(|| format!("read manifest {manifest_digest}"))?;
    let manifest = Manifest::from_reader_validated(raw_manifest.as_slice())
        .with_context(|| format!("parse cached manifest {manifest_digest}"))?;

    let out = &args.output;
    let blobs_dir = out.join("blobs").join("sha256");
    fs::create_dir_all(&blobs_dir).with_context(|| format!("create {}", blobs_dir.display()))?;
    fs::write(out.join("oci-layout"), OCI_LAYOUT_MARKER)?;

    // Manifest, config (unless it is the inline empty config), and every layer.
    copy_blob(&blob_store, &manifest_digest, &blobs_dir)?;
    if manifest.config.data.is_none() {
        let cfg: Digest = manifest
            .config
            .digest
            .parse()
            .with_context(|| format!("invalid config digest: {}", manifest.config.digest))?;
        copy_blob(&blob_store, &cfg, &blobs_dir)?;
    }
    for layer in &manifest.layers {
        let d: Digest = layer
            .digest_str()
            .parse()
            .with_context(|| format!("invalid layer digest: {}", layer.digest_str()))?;
        copy_blob(&blob_store, &d, &blobs_dir)?;
    }

    let index = json!({
        "schemaVersion": 2,
        "mediaType": OCI_IMAGE_INDEX_MEDIA_TYPE,
        "manifests": [{
            "mediaType": OCI_IMAGE_MANIFEST_MEDIA_TYPE,
            "artifactType": MEDIA_TYPE_PICHI_ARTIFACT_V1,
            "digest": manifest_digest.to_string(),
            "size": raw_manifest.len(),
            "annotations": { REF_NAME_ANNOTATION: tag },
        }],
    });
    fs::write(
        out.join("index.json"),
        serde_json::to_vec_pretty(&index).context("serialise index.json")?,
    )?;

    println!("saved {tag} -> {}", out.display());
    Ok(())
}

/// Copy a cache blob into the layout's `blobs/sha256/<hex>` (streamed by the OS,
/// no full read into this process).
fn copy_blob(blob_store: &FilesystemBlobStore, digest: &Digest, blobs_dir: &Path) -> Result<()> {
    let src = blob_store.blob_path(digest);
    let dst = blobs_dir.join(digest.hex());
    fs::copy(&src, &dst).with_context(|| {
        format!(
            "copy blob {digest} ({} -> {})",
            src.display(),
            dst.display()
        )
    })?;
    Ok(())
}

fn resolve_layout(config: &Config) -> Result<CacheLayout> {
    let mut layout = CacheLayout::resolve()?;
    if let Some(p) = &config.storage.graphroot {
        layout.graphroot.clone_from(p);
    }
    if let Some(p) = &config.storage.runroot {
        layout.runroot.clone_from(p);
    }
    Ok(layout)
}
