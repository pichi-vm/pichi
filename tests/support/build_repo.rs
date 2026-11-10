// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Minimal committed plan for host build-launch fixtures.

use assert_cmd::Command;
use std::path::Path;

pub(crate) fn prepare(directory: &Path) -> String {
    std::fs::write(
        directory.join("pichi.yaml"),
        "version: 1\nimage: registry.test/base:1\n",
    )
    .unwrap();
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["add", "pichi.yaml"],
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "fixture",
        ],
    ] {
        Command::new("git")
            .current_dir(directory)
            .args(args)
            .assert()
            .success();
    }
    let head = Command::new("git")
        .current_dir(directory)
        .args(["rev-parse", "HEAD"])
        .assert()
        .success();
    String::from_utf8(head.get_output().stdout.clone())
        .unwrap()
        .trim()
        .to_owned()
}
