// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Process statuses for Pichi errors and informational CLI requests.

#![allow(clippy::unwrap_used)]

use assert_cmd::Command;

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
            .env("XDG_DATA_HOME", root.path())
            .env("XDG_CONFIG_HOME", root.path())
            .env("LOCALAPPDATA", root.path())
            .env("APPDATA", root.path())
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

#[test]
fn retired_storage_option_exits_before_creating_or_modifying_storage() {
    let root = tempfile::tempdir().unwrap();
    let disk = root.path().join("external.raw");
    std::fs::write(&disk, b"existing guest data").unwrap();
    let failed = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", root.path())
        .env("XDG_CONFIG_HOME", root.path())
        .env("LOCALAPPDATA", root.path())
        .env("APPDATA", root.path())
        .args(["run", "--storage-file"])
        .arg(&disk)
        .arg("missing:latest")
        .assert()
        .code(125);
    let diagnostic = String::from_utf8_lossy(&failed.get_output().stderr);
    assert!(
        diagnostic.contains("--storage-file is no longer supported"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains("pichi run and pichi start"),
        "{diagnostic}"
    );
    assert_eq!(std::fs::read(&disk).unwrap(), b"existing guest data");
    assert!(
        !root.path().join("pichi").exists(),
        "rejected option created storage"
    );
}
