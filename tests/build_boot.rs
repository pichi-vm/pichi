// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! End-to-end smoke: `pichi build` against Conglobate's locked context inputs
//! (`tests/distro/context` + `qualification.local/fedora:43`) with the real
//! published build image (`ghcr.io/pichi-vm/conglobate:0.2.0@sha256:…`).
//!
//! Host duties under test:
//! - seed the exact Conglobate Follow-up A index blob
//!   (`sha256:4e335e14…` / PR #8 @ `9fa7f204`) under
//!   `qualification.local/fedora:43`, with a tiny imported carapace behind
//!   the placeholder inner-manifest digest;
//! - thin-launcher wires: virtiofs repo/cache/output + `dev.pichi.build`;
//! - dual-write flat `BuildOutput` → transitional `package_artifact`.
//!
//! Gated on the `vm-tests` feature; skips if `/dev/kvm` is unavailable.
//! x86_64 + KVM only.

#![cfg(all(feature = "vm-tests", target_os = "linux", target_arch = "x86_64"))]

mod common;

use common::{
    SMOKE_LOCK, SMOKE_LOCK_INDEX, SMOKE_RECIPE, SMOKE_SOURCE_REF, SMOKE_SUBDIR, assert_pichi_ok,
    kvm_available, pichi, prepare_smoke_repo, pull_build_image, seed_smoke_source_carapace,
};

/// Bound so a missing guest poweroff cannot hang CI (safety after Conglobate #11).
const OFFICIAL_SMOKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

#[tokio::test]
async fn pichi_build_boots_official_image_to_poweroff() {
    if !kvm_available() {
        eprintln!("skip: no usable /dev/kvm");
        return;
    }
    let tmp = tempfile::TempDir::new().unwrap();
    let graphroot = tmp.path().join("graph");
    std::fs::create_dir_all(&graphroot).unwrap();
    let g = &graphroot;

    // Official build image (DEFAULT_BUILD_IMAGE = :0.2.0@sha256:…). Skip
    // (do not fail CI) until Conglobate publishes that pin; when present,
    // pull succeeds and the real smoke runs.
    if !pull_build_image(g) {
        return;
    }

    // Seed lock-pinned index + carapace behind placeholder (not real Fedora).
    seed_smoke_source_carapace(g, tmp.path()).await;

    // Conglobate Follow-up A tip (lock.yaml lands in PR #8 @ 9fa7f204).
    let (repo, git_ref) = prepare_smoke_repo(tmp.path());

    // Timed spawn: BUILD.md requires guest exit 0 after the build.
    // Conglobate #11 adds SuccessAction=poweroff; keep a bound so a
    // regression cannot hang CI. Inherit stdio so console noise cannot
    // fill a pipe and deadlock dillo.
    let mut child = std::process::Command::new(common::PICHI_BIN)
        .env("XDG_DATA_HOME", g)
        .env("PICHI_DILLO", common::DILLO_BIN)
        .args([
            "build",
            repo.to_str().unwrap(),
            "-t",
            "smoke:out",
            "--ref",
            &git_ref,
            "--subdir",
            SMOKE_SUBDIR,
            "--recipe",
            SMOKE_RECIPE,
            "--lock",
            SMOKE_LOCK,
            "--memory",
            "2048",
            "--cpus",
            "1",
        ])
        .spawn()
        .expect("spawn pichi build");
    let deadline = tokio::time::Instant::now() + OFFICIAL_SMOKE_TIMEOUT;
    let status = loop {
        if let Some(st) = child.try_wait().expect("try_wait pichi build") {
            break st;
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "official Conglobate smoke: guest did not power off within {}s after launch (expected after Conglobate #11 SuccessAction=poweroff). Pin is ghcr.io/pichi-vm/conglobate:0.2.0@sha256:1d526628421ff5841ceb9054bf0bd4c94aebaadfb45b5c1a83f46740edb10a74. Appliance logs go to the journal only (not hvc0). Output mount stays empty when Pipeline fails.",
                OFFICIAL_SMOKE_TIMEOUT.as_secs()
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    };
    let build = std::process::Output {
        status,
        stdout: Vec::new(),
        stderr: Vec::new(),
    };
    assert_pichi_ok("build", &build);

    // Packaged tag must resolve (flat BuildOutput → package_artifact).
    // Success criterion is guest exit 0 + inspectable tag — not a console
    // marker (Conglobate never emitted CONGLOBATE-READY).
    let inspect = pichi(
        g,
        &[],
        &["inspect", "--format", "{{ digest }}", "smoke:out"],
    );
    assert_pichi_ok("inspect smoke:out", &inspect);
    let out_digest = String::from_utf8_lossy(&inspect.stdout).trim().to_string();
    assert!(
        out_digest.starts_with("sha256:"),
        "packaged smoke:out missing digest; source was {SMOKE_SOURCE_REF}@{SMOKE_LOCK_INDEX}"
    );
}
