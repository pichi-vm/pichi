// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The full single-command MVP (#11): one `pichi build` emits a bootable
//! application artifact — conglobate builds the carapace AND seals the PMI from
//! `pmi.yaml` — and `pichi run` boots it to corium mounting the root carapace.
//!
//! The PMI source is a real Fedora rootfs (the generic `from:`), so `pmi.yaml`'s
//! `run: arma build …` executes in a real shell. The build context carries
//! arma, the kernel, and a corium initramfs, referenced via the chroot-bound
//! `/context`; conglobate exports the carapace top root as `PICHI_CARAPACE_ROOT`
//! for the author's cmdline. x86_64 + KVM only.

#![cfg(all(feature = "vm-tests", target_os = "linux", target_arch = "x86_64"))]

mod common;

use common::{
    ARMA_BIN, CORIUM_BIN, assemble_initrd_with, assert_pichi_ok, fedora_ext4, kvm_available,
    make_ext4, pichi, pull_build_image,
};
use corium::CORIUM_ROOT_OK;

const MARKER: &str = "corium-root-marker";

#[tokio::test]
#[ignore = "pichi build deferred; re-enable with build work"]
#[cfg(target_arch = "x86_64")]
async fn single_pichi_build_emits_bootable_artifact() {
    if !kvm_available() {
        eprintln!("skip: no usable /dev/kvm");
        return;
    }
    let tmp = tempfile::TempDir::new().unwrap();
    let xdg = tmp.path().join("xdg");
    std::fs::create_dir_all(&xdg).unwrap();
    let g = &xdg;

    // PMI source: a real Fedora rootfs (bash + glibc for `run:`).
    let fedora = tmp.path().join("fedora.ext4");
    if !fedora_ext4(&fedora, &tmp.path().join("fedora-rootfs")) {
        eprintln!("skip: podman/Fedora rootfs unavailable");
        return;
    }
    assert_pichi_ok(
        "import fedora",
        &pichi(
            g,
            &[],
            &[
                "import",
                "raw",
                fedora.to_str().unwrap(),
                "-t",
                "fedora:base",
            ],
        ),
    );

    // App root carapace: a small marked rootfs (no systemd → corium proves the
    // mount and powers off).
    let app = tmp.path().join("app.ext4");
    make_ext4(&app, &[(MARKER, "hello from the root carapace\n")]);
    assert_pichi_ok(
        "import app",
        &pichi(
            g,
            &[],
            &["import", "raw", app.to_str().unwrap(), "-t", "base:1"],
        ),
    );

    // Build image (conglobate): the real official image, pulled from GHCR.
    pull_build_image(g);

    // Project + build context: pmi.yaml's `run: arma build` references the
    // kernel / initrd / arma via the chroot-bound /context.
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(proj.join("pichi.build")).unwrap();
    std::fs::copy(ARMA_BIN, proj.join("arma")).unwrap();
    std::fs::copy(
        format!("/boot/vmlinuz-{}", common::kver()),
        proj.join("vmlinuz"),
    )
    .unwrap();
    let cor_initrd = assemble_initrd_with(&tmp.path().join("cor"), CORIUM_BIN, &["dm_verity"]);
    std::fs::copy(&cor_initrd, proj.join("corium-initrd.cpio")).unwrap();
    std::fs::write(
        proj.join("kernel.config"),
        "CONFIG_PCI=y\nCONFIG_VIRTIO_PCI=y\nCONFIG_VIRTIO_MMIO=y\nCONFIG_VIRTIO_BLK=y\n",
    )
    .unwrap();
    std::fs::write(proj.join("pichi.build/carapace.yaml"), "from: base:1\n").unwrap();
    std::fs::write(
        proj.join("pichi.build/pmi.yaml"),
        "from: fedora:base\n\
         derive:\n\
         \x20 - run: /context/arma build --profile x86-64-v2 --config /context/kernel.config \
         --kernel /context/vmlinuz --initrd /context/corium-initrd.cpio \
         --cmdline \"console=hvc0 root.carapace=$PICHI_CARAPACE_ROOT\" /tmp/boot.pmi\n\
         into: /tmp/boot.pmi\n",
    )
    .unwrap();

    assert_pichi_ok(
        "update",
        &pichi(g, &[], &["update", proj.to_str().unwrap()]),
    );

    // One build → a bootable application artifact (carapace + sealed PMI).
    assert_pichi_ok(
        "build",
        &pichi(
            g,
            &[],
            &[
                "build",
                proj.to_str().unwrap(),
                "-t",
                "app:1",
                "--memory",
                "2048",
                "--cpus",
                "2",
            ],
        ),
    );

    // Run it: corium must mount the root carapace in the initramfs.
    let out_path = tmp.path().join("run.out");
    let err_path = tmp.path().join("run.err");
    let mut child = std::process::Command::new(common::PICHI_BIN)
        .env("XDG_DATA_HOME", &xdg)
        .env("PICHI_DILLO", common::DILLO_BIN)
        .args(["run", "app:1", "--memory", "1024", "--cpus", "1"])
        .stdout(std::fs::File::create(&out_path).unwrap())
        .stderr(std::fs::File::create(&err_path).unwrap())
        .spawn()
        .expect("spawn pichi run");
    let status = {
        use wait_timeout::ChildExt;
        let Some(s) = child
            .wait_timeout(std::time::Duration::from_secs(180))
            .unwrap()
        else {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "pichi run timed out:\n{}",
                common::read_combined(&out_path, &err_path)
            );
        };
        s
    };
    let combined = common::read_combined(&out_path, &err_path);
    assert!(
        status.success(),
        "pichi run exited non-zero ({status}):\n{combined}"
    );
    assert!(
        combined.contains(CORIUM_ROOT_OK) && combined.contains(MARKER),
        "corium did not mount the root carapace from the single-build artifact:\n{combined}"
    );
}
