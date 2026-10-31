// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Cache lifetime follows OCI indexes, including shared and partially cached images.

use conglobate::artifact::Digest;
use pichi_storage::{BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};
use serde_json::json;
use sha2::{Digest as _, Sha256};
use std::process::{Command, Output};
use tempfile::TempDir;

struct Fixture {
    directory: TempDir,
    store: FilesystemBlobStore,
    tags: FilesystemTagDb,
    manifest: Digest,
    config: Digest,
    layer: Digest,
}

impl Fixture {
    async fn new() -> Self {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("pichi/storage");
        let store = FilesystemBlobStore::new(&root);
        let tags = FilesystemTagDb::open(&root).unwrap();
        let layer = Self::put(&store, &[42; 4096]).await;
        let config = Self::put(&store, b"{}").await;
        // One data block has no tree levels: its root hashes the salt and block.
        let root = Sha256::digest([&[0; 32][..], &[42; 4096][..]].concat());
        let bytes = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "artifactType": "application/vnd.pichi.artifact.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.empty.v1+json",
                "digest": config.to_string(), "size": 2, "data": "e30="
            },
            "layers": [{
                "mediaType": "application/vnd.pichi.scute.v1",
                "digest": layer.to_string(), "size": 4096,
                "annotations": {
                    "dev.pichi.scute.verity.salt": "00".repeat(32),
                    "dev.pichi.scute.verity.data-blocks": "1"
                }
            }],
            "annotations": {
                "dev.pichi.carapace.verity.algo": "sha256",
                "dev.pichi.carapace.verity.data-block-size": "4096",
                "dev.pichi.carapace.verity.hash-block-size": "4096",
                "dev.pichi.carapace.verity.version": "1",
                "dev.pichi.carapace.verity.hash-type": "1",
                "dev.pichi.carapace.verity.hash": hex::encode(root)
            }
        }))
        .unwrap();
        let manifest = Self::put(&store, &bytes).await;
        Self {
            directory,
            store,
            tags,
            manifest,
            config,
            layer,
        }
    }

    async fn put(store: &FilesystemBlobStore, bytes: &[u8]) -> Digest {
        let digest = Digest::Sha256(Sha256::digest(bytes).into());
        store.put_blob(&digest, bytes).await.unwrap();
        digest
    }

    async fn index(&self, tag: &str, partial: bool) -> Digest {
        let bytes = self.store.get_blob(&self.manifest).await.unwrap();
        let mut entries = vec![json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": self.manifest.to_string(), "size": bytes.len(),
            "artifactType": "application/vnd.pichi.artifact.v1+json",
            "platform": {"os": "pichi", "architecture": match std::env::consts::ARCH { "x86_64" => "amd64", "aarch64" => "arm64", arch => arch }}
        })];
        if partial {
            entries.push(json!({
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": format!("sha256:{}", "ab".repeat(32)), "size": 123,
                "platform": {"os": "pichi", "architecture": "arm64"}
            }));
        }
        let bytes = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": entries,
            "annotations": {"org.opencontainers.image.title": tag}
        }))
        .unwrap();
        let digest = Self::put(&self.store, &bytes).await;
        self.tags.set_tag(tag, &digest).await.unwrap();
        digest
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(assert_cmd::cargo::cargo_bin!("pichi"))
            .env("XDG_DATA_HOME", self.directory.path())
            .args(args)
            .output()
            .unwrap()
    }

    async fn assert_content(&self, present: bool) {
        for digest in [&self.manifest, &self.config, &self.layer] {
            assert_eq!(self.store.blob_exists(digest).await, present, "{digest}");
        }
    }
}

