// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `pichi pull <ref>` — REGISTRY-01..07.
//!
//! Throwaway tokio runtime per call (Pattern 1; Plan 01 SPIKE A3 confirmed
//! `current_thread + enable_all` works as a one-shot driver). Pulls the
//! manifest (raw — REGISTRY-05/Pitfall 1), walks OCI Image Indices per
//! D-02, validates host-side per Phase 42 D-11, streams each layer through
//! the D-10 pipeline (compressed-side digest + optional zstd decode +
//! decompressed-side digest + verity feed + LimitWriter + BlobStore
//! tempfile), commits atomically per D-03 refined per Pitfall 11 (live-walk
//! refcounts; no sidecar; `set_tag` is the single commit point and takes
//! its own flock — DO NOT wrap in the cache-wide advisory-lock helper per
//! Pitfall 2 (the helper would self-deadlock against set_tag's internal flock).

#![cfg_attr(test, allow(clippy::unwrap_used))]

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use sha2::{Digest as _, Sha256};

use pichi_artifact::{Digest, Layer, MEDIA_TYPE_PICHI_ARTIFACT_V1, Manifest, Reference};
use pichi_import::verity::{VerityBuilder, VerityOutput, VerityParams};
use pichi_registry::{OCI_IMAGE_INDEX_MEDIA_TYPE, Registry, pick_pichi_entry_from_index};
use pichi_storage::{
    BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb,
    sidecar::{deflated_path, verity_path, write_sidecar_atomic},
};

use crate::cli::{PullArgs, PullPolicy};
use crate::config::Config;
use futures_util::stream::{self, StreamExt};

/// Target platform `os` for D-02 index walks. A carapace is NOT "used on" an
/// OS — it *is* a pichi VM guest's root filesystem — so the OCI `platform.os`
/// field (whose meaning is "the OS this artifact runs on") doesn't apply. We
/// repurpose it as a pichi-owned selector token: producer (`carapace import` /
/// the multi-arch index) and consumer (here) agree on `"pichi"`. This is also
/// honest signalling to container runtimes, which then correctly refuse the
/// carapace as a runnable image (no host platform matches `pichi/*`).
const TARGET_OS: &str = "pichi";

/// Target platform architecture for D-02 index walks: the host CPU (the guest
/// runs the host's architecture — dillo virtualizes the native CPU, no
/// cross-arch emulation), normalized to OCI/GOARCH names. A multi-arch base
/// (e.g. ghcr.io/pichi-vm/fedora) is selected per the running host.
fn target_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}
/// Default per-layer decompressed-bytes cap (compressed-bomb defence;
/// RESEARCH §"Known Threat Patterns" line 1620). Default 16 GiB; future
/// enhancement: per-layer override = `descriptor.size + slack`.
const DEFAULT_DECOMPRESSED_CAP: u64 = 16 * 1024 * 1024 * 1024;
/// Phase 42 D-06 locked verity defaults — used by the pull-side verity feed.
const VERITY_DBS: u32 = 4096;
const VERITY_HBS: u32 = 4096;
/// Max layers downloaded concurrently, matching podman's default of 3.
const PULL_CONCURRENCY: usize = 3;

/// Entry point for `pichi pull`.
pub async fn run(args: PullArgs, config: &Config) -> Result<()> {
    // Defense-in-depth: parse + canonicalise BEFORE any I/O (BL-02 / T-43-02).
    let target_ref: Reference = args
        .reference
        .parse()
        .with_context(|| format!("invalid reference: {}", args.reference))?;
    let policy = args.pull.unwrap_or(PullPolicy::Always); // D-05 default.
    let layout = config.resolve_layout()?;
    let blob_store = FilesystemBlobStore::new(&layout.graphroot);
    let tag_db = FilesystemTagDb::open(&layout.graphroot)
        .with_context(|| format!("opening tag db at {}", layout.graphroot.display()))?;

    // --pull=never short-circuits before any network I/O.
    if policy == PullPolicy::Never {
        return tag_db
            .resolve_tag(&target_ref.to_string())
            .await?
            .map(|_| ())
            .ok_or_else(|| anyhow!("pull policy `never`: ref not in cache: {target_ref}"));
    }

    pull_inner(target_ref, policy, args.quiet, config, &blob_store, &tag_db).await
}

