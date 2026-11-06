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
        let mut command = Command::from_std(self.native_command());
        command.timeout(Duration::from_secs(15));
        command
    }

    fn native_command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_pichi"));
        command
            .env("XDG_DATA_HOME", self.root.path())
            .env("PICHI_DILLO", &self.program);
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
async fn caller_group_loss_preserves_workload_and_reconnection() {
    use nix::{
        sys::signal::{Signal, killpg},
        unistd::Pid,
    };
    use std::{fs::File, os::unix::process::CommandExt, process::Stdio, time::Instant};
    use wait_timeout::ChildExt;

    struct Caller(std::process::Child);
    impl Drop for Caller {
        fn drop(&mut self) {
            if matches!(self.0.try_wait(), Ok(None)) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }

    for signal in [Signal::SIGKILL, Signal::SIGTERM, Signal::SIGHUP] {
        let fixture = WorkloadFixture::new(
            "import os, signal\nsignal.alarm(30)\nos.write(1, b'ready\\n')\nassert os.read(0, 6) == b'again\\n'\nos.write(1, b'reconnected\\n')\nos._exit(37)",
        ).await;
        let output = fixture.root.path().join("caller.out");
        let diagnostics = fixture.root.path().join("caller.err");
        let (input, writer) = std::io::pipe().unwrap();
        let mut caller = Caller(
            fixture
                .native_command()
                .args(["run", "-i", "fixture:1"])
                .process_group(0)
                .stdin(Stdio::from(input))
                .stdout(File::create(&output).unwrap())
                .stderr(File::create(&diagnostics).unwrap())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::fs::read(&output).unwrap() != b"ready\n" {
            assert!(
                caller.0.try_wait().unwrap().is_none(),
                "{}",
                std::fs::read_to_string(&diagnostics).unwrap()
            );
            assert!(Instant::now() < deadline, "foreground output deadline");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let record = fixture.record();
        let id = record.file_name().unwrap().to_str().unwrap();
        // The caller is still our unreaped child, so its process-group ID
        // cannot have been reused. Madrillo must have its own session.
        killpg(Pid::from_raw(i32::try_from(caller.0.id()).unwrap()), signal).unwrap();
        let status = caller
            .0
            .wait_timeout(Duration::from_secs(5))
            .unwrap()
            .expect("caller did not exit after group signal");
        assert!(!status.success());
        drop(writer);
        assert!(
            !record.join("monitor/result.json").exists(),
            "caller death ended workload"
        );
        fixture
            .command()
            .args(["attach", "-i", id])
            .write_stdin(b"again\n")
            .assert()
            .code(37)
            .stdout("reconnected\n");
        fixture
            .command()
            .args(["wait", id])
            .assert()
            .success()
            .stdout("37\n");
    }
}

#[tokio::test]
async fn terminal_restoration_and_hangup_preserve_workload() {
    let fixture = WorkloadFixture::new(
        r"
import os, select, signal, socket, struct, sys
signal.alarm(45)
endpoint = sys.argv[sys.argv.index('--console-resize') + 1]
listener = socket.socket(socket.AF_UNIX)
listener.bind(endpoint)
os.chmod(endpoint, 0o600)
listener.listen(2)
os.write(1, b'console-ready\n')
try:
    while True:
        ready, _, _ = select.select([0, listener], [], [])
        if 0 in ready:
            data = os.read(0, 8192)
            if not data: break
            os.write(1, data)
        if listener in ready:
            client, _ = listener.accept()
            with client:
                client.settimeout(1)
                data = b''
                while len(data) < 4:
                    chunk = client.recv(4 - len(data))
                    assert chunk, 'incomplete terminal size'
                    data += chunk
                cols, rows = struct.unpack('<HH', data)
                os.write(1, f'size={cols}x{rows}\n'.encode())
finally:
    listener.close()
    os.unlink(endpoint)
",
    )
    .await;
    Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/workload_terminal.py"
        ))
        .env("XDG_DATA_HOME", fixture.root.path())
        .env("PICHI_DILLO", &fixture.program)
        .env("PICHI_BIN", env!("CARGO_BIN_EXE_pichi"))
        .timeout(Duration::from_secs(35))
        .assert()
        .success();
}

#[tokio::test]
async fn detached_exit_removes_the_child_resize_socket() {
    let fixture = WorkloadFixture::new(
        r"
import os, socket, sys
endpoint = sys.argv[sys.argv.index('--console-resize') + 1]
listener = socket.socket(socket.AF_UNIX)
listener.bind(endpoint)
os.write(1, b'resize-endpoint-created\n')
# Abrupt process exit deliberately bypasses any endpoint-name cleanup.
os._exit(37)
",
    )
    .await;
    let output = fixture
        .command()
        .args(["run", "-d", "fixture:1"])
        .assert()
        .success();
    let id = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let record = fixture.record();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let identity: serde_json::Value =
            serde_json::from_slice(&std::fs::read(record.join("identity.json")).unwrap()).unwrap();
        let endpoint = PathBuf::from(format!("{}.resize", identity["endpoint"].as_str().unwrap()));
        if record.join("monitor/result.json").exists() && !endpoint.exists() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "exit callback retained resize endpoint"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    fixture
        .command()
        .args(["logs", id.trim()])
        .assert()
        .success()
        .stdout("resize-endpoint-created\n");
    fixture
        .command()
        .args(["wait", id.trim()])
        .assert()
        .success()
        .stdout("37\n");
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

#[tokio::test]
async fn stop_requests_console_eof_and_preserves_the_original_exit_status() {
    let fixture = WorkloadFixture::new(
        "import os, signal\nsignal.alarm(5)\nwhile os.read(0, 4096): pass\nos._exit(37)",
    )
    .await;
    let output = fixture
        .command()
        .args(["run", "-d", "fixture:1"])
        .assert()
        .success();
    let id = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    for _ in 0..2 {
        fixture
            .command()
            .args(["stop", id.trim()])
            .assert()
            .success()
            .stdout(id.clone());
    }
    fixture
        .command()
        .args(["wait", id.trim()])
        .assert()
        .success()
        .stdout("37\n");
    let result: madrillo::Completion = serde_json::from_slice(
        &std::fs::read(fixture.record().join("monitor/result.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(result.cause, madrillo::CompletionCause::LeaderExit);
}

#[tokio::test]
async fn stop_forces_after_grace_and_zero_forces_immediately() {
    for seconds in ["1", "0"] {
        let fixture =
            WorkloadFixture::new("import signal\nsignal.alarm(5)\nwhile True: signal.pause()")
                .await;
        let output = fixture
            .command()
            .args(["run", "-d", "fixture:1"])
            .assert()
            .success();
        let id = String::from_utf8(output.get_output().stdout.clone()).unwrap();
        let started = std::time::Instant::now();
        fixture
            .command()
            .args(["stop", "--time", seconds, id.trim()])
            .assert()
            .success()
            .stdout(id);
        if seconds == "1" {
            assert!(started.elapsed() >= Duration::from_secs(1));
        }
        let result: madrillo::Completion = serde_json::from_slice(
            &std::fs::read(fixture.record().join("monitor/result.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(result.cause, madrillo::CompletionCause::Forced);
        assert!(matches!(
            result.leader,
            madrillo::LeaderResult::Signal { signal: 9, .. }
        ));
    }
}
