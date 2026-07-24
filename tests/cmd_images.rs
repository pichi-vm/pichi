// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for `pichi images` (LOCAL-01 / SC1 + D-12..D-19).

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
        ("org.opencontainers.image.created", "2026-05-01T12:00:00Z"),
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

async fn populate(
    graphroot: &std::path::Path,
    tag: &str,
    with_pmi: bool,
) -> pichi_artifact::Digest {
    let mut layers = vec![Layer::Scute(ScuteDescriptor {
        digest: "sha256:1111111111111111111111111111111111111111111111111111111111111111".into(),
        size: 4096,
        annotations: ScuteAnnotations {
            salt: "dead".into(),
        },
    })];
    if with_pmi {
        layers.push(Layer::Pmi(PmiDescriptor {
            digest: "sha256:2222222222222222222222222222222222222222222222222222222222222222"
                .into(),
            size: 8192,
        }));
    }
    let m = Manifest {
        schema_version: 2,
        media_type: "application/vnd.oci.image.manifest.v1+json".into(),
        artifact_type: MEDIA_TYPE_PICHI_ARTIFACT_V1.into(),
        config: ConfigDescriptor::canonical(),
        layers,
        annotations: chain_annotations(),
    };
    let bytes = m.to_bytes().unwrap();
    let digest = m.digest().unwrap();
    let blob_store = FilesystemBlobStore::new(graphroot);
    blob_store.put_blob(&digest, &bytes).await.unwrap();
    let db = FilesystemTagDb::open(graphroot).unwrap();
    db.set_tag(tag, &digest).await.unwrap();
    digest
}

#[tokio::test]
async fn images_lists_default_columns() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    populate(&g, "docker.io/library/alpine:3", false).await;
    populate(&g, "docker.io/library/myapp:base", true).await;

    let out = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .arg("images")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    for col in ["REPOSITORY", "TAG", "BOOTABLE", "DIGEST", "CREATED", "SIZE"] {
        assert!(s.contains(col), "expected {col} in output, got:\n{s}");
    }
    assert!(s.contains("docker.io/library/alpine"));
    assert!(s.contains("docker.io/library/myapp"));
    // BOOTABLE glyphs (D-13)
    assert!(s.contains("✓"), "expected ✓ glyph for pmi-bearing manifest");
    assert!(
        s.contains("—"),
        "expected — em-dash for non-bootable manifest"
    );
}

#[tokio::test]
async fn images_quiet_prints_full_sha256_digests() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let d = populate(&g, "docker.io/library/alpine:3", false).await;
    let out = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["images", "--quiet"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    // D-18: full sha256:... NOT 12-char prefix.
    assert!(
        s.contains(&d.to_string()),
        "expected full digest {d}, got:\n{s}"
    );
    // 12-char prefix shouldn't be the only thing on the line.
    assert!(!s.lines().any(|l| l.len() < 19 && l.starts_with("sha256:")));
}

#[tokio::test]
async fn images_format_does_not_html_escape() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    populate(&g, "docker.io/library/alpine:3", false).await;
    let out = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args([
            "images",
            "--format",
            "{{.Repository}}:{{.Tag}}|{{.Bootable}}",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    // Pitfall 1: `/` and `:` MUST survive unescaped.
    assert!(s.contains("docker.io/library/alpine:3"), "got:\n{s}");
    // bool renders as `true` / `false`.
    assert!(s.contains("|false"), "got:\n{s}");
}

#[tokio::test]
async fn images_digests_flag_widens_column_to_full_digest() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let d = populate(&g, "docker.io/library/alpine:3", false).await;
    let out = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["images", "--digests"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert!(
        s.contains(&d.to_string()),
        "expected full digest with --digests, got:\n{s}"
    );
}
