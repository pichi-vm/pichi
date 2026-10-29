// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! A rejected guest plan must fail the host command and show its console error.
#![cfg(all(feature = "vm-tests", target_os = "linux", target_arch = "x86_64"))]

mod common;

use std::io::{Read, Seek};
use wait_timeout::ChildExt as _;

use common::{
    SMOKE_CONTEXT, SMOKE_LOCK, SMOKE_PLAN, kvm_available, prepare_smoke_repo, pull_build_image,
    seed_smoke_source_carapace,
};

#[test]
fn pichi_build_rejected_plan_fails_with_console_reason() {
    if !kvm_available() {
        eprintln!("skip: no usable /dev/kvm");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let graphroot = tmp.path().join("graph");
    std::fs::create_dir_all(&graphroot).unwrap();
    // This regression must exercise the guest, so an unavailable appliance
    // is a failure rather than a successful skip.
    assert!(
        pull_build_image(&graphroot),
        "failure test needs build image"
    );
    let index = seed_smoke_source_carapace(&graphroot, tmp.path());
    let (repo, _) = prepare_smoke_repo(tmp.path(), &index);
    std::fs::write(repo.join(SMOKE_CONTEXT).join(SMOKE_PLAN), b"tasks: [\n").unwrap();
    for args in [
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "Use an invalid plan",
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

    // Use files so a verbose console cannot fill an unread pipe. Bound this
    // test separately from the production 30-minute limit.
    let stdout = tempfile::tempfile().unwrap();
    let mut stderr = tempfile::tempfile().unwrap();
    let started = std::time::Instant::now();
    let mut child = std::process::Command::new(common::PICHI_BIN)
        .env("XDG_DATA_HOME", &graphroot)
        .env("PICHI_DILLO", common::DILLO_BIN)
        .args(["build"])
        .arg(&repo)
        .args([
            "-t",
            "failure:out",
            "--ref",
            "HEAD",
            "--context",
            SMOKE_CONTEXT,
            "-p",
            SMOKE_PLAN,
            "-l",
            SMOKE_LOCK,
            "--memory",
            "2048",
            "--cpus",
            "1",
        ])
        .stdout(stdout)
        .stderr(stderr.try_clone().unwrap())
        .spawn()
        .unwrap();
    let status = child
        .wait_timeout(std::time::Duration::from_secs(180))
        .unwrap();
    if status.is_none() {
        child.kill().unwrap();
        child.wait().unwrap();
    }
    stderr.rewind().unwrap();
    let mut log = String::new();
    stderr.read_to_string(&mut log).unwrap();
    let status = status.unwrap_or_else(|| panic!("bad-plan build timed out:\n{log}"));
    eprintln!("bad-plan build finished in {:?}:\n{log}", started.elapsed());
    assert!(!status.success(), "accepted invalid guest plan");
    assert!(
        log.contains("build VM console tail"),
        "missing tail:\n{log}"
    );
    assert!(
        log.contains("Error: conglobate: "),
        "missing main reason:\n{log}"
    );
    assert!(
        log.contains("output directory is empty"),
        "missing verdict:\n{log}"
    );
    let inspect = common::pichi(&graphroot, &[], &["inspect", "failure:out"]);
    assert!(!inspect.status.success(), "published a failed build");
}
