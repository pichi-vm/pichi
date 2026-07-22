// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `pichi import pmi` integration tests (BUILD.md §15).
//!
//! `import pmi` turns a pre-built detached PMI (+ base DTB, + optional config)
//! into a bootable artifact. Without `--carapace` the result is PMI-only
//! (bootable, no rootfs); with `--carapace <ref>` the referenced carapace's
//! scutes are combined in. The carapace reference is read-only — its tag is
//! never modified — and the bootable artifact is always its own `-t`.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use pichi_artifact::{Layer, Manifest};
use pichi_storage::{BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};
use tempfile::TempDir;

fn graphroot(tmp: &TempDir) -> PathBuf {
    let p = tmp.path().join("pichi").join("storage");
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn write_raw(path: &Path) {
    const CHUNK: usize = 16 * 1024;
    let mut buf = vec![0u8; CHUNK * 64];
    buf[5 * CHUNK..6 * CHUNK].fill(0xA1);
    std::fs::write(path, &buf).unwrap();
}

fn pichi(tmp: &TempDir) -> Command {
    let mut c = Command::cargo_bin("pichi").unwrap();
    c.env("XDG_DATA_HOME", tmp.path());
    c
}

async fn resolve_manifest(g: &Path, key: &str) -> Manifest {
    let db = FilesystemTagDb::open(g).unwrap();
    let digest = db
        .resolve_tag(key)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("tag must resolve: {key}"));
    let blob_store = FilesystemBlobStore::new(g);
    let bytes = blob_store.get_blob(&digest).await.unwrap();
    Manifest::from_reader_validated(bytes.as_slice()).unwrap()
}

/// Fixture: a raw image, a PMI, and a DTB in `tmp`.
fn fixtures(tmp: &TempDir) -> (PathBuf, PathBuf, PathBuf) {
    let raw = tmp.path().join("input.raw");
    write_raw(&raw);
    let pmi = tmp.path().join("boot.pmi");
    std::fs::write(&pmi, vec![0xC3u8; 8192]).unwrap();
    let dtb = tmp.path().join("base.dtb");
    std::fs::write(&dtb, vec![0xD7u8; 4096]).unwrap();
    (raw, pmi, dtb)
}

/// Kind 2: `import pmi --carapace` produces a bootable rootfs appliance under
/// its own tag, and leaves the carapace tag untouched (never overwritten).
#[tokio::test]
async fn import_pmi_with_carapace_keeps_base_untouched() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let (raw, pmi, dtb) = fixtures(&tmp);

    pichi(&tmp)
        .args(["import", "raw", raw.to_str().unwrap(), "-t", "app:base"])
        .assert()
        .success();
    pichi(&tmp)
        .args([
            "import",
            "pmi",
            pmi.to_str().unwrap(),
            "--dtb",
            dtb.to_str().unwrap(),
            "--carapace",
            "app:base",
            "-t",
            "app:1",
        ])
        .assert()
        .success();

    // The carapace tag is unchanged — still a non-bootable carapace.
    let base = resolve_manifest(&g, "docker.io/library/app:base").await;
    assert!(
        !base.layers.iter().any(|l| matches!(l, Layer::Pmi(_))),
        "--carapace must not modify the carapace tag"
    );
    assert!(base.layers.iter().any(|l| matches!(l, Layer::Scute(_))));

    // The appliance carries scute + pmi + dtb.
    let app = resolve_manifest(&g, "docker.io/library/app:1").await;
    assert_eq!(app.layers.len(), 3, "scute + pmi + dtb");
    assert!(app.layers.iter().any(|l| matches!(l, Layer::Scute(_))));
    assert!(app.layers.iter().any(|l| matches!(l, Layer::Pmi(_))));
    assert!(app.layers.iter().any(|l| matches!(l, Layer::Dtb(_))));
}

/// Kind 3: `import pmi` without `--carapace` produces a PMI-only bootable
/// artifact — no scute layers, no carapace verity annotations.
#[tokio::test]
async fn import_pmi_without_carapace_is_pmi_only() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let (_raw, pmi, dtb) = fixtures(&tmp);

    pichi(&tmp)
        .args([
            "import",
            "pmi",
            pmi.to_str().unwrap(),
            "--dtb",
            dtb.to_str().unwrap(),
            "-t",
            "boot:1",
        ])
        .assert()
        .success();

    let m = resolve_manifest(&g, "docker.io/library/boot:1").await;
    assert_eq!(m.layers.len(), 2, "pmi + dtb only");
    assert!(m.layers.iter().any(|l| matches!(l, Layer::Pmi(_))));
    assert!(m.layers.iter().any(|l| matches!(l, Layer::Dtb(_))));
    assert!(
        !m.layers.iter().any(|l| matches!(l, Layer::Scute(_))),
        "PMI-only artifact has no scutes"
    );
    assert!(
        m.carapace_verity_hash().is_none(),
        "PMI-only artifact declares no carapace verity hash"
    );
}

