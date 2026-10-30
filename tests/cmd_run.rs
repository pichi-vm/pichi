// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for `pichi run` (boot orchestration → exec dillo).
//!
//! Unix-only: the dillo stub is a shell script and the test exercises the
//! `exec()` handoff path. The Windows spawn arm is covered by a cross-target
//! `cargo check` and the `cmd::run` unit tests (which run on every OS).
#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use conglobate::artifact::{
    ConfigDescriptor, Layer, MEDIA_TYPE_PICHI_ARTIFACT_V1, Manifest, PmiDescriptor,
    ScuteAnnotations, ScuteDescriptor,
};
use pichi_storage::{BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};
use tempfile::TempDir;

fn graphroot(tmp: &TempDir) -> PathBuf {
    let p = tmp.path().join("pichi").join("storage");
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn chain_annotations() -> std::collections::BTreeMap<String, String> {
    [
        ("dev.pichi.carapace.verity.algo", "sha256"),
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
    let blob_store = FilesystemBlobStore::new(graphroot);

    // Real cow bytes so the verity-root derivation in `pichi run` succeeds.
    let cow_bytes = vec![0u8; 4096];
    let cow_digest = sha256_digest(&cow_bytes);
    blob_store.put_blob(&cow_digest, &cow_bytes).await.unwrap();

    // PMI blob contents are opaque to `pichi run` (it only formats the path),
    // but store something so the artifact is realistic.
    let pmi_bytes = b"PMI\0".to_vec();
    let pmi_digest = sha256_digest(&pmi_bytes);
    blob_store.put_blob(&pmi_digest, &pmi_bytes).await.unwrap();

    let manifest = Manifest {
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
                    salt: "00".into(),
                },
            }),
            Layer::Pmi(PmiDescriptor {
                digest: pmi_digest.to_string(),
                size: pmi_bytes.len() as u64,
            }),
        ],
        annotations: chain_annotations(),
    };
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

/// Write a stub `dillo` that appends each argv entry (one per line) to
/// `record`, then exits 0. Returns the stub path for `PICHI_DILLO`.
fn make_stub_dillo(dir: &Path, record: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let stub = dir.join("dillo-stub");
    let script = format!(
        "#!/bin/sh\n: > '{rec}'\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> '{rec}'; done\n",
        rec = record.display()
    );
    std::fs::write(&stub, script).unwrap();
    let mut perms = std::fs::metadata(&stub).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&stub, perms).unwrap();
    stub
}

#[tokio::test]
async fn run_execs_dillo_with_manifest_derived_devices() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let (pmi_path, cow_path) = populate_bootable(&g, "myapp:1").await;

    let record = tmp.path().join("argv.txt");
    let stub = make_stub_dillo(tmp.path(), &record);

    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .env("PICHI_DILLO", &stub)
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
    use pichi_storage::BlobSidecarExt as _;

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
            let dillo = make_stub_dillo(destination.path(), &record);
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
            let data_path = if compressed || length != 8192 {
                blob.deflated_path()
            } else {
                blob.clone()
            };
            data.resize(8192, 0);
            assert_eq!(std::fs::read(&blob).unwrap(), wire);
            assert_eq!(std::fs::read(&data_path).unwrap(), data);
            let verity = std::fs::read(blob.verity_path()).unwrap();
            assert!(!verity.is_empty());
            for _ in 0..2 {
                Command::cargo_bin("pichi")
                    .unwrap()
                    .env("XDG_DATA_HOME", destination.path())
                    .env("PICHI_DILLO", &dillo)
                    .args(["run", tag])
                    .assert()
                    .success();
                let argv = std::fs::read_to_string(&record).unwrap();
                let args: Vec<_> = argv.lines().collect();
                let spec = args[args.iter().position(|arg| *arg == "--gpt").unwrap() + 1];
                let gpt: dillo_config::GptSpec = serde_keyvalue::from_key_values(spec).unwrap();
                assert_eq!(gpt.partitions.len(), 2);
                assert_eq!(gpt.partitions[0].path, data_path);
                assert_eq!(gpt.partitions[1].path, blob.verity_path());
                assert_eq!(std::fs::read(blob.verity_path()).unwrap(), verity);
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
            let stub = make_stub_dillo(tmp.path(), &record);
            let assert = Command::cargo_bin("pichi")
                .unwrap()
                .env("XDG_DATA_HOME", tmp.path())
                .env("PICHI_DILLO", &stub)
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
    let stub = make_stub_dillo(tmp.path(), &record);

    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .env("PICHI_DILLO", &stub)
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
    let stub = make_stub_dillo(tmp.path(), &record);

    // A cache miss triggers an auto-pull (docker/podman `--pull=missing`)
    // rather than a "pull first" error. Point at a closed local port so the
    // pull fails fast offline — the assertion is that run ATTEMPTS the pull.
    let assert = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .env("PICHI_DILLO", &stub)
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
