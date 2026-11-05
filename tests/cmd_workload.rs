// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Actual pichi → madrillo launch and CLI reconnection; Unix fixture programs.
//! PICHI_MADRILLO (or PATH) must provide the real monitor executable.

#![cfg(unix)]
#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used)]

use assert_cmd::Command;
use conglobate::artifact::{
    ConfigDescriptor, Digest, Layer, MEDIA_TYPE_PICHI_ARTIFACT_V1, Manifest, PmiDescriptor,
};
use pichi_storage::{BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};
use sha2::{Digest as _, Sha256};
use std::{os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};

struct WorkloadFixture {
    root: tempfile::TempDir,
    program: PathBuf,
}

impl WorkloadFixture {
    async fn new(program: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let graphroot = root.path().join("pichi/storage");
        let tags = FilesystemTagDb::open(&graphroot).unwrap();
        let blobs = FilesystemBlobStore::new(&graphroot);
        let pmi = b"PMI fixture";
        let digest = Digest::Sha256(Sha256::digest(pmi).into());
        blobs.put_blob(&digest, pmi).await.unwrap();
        let manifest = Manifest {
            schema_version: 2,
            media_type: "application/vnd.oci.image.manifest.v1+json".into(),
            artifact_type: MEDIA_TYPE_PICHI_ARTIFACT_V1.into(),
            config: ConfigDescriptor::canonical(),
            layers: vec![Layer::Pmi(PmiDescriptor {
                digest: digest.to_string(),
                size: pmi.len() as u64,
            })],
            annotations: std::collections::BTreeMap::default(),
        };
        let bytes = serde_json::to_vec(&manifest).unwrap();
        let digest = Digest::Sha256(Sha256::digest(&bytes).into());
        blobs.put_blob(&digest, &bytes).await.unwrap();
        tags.set_tag("docker.io/library/fixture:1", &digest)
            .await
            .unwrap();
        let path = root.path().join("dillo-fixture");
        std::fs::write(&path, format!("#!/usr/bin/env python3\n{program}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            root,
            program: path,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::cargo_bin("pichi").unwrap();
        command
            .env("XDG_DATA_HOME", self.root.path())
            .env("PICHI_DILLO", &self.program)
            .timeout(Duration::from_secs(15));
        command
    }
}

#[tokio::test]
async fn interactive_binary_input_and_native_exit_survive_monitor_transport() {
    let fixture = WorkloadFixture::new(
        "import os, signal\nsignal.alarm(5)\ndata = b''\nwhile len(data) < 5:\n data += os.read(0, 5 - len(data))\nos.write(1, data)\nos._exit(37)",
    ).await;
    fixture
        .command()
        .args(["run", "-i", "fixture:1"])
        .write_stdin([0, 255, 16, 17, 10])
        .assert()
        .code(37)
        .stdout([0, 255, 16, 17, 10].as_slice());
}

#[tokio::test]
async fn detached_launch_is_discoverable_and_later_wait_reads_retained_exit() {
    let fixture = WorkloadFixture::new(
        "import os, signal, time\nsignal.alarm(5)\ntime.sleep(0.2)\nos.write(1, b'detached output\\n')\nos._exit(23)",
    ).await;
    let output = fixture
        .command()
        .args(["run", "-d", "fixture:1"])
        .assert()
        .success();
    let id = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let id = id.trim();
    uuid::Uuid::parse_str(id).unwrap();
    fixture
        .command()
        .args(["wait", id])
        .assert()
        .success()
        .stdout("23\n");
    // The endpoint is gone after monitor exit; the same command uses state.
    fixture
        .command()
        .args(["wait", id])
        .assert()
        .success()
        .stdout("23\n");
    fixture
        .command()
        .args(["attach", id])
        .assert()
        .code(23)
        .stdout("");
    let listed = fixture.command().arg("ps").assert().success();
    let rows = String::from_utf8(listed.get_output().stdout.clone()).unwrap();
    assert!(rows.contains(id) && rows.contains("exited"), "{rows}");
}
