// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `pichi import` integration tests (Phase 43 / IMPORT-01..07).
//!
//! Pattern: TempDir + `XDG_DATA_HOME` env redirect (Phase 42 convention,
//! tests/cmd_tag.rs:60). Each test writes a small fixture file in the
//! TempDir and points `pichi import` at it.

use std::path::PathBuf;

use assert_cmd::Command;
use pichi_artifact::{Layer, Manifest};
use pichi_storage::{BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};
use tempfile::TempDir;

fn graphroot(tmp: &TempDir) -> PathBuf {
    let p = tmp.path().join("pichi").join("storage");
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Write a tiny fixture: 64 chunks of 16 KiB = 1 MiB; chunk 5 non-zero.
fn write_small_fixture(path: &std::path::Path) {
    const CHUNK_BYTES: usize = 16 * 1024;
    const NUM_CHUNKS: usize = 64;
    let mut buf = vec![0u8; CHUNK_BYTES * NUM_CHUNKS];
    buf[5 * CHUNK_BYTES..6 * CHUNK_BYTES].fill(0xA1);
    std::fs::write(path, &buf).unwrap();
}

/// IMPORT-01: end-to-end import produces 3 blobs + 1 tag; manifest is
/// readable via `pichi inspect`.
#[tokio::test]
async fn import_happy_path() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let raw = tmp.path().join("input.raw");
    write_small_fixture(&raw);

    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["import", raw.to_str().unwrap(), "myapp:base"])
        .assert()
        .success();

    // Three blobs (cow, verity, manifest).
    let blobs = g.join("blobs").join("sha256");
    let n = std::fs::read_dir(&blobs).unwrap().count();
    assert_eq!(n, 3, "expected 3 blobs in {}", blobs.display());

    // Tag is set (canonical form: docker.io/library/myapp:base).
    let db = FilesystemTagDb::open(&g).unwrap();
    let manifest_digest = db
        .resolve_tag("docker.io/library/myapp:base")
        .await
        .unwrap()
        .expect("tag should resolve under canonical form");

    // pichi inspect round-trips the manifest.
    let inspect = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["inspect", "myapp:base"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let inspect_str = String::from_utf8(inspect).unwrap();
    assert!(
        inspect_str.contains("application/vnd.pichi.artifact.v1+json"),
        "inspect output should contain artifactType: {inspect_str}"
    );

    // Manifest has exactly one Scute layer (no PMI).
    let blob_store = FilesystemBlobStore::new(&g);
    let bytes = blob_store.get_blob(&manifest_digest).await.unwrap();
    let manifest: Manifest = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(manifest.layers.len(), 1, "exactly one layer (one scute)");
}

/// IMPORT-05 / CONTEXT D-06: `pichi import` accepts non-GPT input
/// (treats input as opaque bytes -- no GPT parsing, no validation).
#[tokio::test]
async fn import_accepts_non_gpt_input() {
    let tmp = TempDir::new().unwrap();
    let raw = tmp.path().join("not-a-disk.tar");
    // A few hundred bytes of "tar header"-ish data -- definitely not a
    // valid GPT. pichi import MUST succeed (per CONTEXT D-06).
    let mut buf = vec![0u8; 16 * 1024 * 4]; // 4 chunks at default 16 KiB
    let msg = b"This is a tar header string, not a GPT partition table at all. \
                pichi import MUST treat this as opaque bytes per CONTEXT D-06. ";
    let n = msg.len().min(buf.len());
    buf[..n].copy_from_slice(&msg[..n]);
    std::fs::write(&raw, &buf).unwrap();

    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["import", raw.to_str().unwrap(), "tar:opaque"])
        .assert()
        .success();
}

/// IMPORT-07: `--chunk-size` rejected for non-power-of-two, < 8, and > 2048.
#[tokio::test]
async fn rejects_bad_chunk_size() {
    let tmp = TempDir::new().unwrap();
    let raw = tmp.path().join("input.raw");
    std::fs::write(&raw, vec![0u8; 4096]).unwrap();

    // Not power of two.
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["import", "--chunk-size", "7", raw.to_str().unwrap(), "x:y"])
        .assert()
        .failure();

    // < 8 sectors.
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["import", "--chunk-size", "4", raw.to_str().unwrap(), "x:y"])
        .assert()
        .failure();

    // > MAX (2048 sectors).
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args([
            "import",
            "--chunk-size",
            "4096",
            raw.to_str().unwrap(),
            "x:y",
        ])
        .assert()
        .failure();
}

/// CONTEXT D-01 (e2e): default-flag import writes salt = 32 zero bytes
/// = 64 hex zero chars in the manifest's scute annotation.
#[tokio::test]
async fn bottom_scute_salt_is_zero_prefix_e2e() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let raw = tmp.path().join("input.raw");
    write_small_fixture(&raw);

    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["import", raw.to_str().unwrap(), "salt:check"])
        .assert()
        .success();

    // Resolve under canonical form.
    let db = FilesystemTagDb::open(&g).unwrap();
    let manifest_digest = db
        .resolve_tag("docker.io/library/salt:check")
        .await
        .unwrap()
        .unwrap();
    let blob_store = FilesystemBlobStore::new(&g);
    let bytes = blob_store.get_blob(&manifest_digest).await.unwrap();
    let manifest: Manifest = serde_json::from_slice(&bytes).unwrap();
    let salt = match &manifest.layers[0] {
        Layer::Scute(s) => s.annotations.salt.clone(),
        _ => panic!("expected scute layer"),
    };
    assert_eq!(
        salt,
        "0".repeat(64),
        "salt prefix is 32 zero bytes (= 64 hex zeros)"
    );
}

