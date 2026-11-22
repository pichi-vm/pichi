// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for `pichi tag` (LOCAL-04 / SC4).

#[path = "support/storage.rs"]
mod storage;
use storage::Storage as _;

use assert_cmd::Command;
use conglobate::artifact::{ConfigDescriptor, Layer, Manifest, ScuteAnnotations, ScuteDescriptor};
use pichi_storage::{BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};
use tempfile::TempDir;

fn graphroot(tmp: &TempDir) -> std::path::PathBuf {
    let p = tmp.path().join("pichi").join("storage");
    std::fs::create_dir_all(&p).unwrap();
    p
}

async fn populate_cache(graphroot: &std::path::Path, tag: &str) -> conglobate::artifact::Digest {
    let m = Manifest {
        schema_version: Default::default(),
        media_type: Default::default(),
        artifact_type: Default::default(),
        config: ConfigDescriptor::canonical(),
        layers: vec![Layer::Scute(ScuteDescriptor {
            media_type: conglobate::artifact::Compressable::Raw(Default::default()),
            digest: conglobate::artifact::Digest::Sha256([0x11; 32]),
            size: 1024,
            annotations: ScuteAnnotations {
                digest: conglobate::artifact::Digest::Sha256([0x11; 32]),
                size: std::num::NonZeroU64::new(1024).unwrap().into(),
                verity_digest: conglobate::artifact::Digest::Sha256([0x00; 32]),
                data_blocks: std::num::NonZeroU64::MIN.into(),
                salt: devmap_verity::Salt::new(&[0, 255]).unwrap().into(),
            },
        })],
        annotations: Default::default(),
    };
    let bytes = serde_json::to_vec(&m).unwrap();
    let digest =
        conglobate::artifact::Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&bytes).into());
    let blob_store = FilesystemBlobStore::new(graphroot);
    blob_store.put_blob(&digest, &bytes).await.unwrap();
    let db = FilesystemTagDb::open(graphroot).unwrap();
    db.set_tag(tag, &digest).await.unwrap();
    digest
}

fn count_blobs(graphroot: &std::path::Path) -> usize {
    let dir = graphroot.join("blobs").join("sha256");
    std::fs::read_dir(&dir).map(|it| it.count()).unwrap_or(0)
}

#[tokio::test]
async fn tag_creates_alias_no_blob_copy() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);

    let original_digest = populate_cache(&g, "docker.io/library/alpine:3").await;
    let blobs_before = count_blobs(&g);

    Command::cargo_bin("pichi")
        .unwrap()
        .storage(tmp.path())
        .args(["tag", "alpine:3", "docker.io/library/alpine:latest"])
        .assert()
        .success();

    let blobs_after = count_blobs(&g);
    assert_eq!(blobs_before, blobs_after, "tag must NOT copy blobs");

    let db = FilesystemTagDb::open(&g).unwrap();
    let digest_a = db.resolve_tag("docker.io/library/alpine:3").await.unwrap();
    let digest_b = db
        .resolve_tag("docker.io/library/alpine:latest")
        .await
        .unwrap();
    assert_eq!(digest_a, Some(original_digest.clone()));
    assert_eq!(digest_b, Some(original_digest));
}

#[tokio::test]
async fn tag_missing_source_errors() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    // ensure the storage directory exists so CacheLayout resolves
    drop(g);

    Command::cargo_bin("pichi")
        .unwrap()
        .storage(tmp.path())
        .args(["tag", "nonexistent:tag", "newalias:1"])
        .assert()
        .failure();
}
