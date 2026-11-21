// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Process statuses for Pichi errors and informational CLI requests.

#![allow(clippy::unwrap_used)]

#[path = "support/storage.rs"]
mod storage;
use storage::Storage as _;

use assert_cmd::Command;

#[test]
fn attach_rejects_conflicting_input_choices() {
    let result = Command::cargo_bin("pichi")
        .unwrap()
        .args(["attach", "-i", "--no-stdin", "workload"])
        .assert()
        .code(125);
    assert!(String::from_utf8_lossy(&result.get_output().stderr).contains("cannot be used with"));
}

#[test]
fn usage_and_command_errors_exit_125() {
    let root = tempfile::tempdir().unwrap();
    for args in [
        vec!["run"],
        vec!["--does-not-exist"],
        vec!["inspect", "missing:latest"],
    ] {
        Command::cargo_bin("pichi")
            .unwrap()
            .storage(root.path())
            .args(args)
            .assert()
            .code(125);
    }
}

#[test]
fn configuration_errors_exit_125_but_help_and_version_succeed() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("pichi")).unwrap();
    std::fs::write(root.path().join("pichi/config.toml"), "invalid TOML [").unwrap();
    let failed = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_CONFIG_HOME", root.path())
        .env("APPDATA", root.path())
        .args(["system", "info"])
        .assert()
        .code(125);
    assert!(
        String::from_utf8_lossy(&failed.get_output().stderr)
            .contains("failed to load pichi config")
    );
    for args in [vec!["--help"], vec!["--version"], vec!["run", "--help"]] {
        Command::cargo_bin("pichi")
            .unwrap()
            .env("XDG_CONFIG_HOME", root.path())
            .env("APPDATA", root.path())
            .args(args)
            .assert()
            .success();
    }
}
