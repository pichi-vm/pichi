// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Host checks and mount wiring with real Git stores; no guest boot required.

use std::path::Path;
use std::process::Command;

struct Repository(tempfile::TempDir);

impl Repository {
    fn new() -> Self {
        let repo = Self(tempfile::tempdir().unwrap());
        repo.git(&["init", "--quiet", "--initial-branch=main"]);
        repo.write("pichi.lock", b"version: 1\nimages: {}\n");
        repo.commit();
        repo
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(self.0.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn write(&self, name: &str, bytes: &[u8]) {
        std::fs::write(self.0.path().join(name), bytes).unwrap();
    }

    fn commit(&self) {
        self.git(&["add", "."]);
        self.git(&["commit", "--quiet", "-m", "fixture"]);
    }

    fn add_module(&self, child: &Self) {
        self.git(&[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "--quiet",
            "--name",
            "store-name",
            child.0.path().to_str().unwrap(),
            "vendor/lib",
        ]);
        self.commit();
    }

    fn build(&self, dir: &Path, data: &Path) -> assert_cmd::Command {
        let mut cmd = assert_cmd::Command::cargo_bin("pichi").unwrap();
        cmd.current_dir(self.0.path())
            .env("XDG_DATA_HOME", data)
            .args(["build"])
            .arg(dir)
            .args([
                "--ref",
                "HEAD",
                "-t",
                "out:1",
                "-b",
                "127.0.0.1:1/appliance:1",
            ]);
        cmd
    }

    fn fails_before_pull(&self, dir: &Path, expected: &[&str]) {
        let data = tempfile::tempdir().unwrap();
        let output = self.build(dir, data.path()).assert().failure();
        let stderr = String::from_utf8_lossy(&output.get_output().stderr);
        for text in expected {
            assert!(stderr.contains(text), "{stderr}");
        }
        assert!(!stderr.contains("pulling"), "{stderr}");
        assert!(!stderr.contains("build VM"), "{stderr}");
    }
}

#[test]
fn worktree_submodule_and_separate_git_directory_fail_early() {
    let repo = Repository::new();
    let data = tempfile::tempdir().unwrap();
    let worktree = data.path().join("worktree");
    repo.git(&[
        "worktree",
        "add",
        "--quiet",
        "--detach",
        worktree.to_str().unwrap(),
    ]);
    repo.fails_before_pull(&worktree, &[".git is a file", "worktrees", "main checkout"]);

    let child = Repository::new();
    repo.add_module(&child);
    repo.fails_before_pull(
        &repo.0.path().join("vendor/lib"),
        &[".git is a file", "submodule checkouts", "main checkout"],
    );

    let separate = data.path().join("separate");
    let git_dir = data.path().join("store");
    repo.git(&[
        "init",
        "--quiet",
        "--separate-git-dir",
        git_dir.to_str().unwrap(),
        separate.to_str().unwrap(),
    ]);
    repo.fails_before_pull(&separate, &[".git is a file", "separate Git directories"]);
}

#[test]
fn context_directory_fails_early() {
    let repo = Repository::new();
    let context_dir = repo.0.path().join("plans");
    std::fs::create_dir(&context_dir).unwrap();
    repo.fails_before_pull(&context_dir, &["not the top", "--context"]);
}
