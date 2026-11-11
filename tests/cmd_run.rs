// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for `pichi run` (boot orchestration → exec dillo).
//!
//! A native executable records the launcher arguments on every host.
#![allow(clippy::unwrap_used)]

#[path = "support/executable_path.rs"]
mod executable_path;

use std::path::{Path, PathBuf};

#[path = "support/build_repo.rs"]
mod build_repo;

use assert_cmd::Command;
use conglobate::artifact::{
    ConfigDescriptor, Layer, MEDIA_TYPE_PICHI_ARTIFACT_V1, Manifest, PmiDescriptor,
    ScuteAnnotations, ScuteDescriptor,
};
use devmap_verity::Format as _;
use pichi::cmd::manifest_ext::ManifestExt as _;
use pichi_storage::{BlobSidecarExt as _, BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};
use tempfile::TempDir;

#[path = "common/process.rs"]
mod process;

#[cfg(windows)]
#[path = "support/windows_terminal.rs"]
mod windows_terminal;

fn graphroot(tmp: &TempDir) -> PathBuf {
    let p = tmp.path().join("pichi").join("storage");
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn chain_annotations() -> std::collections::BTreeMap<String, String> {
    [
        (
            "dev.pichi.carapace.verity.hash",
            "sha256:1111111111111111111111111111111111111111111111111111111111111111",
        ),
        ("dev.pichi.carapace.verity.data-block-size", "4096"),
        ("dev.pichi.carapace.verity.hash-block-size", "4096"),
        ("dev.pichi.carapace.verity.version", "1"),
        ("dev.pichi.carapace.verity.hash-type", "1"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn sha256_digest(bytes: &[u8]) -> conglobate::artifact::Digest {
    use sha2::{Digest as _, Sha256};
    let h = Sha256::digest(bytes);
    format!("sha256:{}", hex::encode(h)).parse().unwrap()
}

/// Populate a bootable artifact (PMI layer + one scute) and bind `tag`.
/// Returns `(pmi_blob_path, cow_blob_path)` for assertions.
async fn populate_bootable(graphroot: &Path, tag: &str) -> (PathBuf, PathBuf) {
    populate_bootable_with_requirements(graphroot, tag, None).await
}

async fn populate_bootable_with_requirements(
    graphroot: &Path,
    tag: &str,
    requirements: Option<serde_json::Value>,
) -> (PathBuf, PathBuf) {
    let blob_store = FilesystemBlobStore::new(graphroot);

    // Prepare the cached scute before launch, as ingestion does.
    let cow_bytes = vec![0u8; 4096];
    let cow_digest = sha256_digest(&cow_bytes);
    blob_store.put_blob(&cow_digest, &cow_bytes).await.unwrap();

    // PMI contents are opaque to the launcher; this fixture records arguments.
    let pmi_bytes = b"PMI\0".to_vec();
    let pmi_digest = sha256_digest(&pmi_bytes);
    blob_store.put_blob(&pmi_digest, &pmi_bytes).await.unwrap();

    let mut manifest = Manifest {
        schema_version: 2,
        media_type: "application/vnd.oci.image.manifest.v1+json".into(),
        artifact_type: MEDIA_TYPE_PICHI_ARTIFACT_V1.into(),
        config: ConfigDescriptor::canonical(),
        layers: vec![
            Layer::Scute(ScuteDescriptor {
                digest: cow_digest.to_string(),
                size: cow_bytes.len() as u64,
                annotations: ScuteAnnotations {
                    data_blocks: (cow_bytes.len() as u64).div_ceil(4096).try_into().unwrap(),
                    salt: "00".repeat(32),
                },
            }),
            Layer::Pmi(PmiDescriptor {
                digest: pmi_digest.to_string(),
                size: pmi_bytes.len() as u64,
            }),
        ],
        annotations: chain_annotations(),
    };
    if let Some(requirements) = requirements {
        let bytes =
            serde_json::to_vec(&serde_json::json!({"version":1,"requirements":requirements}))
                .unwrap();
        let digest = sha256_digest(&bytes);
        blob_store.put_blob(&digest, &bytes).await.unwrap();
        manifest.config = ConfigDescriptor::for_config(digest.to_string(), bytes.len() as u64);
    }
    let Layer::Scute(scute) = &manifest.layers[0] else {
        unreachable!()
    };
    let header = manifest.verity_header(scute, Some(scute.size)).unwrap();
    let source = blob_store.blob_path(&cow_digest);
    std::fs::create_dir_all(source.prepared_dir()).unwrap();
    let verity =
        std::fs::File::create(source.verity_path(&sha256_digest(&header.encode()))).unwrap();
    let root = header.format(cow_bytes.as_slice(), verity).unwrap();
    manifest.annotations.insert(
        "dev.pichi.carapace.verity.hash".into(),
        format!("sha256:{}", hex::encode(root)),
    );
    let bytes = serde_json::to_vec(&manifest).unwrap();
    let digest =
        conglobate::artifact::Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&bytes).into());
    blob_store.put_blob(&digest, &bytes).await.unwrap();
    // Store under the normalized reference key (`myapp:1` →
    // `docker.io/library/myapp:1`), matching how `pichi run` resolves it.
    let key: conglobate::artifact::Reference = tag.parse().unwrap();
    FilesystemTagDb::open(graphroot)
        .unwrap()
        .set_tag(&key.to_string(), &digest)
        .await
        .unwrap();

    (
        blob_store.blob_path(&pmi_digest),
        blob_store.blob_path(&cow_digest),
    )
}

/// Compile once, then copy the native stub beside each test's argument log.
fn make_stub_dillo(dir: &Path) -> PathBuf {
    static STUB: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    let compiled = STUB.get_or_init(|| {
        let directory = TempDir::new().unwrap();
        let output = std::process::Command::new("rustc")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/dillo_stub.rs"))
            .arg("-o")
            .arg(
                directory
                    .path()
                    .join(format!("dillo{}", std::env::consts::EXE_SUFFIX)),
            )
            .output()
            .unwrap();
        assert!(output.status.success(), "compile dillo stub: {output:?}");
        std::fs::read(
            directory
                .path()
                .join(format!("dillo{}", std::env::consts::EXE_SUFFIX)),
        )
        .unwrap()
    });
    let name = format!("dillo{}", std::env::consts::EXE_SUFFIX);
    let stub = dir.join(&name);
    std::fs::write(&stub, compiled).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    stub
}

#[tokio::test]
#[ignore = "requires madrillo and its native port-exchange fixture on PATH"]
async fn build_channel_preserves_guest_first_exchange() {
    let fixture = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| {
            directory.join(format!("madrillo-fixture{}", std::env::consts::EXE_SUFFIX))
        })
        .find(|path| path.is_file())
        .expect("madrillo-fixture on PATH");
    let root = TempDir::new().unwrap();
    let child = root
        .path()
        .join(format!("dillo{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(&fixture, &child).unwrap();
    populate_bootable(&graphroot(&root), "contract:1").await;
    let commit = build_repo::prepare(root.path());
    let marker = root.path().join("port-exchange.json");
    let mut request = Vec::new();
    ciborium::into_writer(
        &pichi_launch::Request::from(pichi_launch::Start::default()),
        &mut request,
    )
    .unwrap();
    ciborium::into_writer(
        &pichi_launch::Request(pichi_launch::Finish(None)),
        &mut request,
    )
    .unwrap();
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
    ciborium::into_writer(&pichi_launch::Response::Ok(None::<()>), &mut response).unwrap();
    let result = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", root.path())
        .env("PATH", executable_path::prepend(root.path()))
        .env(
            "MADRILLO_FIXTURE_PORT_EXCHANGE",
            serde_json::json!({"request":request,"response":response,"marker":marker}).to_string(),
        )
        .arg("build")
        .arg(root.path())
        .args(["--ref", "HEAD", "-b", "contract:1", "-t", "built:1"])
        .timeout(std::time::Duration::from_secs(15))
        .assert()
        .failure();
    let console_logs: Vec<_> = std::fs::read_dir(root.path().join("pichi/storage.workloads"))
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path().join("monitor/console.log")))
        .collect();
    assert!(
        String::from_utf8_lossy(&result.get_output().stderr)
            .contains("build VM exited with status 37"),
        "{result:?}; console logs: {console_logs:?}"
    );
    // The ordinary child creates this marker only after checking that no host
    // bytes precede its request and the full CBOR response matches exactly.
    let arguments: Vec<String> = serde_json::from_slice(&std::fs::read(marker).unwrap()).unwrap();
    assert!(arguments.iter().any(|argument| {
        argument.contains("name=dev.pichi.build")
            && argument.contains("rx=pipe:")
            && argument.contains("tx=pipe:")
    }));
    for tag in ["repo", "cache", "output"] {
        assert!(
            arguments
                .iter()
                .any(|argument| { argument.starts_with(&format!("tag={tag},source=")) })
        );
    }
    println!(
        "native build channel: direct inherited pipes, guest-first CBOR and mount arguments preserved"
    );
}

#[tokio::test]
async fn run_defaults_to_warnings_and_preserves_log_overrides() {
    let tmp = TempDir::new().unwrap();
    populate_bootable(&graphroot(&tmp), "myapp:1").await;
    let stub = make_stub_dillo(tmp.path());
    for (configured, expected) in [
        (None, "warn"),
        (Some("dillo=debug"), "dillo=debug"),
        (Some(""), ""),
    ] {
        let mut command = Command::cargo_bin("pichi").unwrap();
        command
            .env("XDG_DATA_HOME", tmp.path())
            .env("PATH", executable_path::prepend(stub.parent().unwrap()))
            .env_remove("RUST_LOG");
        if let Some(value) = configured {
            command.env("RUST_LOG", value);
        }
        command.args(["run", "myapp:1"]).assert().success();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("rust-log.txt")).unwrap(),
            expected
        );
    }
}

