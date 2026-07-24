// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for `pichi inspect` (LOCAL-02 / SC2 / D-20).

use std::collections::BTreeMap;

use assert_cmd::Command;
use pichi_artifact::{
    ConfigDescriptor, Layer, MEDIA_TYPE_PICHI_ARTIFACT_V1, Manifest, PmiDescriptor,
    ScuteAnnotations, ScuteDescriptor,
};
use pichi_storage::{BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};
use tempfile::TempDir;

fn chain_annotations() -> BTreeMap<String, String> {
    [
        ("dev.pichi.carapace.verity.algo", "sha256"),
        ("dev.pichi.carapace.verity.data-block-size", "4096"),
        ("dev.pichi.carapace.verity.hash-block-size", "4096"),
        ("dev.pichi.carapace.verity.version", "1"),
        ("dev.pichi.carapace.verity.hash-type", "1"),
        (
            "dev.pichi.carapace.verity.hash",
            "abababababababababababababababababababababababababababababababab",
        ),
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

#[tokio::test]
async fn inspect_bare_manifest_returns_full_schema() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let m = Manifest {
        schema_version: 2,
        media_type: "application/vnd.oci.image.manifest.v1+json".into(),
        artifact_type: MEDIA_TYPE_PICHI_ARTIFACT_V1.into(),
        config: ConfigDescriptor::canonical(),
        layers: vec![
            Layer::Scute(ScuteDescriptor {
                digest: "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                    .into(),
                size: 4096,
                annotations: ScuteAnnotations {
                    salt: "dead".into(),
                },
            }),
            Layer::Pmi(PmiDescriptor {
                digest: "sha256:2222222222222222222222222222222222222222222222222222222222222222"
                    .into(),
                size: 8192,
            }),
        ],
        annotations: chain_annotations(),
    };
    let bytes = m.to_bytes().unwrap();
    let digest = m.digest().unwrap();
    let bs = FilesystemBlobStore::new(&g);
    bs.put_blob(&digest, &bytes).await.unwrap();
    let db = FilesystemTagDb::open(&g).unwrap();
    db.set_tag("docker.io/library/myapp:base", &digest)
        .await
        .unwrap();

    let out = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["inspect", "myapp:base"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    let v: serde_json::Value = serde_json::from_str(&s).expect("inspect output must be JSON");
    // Output is the manifest verbatim plus the resolved digest — no computed
    // `_pichi` summary (everything is derivable from the manifest itself).
    assert_eq!(v["digest"].as_str(), Some(digest.to_string().as_str()));
    assert_eq!(
        v["manifest"]["artifactType"].as_str(),
        Some(MEDIA_TYPE_PICHI_ARTIFACT_V1)
    );
    assert_eq!(v["manifest"]["layers"].as_array().map(Vec::len), Some(2));
    // Default JSON keeps canonical (flat) OCI annotation keys.
    assert_eq!(
        v["manifest"]["annotations"]["dev.pichi.carapace.verity.hash"].as_str(),
        Some("abababababababababababababababababababababababababababababababab")
    );
    assert!(v.get("_pichi").is_none(), "no computed sidecar");
}

/// The dotted annotation key is reachable via `--format` using minijinja's
/// subscript syntax — no data reshaping.
#[tokio::test]
async fn inspect_format_reaches_dotted_annotation() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let m = Manifest {
        schema_version: 2,
        media_type: "application/vnd.oci.image.manifest.v1+json".into(),
        artifact_type: MEDIA_TYPE_PICHI_ARTIFACT_V1.into(),
        config: ConfigDescriptor::canonical(),
        layers: vec![Layer::Scute(ScuteDescriptor {
            digest: "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                .into(),
            size: 4096,
            annotations: ScuteAnnotations {
                salt: "dead".into(),
            },
        })],
        annotations: chain_annotations(),
    };
    let bytes = m.to_bytes().unwrap();
    let digest = m.digest().unwrap();
    let bs = FilesystemBlobStore::new(&g);
    bs.put_blob(&digest, &bytes).await.unwrap();
    let db = FilesystemTagDb::open(&g).unwrap();
    db.set_tag("docker.io/library/myapp:base", &digest)
        .await
        .unwrap();

    let out = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args([
            "inspect",
            "myapp:base",
            "--format",
            r#"{{ manifest.annotations["dev.pichi.carapace.verity.hash"] }}"#,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert_eq!(
        s.trim(),
        "abababababababababababababababababababababababababababababababab"
    );
}

#[tokio::test]
async fn inspect_image_index_lists_entries() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    // Hand-craft an OCI image index with one pichi entry + one container entry.
    let index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [
            {
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "artifactType": MEDIA_TYPE_PICHI_ARTIFACT_V1,
                "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "size": 1024,
                "platform": {"architecture": "amd64", "os": "linux"}
            },
            {
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "size": 2048,
                "platform": {"architecture": "amd64", "os": "linux"}
            }
        ]
    });
    let bytes = serde_json::to_vec(&index).unwrap();
    let digest = pichi_artifact::Digest::from_bytes_sha256(&bytes);
    let bs = FilesystemBlobStore::new(&g);
    bs.put_blob(&digest, &bytes).await.unwrap();
    let db = FilesystemTagDb::open(&g).unwrap();
    db.set_tag("registry.io/multi:1", &digest).await.unwrap();

    let out = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["inspect", "registry.io/multi:1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(
        v["media_type"].as_str(),
        Some("application/vnd.oci.image.index.v1+json")
    );
    let entries = v["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 2);
    let pichi_entries: Vec<_> = entries.iter().filter(|e| e["is_pichi"] == true).collect();
    assert_eq!(pichi_entries.len(), 1);
    assert!(v["note"].as_str().unwrap().contains("pichi"));
}
