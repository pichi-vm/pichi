// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! CLI surface for `pichi build`: fixed default filenames, optional
//! `-f`/`--file` and `-l`/`--lock`, and auto-pull of a missing appliance.

#![allow(clippy::unwrap_used)]

use assert_cmd::Command;
use clap::{Args, FromArgMatches};
use pichi::cli::BuildArgs;

#[test]
fn build_help_documents_defaults_and_drops_recipe_flag() {
    let assert = Command::cargo_bin("pichi")
        .unwrap()
        .args(["build", "--help"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("-f, --file") && stdout.contains("[default: pichi.yaml]"),
        "recipe default missing:\n{stdout}"
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
        stdout.contains("Subdirectory of <dir> holding the recipe and lock")
            && stdout.contains("[default: .]"),
        "subdirectory help or default missing:\n{stdout}"
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
fn build_subdir_defaults_to_current_directory() {
    for (extra, expected) in [
        (vec![], "."),
        (vec!["--subdir", "recipes/app"], "recipes/app"),
    ] {
        let mut argv = vec!["build", ".", "--ref", "main"];
        argv.extend(extra);
        let matches = BuildArgs::augment_args(clap::Command::new("build"))
            .try_get_matches_from(argv)
            .unwrap();
        let args = BuildArgs::from_arg_matches(&matches).unwrap();
        assert_eq!(args.subdir, expected);
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
fn build_rejects_absolute_recipe_path() {
    let assert = build_with(&["-f", "/etc/pichi.yaml"]).failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("recipe path must be relative: /etc/pichi.yaml"),
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
fn build_accepts_relative_overrides() {
    // Relative overrides pass argument parsing (failure, if any, comes later
    // from the missing appliance/lock, never from path validation).
    let assert = build_with(&["-f", "sub/other.yaml", "-l", "../other.lock"]);
    let out = assert.get_output();
    let stderr = String::from_utf8(out.stderr.clone()).unwrap();
    assert!(
        !stderr.contains("must be relative") && out.status.code() != Some(2),
        "relative override rejected:\n{stderr}"
    );
}

/// Drive and UNC paths are absolute on Windows. `/etc/...` above is the
/// cross-platform rooted form (`has_root` even when `is_absolute` is false).
#[cfg(windows)]
#[test]
fn build_rejects_windows_absolute_paths() {
    for (flag, path, needle) in [
        (
            "-f",
            r"C:\pichi.yaml",
            r"recipe path must be relative: C:\pichi.yaml",
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
