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
    manifest: Manifest,
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
            manifest,
        }
    }

    async fn build_product(&self) -> PathBuf {
        let product = self.root.path().join("product");
        let blobs = product.join("blobs/sha256");
        std::fs::create_dir_all(&blobs).unwrap();
        let mut manifest = self.manifest.clone();
        manifest.annotations.extend([
            (
                "dev.pichi.build.commit".into(),
                format!("sha1:{}", "1".repeat(40)),
            ),
            ("dev.pichi.build.context".into(), ".".into()),
            ("dev.pichi.build.plan".into(), "pichi.yaml".into()),
            ("dev.pichi.build.lock".into(), "pichi.lock".into()),
        ]);
        let store = FilesystemBlobStore::new(self.root.path().join("pichi/storage"));
        for layer in &manifest.layers {
            let digest: Digest = layer.digest_str().parse().unwrap();
            std::fs::write(
                blobs.join(digest.hex()),
                store.get_blob(&digest).await.unwrap(),
            )
            .unwrap();
        }
        let bytes = serde_json::to_vec(&manifest).unwrap();
        let digest = Digest::Sha256(Sha256::digest(&bytes).into());
        std::fs::write(blobs.join(digest.hex()), &bytes).unwrap();
        std::fs::write(
            product.join("oci-layout"),
            br#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::write(
            product.join("index.json"),
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.index.v1+json",
                "manifests": [{
                    "mediaType": manifest.media_type,
                    "artifactType": manifest.artifact_type,
                    "digest": digest.to_string(),
                    "size": bytes.len(),
                }],
            }))
            .unwrap(),
        )
        .unwrap();
        product
    }

    fn build_command(&self, request: &impl serde::Serialize) -> Command {
        let mut encoded = Vec::new();
        ciborium::into_writer(request, &mut encoded).unwrap();
        let mut command = self.command();
        command
            .arg("build")
            .arg(self.root.path())
            .args(["--ref", "HEAD", "-b", "fixture:1", "-t", "built:1"])
            .env("BUILD_REQUEST", hex::encode(encoded));
        command
    }

    fn record(&self) -> PathBuf {
        let records: Vec<_> = std::fs::read_dir(self.root.path().join("pichi/storage.workloads"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(records.len(), 1);
        records[0].clone()
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
async fn build_port_remains_direct_and_guest_first_through_monitor_launch() {
    let fixture = WorkloadFixture::new(r"
import os, re, select, shutil, signal, sys
signal.alarm(5)
console = sys.argv[sys.argv.index('--console') + 1]
rx = int(re.search(r'rx=pipe:(\d+)', console)[1])
tx = int(re.search(r'tx=pipe:(\d+)', console)[1])
assert not select.select([rx], [], [], 0.1)[0], 'unsolicited host response'
os.write(tx, bytes.fromhex(os.environ['BUILD_REQUEST']))
os.close(tx)
response = b''
while True:
    chunk = os.read(rx, 4096)
    if not chunk: break
    response += chunk
assert response == bytes.fromhex(os.environ['BUILD_RESPONSE']), response.hex()
output = next(arg.removeprefix('tag=output,source=') for arg in sys.argv if arg.startswith('tag=output,source='))
shutil.copytree(os.environ['BUILD_PRODUCT'], output, dirs_exist_ok=True)
os.write(1, b'opaque build port\n')
").await;
    let response = pichi_launch::Response::Ok(pichi_launch::Input {
        r#ref: "HEAD".into(),
        plan: "pichi.yaml".into(),
        lock: "pichi.lock".into(),
        context: ".".into(),
        location: None,
    });
    let mut encoded = Vec::new();
    ciborium::into_writer(&response, &mut encoded).unwrap();
    let built = fixture
        .build_command(&pichi_launch::Request::from(pichi_launch::Build::default()))
        .env("BUILD_RESPONSE", hex::encode(encoded))
        .env("BUILD_PRODUCT", fixture.build_product().await)
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&built.get_output().stdout);
    assert!(stdout.contains("opaque build port\n"), "{stdout}");
    assert!(
        stdout.contains("pichi build: imported built:1 -> sha256:"),
        "{stdout}"
    );
    let record = fixture.record();
    let result: madrillo::Completion =
        serde_json::from_slice(&std::fs::read(record.join("monitor/result.json")).unwrap())
            .unwrap();
    assert_eq!(result.leader, madrillo::LeaderResult::Exit { code: 0 });
    assert_eq!(
        record.join("output").exists(),
        result.cleanup != madrillo::CleanupResult::Complete
    );
    let tags = FilesystemTagDb::open(fixture.root.path().join("pichi/storage")).unwrap();
    assert!(
        tags.resolve_tag("docker.io/library/built:1")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn invalid_build_request_terminates_owned_workload_without_import() {
    let fixture = WorkloadFixture::new(
        r"
import os, re, signal, sys
signal.alarm(5)
console = sys.argv[sys.argv.index('--console') + 1]
tx = int(re.search(r'tx=pipe:(\d+)', console)[1])
os.write(tx, bytes.fromhex(os.environ['BUILD_REQUEST']))
while True: signal.pause()
",
    )
    .await;
    let built = fixture
        .build_command(&("build", 2_u16, ()))
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&built.get_output().stderr);
    assert!(
        stderr.contains("reading build-port guest request"),
        "{stderr}"
    );
    let record = fixture.record();
    let result: madrillo::Completion =
        serde_json::from_slice(&std::fs::read(record.join("monitor/result.json")).unwrap())
            .unwrap();
    assert_eq!(
        result.leader,
        madrillo::LeaderResult::Signal {
            signal: 9,
            core_dumped: false
        }
    );
    assert_eq!(result.cause, madrillo::CompletionCause::Forced);
    assert!(record.join("output").is_dir());
    let tags = FilesystemTagDb::open(fixture.root.path().join("pichi/storage")).unwrap();
    assert!(
        tags.resolve_tag("docker.io/library/built:1")
            .await
            .unwrap()
            .is_none()
    );
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
    fixture
        .command()
        .args(["logs", id])
        .assert()
        .success()
        .stdout("detached output\n");
    let listed = fixture.command().arg("ps").assert().success();
    let rows = String::from_utf8(listed.get_output().stdout.clone()).unwrap();
    assert!(rows.contains(id) && rows.contains("exited"), "{rows}");
}

#[tokio::test]
async fn logs_preserve_binary_console_streams_after_monitor_exit() {
    let fixture = WorkloadFixture::new(
        "import os\nos.write(1, b'first\\x00\\xff\\npartial')\nos.write(2, b'error\\x00\\xfe\\n')\nos._exit(37)",
    ).await;
    let output = fixture
        .command()
        .args(["run", "-d", "fixture:1"])
        .assert()
        .success();
    let id = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    fixture
        .command()
        .args(["wait", id.trim()])
        .assert()
        .success();
    fixture
        .command()
        .args(["logs", id.trim()])
        .assert()
        .success()
        .stdout(b"first\0\xff\npartial".as_slice())
        .stderr(b"error\0\xfe\n".as_slice());
}
