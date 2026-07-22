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
use pichi_storage::sidecar::write_sidecar_atomic;
use pichi_storage::{BlobSidecarExt, BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};

use crate::cli::LoadArgs;
use crate::cmd::manifest_ext::ManifestExt;
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
        prepare_sidecars(&blob_store, &manifest).await?;

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

/// Recompute the prepared sidecars for every scute layer of `manifest`:
/// `<blob>.deflated` (decompressed cow, `+zstd` only) and `<blob>.verity`
/// (dm-verity hash tree). Idempotent — a scute whose `.verity` already exists
/// is skipped. PMI/DTB layers carry no verity and are ignored.
async fn prepare_sidecars(blob_store: &FilesystemBlobStore, manifest: &Manifest) -> Result<()> {
    let data_block_size = manifest.data_block_size();
    let hash_block_size = manifest.hash_block_size();
    let scratch = blob_store
        .scratch_dir()
        .await
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
        let v_path = blob_path.verity_path();
        if tokio::fs::try_exists(&v_path).await.unwrap_or(false) {
            continue;
        }

        let raw = blob_store
            .get_blob(&digest)
            .await
            .with_context(|| format!("read scute cow blob {digest}"))?;
        let salt = hex::decode(salt_hex)
            .with_context(|| format!("scute salt is not valid hex: {salt_hex}"))?;

        // The zstd decode + dm-verity hash are CPU-bound — run them off the
        // runtime. Returns the optional `.deflated` bytes (only for `+zstd`;
        // raw scutes ARE the deflated bytes) and the `.verity` blob.
        let d = digest.clone();
        let (deflated, verity_blob) =
            tokio::task::spawn_blocking(move || -> Result<(Option<Vec<u8>>, Vec<u8>)> {
                let cow = if is_zstd {
                    let mut decoder = ruzstd::decoding::StreamingDecoder::new(&raw[..])
                        .map_err(|e| anyhow!("zstd decoder init for {d}: {e}"))?;
                    let mut out = Vec::new();
                    std::io::Read::read_to_end(&mut decoder, &mut out)
                        .with_context(|| format!("zstd decode scute {d}"))?;
                    out
                } else {
                    raw
                };
                let params = VerityParams {
                    data_block_size,
                    hash_block_size,
                    salt,
                    uuid: [0u8; 16], // metadata only; kernel ignores it at activation.
                };
                let output =
                    compute(&cow, &params).with_context(|| format!("dm-verity for scute {d}"))?;
                let deflated = if is_zstd { Some(cow) } else { None };
                Ok((deflated, output.blob))
            })
            .await
            .context("sidecar computation task panicked")??;

        if let Some(deflated) = deflated {
            write_sidecar_atomic(&scratch, &blob_path.deflated_path(), &deflated)
                .await
                .with_context(|| format!("write .deflated sidecar for {digest}"))?;
        }
        write_sidecar_atomic(&scratch, &v_path, &verity_blob)
            .await
            .with_context(|| format!("write .verity sidecar for {digest}"))?;
    }
    Ok(())
}
