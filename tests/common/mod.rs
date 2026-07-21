// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Shared helpers for the build/boot end-to-end tests.
//!
//! The build image is pulled from GHCR — the real, published official build
//! image ([`pull_build_image`]); `pichi build` then resolves it as its default
//! `--build-image`. The `seal`/`assemble_initrd*` helpers remain for the
//! *runtime* fixtures a build/run test still constructs locally (the corium
//! PMI a built app boots with, and the corium initramfs handed to an in-guest
//! `arma build`) — host distro kernel + an initramfs cpio of a given `/init`
//! and the dep-ordered, decompressed `.ko` for the requested modules. dillo
//! boots a sealed PMI on KVM and routes the virtio-console to stdout.
//! x86_64 + KVM only.

#![cfg(feature = "vm-tests")]
// Each integration test binary that `mod common;`s this file uses a subset of
// the helpers; the rest are legitimately dead (and their `pub` unreachable)
// in that binary.
#![allow(dead_code, unreachable_pub)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use wait_timeout::ChildExt;

pub const ARMA_BIN: &str = env!("CARGO_BIN_FILE_ARMA_arma");
pub const DILLO_BIN: &str = env!("CARGO_BIN_FILE_DILLO_dillo");
pub const CONGLOBATE_BIN: &str = env!("CARGO_BIN_FILE_CONGLOBATE_conglobate");
pub const CORIUM_BIN: &str = env!("CARGO_BIN_FILE_CORIUM_corium");
pub const PICHI_BIN: &str = env!("CARGO_BIN_EXE_pichi");

/// Run the `pichi` binary against an isolated cache (`graphroot` via
/// `XDG_DATA_HOME`) with `PICHI_DILLO` pointed at the built dillo. `envs`
/// adds extra environment (e.g. `PICHI_BUILD_IMAGE`). Returns the captured
/// output; use [`assert_pichi_ok`] to fail loudly with stdout+stderr.
pub fn pichi(graphroot: &Path, envs: &[(&str, &str)], args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(PICHI_BIN);
    cmd.env("XDG_DATA_HOME", graphroot)
        .env("PICHI_DILLO", DILLO_BIN)
        .args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn pichi")
}

/// Pull the official build image (`pichi build`'s default `--build-image`)
/// into the test cache so the subsequent build resolves it. The image is
/// public on GHCR, so the pull is anonymous.
pub fn pull_build_image(graphroot: &Path) {
    assert_pichi_ok(
        "pull build image",
        &pichi(
            graphroot,
            &[],
            &["pull", pichi::cmd::build::DEFAULT_BUILD_IMAGE],
        ),
    );
}