#[tokio::test]
async fn prune_retains_index_descendants_and_cached_config() {
    for partial in [false, true] {
        let fixture = Fixture::new().await;
        let index = fixture.index("registry.example/image:one", partial).await;
        let output = fixture.run(&["system", "prune"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(fixture.store.blob_exists(&index).await);
        fixture.assert_content(true).await;
    }
}

#[tokio::test]
async fn removing_indexes_keeps_shared_content_until_the_last_reference() {
    let fixture = Fixture::new().await;
    let first = fixture.index("registry.example/image:one", false).await;
    let second = fixture.index("registry.example/image:two", false).await;
    let output = fixture.run(&["rmi", "registry.example/image:one"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fixture.store.blob_exists(&first).await);
    assert!(fixture.store.blob_exists(&second).await);
    fixture.assert_content(true).await;
    let output = fixture.run(&["rmi", "registry.example/image:two"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fixture.store.blob_exists(&second).await);
    fixture.assert_content(false).await;
}

#[tokio::test]
async fn prune_retains_config_of_a_directly_tagged_manifest() {
    let fixture = Fixture::new().await;
    fixture
        .tags
        .set_tag("registry.example/image:one", &fixture.manifest)
        .await
        .unwrap();
    let output = fixture.run(&["system", "prune"]);
    assert!(output.status.success());
    fixture.assert_content(true).await;
}

#[tokio::test]
async fn corrupt_live_metadata_stops_collection_before_any_deletion() {
    for corrupt_index in [false, true] {
        let fixture = Fixture::new().await;
        let index = fixture.index("registry.example/image:one", false).await;
        let digest = if corrupt_index {
            &index
        } else {
            &fixture.manifest
        };
        std::fs::write(fixture.store.blob_path(digest), b"{}").unwrap();
        let orphan = Fixture::put(&fixture.store, b"unreferenced").await;
        for args in [["system", "prune"], ["rmi", "registry.example/image:one"]] {
            let output = fixture.run(&args);
            assert!(
                !output.status.success(),
                "collection accepted corrupt metadata"
            );
            fixture.assert_content(true).await;
            assert!(fixture.store.blob_exists(&orphan).await);
        }
    }
}

#[tokio::test]
async fn inconsistent_index_descriptors_abort_collection() {
    for (field, replacement, message) in [
        ("size", json!(1), "index entry size mismatch"),
        (
            "mediaType",
            json!("application/vnd.oci.image.index.v1+json"),
            "index entry media type mismatch",
        ),
    ] {
        let fixture = Fixture::new().await;
        let tag = "registry.example/image:one";
        let index = fixture.index(tag, false).await;
        let mut value: serde_json::Value =
            serde_json::from_slice(&fixture.store.get_blob(&index).await.unwrap()).unwrap();
        value["manifests"][0][field] = replacement;
        let digest = Fixture::put(&fixture.store, &serde_json::to_vec(&value).unwrap()).await;
        fixture.tags.set_tag(tag, &digest).await.unwrap();
        let output = fixture.run(&["system", "prune"]);
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(message), "{error}");
        fixture.assert_content(true).await;
        assert!(fixture.store.blob_exists(&index).await);
    }
}

#[tokio::test]
async fn index_export_and_import_preserve_original_bytes() {
    let fixture = Fixture::new().await;
    let tag = "registry.example/image:one";
    let index = fixture.index(tag, false).await;
    let bytes = fixture.store.get_blob(&index).await.unwrap();
    let layout = fixture.directory.path().join("export");
    let output = fixture.run(&["save", tag, "-o", layout.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(layout.join("blobs/sha256").join(index.hex())).unwrap(),
        bytes
    );
    let destination = TempDir::new().unwrap();
    let output = Command::new(assert_cmd::cargo::cargo_bin!("pichi"))
        .env("XDG_DATA_HOME", destination.path())
        .args(["load", layout.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let store = FilesystemBlobStore::new(destination.path().join("pichi/storage"));
    let tags = FilesystemTagDb::open(destination.path().join("pichi/storage")).unwrap();
    assert_eq!(tags.resolve_tag(tag).await.unwrap(), Some(index.clone()));
    assert_eq!(store.get_blob(&index).await.unwrap(), bytes);
    assert!(store.blob_exists(&fixture.layer).await);
}

#[tokio::test]
async fn partial_index_export_fails_without_replacing_its_identity() {
    let fixture = Fixture::new().await;
    let tag = "registry.example/image:one";
    let index = fixture.index(tag, true).await;
    let bytes = fixture.store.get_blob(&index).await.unwrap();
    let layout = fixture.directory.path().join("export");
    for args in [
        vec!["save", tag, "-o", layout.to_str().unwrap()],
        vec!["push", tag],
    ] {
        let output = fixture.run(&args);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("all referenced platforms must be cached")
        );
        assert_eq!(
            fixture.tags.resolve_tag(tag).await.unwrap(),
            Some(index.clone())
        );
        assert_eq!(fixture.store.get_blob(&index).await.unwrap(), bytes);
    }
    assert!(!layout.exists());
}

#[tokio::test]
async fn indexed_image_commands_select_the_cached_platform() {
    let fixture = Fixture::new().await;
    let tag = "registry.example/image:one";
    let index = fixture.index(tag, false).await;
    let output = fixture.run(&["inspect", tag]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["digest"], index.to_string());
    assert_eq!(value["manifest_digest"], fixture.manifest.to_string());
    assert_eq!(
        value["manifest"]["layers"][0]["digest"],
        fixture.layer.to_string()
    );
    let output = fixture.run(&["images", "--format", "{{ ScuteCount }}"]);
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "1");
    let output = fixture.run(&["manifest", "create", "registry.example/image:combined", tag]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn manifest_create_combines_sources_without_selecting_the_host_platform() {
    let fixture = Fixture::new().await;
    let mut expected = Vec::new();
    for arch in ["amd64", "arm64"] {
        let tag = format!("registry.example/image:{arch}");
        let index = fixture.index(&tag, false).await;
        let mut value: serde_json::Value =
            serde_json::from_slice(&fixture.store.get_blob(&index).await.unwrap()).unwrap();
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fixture.store.get_blob(&fixture.manifest).await.unwrap())
                .unwrap();
        manifest["annotations"]["org.opencontainers.image.architecture"] = json!(arch);
        let bytes = serde_json::to_vec(&manifest).unwrap();
        let digest = Fixture::put(&fixture.store, &bytes).await;
        value["manifests"][0]["digest"] = json!(digest.to_string());
        value["manifests"][0]["size"] = json!(bytes.len());
        value["manifests"][0]["platform"]["architecture"] = json!(arch);
        let index = Fixture::put(&fixture.store, &serde_json::to_vec(&value).unwrap()).await;
        fixture.tags.set_tag(&tag, &index).await.unwrap();
        expected.push((tag, digest, bytes));
    }
    let list = "registry.example/image:combined";
    let output = fixture.run(&["manifest", "create", list, &expected[0].0, &expected[1].0]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for (arch, (tag, _, _)) in ["amd64", "arm64"].iter().zip(&expected) {
        let output = fixture.run(&[
            "manifest", "annotate", list, tag, "--os", "pichi", "--arch", arch,
        ]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let directory = fixture.directory.path().join("pichi/storage/manifests");
    let lists = std::fs::read_dir(directory)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(lists.len(), 1);
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(lists[0].path()).unwrap()).unwrap();
    for (entry, (_, digest, bytes)) in value["manifests"].as_array().unwrap().iter().zip(&expected)
    {
        assert_eq!(entry["digest"], digest.to_string());
        assert_eq!(entry["size"], bytes.len());
        assert_eq!(fixture.store.get_blob(digest).await.unwrap(), *bytes);
    }
    assert_eq!(value["manifests"][0]["platform"]["architecture"], "amd64");
    assert_eq!(value["manifests"][1]["platform"]["architecture"], "arm64");
}

#[tokio::test]
async fn manifest_create_rejects_ambiguous_or_invalid_sources_before_writing() {
    for fault in ["empty", "multiple", "size", "media-type", "digest"] {
        let fixture = Fixture::new().await;
        let tag = "registry.example/image:source";
        let index = fixture.index(tag, false).await;
        let mut value: serde_json::Value =
            serde_json::from_slice(&fixture.store.get_blob(&index).await.unwrap()).unwrap();
        match fault {
            "empty" => value["manifests"] = json!([]),
            "multiple" => {
                let duplicate = value["manifests"][0].clone();
                value["manifests"].as_array_mut().unwrap().push(duplicate);
            }
            "size" => value["manifests"][0]["size"] = json!(1),
            "media-type" => {
                value["manifests"][0]["mediaType"] =
                    json!("application/vnd.oci.image.index.v1+json")
            }
            "digest" => std::fs::write(fixture.store.blob_path(&fixture.manifest), b"{}").unwrap(),
            _ => unreachable!(),
        }
        let index = Fixture::put(&fixture.store, &serde_json::to_vec(&value).unwrap()).await;
        fixture.tags.set_tag(tag, &index).await.unwrap();
        let output = fixture.run(&["manifest", "create", "registry.example/image:combined", tag]);
        assert!(!output.status.success(), "accepted {fault} source");
        assert!(
            !fixture
                .directory
                .path()
                .join("pichi/storage/manifests")
                .exists()
        );
    }
}

#[tokio::test]
async fn cached_index_requires_the_selected_platform() {
    let fixture = Fixture::new().await;
    let tag = "registry.example/image:one";
    fixture.index(tag, true).await;
    let output = fixture.run(&["pull", "--pull=never", tag]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::remove_file(fixture.store.blob_path(&fixture.manifest)).unwrap();
    let output = fixture.run(&["pull", "--pull=never", tag]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("ref not in cache"));
}

#[tokio::test]
async fn local_imports_publish_retained_platform_indexes() {
    let fixture = Fixture::new().await;
    let raw = fixture.directory.path().join("input.raw");
    std::fs::write(&raw, [42; 8192]).unwrap();
    let output = fixture.run(&["import", "raw", raw.to_str().unwrap(), "-t", "local:base"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let root: Digest = String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let bytes = fixture.store.get_blob(&root).await.unwrap();
    let index: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        index["mediaType"],
        "application/vnd.oci.image.index.v1+json"
    );
    assert_eq!(index["manifests"].as_array().unwrap().len(), 1);
    let manifest: Digest = index["manifests"][0]["digest"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        fixture
            .tags
            .resolve_tag("docker.io/library/local:base")
            .await
            .unwrap(),
        Some(root.clone())
    );
    assert!(fixture.run(&["system", "prune"]).status.success());
    assert_eq!(fixture.store.get_blob(&root).await.unwrap(), bytes);
    assert!(fixture.store.blob_exists(&manifest).await);
    let saved = fixture.directory.path().join("saved");
    assert!(
        fixture
            .run(&["save", "local:base", "-o", saved.to_str().unwrap()])
            .status
            .success()
    );
    assert!(
        fixture
            .run(&["load", saved.to_str().unwrap(), "--tag", "local:copy"])
            .status
            .success()
    );
    assert_eq!(
        fixture
            .tags
            .resolve_tag("docker.io/library/local:copy")
            .await
            .unwrap(),
        Some(root)
    );
}

#[tokio::test]
async fn publishers_wait_for_collection_before_writing_blobs() {
    use std::{
        process::Stdio,
        time::{Duration, Instant},
    };
    for kind in ["raw", "pmi", "load", "pull"] {
        let registry = std::env::var("PICHI_TEST_REGISTRY").ok();
        if kind == "pull" && registry.is_none() {
            eprintln!("PICHI_TEST_REGISTRY unset; skipping publication lock pull case");
            continue;
        }
        let fixture = Fixture::new().await;
        let raw = fixture.directory.path().join("input.raw");
        let dtb = fixture.directory.path().join("input.dtb");
        std::fs::write(&raw, [71; 8192]).unwrap();
        std::fs::write(&dtb, [72; 4096]).unwrap();
        let saved = fixture.directory.path().join("saved");
        let tag = registry
            .filter(|_| kind == "pull")
            .map(|registry| format!("{registry}/publication-lock:image"))
            .unwrap_or_else(|| "local:locked".into());
        if matches!(kind, "load" | "pull") {
            for args in [
                vec!["import", "raw", raw.to_str().unwrap(), "-t", &tag],
                if kind == "load" {
                    vec!["save", &tag, "-o", saved.to_str().unwrap()]
                } else {
                    vec!["push", &tag]
                },
                vec!["rmi", &tag],
            ] {
                let output = fixture.run(&args);
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
        let root = fixture.directory.path().join("pichi/storage");
        let before = std::fs::read_dir(root.join("blobs/sha256"))
            .unwrap()
            .count();
        let guard = pichi_storage::lock_exclusive(&root.join("index.json.lock")).unwrap();
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("pichi"));
        command
            .env("XDG_DATA_HOME", fixture.directory.path())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        match kind {
            "load" => {
                command.args(["load", saved.to_str().unwrap(), "--tag", &tag]);
            }
            "pull" => {
                command.args(["pull", &tag]);
            }
            _ => {
                command.args(["import", kind, raw.to_str().unwrap(), "-t", &tag]);
                if kind == "pmi" {
                    command.args(["--dtb", dtb.to_str().unwrap()]);
                }
            }
        }
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut published_while_collecting = false;
        while Instant::now() < deadline {
            if std::fs::read_dir(root.join("blobs/sha256"))
                .unwrap()
                .count()
                != before
            {
                published_while_collecting = true;
                break;
            }
            assert!(
                child.try_wait().unwrap().is_none(),
                "publisher must wait for the collector"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        drop(guard);
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !published_while_collecting,
            "{kind} exposed unreferenced content while collection held the lock"
        );
        assert!(fixture.run(&["system", "prune"]).status.success());
        assert!(fixture.run(&["inspect", &tag]).status.success());
    }
}
