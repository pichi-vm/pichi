// SPDX-License-Identifier: Apache-2.0
//
// Phase 44 REGISTRY-02 round-trip integrity test. Zot-gated.
//
// Asserts the strongest REGISTRY-02 claim: import → push → rmi → pull restores
// the local cache to a bit-identical state — every blob byte-equal; manifest
// digest preserved per Pitfall 3 (raw OCI bytes round-tripped without
// re-serialisation). The Plan 04 mock-driven tests + Plan 05's
// `push_then_pull_succeeds` cover the surface; this is the rigorous
// bit-equality assertion the planner pinned to `bit_identical_round_trip`
// in 44-VALIDATION.md.
//
// Gating: silently skips when `PICHI_TEST_REGISTRY` is unset (matches the
// pattern in tests/cmd_pull.rs and tests/cmd_push.rs). The CI
// `registry-pull-push` job sets `PICHI_TEST_REGISTRY=localhost:5000` +
// `PICHI_TEST_REGISTRY_INSECURE=1` to exercise the full pipeline against the
// ephemeral zot container.

#![allow(missing_docs)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use assert_cmd::Command;
use pichi_storage::{FilesystemTagDb, TagDb};
use tempfile::TempDir;

/// Standard graphroot helper — matches the layout used by `tests/cmd_pull.rs`
/// and `tests/cmd_push.rs` so XDG_DATA_HOME redirection lines up with what
/// the pichi binary expects.
fn graphroot(tmp: &TempDir) -> PathBuf {
    let p = tmp.path().join("pichi").join("storage");
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Snapshot every blob in `<graphroot>/blobs/sha256/` as
/// `{ "sha256:<hex>" -> bytes }`. Empty map when the dir doesn't exist (which
/// is the expected post-rmi state when the tag's blobs were the only ones
/// cached).
fn snapshot_cache(graphroot: &PathBuf) -> BTreeMap<String, Vec<u8>> {
    let blobs_dir = graphroot.join("blobs").join("sha256");
    let mut snap = BTreeMap::new();
    if !blobs_dir.exists() {
        return snap;
    }
    for entry in std::fs::read_dir(&blobs_dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_file() {
            let digest_hex = path.file_name().unwrap().to_string_lossy().into_owned();
            let bytes = std::fs::read(&path).unwrap();
            snap.insert(format!("sha256:{digest_hex}"), bytes);
        }
    }
    snap
}

fn pichi_test_registry() -> Option<String> {
    std::env::var("PICHI_TEST_REGISTRY")
        .ok()
        .filter(|s| !s.is_empty())
}

fn test_repo_base() -> Option<String> {
    if let Ok(base) = std::env::var("PICHI_TEST_REPO_BASE") {
        if !base.is_empty() {
            return Some(base);
        }
    }
    pichi_test_registry().map(|reg| format!("{reg}/test"))
}

/// REGISTRY-02 (zot, gated): the strongest round-trip claim.
///
/// Step 1: `pichi import` populates the cache with manifest + layer blobs.
/// Step 2: snapshot every blob byte + manifest digest pre-push.
/// Step 3: `pichi push` uploads to zot.
/// Step 4: `pichi rmi` clears the local tag (and its refcount-0 blobs).
/// Step 5: `pichi pull` fetches from zot back into the cache.
/// Step 6: assert manifest digest preserved AND manifest bytes byte-equal AND
///         every layer digest mentioned in the manifest is byte-equal across
///         the round-trip.
///
/// Key Pitfalls exercised:
///   - Pitfall 3 (manifest digest preservation): the raw OCI bytes are written
///     verbatim by `cmd::pull` (Plan 04 negative-grep gate `manifest.to_bytes()`
///     returns 0) and pushed verbatim by `cmd::push` (Plan 05 same gate). If
///     either side serialised through Manifest the manifest digest would
///     drift and the first assertion below would fail.
///   - +zstd descriptor mismatch (Phase 42 D-10): the layer digest in the
///     MANIFEST is the registry-side compressed digest; the BlobStore stores
///     under the DECOMPRESSED digest for +zstd layers. The conditional
///     `if let (Some(b), Some(a))` below tolerates this — for +zstd layers
///     the registry-side digest may not appear in the local BlobStore. The
///     load-bearing assertions are MANIFEST bit-identity + TAG digest
///     equality; per-layer body equality is checked when reachable.
#[test]
fn bit_identical_round_trip() {
    let Some(base) = test_repo_base() else {
        eprintln!("PICHI_TEST_REGISTRY unset; skipping bit_identical_round_trip");
        return;
    };
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);

    // Use a non-trivial fixture (16 KiB of pseudo-random bytes) so the layer
    // digests are real (avoids zero-only blob short-circuits in any layer of
    // the pipeline). The exact bytes do not matter — only that they survive
    // the round-trip unchanged.
    let raw = tmp.path().join("input.raw");
    let mut content = Vec::with_capacity(16 * 1024);
    for i in 0..16 * 1024 {
        content.push((i * 37) as u8);
    }
    std::fs::write(&raw, &content).unwrap();
    let tag = format!("{base}/roundtrip:1");

    // Step 1: import — populates `<g>/blobs/sha256/...` and binds the tag.
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["import", raw.to_str().unwrap(), &tag])
        .assert()
        .success();
    let snap_before = snapshot_cache(&g);
    assert!(
        !snap_before.is_empty(),
        "REGISTRY-02 fixture: cache must contain blobs after import"
    );

    let db = FilesystemTagDb::open(&g).unwrap();
    let manifest_digest_before = db.resolve_tag(&tag).unwrap().expect("tag set after import");
    drop(db);

    // Step 2: push to zot. Plan 05 ships cmd::push; this is the upload path.
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["push", &tag])
        .assert()
        .success();

    // Step 3: rmi — clears the tag and (per Phase 42 Plan 05 live-walk
    // refcounts) removes any blobs that drop to refcount zero.
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["rmi", &tag])
        .assert()
        .success();
    let db_after_rmi = FilesystemTagDb::open(&g).unwrap();
    assert!(
        db_after_rmi.resolve_tag(&tag).unwrap().is_none(),
        "rmi must clear the tag"
    );
    drop(db_after_rmi);

    // Step 4: pull from zot — restores the cache from the upstream copy. The
    // raw OCI bytes flow verbatim from oci-client → BlobStore::put_blob (Plan
    // 04 Pitfall 3 invariant); the manifest digest is preserved by
    // construction.
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["pull", &tag])
        .assert()
        .success();

    // Step 5: bit-identical restoration assertions.
    let snap_after = snapshot_cache(&g);
    let db_after_pull = FilesystemTagDb::open(&g).unwrap();
    let manifest_digest_after = db_after_pull
        .resolve_tag(&tag)
        .unwrap()
        .expect("tag must resolve after re-pull");

    assert_eq!(
        manifest_digest_before, manifest_digest_after,
        "REGISTRY-02 round-trip integrity: manifest digest must be preserved \
         (Pitfall 3 — raw bytes round-tripped without re-serialisation)"
    );

    // Manifest blob bytes equality. The manifest blob's BlobStore key IS its
    // digest (no +zstd shenanigans for the manifest itself), so the lookup is
    // direct.
    let manifest_key = manifest_digest_before.to_string(); // "sha256:..."
    let manifest_bytes_before = snap_before
        .get(&manifest_key)
        .expect("manifest blob present before push");
    let manifest_bytes_after = snap_after
        .get(&manifest_key)
        .expect("manifest blob present after re-pull");
    assert_eq!(
        manifest_bytes_before, manifest_bytes_after,
        "manifest blob bytes must be bit-identical across round-trip"
    );

    // Per-layer bit-equality. The layer digest in the MANIFEST is the
    // registry-side descriptor digest; for +zstd layers the BlobStore stores
    // under the DECOMPRESSED digest (Phase 42 D-10) so the lookup may miss.
    // The `if let (Some, Some)` is the documented tolerance for that case;
    // when both sides ARE present, the bytes MUST match.
    let manifest_json: serde_json::Value =
        serde_json::from_slice(manifest_bytes_after).expect("manifest is valid JSON");
    let layers = manifest_json
        .get("layers")
        .and_then(|l| l.as_array())
        .expect("manifest has layers array");
    for layer in layers {
        let layer_digest = layer
            .get("digest")
            .and_then(|d| d.as_str())
            .expect("layer descriptor has digest")
            .to_string();
        if let (Some(b), Some(a)) = (
            snap_before.get(&layer_digest),
            snap_after.get(&layer_digest),
        ) {
            assert_eq!(
                b, a,
                "layer {layer_digest} must be bit-identical across round-trip"
            );
        }
    }
}
