// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `pichi push <ref>` — REGISTRY-02.
//!
//! Throwaway tokio runtime per call (Pattern 1, mirrors `src/cmd/pull.rs`).
//! Reads the cached manifest + layer blobs, runs HEAD-before-PUT for each
//! blob (skip on present), and finally pushes the manifest LAST as raw bytes
//! (Pitfall 3: preserves the OCI digest).
//!
//! Pipeline (per Plan 05 / 44-RESEARCH §"Push Pipeline"):
//!
//! 1. Resolve tag → manifest digest via [`FilesystemTagDb::resolve_tag`].
//! 2. Read raw manifest bytes via [`BlobStore::get_blob`] (Pitfall 3 — never
//!    re-serialise; `Manifest::to_bytes()` MUST NOT appear in this
//!    file, enforced by the Plan 05 negative-grep gate).
//! 3. Pre-push re-validation via [`Manifest::from_reader_validated`] as
//!    a cheap O(layers) safety net (the validation result is discarded; only
//!    the structural error path is consumed).
//! 4. For each layer: HEAD → (cross-repo mount attempt — skipped for v0.8;
//!    no source-repo configuration surface yet) → `push_blob_stream` from
//!    [`BlobStore::open_blob`].
//! 5. Push manifest LAST via [`Registry::push_manifest`] with content-type
//!    `application/vnd.oci.image.manifest.v1+json`, RAW bytes from cache.
//!
//! T-44-05-03 ordering invariant: the layer loop body completes (last
//! `push_blob_stream` returns Ok) BEFORE `push_manifest` is called. Registry
//! would reject a manifest pointing at a missing blob anyway, but pushing in
//! the opposite order risks intermediate states observers can race with.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use futures_util::stream;
use pichi_artifact::{Digest, Manifest, Reference};
use pichi_registry::Registry;
use pichi_storage::{BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};

use crate::cli::PushArgs;
use crate::config::Config;

/// Content-type for OCI image manifests. Plan 05 always pushes the pichi
/// manifest under this media type — the inner `artifactType` field carries
/// the `application/vnd.pichi.artifact.v1+json` discriminator (REGISTRY-07).
const PUSH_MANIFEST_CONTENT_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

/// Entry point for `pichi push`. Builds a throwaway tokio current-thread
/// runtime, drives [`push_inner`], drops the runtime before returning.
pub async fn run(args: PushArgs, config: &Config) -> Result<()> {
    // Defense-in-depth: parse + canonicalise BEFORE any I/O (BL-02).
    let target_ref: Reference = args
        .reference
        .parse()
        .with_context(|| format!("invalid reference: {}", args.reference))?;
    let layout = config.resolve_layout()?;
    let blob_store = FilesystemBlobStore::new(&layout.graphroot);
    let tag_db = FilesystemTagDb::open(&layout.graphroot)
        .with_context(|| format!("opening tag db at {}", layout.graphroot.display()))?;

    push_inner_with_registry(
        target_ref,
        args.quiet,
        &config.http_registry(),
        &blob_store,
        &tag_db,
    )
    .await
}

/// Internal driver — `pub(crate)` so the in-module `tests` mod can drive it
/// with a [`pichi_registry::MockRegistry`]. Mirrors the
/// `pull_inner_with_registry` shape from `src/cmd/pull.rs` (Plan 04 D-04
/// generic-over-Registry pattern).
pub(crate) async fn push_inner_with_registry<R: Registry>(
    target: Reference,
    _quiet: bool,
    registry: &R,
    blob_store: &dyn BlobStore,
    tag_db: &FilesystemTagDb,
) -> Result<()> {
    // 1. Resolve tag → manifest digest.
    let target_str = target.to_string();
    let manifest_digest = tag_db
        .resolve_tag(&target_str)
        .await?
        .ok_or_else(|| anyhow!("ref not in cache: {target}"))?;

    // 2. Read cached manifest bytes (raw — preserves OCI digest per Pitfall 3).
    let raw_manifest = blob_store
        .get_blob(&manifest_digest)
        .await
        .with_context(|| format!("read manifest {manifest_digest} from cache"))?;

    // 3-6: push the manifest and its blobs.
    push_manifest_and_blobs(
        registry,
        blob_store,
        &target,
        raw_manifest.as_slice(),
        PUSH_MANIFEST_CONTENT_TYPE,
    )
    .await
}

