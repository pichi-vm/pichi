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

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use sha2::{Digest as _, Sha256};
use tokio_util::io::SyncIoBridge;

use pichi_artifact::{Digest, Layer, MEDIA_TYPE_PICHI_ARTIFACT_V1, Manifest, Reference};
use pichi_import::verity::{VerityBuilder, VerityOutput, VerityParams};
use pichi_registry::{OCI_IMAGE_INDEX_MEDIA_TYPE, Registry, pick_pichi_entry_from_index};
use pichi_storage::{
    BlobStore, CacheLayout, FilesystemBlobStore, FilesystemTagDb, TagDb,
    sidecar::{deflated_path, verity_path, write_sidecar_atomic},
};

use crate::cli::{PullArgs, PullPolicy};
use crate::cmd::registry_helpers::build_http_registry;
use crate::cmd::streaming_sink::{LimitWriter, TeeWriter, VerityFeedWriter, ZstdDecodeWriter};
use crate::config::Config;

/// Target platform OS for D-02 index walks (v0.8 pichi only ships linux/amd64).
const TARGET_OS: &str = "linux";
/// Target platform architecture for D-02 index walks.
const TARGET_ARCH: &str = "amd64";
/// Default per-layer decompressed-bytes cap (compressed-bomb defence;
/// RESEARCH §"Known Threat Patterns" line 1620). Default 16 GiB; future
/// enhancement: per-layer override = `descriptor.size + slack`.
const DEFAULT_DECOMPRESSED_CAP: u64 = 16 * 1024 * 1024 * 1024;
/// Phase 42 D-06 locked verity defaults — used by the pull-side verity feed.
const VERITY_DBS: u32 = 4096;
const VERITY_HBS: u32 = 4096;
/// 32-byte zero salt prefix per Phase 43 D-01. The pull-side verity output is
/// recomputed locally and not asserted bit-equal against the import-side blob
/// (Phase 43 D-03), so for non-Scute layers (Pmi) we skip verity entirely;
/// for Scute layers we use the layer's per-scute salt annotation.
const SALT_ZERO_PREFIX: &[u8] = &[0u8; 32];

