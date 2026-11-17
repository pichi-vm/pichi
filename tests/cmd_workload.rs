// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Actual pichi → madrillo launch and CLI reconnection; Unix fixture programs.
//! PATH must provide the real monitor executable.

#![cfg(unix)]
#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used)]

#[path = "support/executable_path.rs"]
mod executable_path;

#[path = "support/build_repo.rs"]
mod build_repo;

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
        let pmi = b"PMI\0";
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
        let path = root.path().join("dillo");
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
        let commit = build_repo::prepare(self.root.path());
        let mut encoded = Vec::new();
        ciborium::into_writer(request, &mut encoded).unwrap();
        let mut response = Vec::new();
        ciborium::into_writer(
            &pichi_launch::Response::Ok(pichi_launch::Input {
                r#ref: commit,
                plan: "pichi.yaml".into(),
                lock: "pichi.lock".into(),
                context: ".".into(),
                location: None,
            }),
            &mut response,
        )
        .unwrap();
        let mut command = self.command();
        command
            .env("BUILD_RESPONSE", hex::encode(response))
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

    /// The selected boot's directory: an instance's run, else the record.
    fn run(&self) -> PathBuf {
        let record = self.record();
        match self.identity()["attempt"]["id"].as_str() {
            Some(attempt) => record.join("runs").join(attempt),
            None => record,
        }
    }

    fn endpoint(&self) -> PathBuf {
        let identity = self.identity();
        PathBuf::from(
            identity["attempt"]["endpoint"]
                .as_str()
                .or(identity["endpoint"].as_str())
                .unwrap(),
        )
    }

    fn identity(&self) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(self.record().join("identity.json")).unwrap())
            .unwrap()
    }

    fn command(&self) -> Command {
        let mut command = Command::from_std(self.native_command());
        command.timeout(Duration::from_secs(15));
        command
    }

    fn native_command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_pichi"));
        command.env("XDG_DATA_HOME", self.root.path()).env(
            "PATH",
            executable_path::prepend(self.program.parent().unwrap()),
        );
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
expected = bytes.fromhex(os.environ['BUILD_RESPONSE'])
response = b''
while len(response) < len(expected):
    chunk = os.read(rx, len(expected) - len(response))
    assert chunk, 'host closed before build input'
    response += chunk
assert response == expected, response.hex()
output = next(arg.removeprefix('tag=output,source=').rsplit(',bus=', 1)[0] for arg in sys.argv if arg.startswith('tag=output,source='))
shutil.copytree(os.environ['BUILD_PRODUCT'], output, dirs_exist_ok=True)
os.write(tx, bytes.fromhex(os.environ['BUILD_FINISH']))
ack = b''
while len(ack) < 3:
    chunk = os.read(rx, 3 - len(ack))
    assert chunk, 'host closed before finish acknowledgement'
    ack += chunk