#[tokio::test]
async fn run_execs_dillo_with_manifest_derived_devices() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let (pmi_path, cow_path) = populate_bootable(&g, "myapp:1").await;

    let record = tmp.path().join("argv.txt");
    let stub = make_stub_dillo(tmp.path());

    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .env("PATH", executable_path::prepend(stub.parent().unwrap()))
        .args(["run", "myapp:1"])
        .assert()
        .success();

    let argv = std::fs::read_to_string(&record).unwrap();
    let lines: Vec<&str> = argv.lines().collect();

    // PMI passed by path.
    assert!(lines.contains(&"--pmi"), "missing --pmi in {lines:?}");
    assert!(
        lines.contains(&pmi_path.to_string_lossy().as_ref()),
        "missing pmi path {} in {lines:?}",
        pmi_path.display()
    );

    // One --gpt carapace disk whose partition list references the cow blob.
    assert!(lines.contains(&"--gpt"), "missing --gpt in {lines:?}");
    let gpt = lines
        .iter()
        .find(|l| l.starts_with("partitions="))
        .expect("a partitions= value");
    assert!(
        gpt.contains(&format!("path={}", cow_path.display())),
        "gpt spec missing cow path: {gpt}"
    );
    assert!(
        gpt.contains("label=c:"),
        "gpt spec missing cow label: {gpt}"
    );
    assert!(
        gpt.contains("label=v:"),
        "gpt spec missing verity label: {gpt}"
    );

    // No resource flags when neither CLI nor config set them.
    assert!(!lines.contains(&"--cpus"), "unexpected --cpus: {lines:?}");
    assert!(
        !lines.contains(&"--memory"),
        "unexpected --memory: {lines:?}"
    );
}

