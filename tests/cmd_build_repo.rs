// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Host checks and mount wiring with real Git stores; no guest boot required.

use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
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

    #[cfg(unix)]
    async fn fake_build(&self, data: &Path) -> (assert_cmd::assert::Assert, PathBuf) {
        use conglobate::artifact::{
            ConfigDescriptor, Digest, Layer, MEDIA_TYPE_PICHI_ARTIFACT_V1, Manifest, PmiDescriptor,
            ScuteAnnotations, ScuteDescriptor,
        };
        use pichi_storage::{BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};
        use sha2::{Digest as _, Sha256};
        use std::os::unix::fs::PermissionsExt;

        let graphroot = data.join("pichi/storage");
        let store = FilesystemBlobStore::new(&graphroot);
        let pmi = b"fixture PMI";
        let pmi_digest = Digest::Sha256(Sha256::digest(pmi).into());
        store.put_blob(&pmi_digest, pmi).await.unwrap();
        let cow = vec![0; 4096];
        let cow_digest = Digest::Sha256(Sha256::digest(&cow).into());
        let root = hex::encode(
            Sha256::new()
                .chain_update([0; 32])
                .chain_update(&cow)
                .finalize(),
        );
        store.put_blob(&cow_digest, &cow).await.unwrap();
        let manifest = Manifest {
            schema_version: 2,
            media_type: "application/vnd.oci.image.manifest.v1+json".into(),
            artifact_type: MEDIA_TYPE_PICHI_ARTIFACT_V1.into(),
            config: ConfigDescriptor::canonical(),
            layers: vec![
                Layer::Pmi(PmiDescriptor {
                    digest: pmi_digest.to_string(),
                    size: pmi.len() as u64,
                }),
                Layer::Scute(ScuteDescriptor {
                    digest: cow_digest.to_string(),
                    size: cow.len() as u64,
                    annotations: ScuteAnnotations {
                        data_blocks: 1.try_into().unwrap(),
                        salt: "00".repeat(32),
                    },
                }),
            ],
            annotations: [
                ("dev.pichi.carapace.verity.algo", "sha256"),
                ("dev.pichi.carapace.verity.data-block-size", "4096"),
                ("dev.pichi.carapace.verity.hash-block-size", "4096"),
                ("dev.pichi.carapace.verity.version", "1"),
                ("dev.pichi.carapace.verity.hash-type", "1"),
                ("dev.pichi.carapace.verity.hash", root.as_str()),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
        };
        let bytes = serde_json::to_vec(&manifest).unwrap();
        let manifest_digest = Digest::Sha256(Sha256::digest(&bytes).into());
        store.put_blob(&manifest_digest, &bytes).await.unwrap();
        let architecture = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => other,
        };
        let index = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [{
                "mediaType": manifest.media_type,
                "artifactType": manifest.artifact_type,
                "digest": manifest_digest.to_string(),
                "size": bytes.len(),
                "platform": { "os": "pichi", "architecture": architecture },
            }],
        }))
        .unwrap();
        let digest = Digest::Sha256(Sha256::digest(&index).into());
        store.put_blob(&digest, &index).await.unwrap();
        // The current host lock glue requires at least one cached source.
        self.write(
            "pichi.lock",
            format!("version: 1\nimages:\n  127.0.0.1:1/appliance:1:\n    index: {digest}\n")
                .as_bytes(),
        );
        FilesystemTagDb::open(&graphroot)
            .unwrap()
            .set_tag("127.0.0.1:1/appliance:1", &digest)
            .await
            .unwrap();

        let stub = data.join("dillo");
        let record = data.join("argv");
        std::fs::write(&stub, b"#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$BUILD_TEST_ARGV\"\necho 'submodule \"vendor/lib\": needed store /run/pichi/repo/.git/modules/store-name is unavailable: missing' >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let output = self
            .build(self.0.path(), data)
            .env("PICHI_DILLO", stub)
            .env("BUILD_TEST_ARGV", &record)
            .timeout(std::time::Duration::from_secs(15))
            .assert()
            .failure();
        assert!(
            record.exists(),
            "{}",
            String::from_utf8_lossy(&output.get_output().stderr)
        );
        (output, record)
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
fn context_directory_and_alternates_fail_early() {
    let repo = Repository::new();
    let context_dir = repo.0.path().join("plans");
    std::fs::create_dir(&context_dir).unwrap();
    repo.fails_before_pull(&context_dir, &["not the top", "--context"]);
    repo.write(".git/objects/info/alternates", b"/unavailable/objects\n");
    repo.fails_before_pull(
        repo.0.path(),
        &["alternates", "git repack -a -d", "full clone"],
    );
}

#[cfg(unix)]
#[tokio::test]
async fn repo_mount_keeps_existing_module_store_and_console_output() {
    let repo = Repository::new();
    repo.add_module(&Repository::new());
    let data = tempfile::tempdir().unwrap();
    let (output, record) = repo.fake_build(data.path()).await;
    let argv = std::fs::read_to_string(record).unwrap();
    let mounts: Vec<_> = argv
        .lines()
        .filter(|line| line.starts_with("tag="))
        .collect();
    assert_eq!(mounts.len(), 3, "{argv}");
    let root = repo.0.path().canonicalize().unwrap();
    assert_eq!(
        mounts[0],
        format!("tag=repo,source={},readonly", root.display())
    );
    assert!(root.join(".git/modules/store-name/objects").is_dir());
    let stderr = String::from_utf8_lossy(&output.get_output().stderr);
    assert!(
        stderr.contains("submodule \"vendor/lib\": needed store"),
        "{stderr}"
    );
    assert!(!stderr.contains("Possible unstaged submodule"), "{stderr}");
}

#[cfg(unix)]
#[tokio::test]
async fn unused_missing_store_reaches_launcher_with_static_guidance() {
    let repo = Repository::new();
    repo.add_module(&Repository::new());
    let checkout = repo.0.path().join("vendor/lib");
    std::fs::remove_dir_all(&checkout).unwrap();
    let store = repo.0.path().join(".git/modules/store-name");
    std::fs::remove_dir_all(&store).unwrap();
    // Host preflight does not interpret submodule mappings.
    repo.write(".gitmodules", b"invalid working metadata\n");
    let data = tempfile::tempdir().unwrap();
    let (output, record) = repo.fake_build(data.path()).await;
    assert!(
        record.exists(),
        "unused missing modules must not block preflight"
    );
    let stderr = String::from_utf8_lossy(&output.get_output().stderr);
    assert!(
        stderr.contains("git submodule update --init --recursive -- <path>"),
        "{stderr}"
    );
    assert!(!stderr.contains("Possible unstaged submodule"), "{stderr}");
    assert!(!store.exists());
    assert!(!checkout.exists());
}
