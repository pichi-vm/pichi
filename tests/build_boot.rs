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

// conglobate's console readiness marker (the guest emits it; matched here).
const CONGLOBATE_READY: &str = "CONGLOBATE-READY";

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

    let build = pichi(
        g,
        &[],
        &[
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
        ],
    );
    assert_pichi_ok("build", &build);

    let mut console = String::from_utf8_lossy(&build.stdout).into_owned();
    console.push_str(&String::from_utf8_lossy(&build.stderr));
    assert!(
        console.contains(CONGLOBATE_READY),
        "build image did not reach conglobate (no {CONGLOBATE_READY:?}):\n{console}"
    );

    // Packaged tag must resolve (flat BuildOutput → package_artifact).
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
