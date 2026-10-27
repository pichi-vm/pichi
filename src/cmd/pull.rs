// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Fetch and validate manifests, then stream missing layers through devmap.
//! The formatter drives source writes, digest checks, and optional decompression.
//! Computed roots are retained for chain verification before publishing the tag.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;

use pichi_artifact::{Digest, MEDIA_TYPE_PICHI_ARTIFACT_V1, Manifest, Reference};

use crate::cmd::manifest_ext::{ManifestExt, ScuteRoots};

use super::layer::PreparedLayer;
use pichi_registry::{OCI_IMAGE_INDEX_MEDIA_TYPE, Registry, pick_pichi_entry_from_index};
use pichi_storage::{BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};

use crate::cli::{PullArgs, PullPolicy};
use crate::config::Config;
use futures_util::stream::{self as futures_stream, StreamExt};

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
/// Max layers downloaded concurrently, matching podman's default of 3.
const PULL_CONCURRENCY: usize = 3;

/// Entry point for `pichi pull`.
pub async fn run(args: PullArgs, config: &Config) -> Result<()> {
    // Defense-in-depth: parse + canonicalise BEFORE any I/O (BL-02 / T-43-02).
    // Also strip informational `:tag` from Conglobate `tag@digest` pins so
    // `pichi-artifact::Reference` keeps a clean repo (see canonicalize_reference).
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

    // Download missing layers concurrently. Cached scutes need a bounded-memory
    // root recomputation; freshly downloaded scutes return their formatter root.
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
        .buffer_unordered(PULL_CONCURRENCY);
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

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use pichi_artifact::Layer;
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
    /// dm-verity root of `cow` under the 32-zero salt + locked 4 KiB blocks —
    /// the value `pull` recomputes and checks against the manifest annotation.
    fn zero_salt_root_hex(cow: &[u8]) -> String {
        let out = crate::cmd::verity::VerityParams {
            data_block_size: 4096,
            hash_block_size: 4096,
            salt: vec![0u8; 32],
            uuid: [0u8; 16],
        }
        .compute(cow)
        .unwrap();
        hex::encode(out.root_hash)
    }

    fn make_pichi_manifest(
        scute_digest: &Digest,
        scute_size: u64,
        root_hex: &str,
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
            root_hex.to_string(),
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
                    data_blocks: None,
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
        let layer_digest = Digest::from_bytes_sha256(dummy_layer_data);
        let root_hex = zero_salt_root_hex(dummy_layer_data);
        let (clean_manifest, _clean_digest) =
            make_pichi_manifest(&layer_digest, dummy_layer_data.len() as u64, &root_hex);
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
        let (manifest_bytes, manifest_digest) = make_pichi_manifest(
            &layer_digest,
            layer_data.len() as u64,
            &zero_salt_root_hex(layer_data),
        );
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
        let (picked_manifest_bytes, picked_manifest_digest) = make_pichi_manifest(
            &layer_digest,
            layer_data.len() as u64,
            &zero_salt_root_hex(layer_data),
        );
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
    use pichi_storage::BlobSidecarExt;

    /// Plan 02 helper: build a 1-Scute or 1-ScuteZstd manifest with a
    /// 32-zero salt. Returns (manifest bytes, manifest digest).
    fn make_scute_manifest(
        scute_digest: &Digest,
        scute_size: u64,
        is_zstd: bool,
        root_hex: &str,
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
            root_hex.to_string(),
        );
        let scute_desc = ScuteDescriptor {
            digest: scute_digest.to_string(),
            size: scute_size,
            annotations: ScuteAnnotations {
                data_blocks: None,
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
    /// test can poke a `corium=<hex>` token in there.
    fn make_scute_plus_pmi_manifest(
        scute_digest: &Digest,
        scute_size: u64,
        is_zstd_scute: bool,
        pmi_digest: &Digest,
        pmi_size: u64,
        root_hex: &str,
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
            root_hex.to_string(),
        );
        let scute_desc = ScuteDescriptor {
            digest: scute_digest.to_string(),
            size: scute_size,
            annotations: ScuteAnnotations {
                data_blocks: None,
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
            &zero_salt_root_hex(&decompressed),
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
            blob_path.deflated_path().exists(),
            ".deflated sidecar missing: {}",
            blob_path.deflated_path().display()
        );
        assert!(
            blob_path.verity_path().exists(),
            ".verity sidecar missing: {}",
            blob_path.verity_path().display()
        );
        // .deflated bytes byte-equal the original decompressed payload.
        let deflated_disk = std::fs::read(blob_path.deflated_path()).unwrap();
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
            &zero_salt_root_hex(&layer_data),
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
        assert!(blob_path.verity_path().exists(), ".verity sidecar missing");
        assert!(
            !blob_path.deflated_path().exists(),
            "raw scute must NOT have a .deflated sidecar (D-04): {}",
            blob_path.deflated_path().display()
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
        let expected_root = params
            .compute(&scute_data)
            .expect("verity compute")
            .root_hash;

        // Synthetic PMI bytes: zero pad + a `corium=<hex>` token.
        let mut pmi_bytes = vec![0u8; 4096];
        let pattern = format!("corium={}", hex::encode(expected_root));
        pmi_bytes[100..100 + pattern.len()].copy_from_slice(pattern.as_bytes());
        let pmi_digest = Digest::from_bytes_sha256(&pmi_bytes);

        let (manifest_bytes, _manifest_digest) = make_scute_plus_pmi_manifest(
            &scute_digest,
            scute_data.len() as u64,
            /* is_zstd_scute= */ false,
            &pmi_digest,
            pmi_bytes.len() as u64,
            &hex::encode(expected_root),
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
            !pmi_blob.deflated_path().exists(),
            "PMI must NOT have .deflated"
        );
        assert!(
            !pmi_blob.verity_path().exists(),
            "PMI must NOT have .verity (Pitfall 6)"
        );

        // Scute still has its .verity (raw → no .deflated).
        let scute_blob = blob_store.blob_path(&scute_digest);
        assert!(scute_blob.verity_path().exists(), "scute .verity missing");
        assert!(
            !scute_blob.deflated_path().exists(),
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
        let layer_digest = Digest::from_bytes_sha256(&layer_data);
        let (manifest_bytes, _manifest_digest) = make_scute_manifest(
            &layer_digest,
            layer_data.len() as u64,
            false,
            &zero_salt_root_hex(&layer_data),
        );
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
        let on_disk_verity = std::fs::read(blob_path.verity_path()).unwrap();

        let params = pull_side_verity_params();
        let expected = params.compute(&layer_data).expect("verity::compute").blob;
        assert_eq!(
            on_disk_verity, expected,
            "<src>.verity bytes must equal verity::compute(layer_bytes, params).blob"
        );
    }
    #[tokio::test(flavor = "current_thread")]
    async fn pull_checks_compressed_roots_before_tagging_including_cached_layers() {
        for declared in [false, true] {
            for cached in [false, true] {
                for valid in [false, true] {
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
                    let digest = Digest::from_bytes_sha256(&wire);
                    let root = if valid {
                        zero_salt_root_hex(&data)
                    } else {
                        hex::encode([0; 32])
                    };
                    let (bytes, _) = make_scute_manifest(&digest, wire.len() as u64, true, &root);
                    let mut manifest: Manifest = serde_json::from_slice(&bytes).unwrap();
                    if declared {
                        let Layer::ScuteZstd(scute) = &mut manifest.layers[0] else {
                            unreachable!()
                        };
                        scute.annotations.data_blocks = Some((data.len() as u64).div_ceil(4096));
                    }
                    let bytes = Bytes::from(serde_json::to_vec(&manifest).unwrap());
                    registry.insert_manifest("ghcr.io", "example/foo", "bar", bytes);
                    if cached {
                        store.put_blob(&digest, &wire).await.unwrap();
                        tokio::fs::write(store.blob_path(&digest).deflated_path(), &data)
                            .await
                            .unwrap();
                    } else {
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
                    assert_eq!(
                        result.is_ok(),
                        valid,
                        "declared={declared}, cached={cached}: {result:?}"
                    );
                    assert_eq!(
                        db.resolve_tag(&target.to_string()).await.unwrap().is_some(),
                        valid
                    );
                }
            }
        }
    }
}
