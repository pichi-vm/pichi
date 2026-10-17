// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `pichi load <dir>` — import an OCI image layout directory (produced by
//! `pichi save`) into the local cache, mirroring `podman load`. Blobs are
//! copied in and each `index.json` entry's tag is registered.
//!
//! To assemble a multi-arch image, `load` each per-arch layout (under distinct
//! local tags), then combine them with `pichi manifest`.

use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::fs;

use pichi_artifact::{Digest, Manifest, Reference};
use pichi_storage::{BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};

use crate::cli::LoadArgs;
use crate::cmd::layer::PreparedLayer;
use crate::cmd::manifest_ext::ScuteRoots;
use crate::config::Config;

const REF_NAME_ANNOTATION: &str = "org.opencontainers.image.ref.name";

pub async fn run(args: LoadArgs, config: &Config) -> Result<()> {
    let dir = &args.input;
    let layout = config.resolve_layout()?;
    let blob_store = FilesystemBlobStore::new(&layout.graphroot);
    let tag_db = FilesystemTagDb::open(&layout.graphroot)
        .with_context(|| format!("opening tag db at {}", layout.graphroot.display()))?;

    // Copy every blob in the layout into the cache (dedup on digest).
    let blobs_dir = dir.join("blobs").join("sha256");
    for entry in
        fs::read_dir(&blobs_dir).with_context(|| format!("read {}", blobs_dir.display()))?
    {
        let entry = entry?;
        let hex = entry.file_name().to_string_lossy().into_owned();
        let digest: Digest = format!("sha256:{hex}")
            .parse()
            .with_context(|| format!("layout blob name is not a sha256 digest: {hex}"))?;
        if !blob_store.blob_exists(&digest).await {
            blob_store
                .put_blob_from_path(&entry.path(), &digest)
                .await
                .with_context(|| format!("import blob {digest}"))?;
        }
    }

    // Register the tag(s) from index.json (or the --tag override).
    let index: Value = serde_json::from_slice(
        &tokio::fs::read(dir.join("index.json"))
            .await
            .with_context(|| format!("read {}", dir.display()))?,
    )
    .context("parse index.json")?;
    let manifests = index
        .get("manifests")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("layout index.json has no `manifests` array"))?;
    if manifests.is_empty() {
        return Err(anyhow!("layout index.json has no manifests"));
    }
    for m in manifests {
        let digest: Digest = m
            .get("digest")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("index entry missing digest"))?
            .parse()?;

        // Materialise the prepared cache form (`.deflated` for `+zstd`,
        // `.verity` for every scute). Verity is never distributed (D-04); it
        // is recomputed from the cow + salt on the consumer side, exactly as
        // `pichi pull` does — otherwise `pichi run` has no hash device.
        let raw = blob_store
            .get_blob(&digest)
            .await
            .with_context(|| format!("read loaded manifest {digest}"))?;
        let manifest = Manifest::from_reader_validated(&raw[..])
            .with_context(|| format!("parse loaded manifest {digest}"))?;
        let mut roots = ScuteRoots::default();
        for layer in &manifest.layers {
            if let Some(root) = PreparedLayer::prepare_cached(&manifest, layer, &blob_store).await?
            {
                roots.add(layer, root)?;
            }
        }
        roots
            .verify(&manifest)
            .with_context(|| format!("verifying carapace root for loaded manifest {digest}"))?;

        let tag = args.tag.clone().or_else(|| {
            m.get("annotations")
                .and_then(|a| a.get(REF_NAME_ANNOTATION))
                .and_then(Value::as_str)
                .map(String::from)
        });
        match tag {
            Some(t) => {
                // Canonicalise the ref the same way `pichi tag` does
                // (D-01 / LOCAL-05), so later `resolve_tag` lookups — e.g.
                // from `pichi manifest create` — find it.
                let key = t
                    .parse::<Reference>()
                    .with_context(|| format!("invalid tag {t}"))?
                    .to_string();
                tag_db.set_tag(&key, &digest).await?;
                println!("loaded {key}");
            }
            None => println!("loaded {digest} (untagged)"),
        }
    }
    Ok(())
}
