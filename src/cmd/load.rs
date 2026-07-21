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

use pichi_artifact::{Digest, Layer, Manifest, Reference};
use pichi_import::verity::{VerityParams, compute};
use pichi_storage::sidecar::{deflated_path, verity_path, write_sidecar_atomic};
use pichi_storage::{BlobStore, CacheLayout, FilesystemBlobStore, FilesystemTagDb, TagDb};

use crate::cli::LoadArgs;
use crate::config::Config;

const REF_NAME_ANNOTATION: &str = "org.opencontainers.image.ref.name";
const ANN_DATA_BLOCK_SIZE: &str = "dev.pichi.carapace.verity.data-block-size";
const ANN_HASH_BLOCK_SIZE: &str = "dev.pichi.carapace.verity.hash-block-size";
const DEFAULT_BLOCK_SIZE: u32 = 4096;

pub fn run(args: LoadArgs, config: &Config) -> Result<()> {
    let dir = &args.input;
    let layout = resolve_layout(config)?;
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
        if !blob_store.blob_exists(&digest) {
            blob_store
                .put_blob_from_path(&entry.path(), &digest)
                .with_context(|| format!("import blob {digest}"))?;
        }
    }

    // Register the tag(s) from index.json (or the --tag override).
    let index: Value = serde_json::from_slice(
        &fs::read(dir.join("index.json")).with_context(|| format!("read {}", dir.display()))?,
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
            .with_context(|| format!("read loaded manifest {digest}"))?;
        let manifest = Manifest::from_reader_validated(&raw[..])
            .with_context(|| format!("parse loaded manifest {digest}"))?;
        prepare_sidecars(&blob_store, &manifest)?;

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
                tag_db.set_tag(&key, &digest)?;
                println!("loaded {key}");
            }
            None => println!("loaded {digest} (untagged)"),
        }
    }
    Ok(())
}

/// Recompute the prepared sidecars for every scute layer of `manifest`:
/// `<blob>.deflated` (decompressed cow, `+zstd` only) and `<blob>.verity`
/// (dm-verity hash tree). Idempotent — a scute whose `.verity` already exists
/// is skipped. PMI/DTB layers carry no verity and are ignored.
fn prepare_sidecars(blob_store: &FilesystemBlobStore, manifest: &Manifest) -> Result<()> {
    let data_block_size = block_size(manifest, ANN_DATA_BLOCK_SIZE);
    let hash_block_size = block_size(manifest, ANN_HASH_BLOCK_SIZE);
    let scratch = blob_store
        .scratch_dir()
        .context("preparing scratch dir for sidecars")?;

    for layer in &manifest.layers {
        let (salt_hex, is_zstd) = match layer {
            Layer::Scute(d) => (&d.annotations.salt, false),
            Layer::ScuteZstd(d) => (&d.annotations.salt, true),
            _ => continue,
        };
        let digest: Digest = layer
            .digest_str()
            .parse()
            .with_context(|| format!("parse scute digest {}", layer.digest_str()))?;
        let blob_path = blob_store.blob_path(&digest);
        let v_path = verity_path(&blob_path);
        if v_path.exists() {
            continue;
        }

        let raw = blob_store
            .get_blob(&digest)
            .with_context(|| format!("read scute cow blob {digest}"))?;
        // For `+zstd` the cow must be decompressed before hashing; the
        // decompressed bytes are also persisted as the `.deflated` sidecar
        // (raw scutes ARE the deflated bytes, so no sidecar is written).
        let cow = if is_zstd {
            let mut decoder = ruzstd::decoding::StreamingDecoder::new(&raw[..])
                .map_err(|e| anyhow!("zstd decoder init for {digest}: {e}"))?;
            let mut out = Vec::new();
            std::io::Read::read_to_end(&mut decoder, &mut out)
                .with_context(|| format!("zstd decode scute {digest}"))?;
            write_sidecar_atomic(&scratch, &deflated_path(&blob_path), &out)
                .with_context(|| format!("write .deflated sidecar for {digest}"))?;
            out
        } else {
            raw
        };

        let salt = hex::decode(salt_hex)
            .with_context(|| format!("scute salt is not valid hex: {salt_hex}"))?;
        let params = VerityParams {
            data_block_size,
            hash_block_size,
            salt,
            uuid: [0u8; 16], // metadata only; the kernel ignores it at activation.
        };
        let output = compute(&cow, &params)
            .with_context(|| format!("dm-verity computation for scute {digest}"))?;
        write_sidecar_atomic(&scratch, &v_path, &output.blob)
            .with_context(|| format!("write .verity sidecar for {digest}"))?;
    }
    Ok(())
}

/// Read a u32 chain annotation, falling back to the carapace-locked default.
fn block_size(manifest: &Manifest, key: &str) -> u32 {
    manifest
        .annotations
        .get(key)
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(DEFAULT_BLOCK_SIZE)
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
