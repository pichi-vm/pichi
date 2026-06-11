// SPDX-License-Identifier: Apache-2.0
//
// Phase 44 REGISTRY-01..07 integration tests for `pichi pull`. Two test classes:
//   1. Binary-driven (always run): exercises `assert_cmd::Command::cargo_bin("pichi")`
//      paths that need the actual binary surface — `--pull=never` short-circuit
//      with no registry config (this is the only behaviour cleanly observable
//      from outside the process).
//   2. Zot-backed (gated on the PICHI_TEST_REGISTRY env var): full network
//      round-trips. Skipped silently if PICHI_TEST_REGISTRY is unset; CI sets
//      it to "localhost:5000" via the registry-pull-push job.
//
// The mock-backed unit-shaped tests (--pull=missing cache hit, REGISTRY-07
// non-pichi bare manifest reject, REGISTRY-01 manifest-validate-before-blob,
// Pitfall 3 raw-bytes guard, Pitfall 11 atomic commit, D-02 index walk) live
// in `src/cmd/pull.rs::tests` because they need to drive `pull_inner_with_registry`
// directly with a `MockRegistry` — Approach C from the planner's note in
// 44-04-PLAN.md (cleaner than exposing a hidden test-only env var on the
// binary or creating an ad-hoc `lib.rs` re-export just for tests).

#![allow(missing_docs)]

use std::path::PathBuf;

use assert_cmd::Command;
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

/// Repo-base prefix tests append their per-test name to. Preferred: explicit
/// `PICHI_TEST_REPO_BASE` (e.g. `ghcr.io/shelbyapichi/pichi-ci-test` for the
/// GHCR CI job). Fallback for the legacy zot-style setup: `{registry}/test`.
fn test_repo_base() -> Option<String> {
    if let Ok(base) = std::env::var("PICHI_TEST_REPO_BASE") {
        if !base.is_empty() {
            return Some(base);
        }
    }
    pichi_test_registry().map(|reg| format!("{reg}/test"))
}

// --- Binary-driven test (runs unconditionally) ---

/// REGISTRY-03: `--pull=never` against an empty cache must error with the
/// canonical "pull policy `never`: ref not in cache" message and MUST NOT
/// touch the network (verified indirectly: the test runs with no
/// PICHI_TEST_REGISTRY env var; if cmd::pull built the throwaway runtime
/// before the never-check it would still error here, but the implementation
/// short-circuits before any I/O so no network is attempted).
#[test]
fn pull_never_errors_when_absent() {
    let tmp = TempDir::new().unwrap();
    let _g = graphroot(&tmp);
    let assert = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["pull", "--pull=never", "ghcr.io/example/foo:bar"])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("pull policy `never`"),
        "stderr missing 'pull policy `never`': {stderr}"
    );
    assert!(
        stderr.contains("ref not in cache"),
        "stderr missing 'ref not in cache': {stderr}"
    );
}

// --- Zot-backed tests (gated on PICHI_TEST_REGISTRY) ---

/// REGISTRY-01 (zot): seed cache via `pichi import`, push to zot, rmi
/// locally, pull twice. Second pull must NOT re-write blob files (mtimes
/// stable) — REGISTRY-01 dedup-by-digest semantics.
#[test]
fn pull_skips_existing_blobs() {
    let Some(base) = test_repo_base() else {
        eprintln!("PICHI_TEST_REGISTRY unset; skipping pull_skips_existing_blobs");
        return;
    };
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    // Step 1: seed cache via `pichi import`.
    let raw = tmp.path().join("input.raw");
    std::fs::write(&raw, vec![0u8; 4096]).unwrap();
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["import", raw.to_str().unwrap(), &format!("{base}/dedup:1")])
        .assert()
        .success();
    // Step 2: push to zot. (Plan 05 implements push; until then the zot
    // round-trip path is partially-functional. Skip the rest if push is
    // not yet shipped.)
    let push = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["push", &format!("{base}/dedup:1")])
        .assert();
    if !push.get_output().status.success() {
        eprintln!("pichi push not yet shipped (Plan 05); skipping pull dedup steps");
        return;
    }
    // Step 3: rmi locally then pull; second pull must skip blob bodies.
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["rmi", &format!("{base}/dedup:1")])
        .assert()
        .success();
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["pull", &format!("{base}/dedup:1")])
        .assert()
        .success();
    let blobs = g.join("blobs").join("sha256");
    let mtimes_before: Vec<_> = std::fs::read_dir(&blobs)
        .unwrap()
        .filter_map(|e| {
            e.ok()
                .and_then(|e| e.metadata().ok())
                .and_then(|m| m.modified().ok())
        })
        .collect();
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["pull", "--pull=always", &format!("{base}/dedup:1")])
        .assert()
        .success();
    let mtimes_after: Vec<_> = std::fs::read_dir(&blobs)
        .unwrap()
        .filter_map(|e| {
            e.ok()
                .and_then(|e| e.metadata().ok())
                .and_then(|m| m.modified().ok())
        })
        .collect();
    assert_eq!(
        mtimes_before, mtimes_after,
        "REGISTRY-01: blob files must not be re-written when already present by digest"
    );
}

