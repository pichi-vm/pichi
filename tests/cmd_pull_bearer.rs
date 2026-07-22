// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

// Phase 44 REGISTRY-06 mid-pull bearer-token retry test (B1 revision).
//
// Closes the gap left by Plan 01 SPIKE A2's deferral: oci-client's TokenCache
// must transparently refresh JWT bearer tokens that expire mid-pull. The Plan
// 01 SPIKE deferred this to Plan 06; this is the regression guard.
//
// Behaviour: with a >= 50 MiB blob and a short-TTL JWT (CI sets TTL=5s), the
// streaming pull provably crosses at least one token-refresh window. If
// oci-client's TokenCache failed to refresh transparently, the second-half of
// the blob fetch would 401 and `pichi pull` would error. The assertion is
// just "pull succeeded".
//
// Gating: silently skips when `PICHI_TEST_REGISTRY_BEARER` is unset (matches
// the pattern in tests/cmd_pull.rs). Distinct env var from
// PICHI_TEST_REGISTRY because the bearer-zot listens on a different port
// and rejects anonymous access — running the anonymous-zot integration tests
// against it would fail.
//
// CI status: this test is NOT exercised in CI. The original
// `registry-pull-push-bearer` job was dropped after Phase 44 migrated the
// CI registry to GHCR, which mints hour-long bearer tokens that can't be
// shrunk to the 5s TTL the test needs. Local-dev exercise still works by
// running a zot+htpasswd container with a short-TTL JWT and setting the
// PICHI_TEST_REGISTRY_BEARER* env vars. See VALIDATION.md row 61.

#![allow(missing_docs)]

use std::path::PathBuf;

use assert_cmd::Command;
use tempfile::TempDir;

fn graphroot(tmp: &TempDir) -> PathBuf {
    let p = tmp.path().join("pichi").join("storage");
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn bearer_zot() -> Option<String> {
    std::env::var("PICHI_TEST_REGISTRY_BEARER").ok()
}

/// REGISTRY-06 (zot+htpasswd, gated): mid-pull token-refresh regression guard.
///
/// 1. Render a per-test config.toml with basic-auth credentials for the
///    bearer-zot prefix (CI provides PICHI_TEST_BEARER_USER/PASSWORD).
/// 2. Build a 50 MiB fixture and import → push to the bearer-zot.
/// 3. rmi locally + sleep > TTL window so any cached token expires before
///    the pull starts (defensive — ensures we hit the refresh path even on
///    fast hardware).
/// 4. Pull the 50 MiB blob; the streaming fetch crosses several seconds on
///    any realistic runner (BlobStore tempfile sync + sha256 + verity per
///    Plan 04 streaming pipeline) so at least one token-refresh window
///    will occur mid-stream.
/// 5. Assert pull succeeded — oci-client's TokenCache transparently
///    refreshed the bearer token without any change to pichi-side code.
#[tokio::test]
async fn mid_pull_401_retry() {
    let Some(reg) = bearer_zot() else {
        eprintln!("PICHI_TEST_REGISTRY_BEARER unset; skipping mid_pull_401_retry");
        return;
    };
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);

    // Per-test config.toml supplying basic-auth creds for the test
    // registry. CI sets PICHI_TEST_BEARER_USER/PASSWORD; defaults match the
    // CI job's htpasswd seed (tester / secret) so a developer running the
    // test locally against an equivalently-configured zot does not need to
    // re-export the env vars.
    let user = std::env::var("PICHI_TEST_BEARER_USER").unwrap_or_else(|_| "tester".into());
    let pass = std::env::var("PICHI_TEST_BEARER_PASSWORD").unwrap_or_else(|_| "secret".into());
    let config_dir = tmp.path().join("config").join("pichi");
    std::fs::create_dir_all(&config_dir).unwrap();
    let config_toml = format!(
        r#"[storage]
graphroot = "{}"

[[registries]]
prefix = "{reg}"
insecure = true

[registries.auth]
username = "{user}"
password = "{pass}"
"#,
        g.display()
    );
    std::fs::write(config_dir.join("config.toml"), &config_toml).unwrap();

    // 50 MiB fixture — large enough that the streaming pull pipeline takes
    // many seconds (sha256 + verity feed + tempfile sync per Plan 04
    // streaming_sink composition). The 5s JWT TTL configured in CI's
    // bearer-zot config means a 50 MiB pull crosses at least one refresh
    // window; if the runner is exceptionally fast (e.g. caching all writes
    // in pagecache), bump to 100 MiB or shorten the TTL.
    let raw = tmp.path().join("input.raw");
    let big = vec![0u8; 50 * 1024 * 1024];
    std::fs::write(&raw, &big).unwrap();
    let tag = format!("{reg}/test/bearer:1");

    // Step 1: import + push (uploads the 50 MiB blob to bearer-zot).
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_CONFIG_HOME", tmp.path().join("config"))
        .env("XDG_DATA_HOME", tmp.path())
        .args(["import", "raw", raw.to_str().unwrap(), "-t", &tag])
        .assert()
        .success();
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_CONFIG_HOME", tmp.path().join("config"))
        .env("XDG_DATA_HOME", tmp.path())
        .args(["push", &tag])
        .assert()
        .success();

    // Step 2: rmi + sleep > TTL window so any cached token expires before
    // we pull. Defensive: ensures the FIRST blob fetch in the pull also
    // exercises the refresh path (otherwise only mid-pull refreshes would
    // fire, narrowing the regression guard).
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_CONFIG_HOME", tmp.path().join("config"))
        .env("XDG_DATA_HOME", tmp.path())
        .args(["rmi", &tag])
        .assert()
        .success();
    std::thread::sleep(std::time::Duration::from_secs(7));

    // Step 3: pull. The 50 MiB stream must cross >= 1 token-refresh
    // window (TTL=5s). oci-client's TokenCache transparently refreshes;
    // assertion is "pull succeeded".
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_CONFIG_HOME", tmp.path().join("config"))
        .env("XDG_DATA_HOME", tmp.path())
        .args(["pull", &tag])
        .assert()
        .success();
}