/// Entry point for `pichi pull`. Builds a throwaway tokio current-thread
/// runtime, drives `pull_inner`, and drops the runtime before returning.
pub fn run(args: PullArgs, config: &Config) -> Result<()> {
    // Defense-in-depth: parse + canonicalise BEFORE any I/O (BL-02 / T-43-02).
    let target_ref: Reference = args
        .reference
        .parse()
        .with_context(|| format!("invalid reference: {}", args.reference))?;
    let policy = args.pull.unwrap_or(PullPolicy::Always); // D-05 default.
    let layout = resolve_layout(config)?;
    let blob_store = FilesystemBlobStore::new(&layout.graphroot);
    let tag_db = FilesystemTagDb::open(&layout.graphroot)
        .with_context(|| format!("opening tag db at {}", layout.graphroot.display()))?;

    // --pull=never short-circuits BEFORE building the runtime (no network).
    if policy == PullPolicy::Never {
        return tag_db
            .resolve_tag(&target_ref.to_string())?
            .map(|_| ())
            .ok_or_else(|| anyhow!("pull policy `never`: ref not in cache: {target_ref}"));
    }

    // Throwaway runtime (Pattern 1): `enable_all` includes the blocking pool
    // required by SyncIoBridge (Plan 01 SPIKE A3).
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build per-call tokio runtime")?;
    rt.block_on(async {
        pull_inner(target_ref, policy, args.quiet, config, &blob_store, &tag_db).await
    })
    // rt drops here; tokio worker threads torn down before fn returns.
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
    if policy == PullPolicy::Missing && tag_db.resolve_tag(&target_str)?.is_some() {
        return Ok(());
    }
    // --pull=newer: GET upstream manifest digest, compare to cached
    // (W6 revision: GET-then-compare, HEAD optimisation deferred).
    if policy == PullPolicy::Newer {
        if let Some(cached) = tag_db.resolve_tag(&target_str)? {
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
        let picked_str = pick_pichi_entry_from_index(&raw, TARGET_OS, TARGET_ARCH)?;
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

    // 4. Pull each layer via the streaming pipeline (REGISTRY-01 dedup via
    //    blob_exists). Sidecars (`.deflated` for `+zstd` only, `.verity`
    //    for every scute) are written by `fetch_one_layer` after the
    //    source blob commits. pichi never reads or validates the PMI
    //    cmdline's `roothash=`; consistency between cmdline and verity
    //    tree is the publisher's responsibility, validated at boot time
    //    inside the guest's dm-verity activation.
    for layer in &manifest.layers {
        let layer_digest: Digest = layer
            .digest_str()
            .parse()
            .with_context(|| format!("parse layer digest {}", layer.digest_str()))?;
        if blob_store.blob_exists(&layer_digest) {
            // REGISTRY-01: dedup by descriptor digest. D-01 implicit
            // refcount: source present ⇒ sidecars present (no partial-cache
            // regeneration in v0.9; Phase 47 prune cleans up partial states).
            continue;
        }
        fetch_one_layer(registry, &target, layer, blob_store)
            .await
            .with_context(|| format!("fetch layer {layer_digest}"))?;
    }

    // 5. Atomic commit per D-03 refined per Pitfall 11 / Phase 42 Plan 05
    //    SUMMARY: live-walk refcounts; no sidecar; set_tag is the single
    //    commit point and takes its own flock (DO NOT wrap in
    //    the cache-wide advisory-lock helper per Pitfall 2).
    blob_store
        .put_blob(&final_digest, &final_bytes)
        .with_context(|| format!("put_blob manifest {final_digest}"))?;
    tag_db
        .set_tag(&target_str, &final_digest)
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
    let registry = build_http_registry(config);
    pull_inner_with_registry(target, policy, quiet, &registry, blob_store, tag_db).await
}

/// One-layer fetch: stream the registry's bytes through the pipeline and
/// commit the result via `put_blob_from_path`. The BlobStore key is the
/// descriptor digest (oci-client verifies the wire bytes hash to that digest
/// internally; for `+zstd` layers the descriptor digest IS the compressed
/// digest by OCI convention, so the same key is used).
///
/// **Bridge shape (Plan 01 SPIKE A3 corrected understanding):** `pull_blob`
/// requires an `AsyncWrite` sink, but our pipeline (TeeWriter / DigestWriter
/// / ZstdDecodeWriter / VerityFeedWriter / LimitWriter) is `std::io::Write`.
/// We bridge with `tokio::io::duplex` + `spawn_blocking`:
///   - `pull_blob` writes async into one half of a duplex pipe.
///   - A blocking task reads the OTHER half via `SyncIoBridge` (async-read →
///     sync-read shape, which is what `SyncIoBridge::new(reader)` produces
///     for an `AsyncRead`) and `std::io::copy`-ies the bytes through our
///     sync pipeline.
///   - When `pull_blob` returns, the writer half is dropped → the reader
///     half sees EOF → the blocking task finishes the pipeline and returns
///     the captured tempfile.
async fn fetch_one_layer<R: Registry>(
    registry: &R,
    target: &Reference,
    layer: &Layer,
    blob_store: &dyn BlobStore,
) -> Result<()> {
    let descriptor_digest: Digest = layer.digest_str().parse()?;
    let scratch = blob_store
        .scratch_dir()
        .context("preparing scratch dir for streaming layer")?;
    let temp = tempfile::NamedTempFile::new_in(&scratch)
        .with_context(|| format!("creating layer temp file in {}", scratch.display()))?;

    let is_zstd = layer.is_zstd_variant();
    // For Scute layers, extract the per-scute salt for the verity feed.
    // For Pmi layers, no salt → no verity feed.
    let scute_salt = match layer {
        Layer::Scute(d) | Layer::ScuteZstd(d) => Some(
            hex::decode(&d.annotations.salt)
                .with_context(|| format!("decode scute salt for layer {}", d.digest))?,
        ),
        Layer::Pmi(_) => None,
    };

    // D-04: only `+zstd` scutes get a `<src>.deflated` sidecar — for raw
    // scutes the source IS the deflated bytes (carapace's read path falls
    // back to <src> in Phase 48). The deflated tempfile lives in the same
    // scratch dir as the source tempfile so its eventual rename(2) is
    // same-fs (Pitfall 1). For Pmi layers and raw scutes we pass None.
    let deflated_temp = if is_zstd && scute_salt.is_some() {
        Some(
            tempfile::NamedTempFile::new_in(&scratch)
                .with_context(|| format!("creating deflated temp file in {}", scratch.display()))?,
        )
    } else {
        None
    };

    let (sync_sink, capture) =
        build_layer_pipeline(temp, is_zstd, scute_salt, deflated_temp.as_ref())?;

    // Build the async/sync bridge: 64 KiB internal duplex buffer balances
    // syscall amortisation against memory residency.
    let (writer_async, reader_async) = tokio::io::duplex(64 * 1024);

    // Spawn the sync pipeline on the blocking pool. It owns the sink and the
    // capture; it returns the finalised tempfile (or any pipeline error).
    let pipeline_handle: tokio::task::JoinHandle<Result<LayerFinal>> =
        tokio::task::spawn_blocking(move || {
            let mut sync_sink: Box<dyn std::io::Write + Send> = sync_sink;
            let mut sync_reader = SyncIoBridge::new(reader_async);
            std::io::copy(&mut sync_reader, &mut sync_sink)
                .context("std::io::copy registry → sync pipeline")?;
            // Finalise the pipeline INSIDE the blocking task — capture's
            // hashers + tempfile fsync are sync operations that mustn't run
            // on the async runtime thread.
            capture.finalize_into(sync_sink)
        });

    // Drive pull_blob into the async writer; drop the writer half explicitly
    // so the reader half sees EOF and the spawn_blocking task can finish.
    let pull_result = {
        let mut writer = writer_async;
        let res = registry
            .pull_blob(
                &target.registry,
                &target.repo,
                &descriptor_digest,
                layer.size(),
                &mut writer,
            )
            .await
            .map_err(|e| anyhow!("pull_blob {descriptor_digest}: {e}"));
        // Explicit shutdown + drop so the reader half observes EOF.
        use tokio::io::AsyncWriteExt as _;
        let _ = writer.shutdown().await;
        drop(writer);
        res
    };

    pull_result?;
    // Move the deflated tempfile into the finalised LayerFinal (the pipeline
    // owns only a clone of the underlying file handle for writing; here in
    // the async task we still own the NamedTempFile so we can rename it
    // after put_blob succeeds).
    let mut layer_final = pipeline_handle
        .await
        .context("pipeline blocking task join")??;
    layer_final.deflated_temp = deflated_temp;

    blob_store
        .put_blob_from_path(layer_final.source_temp.path(), &descriptor_digest)
        .with_context(|| format!("put_blob_from_path layer {descriptor_digest}"))?;
    // put_blob_from_path consumed the file via rename(2); tell NamedTempFile
    // not to attempt deletion on drop.
    let _ = layer_final.source_temp.into_temp_path().keep();

    // Per D-03 / D-04: derive sidecars AFTER the source blob is committed.
    // Pmi layers have no verity → return early (Pitfall 6 — PMI has no
    // verity tree). For scute layers, write `<src>.verity` unconditionally;
    // write `<src>.deflated` only for `+zstd` variants (D-04 disk-saving
    // skip). No `.roothash` sidecar — pichi never reads the roothash; the
    // publisher bakes `roothash=<hex>` into the PMI cmdline at arma-build
    // time and the guest reads it from cmdline at boot.
    let Some(verity_out) = layer_final.verity_out else {
        return Ok(());
    };

    let blob_path = blob_store.blob_path(&descriptor_digest);

    // (a) `<src>.deflated` (zstd only). For consistency with the source-blob
    //     path we rename the deflated_temp directly rather than slurping its
    //     bytes through write_sidecar_atomic — multi-GB scutes must not be
    //     materialised in memory. The deflated_temp was created in `scratch`
    //     so the rename is same-fs (Pitfall 1).
    if let Some(deflated_temp) = layer_final.deflated_temp {
        let final_path = deflated_path(&blob_path);
        // Ensure the parent dir exists (BlobStore::put_blob_from_path created
        // it for the source blob; the sidecar shares the same dir but a
        // future BlobStore impl might not, so be defensive).
        if let Some(parent) = final_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "create blob dir for deflated sidecar at {}",
                    parent.display()
                )
            })?;
        }
        // sync_all the deflated tempfile so the post-rename file content is
        // durable on disk — mirrors the source-blob path's
        // `LayerCapture::finalize_into` call to `self.temp.as_file().sync_all()`.
        deflated_temp
            .as_file()
            .sync_all()
            .with_context(|| format!("sync_all deflated tempfile for {descriptor_digest}"))?;
        std::fs::rename(deflated_temp.path(), &final_path).with_context(|| {
            format!(
                "rename deflated tempfile to {} (sidecar persist for {descriptor_digest})",
                final_path.display()
            )
        })?;
        let _ = deflated_temp.into_temp_path().keep();
    }

    // (b) `<src>.verity` — written via the Plan 01 atomic helper. Carapace
    //     (Phase 48) exposes this to the guest as the dm-verity hash device.
    write_sidecar_atomic(&scratch, &verity_path(&blob_path), &verity_out.blob)
        .with_context(|| format!("write verity sidecar for {descriptor_digest}"))?;

    Ok(())
}