#[tokio::test]
async fn loaded_scutes_reach_dillo_as_uncompressed_data_and_verity() {
    for compressed in [false, true] {
        for length in [4097, 8192] {
            let source = TempDir::new().unwrap();
            let root = graphroot(&source);
            populate_bootable(&root, "workflow:source").await;
            let store = FilesystemBlobStore::new(&root);
            let tags = FilesystemTagDb::open(&root).unwrap();
            let tag = "docker.io/library/workflow:source";
            let original = tags.resolve_tag(tag).await.unwrap().unwrap();
            let mut manifest: Manifest =
                serde_json::from_slice(&store.get_blob(&original).await.unwrap()).unwrap();
            let mut data = vec![0x5a; length];
            let wire = if compressed {
                ruzstd::encoding::compress_to_vec(
                    data.as_slice(),
                    ruzstd::encoding::CompressionLevel::Fastest,
                )
            } else {
                data.clone()
            };
            let digest = sha256_digest(&wire);
            store.put_blob(&digest, &wire).await.unwrap();
            store.put_blob(&sha256_digest(b"{}"), b"{}").await.unwrap();
            let descriptor = ScuteDescriptor {
                digest: digest.to_string(),
                size: wire.len() as u64,
                annotations: ScuteAnnotations {
                    data_blocks: 2u64.try_into().unwrap(),
                    salt: "00".repeat(32),
                },
            };
            manifest.layers[0] = if compressed {
                Layer::ScuteZstd(descriptor)
            } else {
                Layer::Scute(descriptor)
            };
            let (Layer::Scute(scute) | Layer::ScuteZstd(scute)) = &manifest.layers[0] else {
                unreachable!()
            };
            let header = manifest.verity_header(scute, None).unwrap();
            data.resize(8192, 0);
            let root = header
                .format(data.as_slice(), std::io::Cursor::new(Vec::new()))
                .unwrap();
            manifest.annotations.insert(
                "dev.pichi.carapace.verity.hash".into(),
                format!("sha256:{}", hex::encode(root)),
            );
            let bytes = serde_json::to_vec(&manifest).unwrap();
            let manifest_digest = sha256_digest(&bytes);
            store.put_blob(&manifest_digest, &bytes).await.unwrap();
            tags.set_tag(tag, &manifest_digest).await.unwrap();
            let layout = source.path().join("layout");
            Command::cargo_bin("pichi")
                .unwrap()
                .env("XDG_DATA_HOME", source.path())
                .args(["save", tag, "-o"])
                .arg(&layout)
                .assert()
                .success();

            let destination = TempDir::new().unwrap();
            let record = destination.path().join("argv.txt");
            let dillo = make_stub_dillo(destination.path());
            Command::cargo_bin("pichi")
                .unwrap()
                .env("XDG_DATA_HOME", destination.path())
                .arg("load")
                .arg(&layout)
                .args(["--tag", tag])
                .assert()
                .success();
            let prepared = FilesystemBlobStore::new(&graphroot(&destination));
            assert_eq!(prepared.get_blob(&manifest_digest).await.unwrap(), bytes);
            let blob = prepared.blob_path(&digest);
            let (Layer::Scute(scute) | Layer::ScuteZstd(scute)) = &manifest.layers[0] else {
                unreachable!()
            };
            let header = manifest.verity_header(scute, None).unwrap();
            let verity_path = blob.verity_path(&sha256_digest(&header.encode()));
            let data_path = if compressed || length != 8192 {
                blob.deflated_path(8192)
            } else {
                blob.clone()
            };
            assert_eq!(std::fs::read(&blob).unwrap(), wire);
            assert_eq!(std::fs::read(&data_path).unwrap(), data);
            let verity = std::fs::read(verity_path.clone()).unwrap();
            assert!(!verity.is_empty());
            for _ in 0..2 {
                Command::cargo_bin("pichi")
                    .unwrap()
                    .env("XDG_DATA_HOME", destination.path())
                    .env("PATH", executable_path::prepend(dillo.parent().unwrap()))
                    .args(["run", tag])
                    .assert()
                    .success();
                let argv = std::fs::read_to_string(&record).unwrap();
                let args: Vec<_> = argv.lines().collect();
                let spec = args[args.iter().position(|arg| *arg == "--gpt").unwrap() + 1];
                let gpt: dillo_config::GptSpec = serde_keyvalue::from_key_values(spec).unwrap();
                assert_eq!(gpt.partitions.len(), 2);
                assert_eq!(gpt.partitions[0].path, data_path);
                assert_eq!(gpt.partitions[1].path, verity_path.clone());
                assert_eq!(std::fs::read(verity_path.clone()).unwrap(), verity);
                assert_eq!(prepared.get_blob(&manifest_digest).await.unwrap(), bytes);
                assert_eq!(std::fs::read(&blob).unwrap(), wire);
            }
        }
    }
}

