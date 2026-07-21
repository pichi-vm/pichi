// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for `pichi tag` (LOCAL-04 / SC4).

use std::collections::BTreeMap;

use assert_cmd::Command;
use pichi_artifact::{
    ConfigDescriptor, Layer, MEDIA_TYPE_PICHI_ARTIFACT_V1, Manifest, ScuteAnnotations,
    ScuteDescriptor,
};
use pichi_storage::{BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};
use tempfile::TempDir;

fn chain_annotations() -> BTreeMap<String, String> {
    [
        ("dev.pichi.carapace.verity.algo", "sha256"),
        ("dev.pichi.carapace.verity.data-block-size", "4096"),
        ("dev.pichi.carapace.verity.hash-block-size", "4096"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn graphroot(tmp: &TempDir) -> std::path::PathBuf {
    let p = tmp.path().join("pichi").join("storage");
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn populate_cache(graphroot: &std::path::Path, tag: &str) -> pichi_artifact::Digest {
    let m = Manifest {
        schema_version: 2,
        media_type: "application/vnd.oci.image.manifest.v1+json".into(),
        artifact_type: MEDIA_TYPE_PICHI_ARTIFACT_V1.into(),
        config: ConfigDescriptor::canonical(),
        layers: vec![Layer::Scute(ScuteDescriptor {
            digest: "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                .into(),
            size: 1024,
            annotations: ScuteAnnotations {
                salt: "00ff".into(),
            },
        })],
        annotations: chain_annotations(),
    };
    let bytes = m.to_bytes().unwrap();
    let digest = m.digest().unwrap();
    let blob_store = FilesystemBlobStore::new(graphroot);
    blob_store.put_blob(&digest, &bytes).unwrap();
    let db = FilesystemTagDb::open(graphroot).unwrap();
    db.set_tag(tag, &digest).unwrap();
    digest
}

fn count_blobs(graphroot: &std::path::Path) -> usize {
    let dir = graphroot.join("blobs").join("sha256");
    std::fs::read_dir(&dir).map(|it| it.count()).unwrap_or(0)
}

#[test]
fn tag_creates_alias_no_blob_copy() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);

    let original_digest = populate_cache(&g, "docker.io/library/alpine:3");
    let blobs_before = count_blobs(&g);

    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["tag", "alpine:3", "docker.io/library/alpine:latest"])
        .assert()
        .success();

    let blobs_after = count_blobs(&g);
    assert_eq!(blobs_before, blobs_after, "tag must NOT copy blobs");

    let db = FilesystemTagDb::open(&g).unwrap();
    let digest_a = db.resolve_tag("docker.io/library/alpine:3").unwrap();
    let digest_b = db.resolve_tag("docker.io/library/alpine:latest").unwrap();
    assert_eq!(digest_a, Some(original_digest.clone()));
    assert_eq!(digest_b, Some(original_digest));
}

#[test]
fn tag_missing_source_errors() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    // ensure the storage directory exists so CacheLayout resolves
    drop(g);

    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["tag", "nonexistent:tag", "newalias:1"])
        .assert()
        .failure();
}
