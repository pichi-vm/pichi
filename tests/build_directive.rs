// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Increment 8c, end to end: a `carapace.yaml` with a `copy:` directive. The
//! source carapace is a real ext4 filesystem; conglobate flattens it into the
//! base scute, then for the directive builds a writable dm-snapshot (COW on a
//! brd ramdisk) over the composed origin, mounts it, copies a file from the
//! build context into it, and re-emits the change as a salt-chained delta
//! scute. The host packages a two-scute carapace.
//!
//! Exercises the novel build machinery: dm-snapshot over a composed carapace
//! in RAM, `write_delta` change capture, and verity salt-chaining. (`run:`
//! chroot/exec needs a shell in the rootfs and is covered by the full
//! build→run MVP.) x86_64 + KVM only.

#![cfg(all(feature = "vm-tests", target_os = "linux", target_arch = "x86_64"))]

mod common;

use common::{assert_pichi_ok, kvm_available, make_ext4, pichi, pull_build_image};

#[tokio::test]
#[ignore = "pichi build deferred; re-enable with build work"]
#[cfg(target_arch = "x86_64")]
async fn pichi_build_applies_a_copy_directive() {
    if !kvm_available() {
        eprintln!("skip: no usable /dev/kvm");
        return;
    }
    let tmp = tempfile::TempDir::new().unwrap();
    let graphroot = tmp.path().join("graph");
    std::fs::create_dir_all(&graphroot).unwrap();
    let g = &graphroot;

    // Build image: the real official image, pulled from GHCR.
    pull_build_image(g);

    // Source carapace: a real ext4 rootfs.
    let src_img = tmp.path().join("source.ext4");
    make_ext4(&src_img, &[("hello", "base rootfs content\n")]);
    assert_pichi_ok(
        "import source",
        &pichi(
            g,
            &[],
            &["import", "raw", src_img.to_str().unwrap(), "-t", "base:1"],
        ),
    );

    // Project: copy a context file into the rootfs.
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(proj.join("pichi.build")).unwrap();
    std::fs::write(proj.join("payload.txt"), "copied by the build\n").unwrap();
    std::fs::write(
        proj.join("pichi.build/carapace.yaml"),
        "from: base:1\nderive:\n  - copy:\n      from: payload.txt\n      into: /opt/payload.txt\n",
    )
    .unwrap();

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

    // build success ⇒ conglobate ran the whole snapshot→mount→copy→
    // write_delta→verity loop in-guest. The packaged carapace must have two
    // scutes: the flattened base + the copy delta.
    let inspect = pichi(g, &[], &["inspect", "out:1"]);
    assert_pichi_ok("inspect out:1", &inspect);
    let info = String::from_utf8_lossy(&inspect.stdout);
    assert!(
        info.contains("\"scute_count\": 2"),
        "expected a two-scute carapace (base + copy delta):\n{info}"
    );
}
