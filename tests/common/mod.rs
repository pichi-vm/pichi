// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Shared helpers for the build/boot end-to-end tests.
//!
//! The build image is the shipping default, or an explicitly rebuilt OCI
//! layout supplied with `PICHI_TEST_BUILD_IMAGE_LAYOUT` for qualification.
//! [`prepare_build_image`] imports it into the isolated test cache. The
//! Source roots are partitioned GPT fixtures; firmware production and runtime
//! boot verification use the same stage and image paths as user builds.

#![cfg(feature = "vm-tests")]
// Each integration test binary that `mod common;`s this file uses a subset of
// the helpers; the rest are legitimately dead (and their `pub` unreachable)
// in that binary.
#![allow(dead_code, unreachable_pub)]

pub mod process;
mod root_disk;
pub mod stages;

const TEST_ARCH: &str = if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "amd64"
};

use std::path::{Path, PathBuf};
use std::process::Command;

pub mod submodules;

pub const CORIUM_BIN: &str = env!("CARGO_BIN_FILE_CORIUM_corium");
pub const DILLO_BIN: &str = env!("CARGO_BIN_FILE_DILLO_dillo");
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
    if args.first() == Some(&"build") {
        cmd.args(["--build-image", build_image()]);
    }
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn pichi")
}

/// Select an explicitly rebuilt appliance during cross-project qualification.
/// With no fixture override, exercise the shipping default.
pub fn build_image() -> &'static str {
    if std::env::var_os("PICHI_TEST_BUILD_IMAGE_LAYOUT").is_some() {
        "qualification.local/builder:staged"
    } else {
        pichi::cmd::build::DEFAULT_BUILD_IMAGE
    }
}

pub fn prepare_build_image(graphroot: &Path) {
    Hypervisor::prepare();
    if let Some(layout) = std::env::var_os("PICHI_TEST_BUILD_IMAGE_LAYOUT") {
        assert_pichi_ok(
            "load rebuilt appliance",
            &pichi(
                graphroot,
                &[],
                &[
                    "load",
                    Path::new(&layout).to_str().unwrap(),
                    "--tag",
                    build_image(),
                ],
            ),
        );
    } else {
        assert_pichi_ok(
            "pull official build image",
            &pichi(graphroot, &[], &["pull", build_image()]),
        );
    }
}

struct Hypervisor;

impl Hypervisor {
    fn prepare() {
        #[cfg(target_os = "linux")]
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .expect("VM tests require usable /dev/kvm");

        #[cfg(target_os = "macos")]
        {
            static SIGNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
            SIGNED.get_or_init(|| {
                let directory = tempfile::tempdir().unwrap();
                let entitlements = directory.path().join("hypervisor.plist");
                std::fs::write(
                    &entitlements,
                    r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>com.apple.security.hypervisor</key><true/></dict></plist>
"#,
                )
                .unwrap();
                let output = Command::new("codesign")
                    .args(["--force", "--sign", "-", "--entitlements"])
                    .arg(&entitlements)
                    .arg(DILLO_BIN)
                    .output()
                    .expect("codesign dillo for HVF");
                assert!(output.status.success(), "codesign dillo: {output:?}");
            });
        }
    }
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

/// Build the tiny source root used by smoke and submodule tests. CI supplies
/// the same ext4 bytes to every host; local Linux runs can generate them.
pub fn make_root_disk(path: &Path) {
    if let Some(filesystem) = std::env::var_os("PICHI_TEST_ROOT_FILESYSTEM") {
        root_disk::RootDisk::write(Path::new(&filesystem), path);
        return;
    }
    let filesystem = path.with_extension("filesystem");
    let dir = path.with_extension("src");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("os-release"), "ID=fedora\nVERSION_ID=43\n").unwrap();
    let status = Command::new("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096"])
        .arg("-d")
        .arg(&dir)
        .arg(&filesystem)
        .arg("16M")
        .status()
        .expect("run mkfs.ext4 or set PICHI_TEST_ROOT_FILESYSTEM to the CI fixture");
    assert!(status.success(), "mkfs.ext4 failed");
    root_disk::RootDisk::write(&filesystem, path);
}

/// Locked Conglobate smoke reference (must match stage input and lock key).
pub const SMOKE_SOURCE_REF: &str = "qualification.local/fedora:43";

/// Conglobate revision supplying the locked context for the real build smoke.
/// Matches the published appliance's source revision.
pub const SMOKE_CONGLOBATE_REF: &str = "d3f395acecdfd95c75ed545d2bc03ba409a99298";

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
    let src = tmp.join("qualification-fedora43.raw");
    make_root_disk(&src);
    assert_pichi_ok(
        "import smoke source carapace",
        &pichi(
            graphroot,
            &[],
            &[
                "import",
                "raw",
                "--arch",
                TEST_ARCH,
                src.to_str().unwrap(),
                "-t",
                SMOKE_SOURCE_REF,
            ],
        ),
    );
    tag_digest(graphroot, SMOKE_SOURCE_REF)
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
        format!("version: 1\nimages:\n  {SMOKE_SOURCE_REF}: {index}\n"),
    )
    .unwrap();
    // The tiny seed has no shell. Retain the upstream COPY, omitting its
    // Fedora-specific exec assertions, and commit those exact fixture inputs.
    let plan = std::fs::read_to_string(context.join(SMOKE_PLAN)).unwrap();
    std::fs::write(
        context.join(SMOKE_PLAN),
        format!(
            "{}image: rootfs\n",
            plan.split("    - exec:").next().unwrap()
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