/// Glue helper: build the per-layer pipeline; return the sync sink + a
/// capture handle that owns the shared hashers + verity builder + temp file
/// for finalisation after the bridge shutdown.
///
/// Pipeline shape (per RESEARCH §"Recommended Trait Shape" §"Pipeline
/// Composition" lines 568-637 + Pitfall 5 + Pitfall 12):
///
/// 1. outer Sha256 over wire bytes (descriptor-digest verify side)
/// 2. zstd::stream::write::Decoder branch IF +zstd, else passthrough
/// 3. inner Sha256 over decoded bytes (= wire bytes when not +zstd)
/// 4. VerityBuilder.add_data_block — fed in lockstep with decoded chunks
///    (only when a per-scute salt is provided; Pmi layers skip verity)
/// 5. LimitWriter (compressed-bomb defence) → BlobStore tempfile writer
fn build_layer_pipeline(
    temp: tempfile::NamedTempFile,
    is_zstd: bool,
    scute_salt: Option<Vec<u8>>,
    deflated_temp: Option<&tempfile::NamedTempFile>,
) -> Result<(Box<dyn std::io::Write + Send>, LayerCapture)> {
    // Shared hashers and (optional) verity builder.
    let outer_hasher = Arc::new(Mutex::new(Sha256::new()));
    let inner_hasher = Arc::new(Mutex::new(Sha256::new()));

    // (5) BlobStore tempfile writer wrapped in BufWriter for syscall
    //     amortisation. We deliberately do NOT consume `temp` here; the
    //     LayerCapture owns it via the file handle clone so finalize_into can
    //     sync_all + return it for put_blob_from_path.
    let file_handle = temp
        .as_file()
        .try_clone()
        .context("clone tempfile handle for BufWriter")?;
    let buffered = std::io::BufWriter::new(file_handle);

    // Decompressed-side cap (Pitfall 12 / compressed-bomb defence).
    let limit = LimitWriter::new(buffered, DEFAULT_DECOMPRESSED_CAP);

    // (4) Optional VerityFeed callback (only for Scute layers with a salt).
    //     For non-Scute (Pmi) layers we skip verity entirely.
    let verity = if let Some(salt_suffix) = scute_salt {
        let mut full_salt: Vec<u8> = SALT_ZERO_PREFIX.to_vec();
        full_salt.extend_from_slice(&salt_suffix);
        let params = VerityParams {
            data_block_size: VERITY_DBS,
            hash_block_size: VERITY_HBS,
            salt: full_salt,
            uuid: [0u8; 16], // pull-side verity output is discarded; uuid irrelevant.
        };
        Some(Arc::new(Mutex::new(
            VerityBuilder::new(&params).context("VerityBuilder::new for pull")?,
        )))
    } else {
        None
    };

    // Build the (Verity → Limit → tempfile) chain.
    let after_verity: Box<dyn std::io::Write + Send> = if let Some(vb) = verity.as_ref() {
        let vb_for_cb = Arc::clone(vb);
        Box::new(VerityFeedWriter::new(
            limit,
            move |block: &[u8]| -> std::io::Result<()> {
                let mut guard = vb_for_cb
                    .lock()
                    .map_err(|_| std::io::Error::other("verity mutex poisoned"))?;
                guard
                    .add_data_block(block)
                    .map_err(|e| std::io::Error::other(format!("verity: {e}")))
            },
        ))
    } else {
        Box::new(limit)
    };

    // (3) Inner Sha256 (decompressed-side) — Tee splits the decoded stream
    //     into (hasher [+optional deflated capture], verity-or-limit →
    //     tempfile). Phase 46 D-04: when capturing the decompressed bytes
    //     into `<src>.deflated`, we nest one more Tee so the inner_hasher
    //     side ALSO writes into the deflated_temp file. The deflated_temp's
    //     file handle is `try_clone`-d here; the `NamedTempFile` itself
    //     stays in `fetch_one_layer`'s scope (the rename(2) at the
    //     post-`put_blob_from_path` step needs the original handle).
    let inner_hasher_writer = SharedSha256Writer {
        hasher: Arc::clone(&inner_hasher),
    };
    let decompressed_tee: Box<dyn std::io::Write + Send> = if let Some(dt) = deflated_temp {
        let dt_handle = dt
            .as_file()
            .try_clone()
            .context("clone deflated tempfile handle for capture")?;
        let dt_buffered = std::io::BufWriter::new(dt_handle);
        let inner_plus_deflated = TeeWriter::new(inner_hasher_writer, dt_buffered);
        Box::new(TeeWriter::new(inner_plus_deflated, after_verity))
    } else {
        Box::new(TeeWriter::new(inner_hasher_writer, after_verity))
    };

    // (2) zstd branch.
    let after_decompress: Box<dyn std::io::Write + Send> = if is_zstd {
        // Pitfall 5: ZstdDecodeWriter MUST be flushed BEFORE the inner hasher
        // is finalised. LayerCapture::finalize_into enforces this ordering.
        Box::new(
            ZstdDecodeWriter::new(decompressed_tee).context("zstd::stream::write::Decoder::new")?,
        )
    } else {
        // Passthrough — for non-zstd layers the "decompressed" hasher is
        // identical to the "compressed" hasher (same bytes flow through both
        // Tee branches).
        decompressed_tee
    };

    // (1) Outer Sha256 (compressed-side) — outermost Tee splits the raw
    //     oci-client stream into (compressed-hasher, decompress-or-passthrough → ...).
    let outer_hasher_writer = SharedSha256Writer {
        hasher: Arc::clone(&outer_hasher),
    };
    let outer = TeeWriter::new(outer_hasher_writer, after_decompress);

    let capture = LayerCapture {
        temp,
        outer_hasher,
        inner_hasher,
        verity,
        is_zstd,
    };
    Ok((Box::new(outer), capture))
}