#[tokio::test]
async fn run_resolves_cached_index_references() {
    for index_stored in [false, true] {
        for informational_tag in [false, true] {
            let tmp = TempDir::new().unwrap();
            let g = graphroot(&tmp);
            let (pmi_path, _) = populate_bootable(&g, "myapp:1").await;
            let store = FilesystemBlobStore::new(&g);
            let db = FilesystemTagDb::open(&g).unwrap();
            let manifest_digest = db
                .resolve_tag("docker.io/library/myapp:1")
                .await
                .unwrap()
                .unwrap();
            let manifest_bytes = store.get_blob(&manifest_digest).await.unwrap();
            let manifest_size = manifest_bytes.len();
            let manifest: Manifest = serde_json::from_slice(&manifest_bytes).unwrap();
            let architecture = match std::env::consts::ARCH {
                "x86_64" => "amd64",
                "aarch64" => "arm64",
                other => other,
            };
            let index = serde_json::json!({
                "schemaVersion": 2,
                "mediaType": pichi_registry::OCI_IMAGE_INDEX_MEDIA_TYPE,
                "manifests": [{
                    "mediaType": manifest.media_type,
                    "artifactType": MEDIA_TYPE_PICHI_ARTIFACT_V1,
                    "digest": format!("sha256:{}", "44".repeat(32)),
                    "size": 1,
                    "platform": { "os": "pichi", "architecture": "not-this-host" },
                }, {
                    "mediaType": manifest.media_type,
                    "artifactType": MEDIA_TYPE_PICHI_ARTIFACT_V1,
                    "digest": manifest_digest.to_string(),
                    "size": manifest_size,
                    "platform": { "os": "pichi", "architecture": architecture },
                }],
            });
            let bytes = serde_json::to_vec(&index).unwrap();
            let digest = sha256_digest(&bytes);
            let canonical = format!("127.0.0.1:9/app@{digest}");
            if index_stored {
                store.put_blob(&digest, &bytes).await.unwrap();
            } else {
                // A legacy mapping cannot replace the missing pinned index.
                db.set_tag(&canonical, &manifest_digest).await.unwrap();
            }
            let reference = if informational_tag {
                format!("127.0.0.1:9/app:label@{digest}")
            } else {
                canonical
            };
            let record = tmp.path().join("argv.txt");
            let stub = make_stub_dillo(tmp.path());
            let assert = Command::cargo_bin("pichi")
                .unwrap()
                .env("XDG_DATA_HOME", tmp.path())
                .env("PATH", executable_path::prepend(stub.parent().unwrap()))
                .args(["run", &reference])
                .assert();
            if !index_stored {
                let assert = assert.failure();
                assert!(String::from_utf8_lossy(&assert.get_output().stderr).contains("pulling"));
                assert!(
                    !record.exists(),
                    "must not launch a different manifest under the missing pin"
                );
                continue;
            }
            let assert = assert.success();
            assert!(
                !String::from_utf8_lossy(&assert.get_output().stderr).contains("pulling"),
                "cached input must not trigger a network pull"
            );
            let argv = std::fs::read_to_string(&record).unwrap();
            assert!(argv.lines().any(|arg| arg == pmi_path.to_string_lossy()));
        }
    }
}

