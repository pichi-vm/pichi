// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for `pichi inspect` (LOCAL-02 / SC2 / D-20).

use std::collections::BTreeMap;

use assert_cmd::Command;
use conglobate::artifact::{
    ConfigDescriptor, Layer, MEDIA_TYPE_PICHI_ARTIFACT_V1, Manifest, PmiDescriptor,
    ScuteAnnotations, ScuteDescriptor,
};
use pichi_storage::{BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};
use tempfile::TempDir;

fn chain_annotations() -> BTreeMap<String, String> {
    [
        ("dev.pichi.carapace.verity.data-block-size", "4096"),
        ("dev.pichi.carapace.verity.hash-block-size", "4096"),
        ("dev.pichi.carapace.verity.version", "1"),
        ("dev.pichi.carapace.verity.hash-type", "1"),
        (
            "dev.pichi.carapace.verity.hash",
            "sha256:1111111111111111111111111111111111111111111111111111111111111111",
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
                    data_blocks: 1u64.try_into().unwrap(),
                    salt: "00".repeat(32),
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
    let bytes = serde_json::to_vec(&m).unwrap();
    let digest =
        conglobate::artifact::Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&bytes).into());
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
        Some("sha256:1111111111111111111111111111111111111111111111111111111111111111")
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
                data_blocks: 1u64.try_into().unwrap(),
                salt: "00".repeat(32),
            },
        })],
        annotations: chain_annotations(),
    };
    let bytes = serde_json::to_vec(&m).unwrap();
    let digest =
        conglobate::artifact::Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&bytes).into());
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
        "sha256:1111111111111111111111111111111111111111111111111111111111111111"
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
    let digest =
        conglobate::artifact::Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&bytes).into());
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

#[tokio::test]
async fn inspect_decodes_launch_config_for_bare_and_indexed_images() {
    let config = serde_json::json!({"version":1,"requirements":{
        "slot":"pci", "cpus":{"required":2,"recommended":"max"},
        "memory":{"required":2048,"recommended":"max"},
        "storage":{"required":1024,"recommended":"max","slot":"mmio"},
        "interfaces":{"public":{"description":"Public traffic","ingress":{"required":[443],"recommended":[80,"all"]}}}
    }});
    for indexed in [false, true] {
        use sha2::Digest as _;
        let tmp = TempDir::new().unwrap();
        let root = graphroot(&tmp);
        let store = FilesystemBlobStore::new(&root);
        let bytes = serde_json::to_vec(&config).unwrap();
        let config_digest =
            conglobate::artifact::Digest::Sha256(sha2::Sha256::digest(&bytes).into());
        store.put_blob(&config_digest, &bytes).await.unwrap();
        let manifest = Manifest {
            schema_version: 2,
            media_type: "application/vnd.oci.image.manifest.v1+json".into(),
            artifact_type: MEDIA_TYPE_PICHI_ARTIFACT_V1.into(),
            config: ConfigDescriptor::for_config(config_digest.to_string(), bytes.len() as u64),
            layers: vec![Layer::Pmi(PmiDescriptor {
                digest: format!("sha256:{}", "11".repeat(32)),
                size: 4096,
            })],
            annotations: BTreeMap::new(),
        };
        let bytes = serde_json::to_vec(&manifest).unwrap();
        let manifest_digest =
            conglobate::artifact::Digest::Sha256(sha2::Sha256::digest(&bytes).into());
        store.put_blob(&manifest_digest, &bytes).await.unwrap();
        let digest = if indexed {
            let architecture = match std::env::consts::ARCH {
                "x86_64" => "amd64",
                "aarch64" => "arm64",
                other => panic!("unsupported test architecture {other}"),
            };
            let index = serde_json::json!({"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{
                "mediaType":"application/vnd.oci.image.manifest.v1+json","artifactType":MEDIA_TYPE_PICHI_ARTIFACT_V1,
                "digest":manifest_digest.to_string(),"size":bytes.len(),"platform":{"os":"pichi","architecture":architecture}
            }]});
            let bytes = serde_json::to_vec(&index).unwrap();
            let digest = conglobate::artifact::Digest::Sha256(sha2::Sha256::digest(&bytes).into());
            store.put_blob(&digest, &bytes).await.unwrap();
            digest
        } else {
            manifest_digest
        };
        FilesystemTagDb::open(&root)
            .unwrap()
            .set_tag("docker.io/library/configured:1", &digest)
            .await
            .unwrap();
        let output = Command::cargo_bin("pichi")
            .unwrap()
            .env("XDG_DATA_HOME", tmp.path())
            .args(["inspect", "configured:1"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(value["config"], config);
        assert_eq!(
            value["manifest"]["config"]["digest"],
            config_digest.to_string()
        );
        Command::cargo_bin("pichi")
            .unwrap()
            .env("XDG_DATA_HOME", tmp.path())
            .args([
                "inspect",
                "configured:1",
                "--format",
                "{{ config.requirements.storage.recommended }}",
            ])
            .assert()
            .success()
            .stdout("max\n");
    }
}
