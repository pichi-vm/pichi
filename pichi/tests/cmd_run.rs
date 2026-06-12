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
use pichi_artifact::{
    EmptyConfigDescriptor, Layer, MEDIA_TYPE_PICHI_ARTIFACT_V1, Manifest, PmiDescriptor,
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
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn sha256_digest(bytes: &[u8]) -> pichi_artifact::Digest {
    use sha2::{Digest as _, Sha256};
    let h = Sha256::digest(bytes);
    format!("sha256:{}", hex::encode(h)).parse().unwrap()
}

/// Populate a bootable artifact (PMI layer + one scute) and bind `tag`.
/// Returns `(pmi_blob_path, cow_blob_path)` for assertions.
fn populate_bootable(graphroot: &Path, tag: &str) -> (PathBuf, PathBuf) {
    let blob_store = FilesystemBlobStore::new(graphroot);

    // Real cow bytes so the verity-root derivation in `pichi run` succeeds.
    let cow_bytes = vec![0u8; 4096];
    let cow_digest = sha256_digest(&cow_bytes);
    blob_store.put_blob(&cow_digest, &cow_bytes).unwrap();

    // PMI blob contents are opaque to `pichi run` (it only formats the path),
    // but store something so the artifact is realistic.
    let pmi_bytes = b"PMI\0".to_vec();
    let pmi_digest = sha256_digest(&pmi_bytes);
    blob_store.put_blob(&pmi_digest, &pmi_bytes).unwrap();

    let manifest = Manifest {
        schema_version: 2,
        media_type: "application/vnd.oci.image.manifest.v1+json".into(),
        artifact_type: MEDIA_TYPE_PICHI_ARTIFACT_V1.into(),
        config: EmptyConfigDescriptor::canonical(),
        layers: vec![
            Layer::Scute(ScuteDescriptor {
                digest: cow_digest.to_string(),
                size: cow_bytes.len() as u64,
                annotations: ScuteAnnotations { salt: "00".into() },
            }),
            Layer::Pmi(PmiDescriptor {
                digest: pmi_digest.to_string(),
                size: pmi_bytes.len() as u64,
            }),
        ],
        annotations: chain_annotations(),
    };
    let bytes = manifest.to_bytes().unwrap();
    let digest = manifest.digest().unwrap();
    blob_store.put_blob(&digest, &bytes).unwrap();
    // Store under the normalized reference key (`myapp:1` →
    // `docker.io/library/myapp:1`), matching how `pichi run` resolves it.
    let key: pichi_artifact::Reference = tag.parse().unwrap();
    FilesystemTagDb::open(graphroot)
        .unwrap()
        .set_tag(&key.to_string(), &digest)
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

#[test]
fn run_execs_dillo_with_manifest_derived_devices() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let (pmi_path, cow_path) = populate_bootable(&g, "myapp:1");

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

#[test]
fn run_forwards_cpus_and_memory() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let _ = populate_bootable(&g, "myapp:1");

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

#[test]
fn run_missing_ref_errors_with_hint() {
    let tmp = TempDir::new().unwrap();
    let _ = graphroot(&tmp);
    let record = tmp.path().join("argv.txt");
    let stub = make_stub_dillo(tmp.path(), &record);

    let assert = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .env("PICHI_DILLO", &stub)
        .args(["run", "ghost:1"])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("ref not in cache") && stderr.contains("pichi pull"),
        "stderr: {stderr}"
    );
    // The stub must never have been exec'd.
    assert!(
        !record.exists(),
        "dillo should not be invoked on a cache miss"
    );
}