/// Assert a `pichi` invocation succeeded, panicking with the labelled
/// stdout+stderr otherwise.
pub fn assert_pichi_ok(label: &str, out: &std::process::Output) {
    assert!(
        out.status.success(),
        "pichi {label} failed ({}):\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

/// Whether this host can run the boot lane (needs a usable `/dev/kvm`).
pub fn kvm_available() -> bool {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_ok()
}

/// The running kernel's release string (`uname -r`).
pub fn kver() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .expect("read osrelease")
        .trim()
        .to_string()
}

/// dep-ordered, deduped `.ko.xz` paths for `mods` (skips built-ins).
pub fn module_ko_paths(mods: &[&str]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for m in mods {
        let o = Command::new("modprobe")
            .args(["--show-depends", m])
            .output()
            .expect("run modprobe");
        assert!(o.status.success(), "modprobe --show-depends {m} failed");
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            if let Some(p) = line.strip_prefix("insmod ") {
                let p = PathBuf::from(p.trim());
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// Assemble an initramfs cpio with conglobate as `/init`.
pub fn assemble_initrd(dir: &Path, mods: &[&str]) -> PathBuf {
    assemble_initrd_with(dir, CONGLOBATE_BIN, mods)
}

/// Assemble an initramfs cpio: `/init` = `init_bin`, `/modules/NN-*.ko` =
/// decompressed modules in dependency order (lexical order = load order).
pub fn assemble_initrd_with(dir: &Path, init_bin: &str, mods: &[&str]) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let stage = dir.join("root");
    std::fs::create_dir_all(stage.join("modules")).unwrap();
    std::fs::copy(init_bin, stage.join("init")).unwrap();
    std::fs::set_permissions(stage.join("init"), std::fs::Permissions::from_mode(0o755)).unwrap();

    for (i, ko_xz) in module_ko_paths(mods).iter().enumerate() {
        let stem = ko_xz
            .file_name()
            .unwrap()
            .to_string_lossy()
            .replace(".ko.xz", "");
        let dest = stage.join("modules").join(format!("{i:02}-{stem}.ko"));
        let out = Command::new("xz")
            .arg("-dc")
            .arg(ko_xz)
            .output()
            .expect("run xz");
        assert!(out.status.success(), "xz -dc {} failed", ko_xz.display());
        std::fs::write(&dest, &out.stdout).unwrap();
    }

    let cpio = dir.join("initrd.cpio");
    let f = std::fs::File::create(&cpio).unwrap();
    let status = Command::new("sh")
        .arg("-c")
        .arg("cd \"$1\" && find . -mindepth 1 -printf '%P\\n' | cpio -o -H newc --quiet")
        .arg("sh")
        .arg(&stage)
        .stdout(f)
        .status()
        .expect("run cpio");
    assert!(status.success(), "cpio newc build failed");
    cpio
}

/// Seal the host kernel + initramfs into a PMI with the given cmdline. arma
/// unwraps the (zstd) bzImage itself, so we hand it `/boot/vmlinuz` as-is.
pub fn seal(dir: &Path, initrd: &Path, cmdline: &str) -> PathBuf {
    let vmlinuz = PathBuf::from(format!("/boot/vmlinuz-{}", kver()));
    assert!(
        vmlinuz.is_file(),
        "host kernel {} not present",
        vmlinuz.display()
    );
    let cfg = dir.join("kernel.config");
    std::fs::write(
        &cfg,
        "CONFIG_PCI=y\nCONFIG_VIRTIO_PCI=y\nCONFIG_VIRTIO_MMIO=y\nCONFIG_VIRTIO_BLK=y\n",
    )
    .unwrap();
    let pmi = dir.join("build-image.pmi");
    let status = Command::new(ARMA_BIN)
        .arg("build")
        .args(["--cmdline", cmdline])
        .args(["--profile", "x86-64-v2"])
        .arg("--config")
        .arg(&cfg)
        .arg("--kernel")
        .arg(&vmlinuz)
        .arg("--initrd")
        .arg(initrd)
        .arg(&pmi)
        .status()
        .expect("spawn arma");
    assert!(status.success(), "arma build failed");
    pmi
}

/// Result of booting the build VM: process success + combined console.
pub struct BootResult {
    pub success: bool,
    pub combined: String,
}

/// Boot a sealed PMI under dillo with `extra_args` (e.g. `--fs`, `--gpt`),
/// capturing the console. Panics on timeout (with the console attached).
pub fn boot(pmi: &Path, mem_mib: u32, cpus: u32, dir: &Path, extra_args: &[&str]) -> BootResult {
    let out_path = dir.join("console.out");
    let err_path = dir.join("console.err");
    let mut cmd = Command::new(DILLO_BIN);
    cmd.arg("--pmi")
        .arg(pmi)
        .args(["--memory", &mem_mib.to_string()])
        .args(["--cpus", &cpus.to_string()])
        .args(extra_args)
        .stdout(std::fs::File::create(&out_path).unwrap())
        .stderr(std::fs::File::create(&err_path).unwrap());
    let mut child = cmd.spawn().expect("spawn dillo");

    let Some(status) = child.wait_timeout(Duration::from_secs(180)).expect("wait") else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("dillo timed out:\n{}", read_combined(&out_path, &err_path));
    };
    BootResult {
        success: status.success(),
        combined: read_combined(&out_path, &err_path),
    }
}

/// Build a tiny ext4 image (16 MiB, 4096-byte blocks) populated from `files`
/// (name → contents). Used as a source carapace's bare filesystem.
pub fn make_ext4(path: &Path, files: &[(&str, &str)]) {
    let dir = path.with_extension("src");
    std::fs::create_dir_all(&dir).unwrap();
    for (name, body) in files {
        std::fs::write(dir.join(name), body).unwrap();
    }
    let status = Command::new("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096"])
        .arg("-d")
        .arg(&dir)
        .arg(path)
        .arg("16M")
        .status()
        .expect("run mkfs.ext4");
    assert!(status.success(), "mkfs.ext4 failed");
}

/// Export a real Fedora rootfs (via podman) into a bare ext4 image at `path` —
/// a generic pmi-source / `from:` with a real `bash` + glibc. Returns false if
/// podman or the pull is unavailable (caller should skip).
pub fn fedora_ext4(path: &Path, rootfs_dir: &Path) -> bool {
    let create = Command::new("podman")
        .args(["create", "registry.fedoraproject.org/fedora:43"])
        .output();
    let Ok(out) = create else { return false };
    if !out.status.success() {
        return false;
    }
    let cid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    std::fs::create_dir_all(rootfs_dir).unwrap();
    let ok = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "podman export {cid} | tar -xf - -C {}",
            rootfs_dir.display()
        ))
        .status()
        .is_ok_and(|s| s.success());
    let _ = Command::new("podman").args(["rm", &cid]).output();
    if !ok {
        return false;
    }
    // The rootfs has unreadable files (e.g. gshadow, mode 0000); as a non-root
    // user `mkfs.ext4 -d` can't read them. The extracted files are owned by us
    // (rootless podman userns), so grant owner read so the pack can proceed.
    let _ = Command::new("chmod")
        .arg("-R")
        .arg("u+rwX")
        .arg(rootfs_dir)
        .status();
    let status = Command::new("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096"])
        .arg("-d")
        .arg(rootfs_dir)
        .arg(path)
        .arg("512M")
        .status()
        .expect("run mkfs.ext4");
    status.success()
}

pub fn read_combined(out: &Path, err: &Path) -> String {
    let mut s = std::fs::read_to_string(out).unwrap_or_default();
    s.push_str(&std::fs::read_to_string(err).unwrap_or_default());
    s
}
