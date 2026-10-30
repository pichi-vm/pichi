// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

#[path = "support/index.rs"]
mod index;

use assert_cmd::Command;
use conglobate::artifact::{Digest, Manifest};
use pichi::cmd::manifest_ext::ManifestExt as _;
use pichi_storage::{BlobSidecarExt, BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};

#[tokio::test]
async fn save_load_preserves_manifest_and_rebuilds_verity_on_cache_hits() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let destination = dir.path().join("destination");
    let raw = dir.path().join("input.raw");
    let layout = dir.path().join("layout");
    std::fs::write(&raw, vec![7; 8192]).unwrap();
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", &source)
        .args(["import", "raw"])
        .arg(&raw)
        .args(["-t", "roundtrip:v1"])
        .assert()
        .success();
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", &source)
        .args(["save", "roundtrip:v1", "-o"])
        .arg(&layout)
        .assert()
        .success();

    let source = source.join("pichi/storage");
    let destination = destination.join("pichi/storage");
    let source_db = FilesystemTagDb::open(&source).unwrap();
    let digest = source_db
        .resolve_tag("docker.io/library/roundtrip:v1")
        .await
        .unwrap()
        .unwrap();
    let source_store = FilesystemBlobStore::new(&source);
    let bytes = source_store.get_blob(&digest).await.unwrap();
    let (manifest_digest, manifest_bytes) = index::manifest(&source_store, &digest).await;
    let manifest = Manifest::decode_json(manifest_bytes.as_slice()).unwrap();
    let cow: Digest = manifest.layers[0].digest_str().parse().unwrap();
    let tree = std::fs::read(source_store.blob_path(&cow).verity_path()).unwrap();
    let store = FilesystemBlobStore::new(&destination);
    for _ in 0..2 {
        Command::cargo_bin("pichi")
            .unwrap()
            .env("XDG_DATA_HOME", dir.path().join("destination"))
            .arg("load")
            .arg(&layout)
            .assert()
            .success();
        let db = FilesystemTagDb::open(&destination).unwrap();
        assert_eq!(
            db.resolve_tag("docker.io/library/roundtrip:v1")
                .await
                .unwrap(),
            Some(digest.clone())
        );
        assert_eq!(store.get_blob(&digest).await.unwrap(), bytes);
        assert_eq!(
            store.get_blob(&manifest_digest).await.unwrap(),
            manifest_bytes
        );
        let sidecar = store.blob_path(&cow).verity_path();
        assert_eq!(std::fs::read(&sidecar).unwrap(), tree);
        std::fs::write(sidecar, b"stale cache verity").unwrap();
    }
}
