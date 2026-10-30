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
    let tree = std::fs::read(
        source_store
            .blob_path(&cow)
            .prepared_files()
            .unwrap()
            .into_iter()
            .next()
            .unwrap(),
    )
    .unwrap();
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
        let sidecar = store
            .blob_path(&cow)
            .prepared_files()
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(std::fs::read(&sidecar).unwrap(), tree);
        std::fs::write(sidecar, b"stale cache verity").unwrap();
    }
}

#[tokio::test]
async fn same_blob_with_different_salts_survives_import_load_and_removal() {
    let dir = tempfile::tempdir().unwrap();
    let source_home = dir.path().join("source");
    let destination_home = dir.path().join("destination");
    let source_store = FilesystemBlobStore::new(source_home.join("pichi/storage"));
    let store = FilesystemBlobStore::new(destination_home.join("pichi/storage"));
    let raw = dir.path().join("input.raw");
    std::fs::write(&raw, vec![7; 8192]).unwrap();
    let combined = dir.path().join("combined");
    let mut manifests = Vec::new();
    let mut expected_trees = Vec::new();
    let mut index: serde_json::Value = serde_json::Value::Null;
    for suffix in ["aa", "bb"] {
        let tag = format!("fixture.local/salt:{suffix}");
        let output = Command::cargo_bin("pichi")
            .unwrap()
            .env("XDG_DATA_HOME", &source_home)
            .args(["import", "raw"])
            .arg(&raw)
            .args(["--salt", suffix, "-t", &tag, "--quiet"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let digest: Digest = std::str::from_utf8(&output)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let bytes = source_store.get_blob(&digest).await.unwrap();
        let (_, manifest_bytes) = index::manifest(&source_store, &digest).await;
        let manifest = Manifest::decode_json(manifest_bytes.as_slice()).unwrap();
        let cow: Digest = manifest.layers[0].digest_str().parse().unwrap();
        let tree = source_store
            .blob_path(&cow)
            .prepared_files()
            .unwrap()
            .into_iter()
            .find(|path| {
                !expected_trees
                    .iter()
                    .any(|(name, _)| path.file_name().unwrap() == name)
            })
            .unwrap();
        expected_trees.push((
            tree.file_name().unwrap().to_owned(),
            std::fs::read(tree).unwrap(),
        ));
        let layout = if suffix == "aa" {
            combined.clone()
        } else {
            dir.path().join("second")
        };
        Command::cargo_bin("pichi")
            .unwrap()
            .env("XDG_DATA_HOME", &source_home)
            .args(["save", &tag, "-o"])
            .arg(&layout)
            .assert()
            .success();
        let next: serde_json::Value =
            serde_json::from_slice(&std::fs::read(layout.join("index.json")).unwrap()).unwrap();
        if index.is_null() {
            index = next;
        } else {
            index["manifests"]
                .as_array_mut()
                .unwrap()
                .extend(next["manifests"].as_array().unwrap().iter().cloned());
            for entry in std::fs::read_dir(layout.join("blobs/sha256")).unwrap() {
                let entry = entry.unwrap();
                std::fs::copy(
                    entry.path(),
                    combined.join("blobs/sha256").join(entry.file_name()),
                )
                .unwrap();
            }
        }
        manifests.push((digest, manifest, bytes));
    }
    assert_eq!(
        manifests[0].1.layers[0].digest_str(),
        manifests[1].1.layers[0].digest_str()
    );
    assert_ne!(expected_trees[0].1, expected_trees[1].1);
    std::fs::write(
        combined.join("index.json"),
        serde_json::to_vec(&index).unwrap(),
    )
    .unwrap();
    let cow: Digest = manifests[0].1.layers[0].digest_str().parse().unwrap();
    for (name, bytes) in &expected_trees {
        assert_eq!(
            std::fs::read(source_store.blob_path(&cow).prepared_dir().join(name)).unwrap(),
            *bytes
        );
    }
    for _ in 0..2 {
        Command::cargo_bin("pichi")
            .unwrap()
            .env("XDG_DATA_HOME", &destination_home)
            .arg("load")
            .arg(&combined)
            .assert()
            .success();
        for (digest, _, bytes) in &manifests {
            assert_eq!(store.get_blob(digest).await.unwrap(), *bytes);
        }
        for (name, bytes) in &expected_trees {
            assert_eq!(
                std::fs::read(store.blob_path(&cow).prepared_dir().join(name)).unwrap(),
                *bytes
            );
        }
    }
    // The single optional transport tree must satisfy every manifest that
    // uses it. A tree valid for only one salt must reject the whole layout.
    let guest_tree = combined
        .join("blobs/sha256")
        .join(cow.hex())
        .with_extension("verity");
    std::fs::write(&guest_tree, &expected_trees[0].1).unwrap();
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", dir.path().join("rejected"))
        .arg("load")
        .arg(&combined)
        .assert()
        .failure();
    // Removing one manifest must preserve the still-referenced COW and both
    // preparation variants; removing the last reference reclaims them all.
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", &destination_home)
        .args(["rmi", "fixture.local/salt:aa"])
        .assert()
        .success();
    assert!(store.blob_path(&cow).exists());
    assert_eq!(store.blob_path(&cow).prepared_files().unwrap().len(), 2);
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", &destination_home)
        .args(["rmi", "fixture.local/salt:bb"])
        .assert()
        .success();
    assert_eq!(
        std::fs::read_dir(destination_home.join("pichi/storage/blobs/sha256"))
            .unwrap()
            .count(),
        0
    );
}
