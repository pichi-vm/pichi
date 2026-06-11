// SPDX-License-Identifier: Apache-2.0
//
// Phase 44 REGISTRY-02 integration tests for `pichi push`. Mirrors the
// gating model used by tests/cmd_pull.rs:
//   1. Binary-driven (always run): exercises the binary surface paths that
//      do not require a registry. The empty-cache error is the only such
//      observable from outside the process.
//   2. Zot-backed (gated on PICHI_TEST_REGISTRY): full network round-trips.
//      Skipped silently when PICHI_TEST_REGISTRY is unset; CI sets it to
//      "localhost:5000" via the registry-pull-push job.
//
// The mock-backed unit-shaped tests for cmd::push (skip-present, upload-
// missing, manifest-after-blobs ordering, raw-bytes Pitfall 3 guard, pre-
// push re-validation) live in src/cmd/push.rs::tests because they need to
// drive `push_inner_with_registry` directly with a `MockRegistry` — Approach
// C from the planner's note (cleaner than exposing a hidden test-only env
// var on the binary or creating an ad-hoc lib.rs re-export just for tests).
//
// Plan 06's tests/cmd_pull_push_roundtrip.rs runs the rigorous bit-identical
// round-trip; this file's `push_then_pull_succeeds` is the lighter-weight
// integration check covering the Plan 04 + Plan 05 binary surfaces together.

#![allow(missing_docs)]

use std::path::PathBuf;

use assert_cmd::Command;
use pichi_storage::{FilesystemTagDb, TagDb};
use tempfile::TempDir;

fn graphroot(tmp: &TempDir) -> PathBuf {
    let p = tmp.path().join("pichi").join("storage");
    std::fs::create_dir_all(&p).unwrap();
    p
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

// --- Binary-driven test (runs unconditionally) ---

/// REGISTRY-02: with an empty cache, `pichi push <ref>` must exit non-zero
/// with stderr containing the canonical "ref not in cache" message. No
/// PICHI_TEST_REGISTRY needed — the error fires locally before any network
/// call (cmd::push resolves the tag from the local cache as the first
/// authority-bearing I/O).
#[test]
fn push_no_cache_errors() {
    let tmp = TempDir::new().unwrap();
    let _g = graphroot(&tmp);
    let assert = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["push", "ghcr.io/example/notpresent:tag"])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("ref not in cache"),
        "stderr missing 'ref not in cache': {stderr}"
    );
}

// --- Zot-backed tests (gated on PICHI_TEST_REGISTRY) ---

/// REGISTRY-02 dedup: import → push → push again. The second push must
/// succeed (HEAD returns true for every blob → skip path; manifest is
/// pushed regardless because manifests don't carry the same dedup
/// semantics as blobs). Asserts both invocations exit 0; the per-blob
/// HEAD-skip is verified by the mock-backed unit test
/// `push_inner_skips_present_blobs` in `src/cmd/push.rs::tests`.
#[test]
fn push_skips_existing_blobs() {
    let Some(base) = test_repo_base() else {
        eprintln!("PICHI_TEST_REGISTRY unset; skipping push_skips_existing_blobs");
        return;
    };
    let tmp = TempDir::new().unwrap();
    let _g = graphroot(&tmp);
    let raw = tmp.path().join("input.raw");
    std::fs::write(&raw, vec![0u8; 4096]).unwrap();
    let tag = format!("{base}/dedup-push:1");

    // Seed cache via `pichi import`.
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["import", raw.to_str().unwrap(), &tag])
        .assert()
        .success();

    // First push uploads everything.
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["push", &tag])
        .assert()
        .success();

    // Second push: HEAD returns 200 for every blob → skip path. Asserts
    // the second invocation also succeeds (the binary does not error
    // when there's nothing to upload).
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["push", &tag])
        .assert()
        .success();
}

/// REGISTRY-02 round-trip basic: import → push → rmi → pull. The final
/// state has the same tag pointing at the same digest as before rmi,
/// proving the registry-side artifact survives the local rmi and the
/// re-pull restores byte-equivalent state. Plan 06's
/// tests/cmd_pull_push_roundtrip.rs runs the rigorous bit-identical
/// round-trip across all blobs; this test is the lighter-weight surface
/// check for the cmd::push + cmd::pull binary integration.
#[test]
fn push_then_pull_succeeds() {
    let Some(base) = test_repo_base() else {
        eprintln!("PICHI_TEST_REGISTRY unset; skipping push_then_pull_succeeds");
        return;
    };
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let raw = tmp.path().join("input.raw");
    std::fs::write(&raw, vec![0u8; 4096]).unwrap();
    let tag = format!("{base}/roundtrip-basic:1");

    // 1. import → push.
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["import", raw.to_str().unwrap(), &tag])
        .assert()
        .success();
    let db_before = FilesystemTagDb::open(&g).unwrap();
    let digest_before = db_before
        .resolve_tag(&tag)
        .unwrap()
        .expect("tag should resolve after import");
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["push", &tag])
        .assert()
        .success();

    // 2. rmi → pull.
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
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["pull", &tag])
        .assert()
        .success();

    // 3. assert digest equality (round-trip integrity).
    let db_after_pull = FilesystemTagDb::open(&g).unwrap();
    let digest_after = db_after_pull
        .resolve_tag(&tag)
        .unwrap()
        .expect("tag should resolve after re-pull");
    assert_eq!(
        digest_before, digest_after,
        "REGISTRY-02 round-trip integrity: pull must restore the original manifest digest"
    );
}
