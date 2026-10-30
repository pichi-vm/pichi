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
/// adds extra environment. Returns the captured output; use
/// [`assert_pichi_ok`] to fail loudly with stdout+stderr.
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
///
/// Returns `true` when the pull succeeded. Returns `false` (and prints a skip
/// line naming the unpublished tag) when GHCR does not yet have
/// `DEFAULT_BUILD_IMAGE` — callers that need the real appliance must early-
/// return so CI stays green until Conglobate publishes the locked pin. Does **not**
/// change the pin; when the digest exists the pull succeeds and the real smoke runs.
pub fn pull_build_image(graphroot: &Path) -> bool {
    let image = pichi::cmd::build::DEFAULT_BUILD_IMAGE;
    let out = pichi(graphroot, &[], &["pull", image]);
    if out.status.success() {
        return true;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    eprintln!(
        "skip: official build image {image} not pullable yet ({}). Waiting on Conglobate GHCR publish of ghcr.io/pichi-vm/conglobate@sha256:b9e26efd9d81ecb5b1a950e3f4891e9a04fbc7d9d222444621bccc95fad3babd.\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status,
    );
    false
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

/// Locked Conglobate smoke reference (must match plan `from:` and lock key).
pub const SMOKE_SOURCE_REF: &str = "qualification.local/fedora:43";

/// Conglobate revision matching the published OCI-only appliance.
/// Includes the locked context plan used by the real build smoke.
pub const SMOKE_CONGLOBATE_REF: &str = "f88a7215ee93f85fe340991730e6e7a8046f02fc";

/// Conglobate smoke context under the repo mount.
pub const SMOKE_CONTEXT: &str = "tests/distro/context";
pub const SMOKE_PLAN: &str = "plan.yaml";
pub const SMOKE_LOCK: &str = "lock.yaml";

/// Resolve the full `sha256:…` digest for a local tag via `pichi inspect`.
pub fn tag_digest(graphroot: &Path, reference: &str) -> String {
    let out = pichi(
        graphroot,
        &[],
        &["inspect", "--format", "{{ digest }}", reference],
    );
    assert_pichi_ok(&format!("inspect {reference}"), &out);
    let dig = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(
        dig.starts_with("sha256:"),
        "expected sha256: digest for {reference}, got {dig:?}"
    );
    dig
}

/// Import a tiny source carapace and retain its authentic platform index.
/// Returns the index digest to record in the smoke repository's lock.
pub async fn seed_smoke_source_carapace(graphroot: &Path, tmp: &Path) -> String {
    use pichi_artifact::{Digest, Reference};
    use pichi_storage::{BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};
    use sha2::{Digest as _, Sha256};

    let src = tmp.join("qualification-fedora43.ext4");
    make_ext4(&src, &[("os-release", "ID=fedora\nVERSION_ID=43\n")]);
    assert_pichi_ok(
        "import smoke source carapace",
        &pichi(
            graphroot,
            &[],
            &[
                "import",
                "raw",
                src.to_str().unwrap(),
                "-t",
                SMOKE_SOURCE_REF,
            ],
        ),
    );
    let storage = graphroot.join("pichi/storage");
    let store = FilesystemBlobStore::new(&storage);
    let db = FilesystemTagDb::open(&storage).unwrap();
    let manifest: Digest = tag_digest(graphroot, SMOKE_SOURCE_REF).parse().unwrap();
    let bytes = store.get_blob(&manifest).await.unwrap();
    let index = serde_json::json!({
        "schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "artifactType": "application/vnd.pichi.artifact.v1+json",
            "digest": manifest.to_string(), "size": bytes.len(),
            "platform": {"os": "pichi", "architecture": "amd64"}
        }]
    });
    let bytes = serde_json::to_vec(&index).unwrap();
    let digest = Digest::Sha256(Sha256::digest(&bytes).into());
    store.put_blob(&digest, &bytes).await.unwrap();
    let reference: Reference = SMOKE_SOURCE_REF.parse().unwrap();
    db.set_tag(&reference.to_string(), &digest).await.unwrap();
    assert_eq!(tag_digest(graphroot, SMOKE_SOURCE_REF), digest.to_string());
    digest.to_string()
}

/// Commit the smoke context from the appliance source revision into a fresh
/// repository. Read the sibling checkout without changing it; CI clones the
/// source when that checkout is absent.
pub fn prepare_smoke_repo(tmp: &Path, index: &str) -> (PathBuf, String) {
    let sibling = Path::new(env!("CARGO_MANIFEST_DIR")).join("../conglobate");
    let has_commit = Command::new("git")
        .args([
            "cat-file",
            "-e",
            &format!("{SMOKE_CONGLOBATE_REF}^{{commit}}"),
        ])
        .current_dir(&sibling)
        .status()
        .is_ok_and(|status| status.success());
    let source = if has_commit {
        sibling
    } else {
        let source = tmp.join("conglobate-source");
        assert!(
            Command::new("git")
                .args([
                    "clone",
                    "-q",
                    "--no-checkout",
                    "https://github.com/pichi-vm/conglobate.git"
                ])
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["fetch", "-q", "origin", SMOKE_CONGLOBATE_REF])
                .current_dir(&source)
                .status()
                .unwrap()
                .success()
        );
        source
    };
    let repo = tmp.join("smoke-repo");
    let context = repo.join(SMOKE_CONTEXT);
    std::fs::create_dir_all(&context).unwrap();
    for name in [SMOKE_PLAN, "message.txt"] {
        let output = Command::new("git")
            .args([
                "show",
                &format!("{SMOKE_CONGLOBATE_REF}:{SMOKE_CONTEXT}/{name}"),
            ])
            .current_dir(&source)
            .output()
            .unwrap();
        assert!(output.status.success(), "read pinned {name}");
        std::fs::write(context.join(name), output.stdout).unwrap();
    }
    std::fs::write(
        context.join(SMOKE_LOCK),
        format!("version: 1\nimages:\n  {SMOKE_SOURCE_REF}:\n    index: {index}\n"),
    )
    .unwrap();
    // The tiny seed has no shell. Retain the upstream COPY, omitting its
    // Fedora-specific RUN assertions, and commit those exact fixture inputs.
    let plan = std::fs::read_to_string(context.join(SMOKE_PLAN)).unwrap();
    std::fs::write(
        context.join(SMOKE_PLAN),
        format!(
            "{}image: rootfs\n",
            plan.split("    - run:").next().unwrap()
        ),
    )
    .unwrap();
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "Commit smoke inputs",
        ],
    ] {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
    }
    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(head.status.success());
    (
        repo,
        String::from_utf8(head.stdout).unwrap().trim().to_owned(),
    )
}