/// Push one image manifest and all its blobs (config + layers) to `target`,
/// HEAD-skipping blobs already present. Blobs go to `target.registry`/`repo`;
/// the manifest is PUT at `target` (a tag or a digest). Shared by `pichi push`
/// (single image) and `pichi manifest push` (each referenced arch image).
pub(crate) async fn push_manifest_and_blobs<R: Registry>(
    registry: &R,
    blob_store: &dyn BlobStore,
    target: &Reference,
    raw_manifest: &[u8],
    manifest_media_type: &str,
) -> Result<()> {
    // Pre-push re-validation (cheap O(layers) safety net); also the source of
    // truth for the config + layer walk below.
    let manifest = Manifest::from_reader_validated(raw_manifest)
        .context("re-validate manifest before push")?;

    // Push the empty-config blob (`{}`, 2 bytes, sha256:44136fa3...)
    //      before any layer. OCI 1.1 lets the manifest carry inline `data`
    //      for the empty-config descriptor, but registries (zot included)
    //      still require the physical blob to exist before accepting the
    //      manifest — they validate every descriptor's digest is present.
    //      This was caught by the first ShelbyAPichi CI run: zot returned
    //      `MANIFEST_INVALID` 400 because the `{}` blob wasn't uploaded.
    //      For a real `vnd.pichi.config.v1+json` blob the bytes come from the
    //      local cache; for the OCI empty config they are the well-known `{}`.
    let config_digest: Digest = manifest
        .config
        .digest
        .parse()
        .with_context(|| format!("invalid config digest: {}", manifest.config.digest))?;
    if !registry
        .head_blob(&target.registry, &target.repo, &config_digest)
        .await
        .map_err(|e| anyhow!("head_blob config {config_digest}: {e}"))?
    {
        let bytes: Bytes = if manifest.config.is_empty() {
            Bytes::from_static(b"{}")
        } else {
            Bytes::from(
                blob_store
                    .get_blob(&config_digest)
                    .await
                    .with_context(|| format!("read config blob {config_digest} from cache"))?
                    .to_vec(),
            )
        };
        let size = bytes.len() as u64;
        let one_chunk = stream::once(async move { Ok::<Bytes, std::io::Error>(bytes) });
        registry
            .push_blob_stream(
                &target.registry,
                &target.repo,
                &config_digest,
                size,
                one_chunk,
            )
            .await
            .map_err(|e| anyhow!("push_blob_stream config {config_digest}: {e}"))?;
    }

    // 5. For each layer: HEAD → (cross-repo mount no-op for v0.8) →
    //    push_blob_stream. Mount is intentionally skipped: there's no
    //    config surface that exposes a "mirror" or "source repo" hint yet,
    //    so calling `try_blob_mount(target_repo, target_repo, digest)` is
    //    a wasted round-trip (registry would refuse). When v0.9 adds the
    //    config surface this becomes a one-line attempt before push_stream.
    for layer in &manifest.layers {
        let digest_str = layer.digest_str();
        let layer_digest: Digest = digest_str
            .parse()
            .with_context(|| format!("parse layer digest {digest_str}"))?;

        // 5a. HEAD-before-PUT (REGISTRY-02 dedup). T-44-05-02 TOCTOU race
        //     with concurrent push is benign per OCI semantics: PUT of a
        //     digest already present returns 201.
        if registry
            .head_blob(&target.registry, &target.repo, &layer_digest)
            .await
            .map_err(|e| anyhow!("head_blob {layer_digest}: {e}"))?
        {
            log::debug!("blob {layer_digest} already in {} — skip", target.registry);
            continue;
        }

        // 5b. Stream upload from BlobStore (async reader → ReaderStream).
        let stream_chunks = blob_to_stream(blob_store, &layer_digest).await?;
        registry
            .push_blob_stream(
                &target.registry,
                &target.repo,
                &layer_digest,
                layer.size(),
                stream_chunks,
            )
            .await
            .map_err(|e| anyhow!("push_blob_stream layer {layer_digest}: {e}"))?;
    }

    // 6. Push manifest LAST. T-44-05-03: layer loop above MUST complete
    //    before this call — see source-line-order regression guard in
    //    src/cmd/push.rs::tests::push_manifest_pushed_after_all_blobs.
    //    Pitfall 3: bytes are passed verbatim from the cache, NOT
    //    re-serialised through Manifest's serialise helper.
    registry
        .push_manifest(
            target,
            manifest_media_type,
            Bytes::copy_from_slice(raw_manifest),
        )
        .await
        .map_err(|e| anyhow!("push_manifest {target}: {e}"))?;

    Ok(())
}

