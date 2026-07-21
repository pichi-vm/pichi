// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Increment 8b, end to end through the real host CLI: `pichi build` of a
//! `from:`-only recipe boots the conglobate build image, which attaches the
//! source carapace, flattens it into one base scute, and emits `build.yaml`;
//! the host then packages a tagged carapace artifact.
//!
//! This exercises the whole host path (`pichi import` → `pichi update` →
//! `pichi build` → package) plus conglobate's source-attach + flatten. The
//! build image is a normal appliance artifact (`pichi import --pmi`); `pichi
//! build` boots its PMI and ignores the unused scute. x86_64 + KVM only.

#![cfg(all(feature = "vm-tests", target_os = "linux", target_arch = "x86_64"))]

mod common;

use common::{assert_pichi_ok, kvm_available, pichi, pull_build_image};

#[test]
#[ignore = "pichi build deferred; re-enable with build work"]
#[cfg(target_arch = "x86_64")]
fn pichi_build_flattens_source_into_a_carapace() {
    if !kvm_available() {
        eprintln!("skip: no usable /dev/kvm");
        return;
    }
    let tmp = tempfile::TempDir::new().unwrap();
    let graphroot = tmp.path().join("graph");
    std::fs::create_dir_all(&graphroot).unwrap();
    let g = &graphroot;

    // The conglobate build image: the real official image, pulled from GHCR.
    pull_build_image(g);

    // The source carapace conglobate will flatten. Opaque content — a few
    // non-zero bytes so the flatten is non-trivial. (import emits the
    // carapace-mandated scute chunk size, so the scute is attachable.)
    let src_raw = tmp.path().join("source.raw");
    let mut src = vec![0u8; 4096 * 3];
    src[5000] = 0xAB;
    src[9000] = 0xCD;
    std::fs::write(&src_raw, &src).unwrap();
    assert_pichi_ok(
        "import source",
        &pichi(g, &[], &["import", src_raw.to_str().unwrap(), "base:1"]),
    );

    // The project: a from:-only carapace recipe (no derive directives → the
    // output is the flattened base scute).
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(proj.join("pichi.build")).unwrap();
    std::fs::write(proj.join("pichi.build/carapace.yaml"), "from: base:1\n").unwrap();

    assert_pichi_ok(
        "update",
        &pichi(g, &[], &["update", proj.to_str().unwrap()]),
    );
    // refs.lock must now pin base:1.
    let refs = std::fs::read_to_string(proj.join("pichi.build/refs.lock")).unwrap();
    assert!(refs.contains("base:1"), "refs.lock missing base:1:\n{refs}");

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

    // build success ⇒ conglobate attached the source, flattened it, and
    // emitted a non-empty build.yaml the host packaged (package_artifact
    // bails on zero scutes). Confirm the artifact is now tagged + inspectable.
    let inspect = pichi(g, &[], &["inspect", "out:1"]);
    assert_pichi_ok("inspect out:1", &inspect);
    let info = String::from_utf8_lossy(&inspect.stdout);
    assert!(
        info.contains("scute") || info.to_lowercase().contains("layer"),
        "inspect out:1 shows no scute layer:\n{info}"
    );
}
