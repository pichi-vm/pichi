// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! End-to-end smoke test: `pichi build` pulls the official build image from
//! GHCR and boots it (conglobate as `/init`) to a clean power-off, emitting
//! the `CONGLOBATE-READY` console marker. The build is a trivial `from:`-only
//! carapace (the source passes straight through), so this exercises the real
//! published image booting and shutting down cleanly.
//!
//! Gated on the `vm-tests` feature; skips if `/dev/kvm` is unavailable.
//! x86_64 + KVM only.

#![cfg(all(feature = "vm-tests", target_os = "linux", target_arch = "x86_64"))]

mod common;

use common::{assert_pichi_ok, kvm_available, make_ext4, pichi, pull_build_image};

// conglobate's console readiness marker (the guest emits it; matched here).
const CONGLOBATE_READY: &str = "CONGLOBATE-READY";

#[tokio::test]
#[ignore = "pichi build deferred; re-enable with build work"]
async fn pichi_build_boots_official_image_to_poweroff() {
    if !kvm_available() {
        eprintln!("skip: no usable /dev/kvm");
        return;
    }
    let tmp = tempfile::TempDir::new().unwrap();
    let graphroot = tmp.path().join("graph");
    std::fs::create_dir_all(&graphroot).unwrap();
    let g = &graphroot;

    // The official build image, pulled from GHCR (the default --build-image).
    pull_build_image(g);

    // A trivial source carapace + a from:-only project: no derive directives,
    // so the source passes through unchanged — but the build VM still boots
    // conglobate end to end and powers off.
    let src = tmp.path().join("source.ext4");
    make_ext4(&src, &[("hello", "base rootfs content\n")]);
    assert_pichi_ok(
        "import source",
        &pichi(
            g,
            &[],
            &["import", "raw", src.to_str().unwrap(), "-t", "base:1"],
        ),
    );

    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(proj.join("pichi.build")).unwrap();
    std::fs::write(proj.join("pichi.build/carapace.yaml"), "from: base:1\n").unwrap();
    assert_pichi_ok(
        "update",
        &pichi(g, &[], &["update", proj.to_str().unwrap()]),
    );

    let build = pichi(
        g,
        &[],
        &[
            "build",
            proj.to_str().unwrap(),
            "-t",
            "out:1",
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
}