/// Internal driver — `pub(crate)` so `tests` modules in this file can drive
/// it directly with a [`pichi_registry::MockRegistry`].
pub(crate) async fn pull_inner_with_registry<R: Registry>(
    target: Reference,
    policy: PullPolicy,
    _quiet: bool,
    registry: &R,
    blob_store: &dyn BlobStore,
    tag_db: &FilesystemTagDb,
) -> Result<()> {
    let target_str = target.to_string();

    // --pull=missing: skip network if cached.
    if policy == PullPolicy::Missing && tag_db.resolve_tag(&target_str).await?.is_some() {
        return Ok(());
    }
    // --pull=newer: GET upstream manifest digest, compare to cached
    // (W6 revision: GET-then-compare, HEAD optimisation deferred).
    if policy == PullPolicy::Newer {
        if let Some(cached) = tag_db.resolve_tag(&target_str).await? {
            let (_raw, upstream) = registry
                .pull_manifest_by_tag(&target)
                .await
                .map_err(|e| anyhow!("--pull=newer fetch for {target}: {e}"))?;
            if upstream == cached {
                return Ok(());
            }
        }
    }

    // 1. Fetch the manifest (REGISTRY-05 raw API; Pitfall 1 — never the
    //    auto-resolving variant).
    let (raw, digest_str) = registry
        .pull_manifest_by_tag(&target)
        .await
        .map_err(|e| anyhow!("pull_manifest_by_tag {target}: {e}"))?;

    // 2. Branch on mediaType (D-02 / D-20 index walk OR bare manifest).
    let v: serde_json::Value = serde_json::from_slice(&raw).context("parse manifest mediaType")?;
    let media_type = v.get("mediaType").and_then(|x| x.as_str()).unwrap_or("");
    let (final_bytes, final_digest): (Bytes, Digest) = if media_type == OCI_IMAGE_INDEX_MEDIA_TYPE {
        // D-02 index walk: pick the pichi entry, fetch by digest, drop
        // the index (do NOT persist it).
        let picked_str = pick_pichi_entry_from_index(&raw, TARGET_OS, target_arch())?;
        let picked: Digest = picked_str
            .parse()
            .with_context(|| format!("parse picked digest {picked_str}"))?;
        let bytes = registry
            .pull_manifest_by_digest(&target.registry, &target.repo, &picked)
            .await
            .map_err(|e| anyhow!("pull_manifest_by_digest {picked}: {e}"))?;
        (bytes, picked)
    } else {
        // Bare manifest: reject if non-pichi artifactType (REGISTRY-07).
        let at = v.get("artifactType").and_then(|x| x.as_str());
        if at != Some(MEDIA_TYPE_PICHI_ARTIFACT_V1) {
            anyhow::bail!(
                "manifest at {target} has artifactType={at:?}, expected {MEDIA_TYPE_PICHI_ARTIFACT_V1}"
            );
        }
        (raw, digest_str)
    };

    // 3. Validate the pichi manifest BEFORE fetching any blob (Phase 42 D-11
    //    host-side validation).
    let manifest = Manifest::from_reader_validated(final_bytes.as_ref())
        .with_context(|| format!("validate pichi manifest {final_digest}"))?;

    // 4a. Pull the config blob when it is a real `vnd.pichi.config.v1+json`
    //     (the OCI empty config rides inline in the manifest — nothing to
    //     fetch). It is small; pull it into memory and commit.
    if !manifest.config.is_empty() {
        let config_digest: Digest = manifest
            .config
            .digest
            .parse()
            .with_context(|| format!("parse config digest {}", manifest.config.digest))?;
        if !blob_store.blob_exists(&config_digest).await {
            let mut buf: Vec<u8> = Vec::new();
            registry
                .pull_blob(
                    &target.registry,
                    &target.repo,
                    &config_digest,
                    manifest.config.size,
                    &mut buf,
                )
                .await
                .map_err(|e| anyhow!("pull config blob {config_digest}: {e}"))?;
            blob_store
                .put_blob(&config_digest, &buf)
                .await
                .with_context(|| format!("put config blob {config_digest}"))?;
        }
    }

    // 4. Pull each layer via the streaming pipeline (REGISTRY-01 dedup via
    //    blob_exists). Sidecars (`.deflated` for `+zstd` only, `.verity`
    //    for every scute) are written by `fetch_one_layer` after the
    //    source blob commits. pichi never reads or validates the PMI
    //    cmdline's `roothash=`; consistency between cmdline and verity
    //    tree is the publisher's responsibility, validated at boot time
    //    inside the guest's dm-verity activation.
    // REGISTRY-01: dedup by descriptor digest before any network I/O. D-01
    // implicit refcount: source present ⇒ sidecars present (no partial-cache
    // regeneration in v0.9; Phase 47 prune cleans up partial states). A
    // malformed digest fails fast here, before a single byte is fetched.
    let mut pending: Vec<&Layer> = Vec::new();
    for layer in &manifest.layers {
        let layer_digest: Digest = layer
            .digest_str()
            .parse()
            .with_context(|| format!("parse layer digest {}", layer.digest_str()))?;
        if !blob_store.blob_exists(&layer_digest).await {
            pending.push(layer);
        }
    }

    // Download the missing layers concurrently, bounded to PULL_CONCURRENCY —
    // podman's default parallel-layer behaviour. `buffer_unordered` polls on
    // this task, so the `&registry`/`&blob_store` borrows need no `'static`;
    // each layer's blocking verity pipeline spreads across the runtime pool.
    let target_ref = &target;
    let mut downloads = stream::iter(pending)
        .map(|layer| {
            let layer_digest = layer.digest_str().to_string();
            async move {
                fetch_one_layer(registry, target_ref, layer, blob_store)
                    .await
                    .with_context(|| format!("fetch layer {layer_digest}"))
            }
        })
        .buffer_unordered(PULL_CONCURRENCY);
    while let Some(result) = downloads.next().await {
        result?;
    }

    // 5. Atomic commit per D-03 refined per Pitfall 11 / Phase 42 Plan 05
    //    SUMMARY: live-walk refcounts; no sidecar; set_tag is the single
    //    commit point and takes its own flock (DO NOT wrap in
    //    the cache-wide advisory-lock helper per Pitfall 2).
    blob_store
        .put_blob(&final_digest, &final_bytes)
        .await
        .with_context(|| format!("put_blob manifest {final_digest}"))?;
    tag_db
        .set_tag(&target_str, &final_digest)
        .await
        .with_context(|| format!("set_tag {target_str}"))?;

    Ok(())
}

/// Internal driver that constructs a production [`pichi_registry::HttpRegistry`]
/// from `config` and dispatches to [`pull_inner_with_registry`]. Kept separate
/// so the test entry point (`pull_inner_with_registry`) can drive a
/// [`pichi_registry::MockRegistry`] without needing an HTTP backend.
pub(crate) async fn pull_inner(
    target: Reference,
    policy: PullPolicy,
    quiet: bool,
    config: &Config,
    blob_store: &dyn BlobStore,
    tag_db: &FilesystemTagDb,
) -> Result<()> {
    let registry = config.http_registry();
    pull_inner_with_registry(target, policy, quiet, &registry, blob_store, tag_db).await
}

