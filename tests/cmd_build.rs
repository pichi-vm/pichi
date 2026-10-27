// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! CLI surface for `pichi build`: fixed default filenames, optional
//! `-f`/`--file` and `-l`/`--lock`, and auto-pull of a missing appliance.

#![allow(clippy::unwrap_used)]

use assert_cmd::Command;

#[test]
fn build_help_documents_defaults_and_drops_recipe_flag() {
    let assert = Command::cargo_bin("pichi")
        .unwrap()
        .args(["build", "--help"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("-f, --file") && stdout.contains("pichi.yaml"),
        "recipe default missing:\n{stdout}"
    );
    assert!(
        stdout.contains("-l, --lock") && stdout.contains("pichi.lock"),
        "lock default missing:\n{stdout}"
    );
    assert!(
        stdout.contains("-b, --build-image") && stdout.contains("Build appliance image"),
        "appliance help missing:\n{stdout}"
    );
    assert!(
        stdout.contains("pulls it"),
        "help should say a missing appliance is pulled:\n{stdout}"
    );
    assert!(
        !stdout.contains("--recipe") && !stdout.contains("PICHI_BUILD_IMAGE"),
        "removed knob still documented:\n{stdout}"
    );
    for flag in [
        "--ref",
        "--subdir",
        "--tag",
        "--location",
        "--memory",
        "--cpus",
    ] {
        assert!(stdout.contains(flag), "missing {flag}:\n{stdout}");
    }
}

#[test]
fn build_missing_appliance_attempts_pull() {
    let tmp = tempfile::TempDir::new().unwrap();
    let assert = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args([
            "build",
            tmp.path().to_str().unwrap(),
            "--ref",
            "main",
            "-t",
            "out:1",
            "-b",
            "127.0.0.1:1/ghost:1",
        ])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("pulling") && stderr.contains("auto-pull"),
        "stderr should show an auto-pull attempt: {stderr}"
    );
}