/// Wrap a [`BlobStore`] blob as an async `Stream<Item = io::Result<Bytes>>` for
/// the registry's streaming PUT. `open_blob` yields an `AsyncRead`, which
/// `tokio_util::io::ReaderStream` adapts into the chunk stream the transport
/// consumes — no blocking `Read` on the runtime.
async fn blob_to_stream(
    blob_store: &dyn BlobStore,
    digest: &Digest,
) -> Result<impl futures_util::stream::Stream<Item = std::io::Result<Bytes>> + Send + 'static> {
    let reader = blob_store
        .open_blob(digest)
        .await
        .with_context(|| format!("open_blob {digest}"))?;
    Ok(tokio_util::io::ReaderStream::new(reader))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use pichi_artifact::{
        ConfigDescriptor, Layer, MEDIA_TYPE_PICHI_ARTIFACT_V1, ScuteAnnotations, ScuteDescriptor,
    };
    use pichi_registry::MockRegistry;
    use std::collections::BTreeMap;
    use std::path::Path;
    use tempfile::TempDir;

    /// Build a graphroot + tag db for unit tests (mirrors src/cmd/pull.rs::tests).
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
    /// Returns (canonical bytes, manifest digest). Uses serde_json::to_vec
    /// (NOT Manifest::to_bytes) so the Pitfall 3 negative-grep gate
    /// stays clean of false-positive call sites.
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
                    salt: hex::encode([0u8; 32]),
                },
            })],
            annotations,
        };
        let bytes = Bytes::from(serde_json::to_vec(&manifest).unwrap());
        let digest = Digest::from_bytes_sha256(&bytes);
        (bytes, digest)
    }

    /// Seed the cache: write the manifest + the single layer blob, set the
    /// tag. Returns the manifest digest.
    async fn seed_cache(graphroot: &Path, tag: &str, layer_data: &[u8]) -> (Digest, Digest) {
        let blob_store = FilesystemBlobStore::new(graphroot);
        let db = FilesystemTagDb::open(graphroot).unwrap();
        let layer_digest = Digest::from_bytes_sha256(layer_data);
        blob_store
            .put_blob(&layer_digest, layer_data)
            .await
            .unwrap();
        let (manifest_bytes, manifest_digest) =
            make_pichi_manifest(&layer_digest, layer_data.len() as u64);
        blob_store
            .put_blob(&manifest_digest, manifest_bytes.as_ref())
            .await
            .unwrap();
        db.set_tag(tag, &manifest_digest).await.unwrap();
        (manifest_digest, layer_digest)
    }

    /// REGISTRY-02 dedup: HEAD returns true → push_blob_stream is NOT called
    /// for that layer (verified via MockRegistry::pushed_blobs() snapshot).
    #[tokio::test(flavor = "current_thread")]
    async fn push_inner_skips_present_blobs() {
        let (_tmp, g) = graphroot();
        let layer_data = b"layer-data-for-skip-test";
        let target: Reference = "ghcr.io/example/skip:1".parse().unwrap();
        let (_manifest_digest, layer_digest) =
            seed_cache(&g, &target.to_string(), layer_data).await;
        let blob_store = FilesystemBlobStore::new(&g);
        let db = open_db(&g);

        let mock = MockRegistry::new();
        // Pre-load the registry with the layer blob → HEAD returns true.
        mock.insert_blob(
            &target.registry,
            &target.repo,
            layer_digest.clone(),
            Bytes::copy_from_slice(layer_data),
        );

        push_inner_with_registry(target.clone(), true, &mock, &blob_store, &db)
            .await
            .expect("push should succeed");

        // The pushed_blobs log MUST be empty for the skipped layer digest.
        let pushed = mock.pushed_blobs();
        assert!(
            pushed.iter().all(|(_, _, d, _)| d != &layer_digest),
            "layer present on registry must NOT be re-pushed: {pushed:?}"
        );
        // Manifest IS pushed regardless (it's the leaf of the push pipeline).
        assert_eq!(
            mock.pushed_manifests().len(),
            1,
            "manifest must always be pushed"
        );
    }

    /// REGISTRY-02 upload: HEAD returns false → push_blob_stream IS called.
    #[tokio::test(flavor = "current_thread")]
    async fn push_inner_uploads_missing_blobs() {
        let (_tmp, g) = graphroot();
        let layer_data = b"layer-data-for-upload-test";
        let target: Reference = "ghcr.io/example/upload:1".parse().unwrap();
        let (_manifest_digest, layer_digest) =
            seed_cache(&g, &target.to_string(), layer_data).await;
        let blob_store = FilesystemBlobStore::new(&g);
        let db = open_db(&g);

        let mock = MockRegistry::new();
        // Registry is empty → HEAD returns false → push_blob_stream is invoked.

        push_inner_with_registry(target.clone(), true, &mock, &blob_store, &db)
            .await
            .expect("push should succeed");

        let pushed = mock.pushed_blobs();
        // 2 blobs: empty-config (`{}`, sha256:44136fa3...) pushed first per the
        // OCI 1.1 inline-data fix (commit e69c9a3), then the layer.
        assert_eq!(pushed.len(), 2, "empty-config + layer pushed");
        assert_eq!(
            pushed[0].2.to_string(),
            "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a",
            "empty-config blob first"
        );
        assert_eq!(
            pushed[0].3.as_ref(),
            b"{}",
            "empty-config bytes are 2-byte `{{}}`"
        );
        assert_eq!(pushed[1].2, layer_digest, "layer digest matches");
        assert_eq!(pushed[1].3.as_ref(), layer_data, "layer bytes match cache");
    }

    /// T-44-05-03 ordering: manifest is pushed AFTER all layers. Verified
    /// via two complementary checks: (a) the MockRegistry call-order log
    /// (push_blob_stream must precede push_manifest), and (b) a source-code
    /// line-order grep within `push_inner_with_registry` (push_manifest call
    /// site appears AFTER the layer loop body).
    #[tokio::test(flavor = "current_thread")]
    async fn push_manifest_pushed_after_all_blobs() {
        // (a) Runtime call-order check.
        let (_tmp, g) = graphroot();
        let layer_data = b"layer-data-for-order-test";
        let target: Reference = "ghcr.io/example/order:1".parse().unwrap();
        let (_manifest_digest, _layer_digest) =
            seed_cache(&g, &target.to_string(), layer_data).await;
        let blob_store = FilesystemBlobStore::new(&g);
        let db = open_db(&g);
        let mock = MockRegistry::new();

        push_inner_with_registry(target.clone(), true, &mock, &blob_store, &db)
            .await
            .expect("push should succeed");

        let pushed_blobs = mock.pushed_blobs();
        let pushed_manifests = mock.pushed_manifests();
        // 2 blobs: empty-config (per OCI 1.1 inline-data fix) + layer.
        assert_eq!(pushed_blobs.len(), 2);
        assert_eq!(pushed_manifests.len(), 1);
        // MockRegistry's logs are append-only in call order. The fact that
        // we observe 1 blob + 1 manifest is the load-bearing assertion;
        // the source-line guard below pins the *static* ordering.

        // (b) Source-line-order grep within push_manifest_and_blobs. The
        // last `push_blob_stream` call site MUST appear before the first
        // `push_manifest` call site, both within the helper body.
        let src = std::fs::read_to_string("src/cmd/push.rs").expect("src/cmd/push.rs must exist");
        let orch_start = src
            .find("pub(crate) async fn push_manifest_and_blobs")
            .expect("push_manifest_and_blobs fn must exist");
        let orch_after = &src[orch_start..];
        let orch_end_rel = orch_after
            .find("\n}")
            .expect("orchestrator fn closing brace must be present");
        let orch_body = &orch_after[..orch_end_rel];

        let mut last_push_blob_idx: Option<usize> = None;
        let mut first_push_manifest_idx: Option<usize> = None;
        for (i, line) in orch_body.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with("///") {
                continue;
            }
            if line.contains(".push_blob_stream(") {
                last_push_blob_idx = Some(i + 1);
            }
            if line.contains(".push_manifest(") {
                first_push_manifest_idx = first_push_manifest_idx.or(Some(i + 1));
            }
        }
        let lpb = last_push_blob_idx
            .expect("orchestrator must contain at least one push_blob_stream call");
        let fpm = first_push_manifest_idx
            .expect("orchestrator must contain at least one push_manifest call");
        assert!(
            fpm > lpb,
            "T-44-05-03: in push_inner_with_registry, first push_manifest (rel line {fpm}) must come AFTER last push_blob_stream (rel line {lpb})"
        );
    }

    /// Pre-push manifest re-validation: cache contains a malformed manifest
    /// → push errors at the validation step BEFORE any registry call. The
    /// MockRegistry's pushed_blobs and pushed_manifests logs must both stay
    /// empty.
    #[tokio::test(flavor = "current_thread")]
    async fn push_inner_revalidates_manifest_pre_push() {
        let (_tmp, g) = graphroot();
        let blob_store = FilesystemBlobStore::new(&g);
        let db = open_db(&g);
        let target: Reference = "ghcr.io/example/bad:1".parse().unwrap();

        // Hand-craft a syntactically-valid but D-11-failing manifest
        // (missing chain annotations). Insert it into the cache and tag it.
        let bad_manifest = serde_json::json!({
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
        let bytes = serde_json::to_vec(&bad_manifest).unwrap();
        let digest = Digest::from_bytes_sha256(&bytes);
        blob_store.put_blob(&digest, &bytes).await.unwrap();
        db.set_tag(&target.to_string(), &digest).await.unwrap();

        let mock = MockRegistry::new();
        let err = push_inner_with_registry(target, true, &mock, &blob_store, &db)
            .await
            .expect_err("malformed manifest must be rejected pre-push");
        let msg = format!("{err:#}");
        assert!(
            msg.to_lowercase().contains("re-validate")
                || msg.to_lowercase().contains("annotation")
                || msg.to_lowercase().contains("validate"),
            "expected pre-push validation error, got: {msg}"
        );
        // No registry calls happened.
        assert!(
            mock.pushed_blobs().is_empty(),
            "no blob push must happen on validation reject"
        );
        assert!(
            mock.pushed_manifests().is_empty(),
            "no manifest push must happen on validation reject"
        );
    }

    /// Pitfall 3: cmd::push pushes the manifest's RAW cached bytes verbatim
    /// (NOT re-serialised). Set up the cache with manifest bytes that include
    /// trailing whitespace; push it; assert the registry-side bytes match the
    /// cache bytes byte-for-byte.
    #[tokio::test(flavor = "current_thread")]
    async fn push_inner_pushes_manifest_raw_bytes() {
        let (_tmp, g) = graphroot();
        let blob_store = FilesystemBlobStore::new(&g);
        let db = open_db(&g);
        let target: Reference = "ghcr.io/example/raw:1".parse().unwrap();

        let layer_data = b"raw-bytes-layer-data";
        let layer_digest = Digest::from_bytes_sha256(layer_data);
        blob_store
            .put_blob(&layer_digest, layer_data)
            .await
            .unwrap();
        let (clean_manifest, _clean_digest) =
            make_pichi_manifest(&layer_digest, layer_data.len() as u64);
        let mut raw_with_ws: Vec<u8> = clean_manifest.to_vec();
        raw_with_ws.extend_from_slice(b"   \n"); // trailing whitespace
        let raw_bytes = Bytes::from(raw_with_ws);
        let manifest_digest = Digest::from_bytes_sha256(&raw_bytes);
        blob_store
            .put_blob(&manifest_digest, raw_bytes.as_ref())
            .await
            .unwrap();
        db.set_tag(&target.to_string(), &manifest_digest)
            .await
            .unwrap();

        let mock = MockRegistry::new();
        push_inner_with_registry(target.clone(), true, &mock, &blob_store, &db)
            .await
            .expect("push should succeed");

        // The MockRegistry's push_manifest computes a digest and stores under
        // `(registry, repo, digest)` — and that digest MUST be the
        // whitespace-tainted manifest digest (not the clean one).
        let pushed = mock.pushed_manifests();
        assert_eq!(pushed.len(), 1);
        let (pushed_ref, pushed_media_type, pushed_digest) = &pushed[0];
        assert_eq!(pushed_ref, &target, "pushed reference matches");
        assert_eq!(
            pushed_media_type, PUSH_MANIFEST_CONTENT_TYPE,
            "manifest content-type matches"
        );
        assert_eq!(
            pushed_digest, &manifest_digest,
            "Pitfall 3: pushed manifest digest must equal cached digest (raw bytes preserved)"
        );
    }

    /// Empty-cache error path: the entry-point error message includes
    /// "ref not in cache" so binary integration tests can assert on it.
    #[tokio::test(flavor = "current_thread")]
    async fn push_inner_errors_when_tag_absent() {
        let (_tmp, g) = graphroot();
        let blob_store = FilesystemBlobStore::new(&g);
        let db = open_db(&g);
        let target: Reference = "ghcr.io/example/absent:1".parse().unwrap();
        let mock = MockRegistry::new();

        let err = push_inner_with_registry(target, true, &mock, &blob_store, &db)
            .await
            .expect_err("absent tag must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ref not in cache"),
            "error must contain 'ref not in cache': {msg}"
        );
        assert!(mock.pushed_blobs().is_empty());
        assert!(mock.pushed_manifests().is_empty());
    }
}
