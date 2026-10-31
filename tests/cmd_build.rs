// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! CLI surface for `pichi build`: fixed default filenames, optional
//! `-p`/`--plan` and `-l`/`--lock`, and auto-pull of a missing appliance.

#![allow(clippy::unwrap_used)]

use assert_cmd::Command;
use clap::{Args, FromArgMatches};
use pichi::cli::BuildArgs;

#[test]
fn build_help_documents_plan_context_and_defaults() {
    let assert = Command::cargo_bin("pichi")
        .unwrap()
        .args(["build", "--help"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("-p, --plan") && stdout.contains("[default: pichi.yaml]"),
        "plan default missing:\n{stdout}"
    );
    assert!(
        stdout.contains("-l, --lock") && stdout.contains("[default: pichi.lock]"),
        "lock default missing:\n{stdout}"
    );
    assert!(
        !stdout.contains("(default `pichi.yaml`)") && !stdout.contains("(default `pichi.lock`)"),
        "defaults should only appear in clap annotations:\n{stdout}"
    );
    assert!(
        stdout.contains("Context directory of <dir> holding the plan and lock")
            && stdout.contains("[default: .]"),
        "context help or default missing:\n{stdout}"
    );
    let usage = stdout
        .lines()
        .find(|line| line.starts_with("Usage:"))
        .unwrap();
    assert!(
        usage.contains("--ref <GIT_REF>") && !usage.contains("[--ref"),
        "usage must require --ref:\n{stdout}"
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
        stdout.contains("Top of the main git checkout")
            && stdout.contains("pichi never fetches them"),
        "checkout and submodule guidance missing:\n{stdout}"
    );
    assert!(
        !["-f, --file", "--recipe", "--subdir", "PICHI_BUILD_IMAGE"]
            .iter()
            .any(|flag| stdout.contains(flag)),
        "removed knob still documented:\n{stdout}"
    );
    for flag in [
        "--ref",
        "--context",
        "--tag",
        "--location",
        "--memory",
        "--cpus",
    ] {
        assert!(stdout.contains(flag), "missing {flag}:\n{stdout}");
    }
}

#[test]
fn build_context_defaults_to_current_directory() {
    for (extra, expected) in [(vec![], "."), (vec!["--context", "plans/app"], "plans/app")] {
        let mut argv = vec!["build", ".", "--ref", "main"];
        argv.extend(extra);
        let matches = BuildArgs::augment_args(clap::Command::new("build"))
            .try_get_matches_from(argv)
            .unwrap();
        let args = BuildArgs::from_arg_matches(&matches).unwrap();
        assert_eq!(args.context, expected);
    }
}

#[test]
fn build_requires_ref_during_argument_parsing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let assert = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["build", tmp.path().to_str().unwrap()])
        .assert()
        .code(2);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("required arguments were not provided")
            && stderr.contains("--ref <GIT_REF>")
            && !stderr.contains("BUILD.md"),
        "missing --ref should produce a clap error:\n{stderr}"
    );
}

#[test]
fn build_missing_appliance_attempts_pull() {
    let tmp = tempfile::TempDir::new().unwrap();
    let git = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .arg(tmp.path())
        .status()
        .unwrap();
    assert!(git.success());
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

fn build_with(extra: &[&str]) -> assert_cmd::assert::Assert {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut args = vec!["build", tmp.path().to_str().unwrap(), "--ref", "HEAD"];
    args.extend_from_slice(extra);
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(&args)
        .assert()
}

#[test]
fn build_rejects_absolute_plan_path() {
    let assert = build_with(&["-p", "/etc/pichi.yaml"]).failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("plan path must be relative: /etc/pichi.yaml"),
        "unexpected stderr:\n{stderr}"
    );
}

#[test]
fn build_rejects_absolute_lock_path() {
    let assert = build_with(&["--lock", "/etc/pichi.lock"]).failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("lock path must be relative: /etc/pichi.lock"),
        "unexpected stderr:\n{stderr}"
    );
}

#[test]
fn build_plan_and_lock_defaults_and_overrides() {
    for (extra, plan, lock) in [
        (vec![], "pichi.yaml", "pichi.lock"),
        (
            vec!["-p", "sub/other.yaml", "-l", "../other.lock"],
            "sub/other.yaml",
            "../other.lock",
        ),
        (
            vec!["--plan", "sub/other.yaml", "--lock", "../other.lock"],
            "sub/other.yaml",
            "../other.lock",
        ),
    ] {
        let mut argv = vec!["build", ".", "--ref", "main"];
        argv.extend(extra);
        let matches = BuildArgs::augment_args(clap::Command::new("build"))
            .try_get_matches_from(argv)
            .unwrap();
        let args = BuildArgs::from_arg_matches(&matches).unwrap();
        assert_eq!(args.plan, plan);
        assert_eq!(args.lock, lock);
    }
}

#[test]
fn build_rejects_legacy_flags() {
    for flag in ["-f", "--file", "--recipe", "--subdir", "--subdirectory"] {
        let err = BuildArgs::augment_args(clap::Command::new("build"))
            .try_get_matches_from(["build", ".", "--ref", "main", flag, "path"])
            .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }
}

/// Drive and UNC paths are absolute on Windows. `/etc/...` above is the
/// cross-platform rooted form (`has_root` even when `is_absolute` is false).
#[cfg(windows)]
#[test]
fn build_rejects_windows_absolute_paths() {
    for (flag, path, needle) in [
        (
            "-p",
            r"C:\pichi.yaml",
            r"plan path must be relative: C:\pichi.yaml",
        ),
        (
            "--lock",
            r"\\server\share\pichi.lock",
            r"lock path must be relative: \\server\share\pichi.lock",
        ),
    ] {
        let assert = build_with(&[flag, path]).failure();
        let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
        assert!(
            stderr.contains(needle),
            "unexpected stderr for {path}:\n{stderr}"
        );
    }
}