#[tokio::test]
async fn run_forwards_cpus_and_memory() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let _ = populate_bootable(&g, "myapp:1").await;

    let record = tmp.path().join("argv.txt");
    let stub = make_stub_dillo(tmp.path());

    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .env("PATH", executable_path::prepend(stub.parent().unwrap()))
        .args(["run", "--cpus", "2", "--memory", "2048", "myapp:1"])
        .assert()
        .success();

    let argv = std::fs::read_to_string(&record).unwrap();
    let lines: Vec<&str> = argv.lines().collect();
    assert!(
        lines.contains(&"--cpus") && lines.contains(&"2"),
        "{lines:?}"
    );
    assert!(
        lines.contains(&"--memory") && lines.contains(&"2048"),
        "{lines:?}"
    );
}

#[tokio::test]
async fn run_missing_ref_auto_pulls() {
    let tmp = TempDir::new().unwrap();
    let _ = graphroot(&tmp);
    let record = tmp.path().join("argv.txt");
    let stub = make_stub_dillo(tmp.path());

    // A cache miss triggers an auto-pull (docker/podman `--pull=missing`)
    // rather than a "pull first" error. Point at a closed local port so the
    // pull fails fast offline — the assertion is that run ATTEMPTS the pull.
    let assert = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .env("PATH", executable_path::prepend(stub.parent().unwrap()))
        .args(["run", "127.0.0.1:1/ghost:1"])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("pulling") && stderr.contains("auto-pull"),
        "stderr should show an auto-pull attempt: {stderr}"
    );
    // The pull failed, so there is nothing to boot — dillo must not be exec'd.
    assert!(
        !record.exists(),
        "dillo should not be invoked when the auto-pull fails"
    );
}