/// Sha2 hasher wrapped in `Arc<Mutex<>>` so it can be shared between the
/// writer adapter (which feeds bytes via `Write::write`) and the
/// [`LayerCapture`] (which finalises after the bridge shutdown).
struct SharedSha256Writer {
    hasher: Arc<Mutex<Sha256>>,
}

impl std::io::Write for SharedSha256Writer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut guard = self
            .hasher
            .lock()
            .map_err(|_| std::io::Error::other("sha256 mutex poisoned"))?;
        guard.update(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Holds the per-layer state needed to finalise the pipeline after the
/// `SyncIoBridge` has shut down. Tear-down order (Pitfall 5): drop sink →
/// finalise verity → file sync_all → finalise sha256 hashers.
struct LayerCapture {
    temp: tempfile::NamedTempFile,
    outer_hasher: Arc<Mutex<Sha256>>,
    inner_hasher: Arc<Mutex<Sha256>>,
    verity: Option<Arc<Mutex<VerityBuilder>>>,
    is_zstd: bool,
}

/// Output of [`LayerCapture::finalize_into`].
///
/// Phase 46 Plan 02 changed the per-layer pipeline's output shape from "just
/// the source tempfile" to a struct carrying (a) the source tempfile (still
/// needed for `put_blob_from_path`), (b) the optional decompressed-bytes
/// tempfile for `<src>.deflated` (only Some for `+zstd` scute layers per
/// D-04), and (c) the verity output (Some for scute layers, None for PMI
/// per Pitfall 6). `fetch_one_layer` consumes all three to (1) commit the
/// source blob, (2) rename the deflated tempfile into place if present, and
/// (3) write the `<src>.verity` sidecar via
/// `pichi_storage::sidecar::write_sidecar_atomic`.
///
/// `deflated_temp` is initialised to `None` here and re-attached by
/// `fetch_one_layer` after `await` (the pipeline body sees only a `try_clone`
/// of the underlying file descriptor; the original `NamedTempFile` stays in
/// `fetch_one_layer`'s scope so its `Drop` semantics — and the post-pipeline
/// `rename(2)` — work correctly).
struct LayerFinal {
    source_temp: tempfile::NamedTempFile,
    /// `None` for non-zstd layers (D-04 disk-saving) AND for Pmi layers.
    /// Set by `fetch_one_layer` AFTER awaiting the blocking pipeline task.
    deflated_temp: Option<tempfile::NamedTempFile>,
    /// `None` for Pmi layers (Pitfall 6 — PMI has no verity tree).
    /// `Some` for scute layers (raw and `+zstd`).
    verity_out: Option<VerityOutput>,
}

impl LayerCapture {
    /// Recover the inner hashers + tempfile + verity output after the
    /// `SyncIoBridge` has shut down. Returns a `LayerFinal` so the caller
    /// can hand the source tempfile to `put_blob_from_path`, then write
    /// the `.deflated` (zstd-only) and `.verity` sidecars (Phase 46 D-01).
    ///
    /// Pitfall 5 ordering: dropping `sink` triggers Drop on the outermost
    /// `TeeWriter`, which in turn triggers Drop on the wrapped
    /// `ZstdDecodeWriter` (which flushes any final residue into the inner
    /// hasher AND into the deflated-capture tempfile). Only AFTER that drop
    /// do we finalise the hashers + the verity builder.
    fn finalize_into(self, sink: Box<dyn std::io::Write + Send>) -> Result<LayerFinal> {
        // Drop the outermost Tee — releases all inner writer references.
        // The cascading Drop chain flushes ZstdDecodeWriter's residue and
        // the BufWriter wrapping the deflated-capture file handle (so the
        // deflated tempfile sees every decompressed byte before sync_all
        // below runs).
        drop(sink);

        // sync_all the tempfile so put_blob_from_path's atomic rename reads
        // a fully-flushed file.
        self.temp
            .as_file()
            .sync_all()
            .context("sync_all layer tempfile")?;

        // Phase 46 D-02 / CACHE-02: capture the verity output instead of
        // discarding it. For Pmi layers (no verity feed), self.verity is
        // None and verity_out stays None — Pitfall 6.
        let verity_out = if let Some(verity) = self.verity {
            Some(
                Arc::try_unwrap(verity)
                    .map_err(|_| anyhow!("verity Arc still has external refs at finalize"))?
                    .into_inner()
                    .map_err(|_| anyhow!("verity mutex poisoned"))?
                    .finalize(),
            )
        } else {
            None
        };

        // Pull the hashers out of their Arcs. After dropping `sink` above,
        // this side holds the only remaining strong reference.
        let outer = Arc::try_unwrap(self.outer_hasher)
            .map_err(|_| anyhow!("outer hasher Arc still has external refs"))?
            .into_inner()
            .map_err(|_| anyhow!("outer hasher mutex poisoned"))?;
        let inner = Arc::try_unwrap(self.inner_hasher)
            .map_err(|_| anyhow!("inner hasher Arc still has external refs"))?
            .into_inner()
            .map_err(|_| anyhow!("inner hasher mutex poisoned"))?;

        let outer_bytes: [u8; 32] = outer.finalize().into();
        let inner_bytes: [u8; 32] = inner.finalize().into();
        let compressed = Digest::Sha256(outer_bytes);
        let decompressed = Digest::Sha256(inner_bytes);

        // For non-zstd layers, both Tee branches saw the same bytes, so the
        // two digests are byte-equal. For zstd layers, they differ (Pitfall
        // 12). We do not enforce here — oci-client verifies wire-bytes hash
        // to descriptor.digest internally; this is a defence-in-depth
        // observation point.
        if !self.is_zstd {
            debug_assert_eq!(
                compressed, decompressed,
                "non-zstd layer must have equal compressed/decompressed digests"
            );
        }

        Ok(LayerFinal {
            source_temp: self.temp,
            // Re-attached by `fetch_one_layer` after `await` — the
            // NamedTempFile lives in that scope so its post-pipeline rename
            // can move the file into the blobs dir.
            deflated_temp: None,
            verity_out,
        })
    }
}

/// Verbatim copy from `src/cmd/import.rs` lines 38-47.
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

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use pichi_artifact::{
        EmptyConfigDescriptor, MEDIA_TYPE_PICHI_ARTIFACT_V1, ScuteAnnotations, ScuteDescriptor,
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
            config: EmptyConfigDescriptor::canonical(),
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
        db.set_tag(&target.to_string(), &cached).unwrap();

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
        let stored = blob_store.get_blob(&expected_digest).unwrap();
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
            blob_store.blob_exists(&layer_digest),
            "layer blob must be present after pull"
        );
        // Tag resolves to manifest digest.
        let resolved = db.resolve_tag(&target.to_string()).unwrap();
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
                "platform": {"os": "linux", "architecture": "amd64"}
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
            blob_store.blob_exists(&picked_manifest_digest),
            "picked manifest blob must be present"
        );
        // Index's own digest is NOT in BlobStore (D-02: index dropped).
        assert!(
            !blob_store.blob_exists(&index_digest),
            "index manifest must NOT be persisted (D-02)"
        );
        // Tag resolves to PICKED manifest digest (not the index's).
        let resolved = db.resolve_tag(&target.to_string()).unwrap();
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
            config: EmptyConfigDescriptor::canonical(),
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
            config: EmptyConfigDescriptor::canonical(),
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
    /// scute with the SAME salt the pull pipeline uses (32-zero prefix +
    /// 32-zero per-scute suffix from the manifest).
    fn pull_side_verity_params() -> pichi_import::verity::VerityParams {
        pichi_import::verity::VerityParams {
            data_block_size: 4096,
            hash_block_size: 4096,
            // SALT_ZERO_PREFIX (32 zeros) + per-scute salt (also 32 zeros
            // from make_scute_manifest's hex::encode([0u8; 32]) annotation).
            salt: vec![0u8; 64],
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
        let compressed = zstd::stream::encode_all(decompressed.as_slice(), 0).unwrap();
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