assert ack == bytes.fromhex('82f5f6'), ack.hex()
os.close(tx)
os.write(1, b'opaque build port\n')
").await;
    let mut finish = Vec::new();
    ciborium::into_writer(
        &pichi_launch::Request(pichi_launch::Finish(None)),
        &mut finish,
    )
    .unwrap();
    let built = fixture
        .build_command(&pichi_launch::Request::from(pichi_launch::Start::default()))
        .env("BUILD_FINISH", hex::encode(finish))
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
async fn unsuccessful_build_completion_never_imports_output() {
    for failure in [None, Some("guest build failed")] {
        let fixture = WorkloadFixture::new(r"
import os, re, shutil, signal, sys
signal.alarm(5)
console = sys.argv[sys.argv.index('--console') + 1]
rx = int(re.search(r'rx=pipe:(\d+)', console)[1])
tx = int(re.search(r'tx=pipe:(\d+)', console)[1])
os.write(tx, bytes.fromhex(os.environ['BUILD_REQUEST']))
expected = bytes.fromhex(os.environ['BUILD_RESPONSE'])
response = b''
while len(response) < len(expected):
    chunk = os.read(rx, len(expected) - len(response))
    assert chunk, 'host closed before build input'
    response += chunk
assert response == expected
output = next(arg.removeprefix('tag=output,source=').rsplit(',bus=', 1)[0] for arg in sys.argv if arg.startswith('tag=output,source='))
shutil.copytree(os.environ['BUILD_PRODUCT'], output, dirs_exist_ok=True)
if os.environ['BUILD_FINISH']:
    os.write(tx, bytes.fromhex(os.environ['BUILD_FINISH']))
    ack = b''
    while len(ack) < 3:
        chunk = os.read(rx, 3 - len(ack))
        assert chunk, 'host closed before finish acknowledgement'
        ack += chunk
    assert ack == bytes.fromhex('82f5f6')
# Exit 0 must not substitute for a successful finish command.
os._exit(0)
").await;
        let mut finish = Vec::new();
        if let Some(error) = failure {
            ciborium::into_writer(
                &pichi_launch::Request(pichi_launch::Finish(Some(error.to_owned()))),
                &mut finish,
            )
            .unwrap();
        }
        let built = fixture
            .build_command(&pichi_launch::Request(pichi_launch::Start::default()))
            .env("BUILD_FINISH", hex::encode(finish))
            .env("BUILD_PRODUCT", fixture.build_product().await)
            .assert()
            .failure();
        let stderr = String::from_utf8_lossy(&built.get_output().stderr);
        assert!(
            stderr.contains(failure.unwrap_or("missing or invalid finish command")),
            "{stderr}"
        );
        let tags = FilesystemTagDb::open(fixture.root.path().join("pichi/storage")).unwrap();
        assert!(
            tags.resolve_tag("docker.io/library/built:1")
                .await
                .unwrap()
                .is_none()
        );
        assert!(fixture.record().join("output").is_dir());
    }
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
        .build_command(&("start", 2_u16, ()))
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&built.get_output().stderr);
    assert!(stderr.contains("reading start command"), "{stderr}");
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
        .env("TMPDIR", fixture.root.path())
        .args(["run", "-d", "fixture:1"])
        .assert()
        .success();
    let id = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let id = id.trim();
    uuid::Uuid::parse_str(id).unwrap();
    let prefix = &id[..8];
    fixture
        .command()
        .args(["wait", id, prefix])
        .assert()
        .success()
        .stdout("23\n23\n");
    // A later caller uses the retained address even with a different TMPDIR.
    // Launch must respect the configured native temp directory, which may be
    // the only location writable inside the caller's filesystem sandbox.
    let endpoint = fixture.endpoint();
    assert_eq!(endpoint.parent(), Some(fixture.root.path()));
    // The endpoint is gone after monitor exit; a prefix uses retained state.
    fixture
        .command()
        .args(["wait", prefix])
        .assert()
        .success()
        .stdout("23\n");
    let refused = fixture
        .command()
        .args(["attach", prefix])
        .assert()
        .code(125)
        .stdout("");
    assert!(
        String::from_utf8_lossy(&refused.get_output().stderr)
            .contains("can only attach to running workloads")
    );
    fixture
        .command()
        .args(["logs", prefix])
        .assert()
        .success()
        .stdout("detached output\n");
    fixture
        .command()
        .args(["ps", "-q"])
        .assert()
        .success()
        .stdout("");
    let listed = fixture.command().args(["ps", "--all"]).assert().success();
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
            !fixture.run().join("monitor/result.json").exists(),
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
import os, select, signal, socket, struct, sys, time
signal.alarm(45)
endpoint = sys.argv[sys.argv.index('--console-resize') + 1]
listener = socket.socket(socket.AF_UNIX)
# The CLI may attach before the child creates its resize listener.
time.sleep(0.5)
listener.bind(endpoint)
os.chmod(endpoint, 0o600)
# A bound socket also exists before it accepts resize connections.
time.sleep(0.5)
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
        .env(
            "PATH",
            executable_path::prepend(fixture.program.parent().unwrap()),
        )
        .arg(env!("CARGO_BIN_EXE_pichi"))
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
        let endpoint = PathBuf::from(format!(
            "{}.resize",
            identity["attempt"]["endpoint"].as_str().unwrap()
        ));
        if fixture.run().join("monitor/result.json").exists() && !endpoint.exists() {
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
async fn multiple_logs_label_streams_and_continue_after_read_errors() {
    let fixture = WorkloadFixture::new(
        "import os\nos.write(1, b'first\\x00\\xff\\npartial')\nos.write(2, b'error\\x00\\xfe\\n')",
    )
    .await;
    let mut ids = Vec::new();
    for _ in 0..2 {
        let output = fixture
            .command()
            .args(["run", "-d", "fixture:1"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let id = String::from_utf8(output).unwrap().trim().to_owned();
        fixture.command().args(["wait", &id]).assert().success();
        ids.push(id);
    }
    let prefixes: Vec<_> = ids
        .iter()
        .map(|id| {
            format!(
                "{} ",
                &uuid::Uuid::parse_str(id).unwrap().simple().to_string()[..12]
            )
        })
        .collect();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    for prefix in &prefixes {
        stdout.extend_from_slice(prefix.as_bytes());
        stdout.extend_from_slice(b"first\0\xff\n");
        stdout.extend_from_slice(prefix.as_bytes());
        stdout.extend_from_slice(b"partial");
        stderr.extend_from_slice(prefix.as_bytes());
        stderr.extend_from_slice(b"error\0\xfe\n");
    }
    fixture
        .command()
        .args(["logs", &ids[0][..8], &ids[1][..8]])
        .assert()
        .success()
        .stdout(stdout.clone())
        .stderr(stderr);
    fixture
        .command()
        .args(["logs", &ids[0], "missing"])
        .assert()
        .code(125)
        .stdout("");
    // A resolved workload with a missing log does not prevent reading the next.
    let record = fixture
        .root
        .path()
        .join("pichi/storage.workloads")
        .join(&ids[0]);
    let identity: serde_json::Value =
        serde_json::from_slice(&std::fs::read(record.join("identity.json")).unwrap()).unwrap();
    std::fs::remove_file(
        record
            .join("runs")
            .join(identity["attempt"]["id"].as_str().unwrap())
            .join("monitor/console.log"),
    )
    .unwrap();
    let failed = fixture
        .command()
        .args(["logs", &ids[0], &ids[1]])
        .assert()
        .code(125)
        .stdout(stdout[stdout.len() / 2..].to_vec());
    assert!(String::from_utf8_lossy(&failed.get_output().stderr).contains(&ids[0]));
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
    fixture
        .command()
        .args(["stop", id.trim(), "00000000-0000-0000-0000-000000000000"])
        .assert()
        .code(125)
        .stdout("");
    assert!(!fixture.record().join("monitor/result.json").exists());
    fixture
        .command()
        .args(["stop", id.trim(), &id[..8]])
        .assert()
        .success()
        .stdout(format!("{}\n{}\n", id.trim(), &id[..8]));
    fixture
        .command()
        .args(["wait", id.trim()])
        .assert()
        .success()
        .stdout("37\n");
    let result: madrillo::Completion =
        serde_json::from_slice(&std::fs::read(fixture.run().join("monitor/result.json")).unwrap())
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
            &std::fs::read(fixture.run().join("monitor/result.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(result.cause, madrillo::CompletionCause::Forced);
        assert!(matches!(
            result.leader,
            madrillo::LeaderResult::Signal { signal: 9, .. }
        ));
    }
}

#[tokio::test]
async fn blocked_console_input_keeps_output_flowing_and_preserves_every_byte() {
    use std::{io::BufRead as _, process::Stdio, sync::mpsc};
    use wait_timeout::ChildExt as _;
    const SIZE: usize = 8 * 1024 * 1024;
    let fixture = WorkloadFixture::new(
        r"
import os, signal, time
signal.alarm(40)
os.write(1, b'early\n')
time.sleep(1.5)
os.write(1, b'late\n')
while not os.path.exists(os.environ['READ_GATE']): time.sleep(0.01)
total = 0
while total < int(os.environ['INPUT_SIZE']):
    data = os.read(0, min(65536, int(os.environ['INPUT_SIZE']) - total))
    assert data, 'input ended before all bytes arrived'
    assert data == b'z' * len(data)
    total += len(data)
os.write(1, f'count={total}\n'.encode())
",
    )
    .await;
    let input = fixture.root.path().join("input");
    let gate = fixture.root.path().join("read-gate");
    std::fs::write(&input, vec![b'z'; SIZE]).unwrap();
    let mut child = fixture
        .native_command()
        .args(["run", "-i", "fixture:1"])
        .env("READ_GATE", &gate)
        .env("INPUT_SIZE", SIZE.to_string())
        .stdin(Stdio::from(std::fs::File::open(input).unwrap()))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (send, lines) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            if send.send(line).is_err() {
                break;
            }
        }
    });
    let early = lines.recv_timeout(Duration::from_secs(5));
    let late = lines.recv_timeout(Duration::from_secs(4));
    // Keep input blocked beyond the former five-second write deadline.
    std::thread::sleep(Duration::from_secs(5));
    // Release the fixture even on a failed observation so no guest stays blocked.
    std::fs::write(&gate, b"").unwrap();
    let count = lines.recv_timeout(Duration::from_secs(30));
    let exited = child.wait_timeout(Duration::from_secs(10)).unwrap();
    if exited.is_none() {
        let _ = child.kill();
    }
    let output = child.wait_with_output().unwrap();
    reader.join().unwrap();
    assert_eq!(early.unwrap().unwrap(), "early");
    assert_eq!(late.unwrap().unwrap(), "late", "output waited for input");
    assert_eq!(
        count.as_ref().ok().and_then(|line| line.as_ref().ok()),
        Some(&format!("count={SIZE}")),
        "count={count:?}, remaining={:?}, status={}, stderr={}",
        lines.try_iter().collect::<Vec<_>>(),
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(exited.is_some(), "attachment did not finish");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn blocked_console_input_observes_stop_and_caller_signals() {
    use std::{io::BufRead as _, process::Stdio};
    use wait_timeout::ChildExt as _;
    for stop_guest in [true, false] {
        let fixture = WorkloadFixture::new(
            "import os, signal\nsignal.alarm(30)\nos.write(1, b'ready\\n')\nwhile True: signal.pause()",
        ).await;
        let input = fixture.root.path().join("input");
        std::fs::write(&input, vec![b'z'; 8 * 1024 * 1024]).unwrap();
        let mut child = fixture
            .native_command()
            .args(["run", "-i", "fixture:1"])
            .stdin(Stdio::from(std::fs::File::open(input).unwrap()))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut reader = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut ready = String::new();
        reader.read_line(&mut ready).unwrap();
        assert_eq!(ready, "ready\n");
        let id = fixture
            .record()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        // Let the monitor's finite input budget fill while the guest never reads.
        std::thread::sleep(Duration::from_millis(1500));
        if stop_guest {
            fixture
                .command()
                .args(["stop", "--time", "0", &id])
                .assert()
                .success();
        } else {
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap()),
                nix::sys::signal::Signal::SIGTERM,
            )
            .unwrap();
        }
        let exited = child.wait_timeout(Duration::from_secs(3)).unwrap();
        if exited.is_none() {
            let _ = child.kill();
        }
        let output = child.wait_with_output().unwrap();
        // A caller signal must leave the guest alive, then clean it up explicitly.
        if !stop_guest {
            let live = fixture.command().arg("ps").assert().success();
            let listed = String::from_utf8_lossy(&live.get_output().stdout);
            let running = listed
                .lines()
                .any(|line| line.contains(&id) && line.contains("running"));
            fixture
                .command()
                .args(["stop", "--time", "0", &id])
                .assert()
                .success();
            assert!(running, "{listed}");
        }
        assert!(
            exited.is_some(),
            "attachment ignored shutdown while writing"
        );
        assert_eq!(
            output.status.code(),
            Some(if stop_guest { 137 } else { 143 }),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[tokio::test]
async fn concurrent_restarts_admit_only_one_boot() {
    let fixture =
        WorkloadFixture::new("import signal\nsignal.alarm(30)\nwhile True: signal.pause()").await;
    let started = fixture
        .command()
        .args(["run", "-d", "fixture:1"])
        .assert()
        .success();
    let id = String::from_utf8(started.get_output().stdout.clone()).unwrap();
    let id = id.trim();
    fixture
        .command()
        .args(["stop", "--time", "0", id])
        .assert()
        .success();
    let starts: Vec<_> = (0..2)
        .map(|_| {
            fixture
                .native_command()
                .args(["start", id])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let mut succeeded = 0;
    for child in starts {
        let output = child.wait_with_output().unwrap();
        if output.status.success() {
            succeeded += 1;
            assert_eq!(output.stdout, format!("{id}\n").as_bytes());
        } else {
            assert_eq!(output.status.code(), Some(125), "{output:?}");
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("busy"),
                "{output:?}"
            );
        }
    }
    assert_eq!(succeeded, 1, "only one competing start may launch a boot");
    assert_eq!(
        std::fs::read_dir(fixture.record().join("runs"))
            .unwrap()
            .count(),
        2
    );
    fixture
        .command()
        .args(["stop", "--time", "0", id])
        .assert()
        .success();
}

#[tokio::test]
async fn failed_run_retains_an_instance_that_can_be_started() {
    let fixture = WorkloadFixture::new("import os\nos._exit(7)").await;
    fixture
        .command()
        .env("PATH", "")
        .args(["run", "fixture:1"])
        .assert()
        .code(125);
    let record = fixture.record();
    let id = record.file_name().unwrap().to_str().unwrap();
    fixture.command().args(["wait", id]).assert().code(125);
    fixture
        .command()
        .args(["stop", id])
        .assert()
        .success()
        .stdout(format!("{id}\n"));
    fixture
        .command()
        .args(["logs", id])
        .assert()
        .success()
        .stdout("");
    fixture.command().args(["attach", id]).assert().code(125);
    assert!(fixture.identity()["attempt"].is_null());
    assert!(fixture.record().join("storage.qcow2").is_file());
    fixture
        .command()
        .args(["ps", "-q"])
        .assert()
        .success()
        .stdout("");
    fixture
        .command()
        .args(["ps", "-aq"])
        .assert()
        .success()
        .stdout(format!("{id}\n"));
    fixture.command().args(["start", id]).assert().success();
    fixture
        .command()
        .args(["wait", id])
        .assert()
        .success()
        .stdout("7\n");
}