#[tokio::test]
async fn run_propagates_dillo_exit_status_with_spaced_paths() {
    let tmp = tempfile::Builder::new()
        .prefix("pichi run ")
        .tempdir()
        .unwrap();
    populate_bootable(&graphroot(&tmp), "exit-status:1").await;
    let record = tmp.path().join("argv.txt");
    let stub = make_stub_dillo(tmp.path());
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .env("PATH", executable_path::prepend(stub.parent().unwrap()))
        .env("PICHI_TEST_DILLO_EXIT", "7")
        .args(["run", "exit-status:1"])
        .assert()
        .code(7);
    assert!(record.is_file());
}

#[tokio::test]
async fn image_config_defaults_and_manual_allocations_are_enforced() {
    let tmp = TempDir::new().unwrap();
    let requirements = serde_json::json!({
        "slot":"pci", "cpus":{"required":2,"recommended":8},
        "memory":{"required":512,"recommended":1024},
        "interfaces":{"public":{"slot":"mmio"}}
    });
    populate_bootable_with_requirements(&graphroot(&tmp), "sized:1", Some(requirements)).await;
    let stub = make_stub_dillo(tmp.path());
    for (extra, cpus, memory, warning) in [
        (vec![], "8", "1024", false),
        (vec!["--cpus", "4", "--memory", "768"], "4", "768", true),
    ] {
        let mut command = Command::cargo_bin("pichi").unwrap();
        command
            .env("XDG_DATA_HOME", tmp.path())
            .env("PATH", executable_path::prepend(stub.parent().unwrap()))
            .env_remove("RUST_LOG")
            .args(["run", "sized:1"])
            .args(extra);
        let output = command.assert().success().get_output().clone();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(stderr.contains("below the recommended"), warning);
        let argv = std::fs::read_to_string(tmp.path().join("argv.txt")).unwrap();
        let args: Vec<_> = argv.lines().collect();
        let value = |flag| args[args.iter().position(|a| *a == flag).unwrap() + 1];
        assert_eq!(value("--cpus"), cpus);
        assert_eq!(value("--memory"), memory);
        assert!(!args.contains(&"--bus"), "Dillo has no global bus setting");
        let console: dillo_config::ConsoleSpec =
            serde_keyvalue::from_key_values(value("--console")).unwrap();
        let disk: dillo_config::GptSpec = serde_keyvalue::from_key_values(value("--gpt")).unwrap();
        let network: dillo_config::NetSpec =
            serde_keyvalue::from_key_values(value("--net")).unwrap();
        assert_eq!(console.bus, Some(dillo_config::Bus::Pci));
        assert_eq!(disk.bus, Some(dillo_config::Bus::Pci));
        assert_eq!(network.bus, Some(dillo_config::Bus::Mmio));
        assert_eq!(console.slot, None);
        assert_eq!(disk.slot, None);
        assert_eq!(network.slot, None);
    }
    let output = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .env("PATH", executable_path::prepend(stub.parent().unwrap()))
        .args(["run", "sized:1", "--cpus", "1"])
        .assert()
        .failure()
        .get_output()
        .clone();
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("required minimum")
    );
}