/// IMPORT-01 (extended): without --pmi, pichi inspect reports pmi_present: false.
/// Extends import_happy_path by running inspect and asserting on pmi_present.
#[tokio::test]
async fn import_without_pmi_reports_pmi_present_false() {
    let tmp = TempDir::new().unwrap();
    let raw = tmp.path().join("input.raw");
    write_small_fixture(&raw);

    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["import", raw.to_str().unwrap(), "nopmi:base"])
        .assert()
        .success();

    let inspect = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["inspect", "nopmi:base"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let inspect_str = String::from_utf8(inspect).unwrap();
    assert!(
        inspect_str.contains(r#""pmi_present": false"#),
        "inspect must report pmi_present: false for a base (non-appliance) import: {inspect_str}"
    );
}

/// Happy path with --pmi: produces 4 blobs, 2-layer manifest, pmi_present: true.
#[tokio::test]
async fn import_with_pmi_bundles_layer() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let raw = tmp.path().join("input.raw");
    write_small_fixture(&raw);

    // Fake PMI: 8 KiB of arbitrary bytes (opaque to pichi import).
    let fake_pmi = tmp.path().join("payload.pmi");
    std::fs::write(&fake_pmi, vec![0xC3u8; 8192]).unwrap();

    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args([
            "import",
            "--pmi",
            fake_pmi.to_str().unwrap(),
            raw.to_str().unwrap(),
            "appliance:bundle",
        ])
        .assert()
        .success();

    // Four blobs: cow, verity, pmi, manifest.
    let blobs = g.join("blobs").join("sha256");
    let n = std::fs::read_dir(&blobs).unwrap().count();
    assert_eq!(
        n,
        4,
        "expected 4 blobs (cow + verity + pmi + manifest) in {}",
        blobs.display()
    );

    // Tag resolves.
    let db = FilesystemTagDb::open(&g).unwrap();
    let manifest_digest = db
        .resolve_tag("docker.io/library/appliance:bundle")
        .await
        .unwrap()
        .expect("tag must resolve after --pmi import");

    // Manifest has exactly 2 layers: Scute + Pmi.
    let blob_store = FilesystemBlobStore::new(&g);
    let bytes = blob_store.get_blob(&manifest_digest).await.unwrap();
    let manifest: Manifest = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        manifest.layers.len(),
        2,
        "appliance manifest must have 2 layers (Scute + Pmi)"
    );
    let has_pmi = manifest.layers.iter().any(|l| matches!(l, Layer::Pmi(_)));
    assert!(
        has_pmi,
        "appliance manifest must contain a Layer::Pmi layer"
    );

    // PMI descriptor size matches the fake file byte length.
    let pmi_layer = manifest
        .layers
        .iter()
        .find(|l| matches!(l, Layer::Pmi(_)))
        .expect("must have PMI layer");
    assert_eq!(
        pmi_layer.size(),
        8192u64,
        "PMI descriptor size must match the input file's byte length"
    );

    // pichi inspect reports pmi_present: true.
    let inspect = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["inspect", "appliance:bundle"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let inspect_str = String::from_utf8(inspect).unwrap();
    assert!(
        inspect_str.contains(r#""pmi_present": true"#),
        "pichi inspect must report pmi_present: true for appliance import: {inspect_str}"
    );
}

/// Negative: --pmi with a missing file fails non-zero, leaves no tag.
#[tokio::test]
async fn import_with_missing_pmi_file_errors() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let raw = tmp.path().join("input.raw");
    write_small_fixture(&raw);

    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args([
            "import",
            "--pmi",
            "/nonexistent/pmi/path.bin",
            raw.to_str().unwrap(),
            "bad:pmi",
        ])
        .assert()
        .failure();

    // No partial state: the tag must not be set.
    let db = FilesystemTagDb::open(&g).unwrap();
    let resolved = db.resolve_tag("docker.io/library/bad:pmi").await.unwrap();
    assert!(
        resolved.is_none(),
        "tag must NOT be set after a missing-pmi failure (no partial state)"
    );
}

/// VALIDATION row "Manifest contains chain-wide verity annotations":
/// chain annotations carry locked Phase 42 D-06 values.
#[tokio::test]
async fn manifest_has_locked_verity_params_e2e() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let raw = tmp.path().join("input.raw");
    write_small_fixture(&raw);

    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["import", raw.to_str().unwrap(), "params:check"])
        .assert()
        .success();

    // Resolve under canonical form.
    let db = FilesystemTagDb::open(&g).unwrap();
    let manifest_digest = db
        .resolve_tag("docker.io/library/params:check")
        .await
        .unwrap()
        .unwrap();
    let blob_store = FilesystemBlobStore::new(&g);
    let bytes = blob_store.get_blob(&manifest_digest).await.unwrap();
    let manifest: Manifest = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        manifest
            .annotations
            .get("dev.pichi.carapace.verity.algo")
            .map(String::as_str),
        Some("sha256")
    );
    assert_eq!(
        manifest
            .annotations
            .get("dev.pichi.carapace.verity.data-block-size")
            .map(String::as_str),
        Some("4096")
    );
    assert_eq!(
        manifest
            .annotations
            .get("dev.pichi.carapace.verity.hash-block-size")
            .map(String::as_str),
        Some("4096")
    );
}