/// `import raw` without `-t` caches an ephemeral carapace (no tag) and prints
/// the manifest digest — which is directly usable as a `--carapace` reference.
#[tokio::test]
async fn import_raw_without_tag_prints_digest_usable_as_carapace() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let raw = tmp.path().join("input.raw");
    write_raw(&raw);
    let pmi = tmp.path().join("boot.pmi");
    std::fs::write(&pmi, vec![0xC3u8; 8192]).unwrap();
    let dtb = tmp.path().join("base.dtb");
    std::fs::write(&dtb, vec![0xD7u8; 4096]).unwrap();

    let out = pichi(&tmp)
        .args(["import", "raw", raw.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let digest = String::from_utf8(out).unwrap();
    let digest = digest.trim();
    assert!(
        digest.starts_with("sha256:") && digest.len() == "sha256:".len() + 64,
        "import prints the manifest digest: {digest:?}"
    );

    // No tags were created.
    let db = FilesystemTagDb::open(&g).unwrap();
    assert!(
        db.list_tags().await.unwrap().is_empty(),
        "untagged import must not create a tag"
    );

    // The printed digest works directly as a --carapace reference.
    pichi(&tmp)
        .args([
            "import",
            "pmi",
            pmi.to_str().unwrap(),
            "--dtb",
            dtb.to_str().unwrap(),
            "--carapace",
            digest,
            "-t",
            "app:1",
        ])
        .assert()
        .success();
    let app = resolve_manifest(&g, "docker.io/library/app:1").await;
    assert!(app.layers.iter().any(|l| matches!(l, Layer::Scute(_))));
    assert!(app.layers.iter().any(|l| matches!(l, Layer::Pmi(_))));
}

/// `--config` stores the launch contract as the manifest config blob
/// (`vnd.pichi.config.v1+json`) instead of the OCI empty config.
#[tokio::test]
async fn import_pmi_with_config_sets_config_blob() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let (_raw, pmi, dtb) = fixtures(&tmp);
    let cfg = tmp.path().join("config.yaml");
    std::fs::write(
        &cfg,
        "requirements:\n  memory:\n    required: 536870912\n  cpus:\n    required: 2\n",
    )
    .unwrap();

    pichi(&tmp)
        .args([
            "import",
            "pmi",
            pmi.to_str().unwrap(),
            "--dtb",
            dtb.to_str().unwrap(),
            "--config",
            cfg.to_str().unwrap(),
            "-t",
            "boot:1",
        ])
        .assert()
        .success();

    let m = resolve_manifest(&g, "docker.io/library/boot:1").await;
    assert!(
        !m.config.is_empty(),
        "config blob must be a real vnd.pichi.config.v1+json, not the empty config"
    );
    assert_eq!(m.config.media_type, "application/vnd.pichi.config.v1+json");
}

/// `import pmi` without `-t` caches the bootable artifact untagged and prints
/// its manifest digest (consistent with `import raw`).
#[tokio::test]
async fn import_pmi_without_tag_prints_digest_and_leaves_no_tag() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let (_raw, pmi, dtb) = fixtures(&tmp);

    let out = pichi(&tmp)
        .args([
            "import",
            "pmi",
            pmi.to_str().unwrap(),
            "--dtb",
            dtb.to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let digest = String::from_utf8(out).unwrap();
    assert!(
        digest.trim().starts_with("sha256:"),
        "prints the manifest digest: {digest:?}"
    );

    let db = FilesystemTagDb::open(&g).unwrap();
    assert!(
        db.list_tags().await.unwrap().is_empty(),
        "untagged import pmi must not create a tag"
    );
}

/// A missing PMI file aborts before any tag is created (no partial state).
#[tokio::test]
async fn import_pmi_missing_file_errors() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let (_raw, _pmi, dtb) = fixtures(&tmp);

    pichi(&tmp)
        .args([
            "import",
            "pmi",
            "/nonexistent/boot.pmi",
            "--dtb",
            dtb.to_str().unwrap(),
            "-t",
            "boot:1",
        ])
        .assert()
        .failure();

    let db = FilesystemTagDb::open(&g).unwrap();
    assert!(
        db.list_tags().await.unwrap().is_empty(),
        "no tag on failure"
    );
}

/// `--carapace` pointing at a missing reference fails cleanly.
#[tokio::test]
async fn import_pmi_unknown_carapace_errors() {
    let tmp = TempDir::new().unwrap();
    let _g = graphroot(&tmp);
    let (_raw, pmi, dtb) = fixtures(&tmp);

    pichi(&tmp)
        .args([
            "import",
            "pmi",
            pmi.to_str().unwrap(),
            "--dtb",
            dtb.to_str().unwrap(),
            "--carapace",
            "nope:1",
            "-t",
            "app:1",
        ])
        .assert()
        .failure();
}
