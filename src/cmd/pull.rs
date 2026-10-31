// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Fetch and validate manifests, then stream missing layers through devmap.
//! The formatter drives source writes, digest checks, and optional decompression.
//! Computed roots are retained for chain verification before publishing the tag.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use anyhow::{Context, Result, anyhow, ensure};
#[cfg(test)]
use conglobate::artifact::Manifest;
use conglobate::artifact::{Digest, Reference};

use crate::cmd::manifest_ext::{ManifestExt, ScuteRoots};

use super::cache::{ImageMetadata, ImageStore as _};
use super::layer::PreparedLayer;
use pichi_registry::Registry;
use pichi_storage::{BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};

use crate::cli::{PullArgs, PullPolicy};
use crate::config::Config;
use futures_util::stream::{self as futures_stream, StreamExt};

/// Max layers downloaded concurrently, matching podman's default of 3.
const PULL_CONCURRENCY: usize = 3;

/// Entry point for `pichi pull`.
pub async fn run(args: PullArgs, config: &Config) -> Result<()> {
    // Defense-in-depth: parse + canonicalise BEFORE any I/O (BL-02 / T-43-02).
    // Also strip informational `:tag` from Conglobate `tag@digest` pins so
    // `conglobate::artifact::Reference` keeps a clean repo (see canonicalize_reference).
    let canonical = crate::cmd::build::canonicalize_reference(&args.reference);
    let target_ref: Reference = canonical
        .parse()
        .with_context(|| format!("invalid reference: {}", args.reference))?;
    let policy = args.pull.unwrap_or(PullPolicy::Always); // D-05 default.
    let layout = config.resolve_layout()?;
    let blob_store = FilesystemBlobStore::new(&layout.graphroot);
    let tag_db = FilesystemTagDb::open(&layout.graphroot)
        .with_context(|| format!("opening tag db at {}", layout.graphroot.display()))?;

    // --pull=never short-circuits before any network I/O.
    if policy == PullPolicy::Never {
        ensure!(
            blob_store.cached(&target_ref, &tag_db).await?,
            "pull policy `never`: ref not in cache: {target_ref}"
        );
        return Ok(());
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
    let _guard = tag_db.lock_index().await?;
    let target_str = target.to_string();

    // --pull=missing: skip network if cached.
    if policy == PullPolicy::Missing && blob_store.cached(&target, tag_db).await? {
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
            if upstream == cached && blob_store.cached(&target, tag_db).await? {
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

    if let conglobate::artifact::ReferenceKind::Digest(expected) = &target.kind {
        ensure!(
            &digest_str == expected,
            "registry returned a different image digest"
        );
    }
    let root = ImageMetadata::try_from((digest_str, raw.to_vec()))?;
    let selected = if root.is_index() {
        let entry = root.selected_entry()?;
        let picked: Digest = entry["digest"]
            .as_str()
            .context("index entry has no digest")?
            .parse()?;
        let bytes = registry
            .pull_manifest_by_digest(&target.registry, &target.repo, &picked)
            .await
            .map_err(|e| anyhow!("pull_manifest_by_digest {picked}: {e}"))?;
        let selected = ImageMetadata::try_from((picked, bytes.to_vec()))?;
        selected.check_descriptor(entry)?;
        Some(selected)
    } else {
        None
    };
    let selected = selected.as_ref().unwrap_or(&root);
    let final_digest = &selected.digest;
    let final_bytes = &selected.bytes;

    // 3. Validate the pichi manifest BEFORE fetching any blob (Phase 42 D-11
    //    host-side validation).
    let manifest = selected
        .manifest()
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

    // Download missing layers concurrently. Cached scutes need a bounded-memory
    // root recomputation; freshly downloaded scutes return their formatter root.
    // Consume results in manifest order even when downloads finish out of order.
    let target_ref = &target;
    let manifest_ref = &manifest;
    let mut downloads = futures_stream::iter(&manifest.layers)
        .map(|layer| async move {
            let digest: Digest = layer.digest_str().parse()?;
            let root = if blob_store.blob_exists(&digest).await {
                manifest_ref.cached_scute_root(layer, blob_store).await?
            } else {
                PreparedLayer::fetch(registry, target_ref, manifest_ref, layer, blob_store)
                    .await
                    .with_context(|| format!("fetch layer {digest}"))?
            };
            Ok::<_, anyhow::Error>((layer, root))
        })
        .buffered(PULL_CONCURRENCY);
    let mut roots = ScuteRoots::default();
    while let Some(result) = downloads.next().await {
        let (layer, root) = result?;
        if let Some(root) = root {
            roots.add(layer, root)?;
        }
    }
    roots
        .verify(&manifest)
        .with_context(|| format!("verifying carapace root for {final_digest}"))?;

    // Publish the root only after every layer has been prepared. The index
    // lock covers shared-cache writes and this final tag update, so collection
    // cannot delete an in-progress image before it becomes reachable.
    if root.is_index() {
        blob_store.put_blob(final_digest, final_bytes).await?;
    }
    blob_store.put_blob(&root.digest, &root.bytes).await?;
    tag_db
        .set_tag_locked(&target_str, &root.digest)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::platform::PlatformExt as _;
    use bytes::Bytes;
    use conglobate::artifact::Layer;
    use conglobate::artifact::Platform;
    use conglobate::artifact::{
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
        let layer = Digest::Sha256([42; 32]);
        let (bytes, cached) = make_scute_manifest(&layer, 4096, &[0; 4096], false);
        blob_store.put_blob(&cached, &bytes).await.unwrap();
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
        // Valid JSON, valid artifactType, has a scute layer (so the chain
        // annotations are required) but carries none → from_reader_validated
        // rejects it.
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
            "layers": [{
                "mediaType": "application/vnd.pichi.scute.v1",
                "digest": "sha256:1111111111111111111111111111111111111111111111111111111111111111",
                "size": 1024,
                "annotations": { "dev.pichi.scute.verity.salt": "00" }
            }]
            // NOTE: no top-level annotations → missing chain annotations.
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
        let layer_digest =
            Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(dummy_layer_data).into());
        let (clean_manifest, _clean_digest) = make_scute_manifest(
            &layer_digest,
            dummy_layer_data.len() as u64,
            dummy_layer_data,
            false,
        );
        let mut raw_with_ws: Vec<u8> = clean_manifest.to_vec();
        raw_with_ws.extend_from_slice(b"   \n"); // trailing whitespace
        let raw_bytes = Bytes::from(raw_with_ws);
        let expected_digest =
            Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&raw_bytes).into());

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
            if line.contains(".set_tag_locked(") {
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
        let layer_digest =
            Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(layer_data).into());
        let (manifest_bytes, manifest_digest) =
            make_scute_manifest(&layer_digest, layer_data.len() as u64, layer_data, false);
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

    /// Pull preserves the registry index and only fetches the selected platform.
    #[tokio::test(flavor = "current_thread")]
    async fn pull_preserves_index_identity() {
        let (_tmp, g) = graphroot();
        let db = open_db(&g);
        let blob_store = FilesystemBlobStore::new(&g);
        let target: Reference = "ghcr.io/example/foo:bar".parse().unwrap();
        let mock = MockRegistry::new();

        // Build a pichi manifest first (the picked entry).
        let layer_data = b"index-walk-layer-bytes";
        let layer_digest =
            Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(layer_data).into());
        let (picked_manifest_bytes, picked_manifest_digest) =
            make_scute_manifest(&layer_digest, layer_data.len() as u64, layer_data, false);
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
                "platform": {"os": "pichi", "architecture": Platform::host_architecture()}
            }, {
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": format!("sha256:{}", "ab".repeat(32)),
                "size": 123,
                "artifactType": MEDIA_TYPE_PICHI_ARTIFACT_V1,
                "platform": {"os": "pichi", "architecture": "not-this-host"}
            }]
        });
        let index_bytes = Bytes::from(serde_json::to_vec_pretty(&index).unwrap());
        let index_digest =
            Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&index_bytes).into());
        // Insert the index under the tag.
        mock.insert_manifest("ghcr.io", "example/foo", "bar", index_bytes.clone());
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
        assert_eq!(
            blob_store.get_blob(&index_digest).await.unwrap(),
            index_bytes
        );
        let resolved = db.resolve_tag(&target.to_string()).await.unwrap();
        assert_eq!(resolved.as_ref(), Some(&index_digest));
        tokio::fs::remove_file(blob_store.blob_path(&picked_manifest_digest))
            .await
            .unwrap();
        pull_inner_with_registry(
            target.clone(),
            PullPolicy::Newer,
            true,
            &mock,
            &blob_store,
            &db,
        )
        .await
        .unwrap();
        assert!(blob_store.blob_exists(&picked_manifest_digest).await);
        mock.insert_manifest("ghcr.io", "example/foo", "bar", picked_manifest_bytes);
        let (_fresh, fresh_root) = graphroot();
        let fresh_store = FilesystemBlobStore::new(&fresh_root);
        let fresh_db = open_db(&fresh_root);
        let pin: Reference = format!("ghcr.io/example/foo@{index_digest}")
            .parse()
            .unwrap();
        pull_inner_with_registry(
            pin.clone(),
            PullPolicy::Missing,
            true,
            &mock,
            &fresh_store,
            &fresh_db,
        )
        .await
        .unwrap();
        assert_eq!(
            fresh_db.resolve_tag(&pin.to_string()).await.unwrap(),
            Some(index_digest.clone())
        );
        assert_eq!(
            fresh_store.get_blob(&index_digest).await.unwrap(),
            index_bytes
        );
    }

    // ====================================================================
    // Phase 46 Plan 02 Task 1 — sidecar derivation tests (CACHE-01, CACHE-02)
    // ====================================================================

    use conglobate::artifact::PmiDescriptor;
    use pichi_storage::BlobSidecarExt;

    /// Plan 02 helper: build a 1-Scute or 1-ScuteZstd manifest with a
    /// 32-zero salt. Returns (manifest bytes, manifest digest).
    fn make_scute_manifest(
        scute_digest: &Digest,
        scute_size: u64,
        data: &[u8],
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
        annotations.insert("dev.pichi.carapace.verity.version".into(), "1".into());
        annotations.insert("dev.pichi.carapace.verity.hash-type".into(), "1".into());
        annotations.insert(
            "dev.pichi.carapace.verity.hash".into(),
            hex::encode(pull_side_verity_params().compute(data).unwrap().root_hash),
        );
        let scute_desc = ScuteDescriptor {
            digest: scute_digest.to_string(),
            size: scute_size,
            annotations: ScuteAnnotations {
                data_blocks: (data.len() as u64).div_ceil(4096).try_into().unwrap(),
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
        let digest = Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&bytes).into());
        (bytes, digest)
    }

    /// Plan 02 helper: build the locked Phase 42 D-06 verity params for a
    /// scute with the SAME salt the pull pipeline uses — the full salt from
    /// the `dev.pichi.scute.verity.salt` annotation, verbatim (here the base
    /// scute's 32 zero bytes from make_scute_manifest's hex::encode([0u8; 32])).
    fn pull_side_verity_params() -> crate::cmd::verity::VerityParams {
        crate::cmd::verity::VerityParams {
            data_block_size: 4096,
            hash_block_size: 4096,
            // The per-scute salt annotation, used verbatim (no extra prefix).
            salt: vec![0u8; 32],
            // UUID is metadata only and does not affect the root.
            uuid: [0u8; 16],
        }
    }

    #[tokio::test]
    async fn pull_prepares_both_encodings_for_run_and_build() {
        for compressed in [false, true] {
            for length in [4097, 8192] {
                let (_tmp, g) = graphroot();
                let db = open_db(&g);
                let store = FilesystemBlobStore::new(&g);
                let target: Reference = "ghcr.io/example/foo:bar".parse().unwrap();
                let registry = MockRegistry::new();
                let data = vec![0x5a; length];
                let wire = if compressed {
                    ruzstd::encoding::compress_to_vec(
                        data.as_slice(),
                        ruzstd::encoding::CompressionLevel::Fastest,
                    )
                } else {
                    data.clone()
                };
                let digest = Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&wire).into());
                let (bytes, manifest_digest) =
                    make_scute_manifest(&digest, wire.len() as u64, &data, compressed);
                let manifest: Manifest = serde_json::from_slice(&bytes).unwrap();
                registry.insert_manifest("ghcr.io", "example/foo", "bar", bytes.clone());
                registry.insert_blob(
                    "ghcr.io",
                    "example/foo",
                    digest.clone(),
                    Bytes::from(wire.clone()),
                );
                let source = store.blob_path(&digest);
                let files =
                    crate::cmd::layer::ScuteFiles::new(&manifest, &manifest.layers[0], &store)
                        .unwrap();
                let expected_data = if compressed || length != 8192 {
                    source.deflated_path(8192)
                } else {
                    source.clone()
                };
                let mut padded = data;
                padded.resize(8192, 0);
                for _ in 0..2 {
                    pull_inner_with_registry(
                        target.clone(),
                        PullPolicy::Always,
                        true,
                        &registry,
                        &store,
                        &db,
                    )
                    .await
                    .unwrap();
                    assert_eq!(store.get_blob(&manifest_digest).await.unwrap(), bytes);
                    assert_eq!(std::fs::read(&source).unwrap(), wire);
                    assert_eq!(std::fs::read(&expected_data).unwrap(), padded);
                    let tree = std::fs::read(files.verity.clone()).unwrap();
                    assert!(!tree.is_empty());
                    let spec = crate::cmd::run::build_gpt_spec(
                        &manifest,
                        &manifest.scute_layers(),
                        &store,
                    )
                    .await
                    .unwrap();
                    let gpt: dillo_config::GptSpec =
                        serde_keyvalue::from_key_values(&spec).unwrap();
                    assert_eq!(gpt.partitions[0].path, expected_data);
                    assert_eq!(gpt.partitions[1].path, files.verity.clone());
                    assert_eq!(std::fs::read(files.verity.clone()).unwrap(), tree);
                }
            }
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
        let layer_digest =
            Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&compressed).into());
        let (manifest_bytes, _manifest_digest) = make_scute_manifest(
            &layer_digest,
            compressed.len() as u64,
            &decompressed,
            /* is_zstd= */ true,
        );
        let manifest: Manifest = serde_json::from_slice(&manifest_bytes).unwrap();
        let files =
            crate::cmd::layer::ScuteFiles::new(&manifest, manifest.scute_layers()[0], &blob_store)
                .unwrap();
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
            files.data.exists(),
            ".deflated sidecar missing: {}",
            files.data.display()
        );
        assert!(
            files.verity.exists(),
            ".verity sidecar missing: {}",
            files.verity.display()
        );
        // .deflated bytes byte-equal the original decompressed payload.
        let deflated_disk = std::fs::read(&files.data).unwrap();
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
        let layer_digest =
            Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&layer_data).into());
        let (manifest_bytes, _manifest_digest) = make_scute_manifest(
            &layer_digest,
            layer_data.len() as u64,
            &layer_data,
            /* is_zstd= */ false,
        );
        let manifest: Manifest = serde_json::from_slice(&manifest_bytes).unwrap();
        let files =
            crate::cmd::layer::ScuteFiles::new(&manifest, manifest.scute_layers()[0], &blob_store)
                .unwrap();
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
        assert!(files.verity.exists(), ".verity sidecar missing");
        assert!(
            !blob_path.deflated_path(8192).exists(),
            "raw scute must NOT have a .deflated sidecar (D-04): {}",
            blob_path.deflated_path(8192).display()
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
        let scute_digest =
            Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&scute_data).into());
        let params = pull_side_verity_params();
        let expected_root = params
            .compute(&scute_data)
            .expect("verity compute")
            .root_hash;

        // Synthetic PMI bytes: zero pad + a `corium=<hex>` token.
        let mut pmi_bytes = vec![0u8; 4096];
        let pattern = format!("corium={}", hex::encode(expected_root));
        pmi_bytes[100..100 + pattern.len()].copy_from_slice(pattern.as_bytes());
        let pmi_digest = Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&pmi_bytes).into());

        let (bytes, _) =
            make_scute_manifest(&scute_digest, scute_data.len() as u64, &scute_data, false);
        let mut manifest: Manifest = serde_json::from_slice(&bytes).unwrap();
        manifest.layers.push(Layer::Pmi(PmiDescriptor {
            digest: pmi_digest.to_string(),
            size: pmi_bytes.len() as u64,
        }));
        let manifest_bytes = Bytes::from(serde_json::to_vec(&manifest).unwrap());
        let files =
            crate::cmd::layer::ScuteFiles::new(&manifest, manifest.scute_layers()[0], &blob_store)
                .unwrap();
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
            !pmi_blob.prepared_dir().exists(),
            "PMI must not have prepared files"
        );

        // Scute still has its .verity (raw → no .deflated).
        let scute_blob = blob_store.blob_path(&scute_digest);
        assert!(files.verity.exists(), "scute .verity missing");
        assert!(
            !scute_blob.deflated_path(4096).exists(),
            "raw scute must NOT have .deflated"
        );
    }

    /// Plan 02 Task 1 (d): pull a raw scute, then assert the on-disk
    /// `<src>.verity` bytes equal `crate::cmd::verity::compute(layer_bytes,
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
        let layer_digest =
            Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&layer_data).into());
        let (manifest_bytes, _manifest_digest) =
            make_scute_manifest(&layer_digest, layer_data.len() as u64, &layer_data, false);
        let manifest: Manifest = serde_json::from_slice(&manifest_bytes).unwrap();
        let files =
            crate::cmd::layer::ScuteFiles::new(&manifest, manifest.scute_layers()[0], &blob_store)
                .unwrap();
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

        let on_disk_verity = std::fs::read(&files.verity).unwrap();

        let params = pull_side_verity_params();
        let expected = params.compute(&layer_data).expect("verity::compute").blob;
        assert_eq!(
            on_disk_verity, expected,
            "<src>.verity bytes must equal verity::compute(layer_bytes, params).blob"
        );
    }
    #[tokio::test(flavor = "current_thread")]
    async fn pull_requires_manifest_chain_order_for_downloads_and_cache_hits() {
        for order in [[0, 1, 2], [2, 1, 0], [0, 2, 1]] {
            let (_tmp, graph) = graphroot();
            let db = open_db(&graph);
            let store = FilesystemBlobStore::new(&graph);
            let target: Reference = "ghcr.io/example/foo:bar".parse().unwrap();
            let registry = MockRegistry::new();
            let mut parent = [0; 32];
            let mut layers = Vec::new();
            let mut manifest = None;
            // Unequal layers exercise concurrent preparation. Completion order
            // is not the order in which the manifest's links must be checked.
            for (index, size) in [256 * 1024, 4096, 8192].into_iter().enumerate() {
                let data = vec![index as u8 + 1; size];
                let compressed = index == 1;
                let wire = if compressed {
                    ruzstd::encoding::compress_to_vec(
                        data.as_slice(),
                        ruzstd::encoding::CompressionLevel::Fastest,
                    )
                } else {
                    data.clone()
                };
                let digest = Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&wire).into());
                let (bytes, _) = make_scute_manifest(&digest, wire.len() as u64, &data, compressed);
                let mut next: Manifest = serde_json::from_slice(&bytes).unwrap();
                let (Layer::Scute(scute) | Layer::ScuteZstd(scute)) = &mut next.layers[0] else {
                    unreachable!()
                };
                scute.annotations.salt = hex::encode(parent);
                let mut parameters = pull_side_verity_params();
                parameters.salt = parent.to_vec();
                parent = parameters.compute(&data).unwrap().root_hash;
                layers.push(next.layers[0].clone());
                manifest = Some(next);
                registry.insert_blob("ghcr.io", "example/foo", digest, Bytes::from(wire));
            }
            let mut manifest = manifest.unwrap();
            manifest
                .annotations
                .insert("dev.pichi.carapace.verity.hash".into(), hex::encode(parent));
            manifest.layers = order
                .into_iter()
                .map(|index| layers[index].clone())
                .collect();
            registry.insert_manifest(
                "ghcr.io",
                "example/foo",
                "bar",
                Bytes::from(serde_json::to_vec(&manifest).unwrap()),
            );
            for _ in 0..2 {
                let result = pull_inner_with_registry(
                    target.clone(),
                    PullPolicy::Always,
                    true,
                    &registry,
                    &store,
                    &db,
                )
                .await;
                let valid = order == [0, 1, 2];
                assert_eq!(result.is_ok(), valid, "order={order:?}: {result:?}");
                assert_eq!(
                    db.resolve_tag(&target.to_string()).await.unwrap().is_some(),
                    valid
                );
                if valid {
                    assert_eq!(manifest.carapace_top_root(&store).await.unwrap(), parent);
                }
            }
        }
    }

    #[tokio::test]
    async fn cached_blob_prepares_each_salt_without_refetching() {
        for compressed in [false, true] {
            let (_tmp, graphroot) = graphroot();
            let db = open_db(&graphroot);
            let store = FilesystemBlobStore::new(&graphroot);
            let data = vec![0x5a; 8192];
            let wire = if compressed {
                ruzstd::encoding::compress_to_vec(
                    data.as_slice(),
                    ruzstd::encoding::CompressionLevel::Fastest,
                )
            } else {
                data.clone()
            };
            let digest = Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&wire).into());
            let (bytes, _) = make_scute_manifest(&digest, wire.len() as u64, &data, compressed);
            let mut manifest: Manifest = serde_json::from_slice(&bytes).unwrap();
            let mut trees = Vec::new();
            for suffix in [0xaa, 0xbb] {
                let mut params = pull_side_verity_params();
                params.salt.push(suffix);
                let (Layer::Scute(scute) | Layer::ScuteZstd(scute)) = &mut manifest.layers[0]
                else {
                    unreachable!()
                };
                scute.annotations.salt = hex::encode(&params.salt);
                manifest.annotations.insert(
                    "dev.pichi.carapace.verity.hash".into(),
                    hex::encode(params.compute(&data).unwrap().root_hash),
                );
                let registry = MockRegistry::new();
                registry.insert_manifest(
                    "ghcr.io",
                    "example/foo",
                    "bar",
                    Bytes::from(serde_json::to_vec(&manifest).unwrap()),
                );
                if trees.is_empty() {
                    registry.insert_blob(
                        "ghcr.io",
                        "example/foo",
                        digest.clone(),
                        Bytes::from(wire.clone()),
                    );
                }
                pull_inner_with_registry(
                    "ghcr.io/example/foo:bar".parse().unwrap(),
                    PullPolicy::Always,
                    true,
                    &registry,
                    &store,
                    &db,
                )
                .await
                .unwrap();
                let files =
                    crate::cmd::layer::ScuteFiles::new(&manifest, &manifest.layers[0], &store)
                        .unwrap();
                trees.push((files.verity, params.compute(&data).unwrap().blob));
            }
            assert_ne!(trees[0].0, trees[1].0);
            for (path, expected) in trees {
                assert_eq!(std::fs::read(path).unwrap(), expected);
            }
            let files = store.blob_path(&digest).prepared_files().unwrap();
            assert_eq!(files.len(), if compressed { 3 } else { 2 });
        }
    }

    /// Cached and uncached input must reconstruct the declared tip with a
    /// valid block count before replacing an existing tag.
    #[tokio::test(flavor = "current_thread")]
    async fn pull_requires_tip_commitment_and_block_count() {
        for declared in [None, Some(0u64), Some(2)] {
            for cached in [false, true] {
                for tip in ["correct", "missing", "zero", "malformed", "wrong"] {
                    let (_tmp, g) = graphroot();
                    let db = open_db(&g);
                    let store = FilesystemBlobStore::new(&g);
                    let target: Reference = "ghcr.io/example/foo:bar".parse().unwrap();
                    let registry = MockRegistry::new();
                    let data = vec![0x5a; 4097];
                    let wire = ruzstd::encoding::compress_to_vec(
                        data.as_slice(),
                        ruzstd::encoding::CompressionLevel::Fastest,
                    );
                    let digest =
                        Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&wire).into());
                    let (bytes, _) = make_scute_manifest(&digest, wire.len() as u64, &data, true);
                    let mut manifest: Manifest = serde_json::from_slice(&bytes).unwrap();
                    if cached {
                        store.put_blob(&digest, &wire).await.unwrap();
                        PreparedLayer::prepare_cached(&manifest, &manifest.layers[0], &store)
                            .await
                            .unwrap();
                    }
                    let key = "dev.pichi.carapace.verity.hash";
                    match tip {
                        "missing" => {
                            manifest.annotations.remove(key);
                        }
                        "zero" => {
                            manifest
                                .annotations
                                .insert(key.into(), hex::encode([0; 32]));
                        }
                        "malformed" => {
                            manifest.annotations.insert(key.into(), "not a root".into());
                        }
                        "wrong" => {
                            manifest
                                .annotations
                                .insert(key.into(), hex::encode([42; 32]));
                        }
                        _ => {}
                    }
                    let mut manifest = serde_json::to_value(manifest).unwrap();
                    let annotations = manifest["layers"][0]["annotations"]
                        .as_object_mut()
                        .unwrap();
                    if let Some(blocks) = declared {
                        annotations.insert(
                            "dev.pichi.scute.verity.data-blocks".into(),
                            blocks.to_string().into(),
                        );
                    } else {
                        annotations.remove("dev.pichi.scute.verity.data-blocks");
                    }
                    let bytes = Bytes::from(serde_json::to_vec(&manifest).unwrap());
                    let old = Digest::Sha256([0x77; 32]);
                    db.set_tag(&target.to_string(), &old).await.unwrap();
                    let expected =
                        Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&bytes).into());
                    registry.insert_manifest("ghcr.io", "example/foo", "bar", bytes);
                    if !cached {
                        registry.insert_blob("ghcr.io", "example/foo", digest, Bytes::from(wire));
                    }
                    let result = pull_inner_with_registry(
                        target.clone(),
                        PullPolicy::Always,
                        true,
                        &registry,
                        &store,
                        &db,
                    )
                    .await;
                    let valid = declared == Some(2) && tip == "correct";
                    assert_eq!(
                        result.is_ok(),
                        valid,
                        "tip={tip}, declared={declared:?}, cached={cached}: {result:?}"
                    );
                    assert_eq!(
                        db.resolve_tag(&target.to_string()).await.unwrap(),
                        Some(if valid { expected } else { old }),
                        "tip={tip}, declared={declared:?}, cached={cached}: unexpected tag state"
                    );
                }
            }
        }
    }
}
