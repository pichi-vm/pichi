// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! End-to-end smoke: `pichi build` against Conglobate's locked context inputs
//! (`tests/distro/context` + `qualification.local/fedora:43`) with the real
//! appliance selected by `common::prepare_build_image`.
//!
//! Host duties under test:
//! - import a tiny source carapace, retain its genuine platform index and
//!   commit the matching lock in the smoke repository;
//! - thin-launcher wires: virtiofs repo/cache/output + `dev.pichi.build`;
//! - complete guest OCI import and host verity rebuild.
//!
//! Gated on the `vm-tests` feature; skips if `/dev/kvm` is unavailable.
//! x86_64 + KVM only.
//!
#![cfg(all(feature = "vm-tests", target_os = "linux", target_arch = "x86_64"))]

mod common;

use common::{
    SMOKE_CONTEXT, SMOKE_LOCK, SMOKE_PLAN, SMOKE_SOURCE_REF, assert_pichi_ok, kvm_available, pichi,
    prepare_build_image, prepare_smoke_repo, seed_smoke_source_carapace,
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

    // Import the selected appliance into this test's cache.
    prepare_build_image(g);

    // Seed an authenticated index and record its actual digest.
    let index = seed_smoke_source_carapace(g, tmp.path()).await;

    // Use the same Conglobate revision as the engine dependency.
    // That tree stores `plan.yaml` / `lock.yaml`; the smoke passes
    // those paths with `-p` / `-l`.
    let (repo, _) = prepare_smoke_repo(tmp.path(), &index);
    assert_pichi_ok(
        "update",
        &pichi(
            g,
            &[],
            &[
                "update",
                repo.join(SMOKE_CONTEXT).to_str().unwrap(),
                "-p",
                SMOKE_PLAN,
                "-l",
                SMOKE_LOCK,
                "--pull=never",
            ],
        ),
    );
    let lock: conglobate::build::Lock = serde_saphyr::from_slice(
        &std::fs::read(repo.join(SMOKE_CONTEXT).join(SMOKE_LOCK)).unwrap(),
    )
    .unwrap();
    assert_eq!(
        lock.images[&conglobate::plan::Remote::new(SMOKE_SOURCE_REF.into()).unwrap()].to_string(),
        index
    );
    assert_pichi_ok("prune after update", &pichi(g, &[], &["system", "prune"]));
    // Build the committed lock produced by update, after cache collection.
    for args in [
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-qm",
            "Record generated lock",
        ],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
    }
    let revision = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(revision.status.success());
    let git_ref = String::from_utf8(revision.stdout)
        .unwrap()
        .trim()
        .to_string();

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
            "--context",
            SMOKE_CONTEXT,
            "-p",
            SMOKE_PLAN,
            "-l",
            SMOKE_LOCK,
            "--build-image",
            common::build_image(),
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
                "official Conglobate smoke: guest did not power off within {}s after launch. Build image: {}. Appliance logs go to the journal and hvc0 console.",
                OFFICIAL_SMOKE_TIMEOUT.as_secs(),
                common::build_image()
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

    // The guest OCI manifest must import under the requested local tag.
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
        "imported smoke:out missing digest; source was {SMOKE_SOURCE_REF}@{index}"
    );
}