#[tokio::test]
async fn detached_storage_stays_reserved_until_workload_exit() {
    struct StopOnDrop {
        root: PathBuf,
        id: Option<String>,
    }
    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            if let Some(id) = &self.id {
                let _ = std::process::Command::new(env!("CARGO_BIN_EXE_pichi"))
                    .env("XDG_DATA_HOME", &self.root)
                    .args(["stop", id, "--time", "0"])
                    .output();
            }
        }
    }
    for retained in [false, true] {
        let tmp = TempDir::new().unwrap();
        populate_bootable(&graphroot(&tmp), "storage:detached").await;
        let stub = make_stub_dillo(tmp.path());
        let disk = tmp.path().join("retained.raw");
        let mut command = Command::cargo_bin("pichi").unwrap();
        command
            .env("XDG_DATA_HOME", tmp.path())
            .env("PATH", executable_path::prepend(stub.parent().unwrap()))
            .env("PICHI_TEST_STORAGE", "1")
            .args(["run", "-d", "--storage", "2", "storage:detached"]);
        if retained {
            command
                .arg("--storage-file")
                .arg(&disk)
                .env("PICHI_TEST_DILLO_WAIT_FOR_EOF", "1");
        } else {
            command.env("PICHI_TEST_DILLO_EXIT_GATE", tmp.path().join("exit-gate"));
        }
        let output = command.assert().success().get_output().stdout.clone();
        let id = String::from_utf8(output).unwrap().trim().to_owned();
        let mut cleanup = StopOnDrop {
            root: tmp.path().to_owned(),
            id: Some(id.clone()),
        };
        let report = tmp.path().join("storage-path.txt");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !report.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "workload did not open storage"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let path = PathBuf::from(std::fs::read_to_string(report).unwrap());
        assert!(path.exists(), "detach removed the backing");
        Command::cargo_bin("pichi")
            .unwrap()
            .env("XDG_DATA_HOME", tmp.path())
            .env("PATH", executable_path::prepend(stub.parent().unwrap()))
            .args(["run", "-d", "--storage-file"])
            .arg(&path)
            .arg("storage:detached")
            .assert()
            .failure();
        if retained {
            Command::cargo_bin("pichi")
                .unwrap()
                .env("XDG_DATA_HOME", tmp.path())
                .args(["stop", &id, "--time", "2"])
                .assert()
                .success();
            assert_eq!(&std::fs::read(&path).unwrap()[..10], b"guest data");
            // A completed workload releases the reservation for a new run.
            Command::cargo_bin("pichi")
                .unwrap()
                .env("XDG_DATA_HOME", tmp.path())
                .env("PATH", executable_path::prepend(stub.parent().unwrap()))
                .args(["run", "--storage-file"])
                .arg(&path)
                .arg("storage:detached")
                .assert()
                .success();
        } else {
            std::fs::write(tmp.path().join("exit-gate"), b"exit").unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while path.exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "exit callback did not remove storage"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        cleanup.id = None;
    }
}

#[tokio::test]
async fn vm_timeout_cleanup_stops_workloads_after_client_exit() {
    use process::ChildExt as _;
    use std::{
        process::Stdio,
        time::{Duration, Instant},
    };
    use wait_timeout::ChildExt as _;

    for detached in [false, true] {
        let tmp = TempDir::new().unwrap();
        populate_bootable(&graphroot(&tmp), "cleanup:1").await;
        let stub = make_stub_dillo(tmp.path());
        let disk = tmp.path().join("retained.raw");
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_pichi"));
        command
            .env("XDG_DATA_HOME", tmp.path())
            .env("PATH", executable_path::prepend(stub.parent().unwrap()))
            .env("PICHI_TEST_STORAGE", "1")
            .env("PICHI_TEST_DILLO_WAIT_FOR_EOF", "1")
            .args(["run", "--storage", "2", "--storage-file"])
            .arg(&disk)
            .arg("cleanup:1")
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        if detached {
            command.arg("-d");
        }
        let mut child = command.spawn().unwrap();
        let report = tmp.path().join("storage-path.txt");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !report.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let ready = report.exists();
        let detached_exit = !detached
            || child
                .wait_timeout(Duration::from_secs(10))
                .unwrap()
                .is_some();
        // Always attempt cleanup before asserting readiness, including on failure.
        child.kill_vm(tmp.path()).unwrap();
        assert!(ready && detached_exit, "workload did not become ready");
        assert!(child.try_wait().unwrap().is_some());
        // Madrillo's reservation survives client exit but must be gone now.
        let file = std::fs::File::options()
            .read(true)
            .write(true)
            .open(&disk)
            .unwrap();
        file.try_lock()
            .expect("monitor retained the storage reservation");
        // Windows locks also exclude reads through a second file handle.
        drop(file);
        assert_eq!(&std::fs::read(&disk).unwrap()[..10], b"guest data");
        child.kill_vm(tmp.path()).unwrap();
    }
}

