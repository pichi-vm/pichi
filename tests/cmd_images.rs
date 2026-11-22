// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for `pichi images` (LOCAL-01 / SC1 + D-12..D-19).

#[path = "support/storage.rs"]
mod storage;
use storage::Storage as _;

use assert_cmd::Command;
use conglobate::artifact::{
    ConfigDescriptor, Layer, Manifest, PmiDescriptor, ScuteAnnotations, ScuteDescriptor,
};
use pichi_storage::{BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};
use tempfile::TempDir;

fn chain_annotations() -> conglobate::artifact::ManifestAnnotations {
    conglobate::artifact::ManifestAnnotations {
        verity: conglobate::artifact::CarapaceVerity {
            root: Some(conglobate::artifact::Digest::Sha256([0x11; 32])),
            ..Default::default()
        },
        oci: conglobate::artifact::OciAnnotations {
            created: Some("2026-05-01T12:00:00Z".into()),
            ..Default::default()
        },
    }
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
) -> conglobate::artifact::Digest {
    let mut layers = vec![Layer::Scute(ScuteDescriptor {
        digest: conglobate::artifact::Digest::Sha256([0x11; 32]),
        size: 4096,
        annotations: ScuteAnnotations {
            digest: conglobate::artifact::Digest::Sha256([0x11; 32]),
            size: std::num::NonZeroU64::new(4096).unwrap().into(),
            verity_digest: conglobate::artifact::Digest::Sha256([0x00; 32]),
            data_blocks: std::num::NonZeroU64::MIN.into(),
            salt: devmap_verity::Salt::new(&[0; 32]).unwrap().into(),
        },
    })];
    if with_pmi {
        layers.push(Layer::Pmi(PmiDescriptor {
            digest: conglobate::artifact::Digest::Sha256([0x22; 32]),
            size: 8192,
        }));
    }
    let m = Manifest {
        schema_version: Default::default(),
        media_type: "application/vnd.oci.image.manifest.v1+json".into(),
        artifact_type: Default::default(),
        config: ConfigDescriptor::canonical(),
        layers,
        annotations: chain_annotations(),
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

#[tokio::test]
async fn images_lists_default_columns() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    populate(&g, "docker.io/library/alpine:3", false).await;
    populate(&g, "docker.io/library/myapp:base", true).await;

    let out = Command::cargo_bin("pichi")
        .unwrap()
        .storage(tmp.path())
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
        .storage(tmp.path())
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
        .storage(tmp.path())
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
        .storage(tmp.path())
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