/// One-layer fetch with a zero-copy, multi-consumer fan-out.
///
/// The transport yields owned `Bytes` chunks (`pull_blob_stream` — no copy).
/// Each chunk is fanned out to parallel consumers by `Bytes::clone` (an `Arc`
/// refcount bump, NOT a byte copy), so they all read the same buffer:
///
/// - **disk writer** (async I/O) — streams the wire bytes into a temp blob.
/// - **outer SHA-256** (CPU) — verifies the descriptor digest.
/// - for a raw scute: **verity builder** (CPU) consumes the same wire bytes.
/// - for a `+zstd` scute: a **decoder** (CPU) consumes the wire bytes and
///   re-fans-out the decompressed bytes to the verity builder and a
///   **`.deflated` writer** (async I/O).
///
/// All consumers run concurrently (I/O on the runtime, CPU on the blocking
/// pool) and are joined on completion; only then is the digest verified and
/// the blob + sidecars atomically committed.
async fn fetch_one_layer<R: Registry>(
    registry: &R,
    target: &Reference,
    layer: &Layer,
    blob_store: &dyn BlobStore,
) -> Result<()> {
    use tokio::io::AsyncWriteExt as _;
    use tokio::sync::mpsc;

    let descriptor_digest: Digest = layer.digest_str().parse()?;
    let scratch = blob_store
        .scratch_dir()
        .await
        .context("preparing scratch dir for streaming layer")?;
    let is_zstd = layer.is_zstd_variant();
    // Scute layers carry a per-scute salt and get a verity tree; PMI/DTB don't.
    // The salt is used verbatim (it already is the full salt — matches carapace
    // and the importer; do NOT prepend a zero prefix).
    let scute_salt: Option<Vec<u8>> = match layer {
        Layer::Scute(d) | Layer::ScuteZstd(d) => Some(
            hex::decode(&d.annotations.salt)
                .with_context(|| format!("decode scute salt for layer {}", d.digest))?,
        ),
        Layer::Pmi(_) | Layer::Dtb(_) => None,
    };

    let mut source = registry
        .pull_blob_stream(
            &target.registry,
            &target.repo,
            &descriptor_digest,
            layer.size(),
        )
        .await
        .map_err(|e| anyhow!("pull_blob_stream {descriptor_digest}: {e}"))?;

    const CHAN_DEPTH: usize = 8;
    let verity_params = scute_salt.as_ref().map(|salt| VerityParams {
        data_block_size: VERITY_DBS,
        hash_block_size: VERITY_HBS,
        salt: salt.clone(),
        uuid: [0u8; 16],
    });

    // Consumer: disk writer (async) → wire bytes into a same-fs temp blob.
    let src_temp = scratch.join(unique_temp_name("src"));
    let (disk_tx, mut disk_rx) = mpsc::channel::<Bytes>(CHAN_DEPTH);
    let disk_path = src_temp.clone();
    let disk_task = tokio::spawn(async move {
        let mut f = tokio::fs::File::create(&disk_path)
            .await
            .with_context(|| format!("create layer temp {}", disk_path.display()))?;
        while let Some(chunk) = disk_rx.recv().await {
            f.write_all(&chunk).await.context("write layer temp")?;
        }
        f.sync_all().await.context("fsync layer temp")?;
        Ok::<(), anyhow::Error>(())
    });

    // Consumer: outer SHA-256 (CPU) over the wire bytes.
    let (ohash_tx, ohash_rx) = mpsc::channel::<Bytes>(CHAN_DEPTH);
    let ohash_task = tokio::task::spawn_blocking(move || outer_hash(ohash_rx));

    // Consumer: verity builder (CPU), fed either the wire bytes (raw) or the
    // decompressed bytes (zstd, via the decoder below).
    let (verity_task, verity_content_tx) = match &verity_params {
        Some(params) => {
            let params = params.clone();
            let (vtx, vrx) = mpsc::channel::<Bytes>(CHAN_DEPTH);
            let d = descriptor_digest.clone();
            let handle = tokio::task::spawn_blocking(move || verity_consumer(vrx, params, d));
            (Some(handle), Some(vtx))
        }
        None => (None, None),
    };

    // For `+zstd` scutes: a `.deflated` writer (async) + a decoder (CPU) that
    // re-fans-out the decompressed bytes to verity + the `.deflated` writer.
    let want_deflated = is_zstd && verity_params.is_some();
    let deflated_temp = want_deflated.then(|| scratch.join(unique_temp_name("deflated")));
    let (deflated_task, deflated_tx) = match &deflated_temp {
        Some(path) => {
            let (dtx, mut drx) = mpsc::channel::<Bytes>(CHAN_DEPTH);
            let dpath = path.clone();
            let handle = tokio::spawn(async move {
                let mut f = tokio::fs::File::create(&dpath)
                    .await
                    .with_context(|| format!("create deflated temp {}", dpath.display()))?;
                while let Some(chunk) = drx.recv().await {
                    f.write_all(&chunk).await.context("write deflated temp")?;
                }
                f.sync_all().await.context("fsync deflated temp")?;
                Ok::<(), anyhow::Error>(())
            });
            (Some(handle), Some(dtx))
        }
        None => (None, None),
    };

    // Decoder (zstd only): owns the verity + deflated senders so they close
    // when decoding finishes.
    let (decode_task, decode_tx) = if is_zstd && verity_params.is_some() {
        let (dctx, dcrx) = mpsc::channel::<Bytes>(CHAN_DEPTH);
        let vtx = verity_content_tx.clone();
        let dtx = deflated_tx.clone();
        let d = descriptor_digest.clone();
        let handle = tokio::task::spawn_blocking(move || decode_consumer(dcrx, vtx, dtx, d));
        (Some(handle), Some(dctx))
    } else {
        (None, None)
    };
    // The raw path feeds verity directly; the zstd path feeds the decoder.
    // Drop the distributor's copies of the inner senders so only the decoder
    // (zstd) keeps them alive.
    let raw_verity_tx = if is_zstd {
        None
    } else {
        verity_content_tx.clone()
    };
    drop(verity_content_tx);
    drop(deflated_tx);

    // Distributor: fan each wire chunk out to the compressed-side consumers.
    let drive_result = async {
        while let Some(item) = source.next().await {
            let chunk =
                item.map_err(|e| anyhow!("pull_blob_stream chunk {descriptor_digest}: {e}"))?;
            if disk_tx.send(chunk.clone()).await.is_err() {
                break;
            }
            if ohash_tx.send(chunk.clone()).await.is_err() {
                break;
            }
            if let Some(tx) = &decode_tx {
                if tx.send(chunk).await.is_err() {
                    break;
                }
            } else if let Some(tx) = &raw_verity_tx {
                if tx.send(chunk).await.is_err() {
                    break;
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
    // Close the compressed-side channels so consumers observe EOF.
    drop(disk_tx);
    drop(ohash_tx);
    drop(decode_tx);
    drop(raw_verity_tx);
    drive_result?;

    // Join all consumers (surfaces the first real error).
    disk_task.await.context("disk writer join")??;
    let outer: [u8; 32] = ohash_task.await.context("outer hash join")?;
    if let Some(t) = decode_task {
        t.await.context("decoder join")??;
    }
    let verity_out = match verity_task {
        Some(t) => Some(t.await.context("verity join")??),
        None => None,
    };
    if let Some(t) = deflated_task {
        t.await.context("deflated writer join")??;
    }

    // Verify the descriptor digest before committing anything.
    let outer_digest = Digest::Sha256(outer);
    if outer_digest != descriptor_digest {
        let _ = tokio::fs::remove_file(&src_temp).await;
        if let Some(d) = &deflated_temp {
            let _ = tokio::fs::remove_file(d).await;
        }
        bail!("layer digest mismatch: expected {descriptor_digest}, got {outer_digest}");
    }

    // Commit: rename the wire blob into place, then its sidecars.
    blob_store
        .put_blob_from_path(&src_temp, &descriptor_digest)
        .await
        .with_context(|| format!("put_blob_from_path layer {descriptor_digest}"))?;
    let blob_path = blob_store.blob_path(&descriptor_digest);
    if let Some(dtemp) = &deflated_temp {
        tokio::fs::rename(dtemp, deflated_path(&blob_path))
            .await
            .with_context(|| format!("commit deflated sidecar for {descriptor_digest}"))?;
    }
    if let Some(vout) = verity_out {
        write_sidecar_atomic(&scratch, &verity_path(&blob_path), &vout.blob)
            .await
            .with_context(|| format!("write verity sidecar for {descriptor_digest}"))?;
    }
    Ok(())
}

/// Unique temp filename within the scratch dir (pid + process-local counter),
/// avoiding a random-number dependency.
fn unique_temp_name(kind: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        ".pichi-{kind}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

/// Outer SHA-256 consumer (CPU): hashes every wire chunk. Runs on the blocking
/// pool, reading via `blocking_recv`.
fn outer_hash(mut rx: tokio::sync::mpsc::Receiver<Bytes>) -> [u8; 32] {
    let mut h = Sha256::new();
    while let Some(chunk) = rx.blocking_recv() {
        h.update(&chunk);
    }
    h.finalize().into()
}

/// Verity consumer (CPU): feeds the dm-verity builder in `data_block_size`
/// blocks, buffering across chunk boundaries. Runs on the blocking pool.
fn verity_consumer(
    mut rx: tokio::sync::mpsc::Receiver<Bytes>,
    params: VerityParams,
    digest: Digest,
) -> Result<VerityOutput> {
    let dbs = params.data_block_size as usize;
    let mut builder =
        VerityBuilder::new(&params).with_context(|| format!("VerityBuilder::new for {digest}"))?;
    let mut buf: Vec<u8> = Vec::with_capacity(dbs * 2);
    while let Some(chunk) = rx.blocking_recv() {
        buf.extend_from_slice(&chunk);
        let mut off = 0;
        while buf.len() - off >= dbs {
            builder
                .add_data_block(&buf[off..off + dbs])
                .with_context(|| format!("verity block for {digest}"))?;
            off += dbs;
        }
        if off > 0 {
            buf.drain(..off);
        }
    }
    if !buf.is_empty() {
        builder
            .add_data_block(&buf)
            .with_context(|| format!("final verity block for {digest}"))?;
    }
    Ok(builder.finalize())
}

/// Decoder consumer (CPU, `+zstd` only): decompresses the wire stream and
/// re-fans-out the decompressed bytes to the verity + `.deflated` consumers.
/// Enforces the decompressed-size cap (compression-bomb defence).
fn decode_consumer(
    rx: tokio::sync::mpsc::Receiver<Bytes>,
    verity_tx: Option<tokio::sync::mpsc::Sender<Bytes>>,
    deflated_tx: Option<tokio::sync::mpsc::Sender<Bytes>>,
    digest: Digest,
) -> Result<()> {
    use std::io::Read as _;
    let mut dec = ruzstd::decoding::StreamingDecoder::new(ChannelReader::new(rx))
        .map_err(|e| anyhow!("zstd decoder init for {digest}: {e}"))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = dec
            .read(&mut buf)
            .with_context(|| format!("zstd decode {digest}"))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > DEFAULT_DECOMPRESSED_CAP {
            bail!("decompressed size exceeds cap ({DEFAULT_DECOMPRESSED_CAP} bytes) for {digest}");
        }
        // One copy out of the decoder's buffer (inherent); the fan-out clones
        // are refcount-only.
        let chunk = Bytes::copy_from_slice(&buf[..n]);
        if let Some(tx) = &verity_tx {
            if tx.blocking_send(chunk.clone()).is_err() {
                break;
            }
        }
        if let Some(tx) = &deflated_tx {
            if tx.blocking_send(chunk).is_err() {
                break;
            }
        }
    }
    Ok(())
}

/// `std::io::Read` over an mpsc channel of `Bytes`, for driving the sync zstd
/// decoder from the async fan-out. `blocking_recv` blocks the decoder's own
/// blocking-pool thread only; `Bytes::split_to` advances without copying.
struct ChannelReader {
    rx: tokio::sync::mpsc::Receiver<Bytes>,
    cur: Bytes,
}

impl ChannelReader {
    fn new(rx: tokio::sync::mpsc::Receiver<Bytes>) -> Self {
        Self {
            rx,
            cur: Bytes::new(),
        }
    }
}

impl std::io::Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.cur.is_empty() {
            match self.rx.blocking_recv() {
                Some(b) => self.cur = b,
                None => return Ok(0),
            }
        }
        let n = out.len().min(self.cur.len());
        out[..n].copy_from_slice(&self.cur[..n]);
        let _ = self.cur.split_to(n);
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use pichi_artifact::{
        ConfigDescriptor, MEDIA_TYPE_PICHI_ARTIFACT_V1, ScuteAnnotations, ScuteDescriptor,
    };
    use pichi_registry::MockRegistry;
    use std::collections::BTreeMap;
    use std::path::Path;
    use tempfile::TempDir;

    /// Build a graphroot + tag db for unit tests (mirrors tests/cmd_import.rs
    /// pattern). Returns `(TempDir, graphroot)`.
    fn graphroot() -> (TempDir, std::path::PathBuf) {
        let tmp = TempDir::new().unwrap();
        let g = tmp.path().join("storage");
        std::fs::create_dir_all(&g).unwrap();
        (tmp, g)
    }

    fn open_db(graphroot: &Path) -> FilesystemTagDb {
        FilesystemTagDb::open(graphroot).unwrap()
    }

    /// Build a minimal D-07-valid pichi manifest with one Scute layer.
    /// Returns (canonical bytes, manifest digest).
    fn make_pichi_manifest(scute_digest: &Digest, scute_size: u64) -> (Bytes, Digest) {
        let mut annotations = BTreeMap::new();
        annotations.insert("dev.pichi.carapace.verity.algo".into(), "sha256".into());
        annotations.insert(
            "dev.pichi.carapace.verity.data-block-size".into(),
            "4096".into(),
        );
        annotations.insert(
            "dev.pichi.carapace.verity.hash-block-size".into(),
            "4096".into(),
        );
        let manifest = Manifest {
            schema_version: 2,
            media_type: "application/vnd.oci.image.manifest.v1+json".into(),
            artifact_type: MEDIA_TYPE_PICHI_ARTIFACT_V1.into(),
            config: ConfigDescriptor::canonical(),
            layers: vec![Layer::Scute(ScuteDescriptor {
                digest: scute_digest.to_string(),
                size: scute_size,
                annotations: ScuteAnnotations {
                    // 32 zero bytes (SALT_ZERO_PREFIX) so the verity feed has
                    // a valid salt to use on the pull side.
                    salt: hex::encode([0u8; 32]),
                },
            })],
            annotations,
        };
        // Use serde_json directly: the Pitfall 3 negative-grep gate forbids
        // calling Manifest's serialise helper in this file even for
        // fixture construction (Plan 03's prophylaxis pattern: keep the gate
        // clean of false-positive call sites that future contributors might
        // cargo-cult into production code).
        let bytes = Bytes::from(serde_json::to_vec(&manifest).unwrap());
        let digest = Digest::from_bytes_sha256(&bytes);
        (bytes, digest)
    }

    /// REGISTRY-03: --pull=missing skips network when cached. Drives
    /// pull_inner_with_registry with a MockRegistry that PANICS on use to
    /// prove no method is invoked.
    #[tokio::test(flavor = "current_thread")]
    async fn pull_missing_skips_when_cached() {
        let (_tmp, g) = graphroot();
        let db = open_db(&g);
        let blob_store = FilesystemBlobStore::new(&g);
        let target: Reference = "ghcr.io/example/foo:bar".parse().unwrap();
        // Pre-set the tag in the cache.
        let cached = Digest::from_bytes_sha256(b"any cached manifest");
        db.set_tag(&target.to_string(), &cached).await.unwrap();

        let mock = MockRegistry::new();
        // No insert_* calls; if pull_inner touches the registry it will
        // produce NotFound which we'd see as a Result::Err.
        pull_inner_with_registry(target, PullPolicy::Missing, true, &mock, &blob_store, &db)
            .await
            .expect("missing+cached must short-circuit before any network call");
    }

    /// REGISTRY-07: bare manifest with non-pichi artifactType is rejected
    /// with a clear error and no blobs are added to the BlobStore.
    #[tokio::test(flavor = "current_thread")]
    async fn pull_rejects_non_pichi_bare_manifest() {
        let (_tmp, g) = graphroot();
        let db = open_db(&g);
        let blob_store = FilesystemBlobStore::new(&g);
        let target: Reference = "ghcr.io/example/foo:bar".parse().unwrap();
        let mock = MockRegistry::new();
        // Bare manifest with WRONG artifactType (non-pichi).
        let bad_manifest = Bytes::from_static(
            br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","artifactType":"application/vnd.oci.image.config.v1+json"}"#,
        );
        mock.insert_manifest("ghcr.io", "example/foo", "bar", bad_manifest);
        let err =
            pull_inner_with_registry(target, PullPolicy::Always, true, &mock, &blob_store, &db)
                .await
                .expect_err("non-pichi bare manifest must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("expected application/vnd.pichi.artifact.v1+json"),
            "wrong rejection message: {msg}"
        );
        // No blobs written — BlobStore dir should be empty (or at most have
        // sha256 dir created lazily).
        let blobs_dir = g.join("blobs").join("sha256");
        let n = std::fs::read_dir(&blobs_dir)
            .map(|r| r.count())
            .unwrap_or(0);
        assert_eq!(n, 0, "no blobs must be written on REGISTRY-07 reject");
    }

    /// REGISTRY-01: invalid (D-07-failing) manifest aborts BEFORE any blob
    /// fetch. Drives MockRegistry that has a syntactically-valid but
    /// validate()-failing manifest (missing chain annotations); verifies no
    /// blob fetch was attempted (push-blobs log is empty since pull_blob
    /// would not be called).
    #[tokio::test(flavor = "current_thread")]
    async fn pull_invalid_manifest_aborts_before_blobs() {
        let (_tmp, g) = graphroot();
        let db = open_db(&g);
        let blob_store = FilesystemBlobStore::new(&g);
        let target: Reference = "ghcr.io/example/foo:bar".parse().unwrap();
        let mock = MockRegistry::new();
        // Valid JSON, valid artifactType, but missing chain annotations →
        // Manifest::from_reader_validated rejects it.
        let bad_pichi = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "artifactType": MEDIA_TYPE_PICHI_ARTIFACT_V1,
            "config": {
                "mediaType": "application/vnd.oci.empty.v1+json",
                "digest": "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a",
                "size": 2,
                "data": "e30="
            },
            "layers": []
            // NOTE: no annotations field at all → missing chain annotations.
        });
        let bytes = Bytes::from(serde_json::to_vec(&bad_pichi).unwrap());
        mock.insert_manifest("ghcr.io", "example/foo", "bar", bytes);

        let err =
            pull_inner_with_registry(target, PullPolicy::Always, true, &mock, &blob_store, &db)
                .await
                .expect_err("invalid pichi manifest must be rejected by validate");
        let msg = format!("{err:#}");
        assert!(
            msg.to_lowercase().contains("validate") || msg.contains("annotation"),
            "expected validation error, got: {msg}"
        );
        // No blob put_blob_from_path call could have happened because the
        // registry layer 'blobs' map is empty AND there's no layer to fetch
        // anyway — but the load-bearing assertion is that `put_blob` for the
        // manifest itself was NOT called (we error out before atomic commit).
        let blobs_dir = g.join("blobs").join("sha256");
        let n = std::fs::read_dir(&blobs_dir)
            .map(|r| r.count())
            .unwrap_or(0);
        assert_eq!(n, 0, "no blobs must be written on validation reject");
    }

    /// Pitfall 3 regression guard — cmd::pull writes the manifest's RAW bytes
    /// (from pull_manifest_*) to BlobStore, never a re-serialised form. Set
    /// up the mock with manifest bytes that include trailing whitespace; if
    /// cmd::pull went through Manifest::to_bytes the whitespace would
    /// be lost. Asserts BlobStore::get_blob returns the EXACT mock bytes.
    #[tokio::test(flavor = "current_thread")]
    async fn pull_inner_writes_manifest_raw_bytes() {
        let (_tmp, g) = graphroot();
        let db = open_db(&g);
        let blob_store = FilesystemBlobStore::new(&g);
        let target: Reference = "ghcr.io/example/foo:bar".parse().unwrap();
        let mock = MockRegistry::new();

        // Build a valid pichi manifest, then add trailing whitespace AFTER
        // the closing brace. The whitespace is irrelevant to JSON parsing
        // but changes the byte content + sha256 digest. If cmd::pull
        // re-serialises, the stored bytes won't match the mock's bytes.
        let dummy_layer_data = b"hello-layer-data";
        let layer_digest = Digest::from_bytes_sha256(dummy_layer_data);
        let (clean_manifest, _clean_digest) =
            make_pichi_manifest(&layer_digest, dummy_layer_data.len() as u64);
        let mut raw_with_ws: Vec<u8> = clean_manifest.to_vec();
        raw_with_ws.extend_from_slice(b"   \n"); // trailing whitespace
        let raw_bytes = Bytes::from(raw_with_ws);
        let expected_digest = Digest::from_bytes_sha256(&raw_bytes);

        mock.insert_manifest("ghcr.io", "example/foo", "bar", raw_bytes.clone());
        // Insert the layer blob too so fetch_one_layer can succeed.
        mock.insert_blob(
            "ghcr.io",
            "example/foo",
            layer_digest.clone(),
            Bytes::from_static(dummy_layer_data),
        );

        pull_inner_with_registry(target, PullPolicy::Always, true, &mock, &blob_store, &db)
            .await
            .expect("pull_inner should succeed");

        // The manifest blob in BlobStore must equal the mock bytes verbatim
        // (not a re-serialised round-trip).
        let stored = blob_store.get_blob(&expected_digest).await.unwrap();
        assert_eq!(
            stored, raw_bytes,
            "manifest blob must be byte-identical to wire bytes (Pitfall 3)"
        );
    }

    /// Pitfall 11 / Pitfall 2 regression guard — composite assertion:
    ///   (a) source-code grep: the LAST `set_tag` call in cmd/pull.rs
    ///       appears AFTER all `put_blob*` call sites (line-number check).
    ///   (b) end-state: after a 1-layer pull, the layer blob exists and
    ///       the tag resolves to the manifest digest.
    #[tokio::test(flavor = "current_thread")]
    async fn pull_inner_atomic_commit_writes_blobs_then_tag() {
        // (a) source-code line-order grep WITHIN the orchestrator's atomic
        //     commit block. We scan `pull_inner_with_registry` only — the
        //     fetch_one_layer helper appears LATER in the file but is called
        //     from within pull_inner_with_registry BEFORE set_tag. The
        //     load-bearing invariant (Pitfall 11/2) is that within the
        //     orchestrator body the manifest `put_blob` precedes `set_tag`.
        let src = std::fs::read_to_string("src/cmd/pull.rs").expect("src/cmd/pull.rs must exist");
        // Slice between the orchestrator declaration and its closing brace
        // (the next `}` at column 0 after `pub(crate) async fn pull_inner_with_registry`).
        let orch_start = src
            .find("pub(crate) async fn pull_inner_with_registry")
            .expect("orchestrator fn must exist");
        let orch_after = &src[orch_start..];
        let orch_end_rel = orch_after
            .find("\n}")
            .expect("orchestrator fn closing brace must be present");
        let orch_body = &orch_after[..orch_end_rel];
        let mut last_put_blob_idx: Option<usize> = None;
        let mut first_set_tag_idx: Option<usize> = None;
        for (i, line) in orch_body.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with("///") {
                continue;
            }
            if line.contains(".put_blob(") || line.contains(".put_blob_from_path(") {
                last_put_blob_idx = Some(i + 1);
            }
            if line.contains(".set_tag(") {
                first_set_tag_idx = first_set_tag_idx.or(Some(i + 1));
            }
        }
        let lpb = last_put_blob_idx.expect("orchestrator must contain at least one put_blob* call");
        let fst = first_set_tag_idx.expect("orchestrator must contain at least one set_tag call");
        assert!(
            fst > lpb,
            "Pitfall 11/2: in pull_inner_with_registry, first set_tag (rel line {fst}) must come AFTER last put_blob* (rel line {lpb})"
        );

        // (b) end-state: drive a real 1-layer pull and assert blob+tag.
        let (_tmp, g) = graphroot();
        let db = open_db(&g);
        let blob_store = FilesystemBlobStore::new(&g);
        let target: Reference = "ghcr.io/example/foo:bar".parse().unwrap();
        let mock = MockRegistry::new();
        let layer_data = b"layer-blob-bytes-for-end-state-check";
        let layer_digest = Digest::from_bytes_sha256(layer_data);
        let (manifest_bytes, manifest_digest) =
            make_pichi_manifest(&layer_digest, layer_data.len() as u64);
        mock.insert_manifest("ghcr.io", "example/foo", "bar", manifest_bytes);
        mock.insert_blob(
            "ghcr.io",
            "example/foo",
            layer_digest.clone(),
            Bytes::from_static(layer_data),
        );

        pull_inner_with_registry(
            target.clone(),
            PullPolicy::Always,
            true,
            &mock,
            &blob_store,
            &db,
        )
        .await
        .expect("pull_inner should succeed");

        // Layer blob present in BlobStore.
        assert!(
            blob_store.blob_exists(&layer_digest).await,
            "layer blob must be present after pull"
        );
        // Tag resolves to manifest digest.
        let resolved = db.resolve_tag(&target.to_string()).await.unwrap();
        assert_eq!(
            resolved.as_ref(),
            Some(&manifest_digest),
            "tag must resolve to manifest digest after pull"
        );
    }

    /// REGISTRY-05 / D-02: index manifest is walked via
    /// pick_pichi_entry_from_index; the picked pichi manifest is fetched and
    /// committed; the index's own digest is NOT present in BlobStore.
    #[tokio::test(flavor = "current_thread")]
    async fn pull_index_walks_and_drops_index() {
        let (_tmp, g) = graphroot();
        let db = open_db(&g);
        let blob_store = FilesystemBlobStore::new(&g);
        let target: Reference = "ghcr.io/example/foo:bar".parse().unwrap();
        let mock = MockRegistry::new();

        // Build a pichi manifest first (the picked entry).
        let layer_data = b"index-walk-layer-bytes";
        let layer_digest = Digest::from_bytes_sha256(layer_data);
        let (picked_manifest_bytes, picked_manifest_digest) =
            make_pichi_manifest(&layer_digest, layer_data.len() as u64);
        // Insert the picked manifest by digest (no tag).
        mock.insert_manifest_by_digest("ghcr.io", "example/foo", picked_manifest_bytes.clone());
        // Build the index pointing at it.
        let index = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [{
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": picked_manifest_digest.to_string(),
                "size": picked_manifest_bytes.len(),
                "artifactType": MEDIA_TYPE_PICHI_ARTIFACT_V1,
                "platform": {"os": "pichi", "architecture": target_arch()}
            }]
        });
        let index_bytes = Bytes::from(serde_json::to_vec(&index).unwrap());
        let index_digest = Digest::from_bytes_sha256(&index_bytes);
        // Insert the index under the tag.
        mock.insert_manifest("ghcr.io", "example/foo", "bar", index_bytes);
        // Insert the layer blob.
        mock.insert_blob(
            "ghcr.io",
            "example/foo",
            layer_digest.clone(),
            Bytes::from_static(layer_data),
        );

        pull_inner_with_registry(
            target.clone(),
            PullPolicy::Always,
            true,
            &mock,
            &blob_store,
            &db,
        )
        .await
        .expect("index walk should succeed");

        // Picked manifest IS in BlobStore.
        assert!(
            blob_store.blob_exists(&picked_manifest_digest).await,
            "picked manifest blob must be present"
        );
        // Index's own digest is NOT in BlobStore (D-02: index dropped).
        assert!(
            !blob_store.blob_exists(&index_digest).await,
            "index manifest must NOT be persisted (D-02)"
        );
        // Tag resolves to PICKED manifest digest (not the index's).
        let resolved = db.resolve_tag(&target.to_string()).await.unwrap();
        assert_eq!(resolved.as_ref(), Some(&picked_manifest_digest));
    }

    // ====================================================================
    // Phase 46 Plan 02 Task 1 — sidecar derivation tests (CACHE-01, CACHE-02)
    // ====================================================================

    use pichi_artifact::PmiDescriptor;
    use pichi_storage::sidecar::{deflated_path, verity_path};

    /// Plan 02 helper: build a 1-Scute or 1-ScuteZstd manifest with a
    /// 32-zero salt. Returns (manifest bytes, manifest digest).
    fn make_scute_manifest(
        scute_digest: &Digest,
        scute_size: u64,
        is_zstd: bool,
    ) -> (Bytes, Digest) {
        let mut annotations = BTreeMap::new();
        annotations.insert("dev.pichi.carapace.verity.algo".into(), "sha256".into());
        annotations.insert(
            "dev.pichi.carapace.verity.data-block-size".into(),
            "4096".into(),
        );
        annotations.insert(
            "dev.pichi.carapace.verity.hash-block-size".into(),
            "4096".into(),
        );
        let scute_desc = ScuteDescriptor {
            digest: scute_digest.to_string(),
            size: scute_size,
            annotations: ScuteAnnotations {
                salt: hex::encode([0u8; 32]),
            },
        };
        let layer = if is_zstd {
            Layer::ScuteZstd(scute_desc)
        } else {
            Layer::Scute(scute_desc)
        };
        let manifest = Manifest {
            schema_version: 2,
            media_type: "application/vnd.oci.image.manifest.v1+json".into(),
            artifact_type: MEDIA_TYPE_PICHI_ARTIFACT_V1.into(),
            config: ConfigDescriptor::canonical(),
            layers: vec![layer],
            annotations,
        };
        let bytes = Bytes::from(serde_json::to_vec(&manifest).unwrap());
        let digest = Digest::from_bytes_sha256(&bytes);
        (bytes, digest)
    }

    /// Plan 02 helper: build a 2-layer manifest (1 Scute + 1 PMI) with a
    /// 32-zero salt on the scute. The PMI bytes are caller-supplied so a
    /// test can poke a `roothash=<hex>` token in there.
    fn make_scute_plus_pmi_manifest(
        scute_digest: &Digest,
        scute_size: u64,
        is_zstd_scute: bool,
        pmi_digest: &Digest,
        pmi_size: u64,
    ) -> (Bytes, Digest) {
        let mut annotations = BTreeMap::new();
        annotations.insert("dev.pichi.carapace.verity.algo".into(), "sha256".into());
        annotations.insert(
            "dev.pichi.carapace.verity.data-block-size".into(),
            "4096".into(),
        );
        annotations.insert(
            "dev.pichi.carapace.verity.hash-block-size".into(),
            "4096".into(),
        );
        let scute_desc = ScuteDescriptor {
            digest: scute_digest.to_string(),
            size: scute_size,
            annotations: ScuteAnnotations {
                salt: hex::encode([0u8; 32]),
            },
        };
        let scute_layer = if is_zstd_scute {
            Layer::ScuteZstd(scute_desc)
        } else {
            Layer::Scute(scute_desc)
        };
        let manifest = Manifest {
            schema_version: 2,
            media_type: "application/vnd.oci.image.manifest.v1+json".into(),
            artifact_type: MEDIA_TYPE_PICHI_ARTIFACT_V1.into(),
            config: ConfigDescriptor::canonical(),
            layers: vec![
                scute_layer,
                Layer::Pmi(PmiDescriptor {
                    digest: pmi_digest.to_string(),
                    size: pmi_size,
                }),
            ],
            annotations,
        };
        let bytes = Bytes::from(serde_json::to_vec(&manifest).unwrap());
        let digest = Digest::from_bytes_sha256(&bytes);
        (bytes, digest)
    }

    /// Plan 02 helper: build the locked Phase 42 D-06 verity params for a
    /// scute with the SAME salt the pull pipeline uses — the full salt from
    /// the `dev.pichi.scute.verity.salt` annotation, verbatim (here the base
    /// scute's 32 zero bytes from make_scute_manifest's hex::encode([0u8; 32])).
    fn pull_side_verity_params() -> pichi_import::verity::VerityParams {
        pichi_import::verity::VerityParams {
            data_block_size: 4096,
            hash_block_size: 4096,
            // The per-scute salt annotation, used verbatim (no extra prefix).
            salt: vec![0u8; 32],
            // pull-side uuid is hard-coded to 0u8; 16 in build_layer_pipeline.
            uuid: [0u8; 16],
        }
    }

    /// Plan 02 Task 1 (a): pull a `+zstd` scute and assert the source blob
    /// + `.deflated` + `.verity` sidecars are committed to disk.
    #[tokio::test(flavor = "current_thread")]
    async fn pull_zstd_layer_writes_sidecars() {
        let (_tmp, g) = graphroot();
        let db = open_db(&g);
        let blob_store = FilesystemBlobStore::new(&g);
        let target: Reference = "ghcr.io/example/foo:bar".parse().unwrap();
        let mock = MockRegistry::new();

        // Decompressed payload — needs to be >= 4096 bytes so the verity
        // feed's data_block_size aligns sensibly. Use a deterministic
        // pattern (NOT all-zeros) so a future zstd quirk doesn't accidentally
        // produce a 1-byte compressed blob that no longer exercises the tee.
        let decompressed: Vec<u8> = (0u32..).map(|i| (i & 0xFF) as u8).take(4096 * 4).collect();
        let compressed = ruzstd::encoding::compress_to_vec(
            decompressed.as_slice(),
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        let layer_digest = Digest::from_bytes_sha256(&compressed);
        let (manifest_bytes, _manifest_digest) = make_scute_manifest(
            &layer_digest,
            compressed.len() as u64,
            /* is_zstd= */ true,
        );
        mock.insert_manifest("ghcr.io", "example/foo", "bar", manifest_bytes);
        mock.insert_blob(
            "ghcr.io",
            "example/foo",
            layer_digest.clone(),
            Bytes::from(compressed),
        );

        pull_inner_with_registry(target, PullPolicy::Always, true, &mock, &blob_store, &db)
            .await
            .expect("pull of +zstd scute should succeed");

        let blob_path = blob_store.blob_path(&layer_digest);
        // Source blob committed.
        assert!(
            blob_path.exists(),
            "source blob missing: {}",
            blob_path.display()
        );
        // Both sidecars present (D-04: zstd → has .deflated).
        assert!(
            deflated_path(&blob_path).exists(),
            ".deflated sidecar missing: {}",
            deflated_path(&blob_path).display()
        );
        assert!(
            verity_path(&blob_path).exists(),
            ".verity sidecar missing: {}",
            verity_path(&blob_path).display()
        );
        // .deflated bytes byte-equal the original decompressed payload.
        let deflated_disk = std::fs::read(deflated_path(&blob_path)).unwrap();
        assert_eq!(
            deflated_disk, decompressed,
            ".deflated must equal the decompressed payload byte-for-byte"
        );
    }

    /// Plan 02 Task 1 (b): pull a RAW (non-zstd) scute and assert the
    /// source blob + .verity sidecar are present, but .deflated is NOT
    /// (D-04 disk-saving skip — carapace falls back to <src> in Phase 48).
    #[tokio::test(flavor = "current_thread")]
    async fn pull_raw_scute_omits_deflated() {
        let (_tmp, g) = graphroot();
        let db = open_db(&g);
        let blob_store = FilesystemBlobStore::new(&g);
        let target: Reference = "ghcr.io/example/foo:bar".parse().unwrap();
        let mock = MockRegistry::new();

        let layer_data: Vec<u8> = (0u32..).map(|i| (i & 0xFF) as u8).take(4096 * 2).collect();
        let layer_digest = Digest::from_bytes_sha256(&layer_data);
        let (manifest_bytes, _manifest_digest) = make_scute_manifest(
            &layer_digest,
            layer_data.len() as u64,
            /* is_zstd= */ false,
        );
        mock.insert_manifest("ghcr.io", "example/foo", "bar", manifest_bytes);
        mock.insert_blob(
            "ghcr.io",
            "example/foo",
            layer_digest.clone(),
            Bytes::from(layer_data),
        );

        pull_inner_with_registry(target, PullPolicy::Always, true, &mock, &blob_store, &db)
            .await
            .expect("pull of raw scute should succeed");

        let blob_path = blob_store.blob_path(&layer_digest);
        assert!(blob_path.exists(), "source blob missing");
        assert!(verity_path(&blob_path).exists(), ".verity sidecar missing");
        assert!(
            !deflated_path(&blob_path).exists(),
            "raw scute must NOT have a .deflated sidecar (D-04): {}",
            deflated_path(&blob_path).display()
        );
    }

    /// Plan 02 Task 1 (c): pull an artifact with an PMI layer and assert
    /// the PMI digest gets NO sidecars (Pitfall 6 — PMI has no verity
    /// tree); the scute layer in the same artifact still gets its `.verity`.
    #[tokio::test(flavor = "current_thread")]
    async fn pull_pmi_layer_writes_no_sidecars() {
        let (_tmp, g) = graphroot();
        let db = open_db(&g);
        let blob_store = FilesystemBlobStore::new(&g);
        let target: Reference = "ghcr.io/example/foo:bar".parse().unwrap();
        let mock = MockRegistry::new();

        // Build the scute payload + compute its expected roothash so the
        // synthetic PMI can carry the matching value.
        let scute_data: Vec<u8> = (0u32..).map(|i| (i & 0xFF) as u8).take(4096).collect();
        let scute_digest = Digest::from_bytes_sha256(&scute_data);
        let params = pull_side_verity_params();
        let expected_root = pichi_import::verity::compute(&scute_data, &params)
            .expect("verity compute")
            .root_hash;

        // Synthetic PMI bytes: zero pad + a `roothash=<hex>` token.
        let mut pmi_bytes = vec![0u8; 4096];
        let pattern = format!("roothash={}", hex::encode(expected_root));
        pmi_bytes[100..100 + pattern.len()].copy_from_slice(pattern.as_bytes());
        let pmi_digest = Digest::from_bytes_sha256(&pmi_bytes);

        let (manifest_bytes, _manifest_digest) = make_scute_plus_pmi_manifest(
            &scute_digest,
            scute_data.len() as u64,
            /* is_zstd_scute= */ false,
            &pmi_digest,
            pmi_bytes.len() as u64,
        );
        mock.insert_manifest("ghcr.io", "example/foo", "bar", manifest_bytes);
        mock.insert_blob(
            "ghcr.io",
            "example/foo",
            scute_digest.clone(),
            Bytes::from(scute_data),
        );
        mock.insert_blob(
            "ghcr.io",
            "example/foo",
            pmi_digest.clone(),
            Bytes::from(pmi_bytes),
        );

        pull_inner_with_registry(target, PullPolicy::Always, true, &mock, &blob_store, &db)
            .await
            .expect("pull of pmi+scute should succeed");

        // PMI has NO sidecars (Pitfall 6).
        let pmi_blob = blob_store.blob_path(&pmi_digest);
        assert!(pmi_blob.exists(), "PMI source blob missing");
        assert!(
            !deflated_path(&pmi_blob).exists(),
            "PMI must NOT have .deflated"
        );
        assert!(
            !verity_path(&pmi_blob).exists(),
            "PMI must NOT have .verity (Pitfall 6)"
        );

        // Scute still has its .verity (raw → no .deflated).
        let scute_blob = blob_store.blob_path(&scute_digest);
        assert!(verity_path(&scute_blob).exists(), "scute .verity missing");
        assert!(
            !deflated_path(&scute_blob).exists(),
            "raw scute must NOT have .deflated"
        );
    }

    /// Plan 02 Task 1 (d): pull a raw scute, then assert the on-disk
    /// `<src>.verity` bytes equal `pichi_import::verity::compute(layer_bytes,
    /// &params).blob` for the same params the pull pipeline uses.
    /// Cross-validates pull-side verity output against the in-memory
    /// reference path.
    #[tokio::test(flavor = "current_thread")]
    async fn verity_sidecar_byte_equal_to_compute() {
        let (_tmp, g) = graphroot();
        let db = open_db(&g);
        let blob_store = FilesystemBlobStore::new(&g);
        let target: Reference = "ghcr.io/example/foo:bar".parse().unwrap();
        let mock = MockRegistry::new();

        let layer_data: Vec<u8> = (0u32..).map(|i| (i & 0xFF) as u8).take(4096 * 3).collect();
        let layer_digest = Digest::from_bytes_sha256(&layer_data);
        let (manifest_bytes, _manifest_digest) =
            make_scute_manifest(&layer_digest, layer_data.len() as u64, false);
        mock.insert_manifest("ghcr.io", "example/foo", "bar", manifest_bytes);
        mock.insert_blob(
            "ghcr.io",
            "example/foo",
            layer_digest.clone(),
            Bytes::from(layer_data.clone()),
        );

        pull_inner_with_registry(target, PullPolicy::Always, true, &mock, &blob_store, &db)
            .await
            .expect("pull should succeed");

        let blob_path = blob_store.blob_path(&layer_digest);
        let on_disk_verity = std::fs::read(verity_path(&blob_path)).unwrap();

        let params = pull_side_verity_params();
        let expected = pichi_import::verity::compute(&layer_data, &params)
            .expect("verity::compute")
            .blob;
        assert_eq!(
            on_disk_verity, expected,
            "<src>.verity bytes must equal verity::compute(layer_bytes, params).blob"
        );
    }
}