#[tokio::test]
async fn writable_storage_lives_through_the_child_and_is_cleaned_on_exit() {
    for (requirements, expected_bus, expected_mib) in [
        (serde_json::json!({}), dillo_config::Bus::Mmio, 1024),
        (
            serde_json::json!({"storage":{"required":1,"recommended":2}}),
            dillo_config::Bus::Mmio,
            2,
        ),
        (
            serde_json::json!({"slot":"pci","storage":{"required":1,"recommended":2}}),
            dillo_config::Bus::Pci,
            2,
        ),
        (
            serde_json::json!({"slot":"pci","storage":{"required":1,"recommended":2,"slot":"mmio"}}),
            dillo_config::Bus::Mmio,
            2,
        ),
    ] {
        for exit in [0, 7] {
            let tmp = TempDir::new().unwrap();
            let root = graphroot(&tmp);
            populate_bootable_with_requirements(&root, "storage:1", Some(requirements.clone()))
                .await;
            let stub = make_stub_dillo(tmp.path());
            let mut command = Command::cargo_bin("pichi").unwrap();
            command
                .env("XDG_DATA_HOME", tmp.path())
                .env("PATH", executable_path::prepend(stub.parent().unwrap()))
                .env("PICHI_TEST_STORAGE", "1")
                .env("PICHI_TEST_DILLO_EXIT", exit.to_string())
                .args(["run", "storage:1"]);
            command.assert().code(exit);
            assert_eq!(
                std::fs::read_to_string(tmp.path().join("storage-capacity.txt")).unwrap(),
                (expected_mib * 1024 * 1024).to_string()
            );
            let path = std::fs::read_to_string(tmp.path().join("storage-path.txt")).unwrap();
            assert!(!Path::new(&path).exists());
            let argv = std::fs::read_to_string(tmp.path().join("argv.txt")).unwrap();
            let args: Vec<_> = argv.lines().collect();
            let spec: dillo_config::BlkSpec = serde_keyvalue::from_key_values(
                args[args.iter().position(|a| *a == "--blk").unwrap() + 1],
            )
            .unwrap();
            assert_eq!(spec.path, PathBuf::from(path));
            assert!(!spec.readonly);
            assert_eq!(spec.bus, Some(expected_bus));
            assert_eq!(spec.slot, None);
        }
    }
}

#[tokio::test]
async fn cpu_max_uses_host_availability_as_the_default() {
    let tmp = TempDir::new().unwrap();
    populate_bootable_with_requirements(
        &graphroot(&tmp),
        "max:1",
        Some(serde_json::json!({"cpus":{"recommended":"max"}})),
    )
    .await;
    let stub = make_stub_dillo(tmp.path());
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .env("PATH", executable_path::prepend(stub.parent().unwrap()))
        .args(["run", "max:1"])
        .assert()
        .success();
    let argv = std::fs::read_to_string(tmp.path().join("argv.txt")).unwrap();
    let args: Vec<_> = argv.lines().collect();
    assert_eq!(
        args[args.iter().position(|a| *a == "--cpus").unwrap() + 1]
            .parse::<usize>()
            .unwrap(),
        std::thread::available_parallelism().unwrap().get()
    );
}
