// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Process statuses for Pichi errors and informational CLI requests.

#![allow(clippy::unwrap_used)]

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
        vec!["ps", "-q", "--format", "json"],
        vec!["ps", "--format", "unsupported"],
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
fn empty_workload_json_is_an_array() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("pichi")).unwrap();
    std::fs::write(
        root.path().join("pichi/config.toml"),
        format!(
            "[storage]\ngraphroot = {}\n",
            serde_json::to_string(&root.path().join("cache")).unwrap()
        ),
    )
    .unwrap();
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", root.path())
        .env("XDG_CONFIG_HOME", root.path())
        .env("LOCALAPPDATA", root.path())
        .env("APPDATA", root.path())
        .args(["ps", "--format", "json"])
        .assert()
        .success()
        .stdout("[]\n");
}

#[cfg(target_os = "linux")]
#[test]
fn workload_json_write_errors_exit_125() {
    use std::os::unix::fs::DirBuilderExt as _;

    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("pichi")).unwrap();
    std::fs::write(
        root.path().join("pichi/config.toml"),
        format!(
            "[storage]\ngraphroot = {}\n",
            serde_json::to_string(&root.path().join("cache")).unwrap()
        ),
    )
    .unwrap();
    for existing in [false, true] {
        if existing {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(root.path().join("cache.workloads"))
                .unwrap();
        }
        let output = std::process::Command::new(assert_cmd::cargo::cargo_bin!("pichi"))
            .env("XDG_DATA_HOME", root.path())
            .env("XDG_CONFIG_HOME", root.path())
            .args(["ps", "--format", "json"])
            .stdout(
                std::fs::OpenOptions::new()
                    .write(true)
                    .open("/dev/full")
                    .unwrap(),
            )
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(125), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("No space left on device"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
    }
}