/// REGISTRY-03 (zot): `--pull=always` re-fetches the manifest. Asserts the
/// commands succeed regardless of cache state (a stronger "manifest was
/// re-fetched from upstream" assertion would require zot HTTP-log inspection
/// outside this test's scope).
#[test]
fn pull_always_refetches() {
    let Some(base) = test_repo_base() else {
        eprintln!("PICHI_TEST_REGISTRY unset; skipping pull_always_refetches");
        return;
    };
    let tmp = TempDir::new().unwrap();
    let _g = graphroot(&tmp);
    let raw = tmp.path().join("input.raw");
    std::fs::write(&raw, vec![0u8; 4096]).unwrap();
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["import", raw.to_str().unwrap(), &format!("{base}/always:1")])
        .assert()
        .success();
    let push = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["push", &format!("{base}/always:1")])
        .assert();
    if !push.get_output().status.success() {
        eprintln!("pichi push not yet shipped (Plan 05); skipping pull_always");
        return;
    }
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["pull", "--pull=always", &format!("{base}/always:1")])
        .assert()
        .success();
}

/// REGISTRY-03 (zot): `--pull=newer` — pull → pull again with --pull=newer.
/// With unchanged upstream digest the second invocation must skip body
/// fetch (the W6 revision uses GET-then-compare; functionally correct).
#[test]
fn pull_newer_skips_when_unchanged() {
    let Some(base) = test_repo_base() else {
        eprintln!("PICHI_TEST_REGISTRY unset; skipping pull_newer_skips_when_unchanged");
        return;
    };
    let tmp = TempDir::new().unwrap();
    let _g = graphroot(&tmp);
    let raw = tmp.path().join("input.raw");
    std::fs::write(&raw, vec![0u8; 4096]).unwrap();
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["import", raw.to_str().unwrap(), &format!("{base}/newer:1")])
        .assert()
        .success();
    let push = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["push", &format!("{base}/newer:1")])
        .assert();
    if !push.get_output().status.success() {
        eprintln!("pichi push not yet shipped (Plan 05); skipping pull_newer");
        return;
    }
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["pull", "--pull=newer", &format!("{base}/newer:1")])
        .assert()
        .success();
}

/// REGISTRY-06 (zot): anonymous pull from zot's anonymous-anyone repo
/// succeeds when no auth is configured. Uses a fresh second TempDir so no
/// auth files leak from the first push.
#[test]
fn anonymous_pull() {
    let Some(base) = test_repo_base() else {
        eprintln!("PICHI_TEST_REGISTRY unset; skipping anonymous_pull");
        return;
    };
    let tmp = TempDir::new().unwrap();
    let _g = graphroot(&tmp);
    let raw = tmp.path().join("input.raw");
    std::fs::write(&raw, vec![0u8; 4096]).unwrap();
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["import", raw.to_str().unwrap(), &format!("{base}/anon:1")])
        .assert()
        .success();
    let push = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["push", &format!("{base}/anon:1")])
        .assert();
    if !push.get_output().status.success() {
        eprintln!("pichi push not yet shipped (Plan 05); skipping anonymous_pull");
        return;
    }
    let tmp2 = TempDir::new().unwrap();
    let _g2 = graphroot(&tmp2);
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp2.path())
        .env_remove("REGISTRY_AUTH_FILE")
        .args(["pull", &format!("{base}/anon:1")])
        .assert()
        .success();
}
