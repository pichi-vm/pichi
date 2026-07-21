// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for `pichi system prune` (PRUNE-01..04 functional surface).
//!
//! No concurrency / barrier / mock-registry tests live here — flock
//! binding is verified statically by the grep gate in `src/cmd/prune.rs`'s
//! plan task per D-prune-T-FLOCK; flock semantics themselves are a
//! kernel primitive proven by Phase 42 T-42-02.

use std::collections::BTreeMap;

use assert_cmd::Command;
use pichi_artifact::{
    ConfigDescriptor, Layer, MEDIA_TYPE_PICHI_ARTIFACT_V1, Manifest, ScuteAnnotations,
    ScuteDescriptor,
};
use pichi_storage::{FilesystemBlobStore, FilesystemTagDb, TagDb};
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

/// Insert a manifest with a unique scute digest based on `salt_seed`.
/// (Mirrors `tests/cmd_rmi.rs::insert_manifest`.)
fn insert_manifest(
    graphroot: &std::path::Path,
    tag: &str,
    salt_seed: &str,
) -> (pichi_artifact::Digest, pichi_artifact::Digest) {
    let scute_digest = format!("sha256:{:0<64}", salt_seed);
    let m = Manifest {
        schema_version: 2,
        media_type: "application/vnd.oci.image.manifest.v1+json".into(),
        artifact_type: MEDIA_TYPE_PICHI_ARTIFACT_V1.into(),
        config: ConfigDescriptor::canonical(),
        layers: vec![Layer::Scute(ScuteDescriptor {
            digest: scute_digest.clone(),
            size: 1024,
            annotations: ScuteAnnotations {
                salt: format!("{:0<8}", salt_seed),
            },
        })],
        annotations: chain_annotations(),
    };
    let mbytes = m.to_bytes().unwrap();
    let mdigest = m.digest().unwrap();
    let bs = FilesystemBlobStore::new(graphroot);
    use pichi_storage::BlobStore;
    bs.put_blob(&mdigest, &mbytes).unwrap();
    let parsed_scute: pichi_artifact::Digest = scute_digest.parse().unwrap();
    bs.put_blob(&parsed_scute, b"fake-scute-bytes").unwrap();
    let db = FilesystemTagDb::open(graphroot).unwrap();
    db.set_tag(tag, &mdigest).unwrap();
    (mdigest, parsed_scute)
}

/// Helper: fresh raw fixture (1 MiB, one non-zero chunk).
/// (Verbatim from `tests/cmd_rmi.rs:140-146`.)
fn write_raw_fixture(path: &std::path::Path) {
    const CHUNK_BYTES: usize = 16 * 1024;
    const NUM_CHUNKS: usize = 64;
    let mut buf = vec![0u8; CHUNK_BYTES * NUM_CHUNKS];
    buf[5 * CHUNK_BYTES..6 * CHUNK_BYTES].fill(0xA1);
    std::fs::write(path, &buf).unwrap();
}

/// Helper: count files in `<graphroot>/blobs/sha256/`.
/// (Verbatim from `tests/cmd_rmi.rs:149-158`.)
fn count_blob_files(graphroot: &std::path::Path) -> usize {
    let blobs = graphroot.join("blobs").join("sha256");
    std::fs::read_dir(&blobs)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
                .count()
        })
        .unwrap_or(0)
}

/// (1) Empty graphroot: `Nothing to prune.\n`, exit 0.
#[test]
fn prune_empty_cache_prints_nothing_to_prune() {
    let tmp = TempDir::new().unwrap();
    let _ = graphroot(&tmp);

    let out = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["system", "prune"])
        .output()
        .unwrap();

    assert!(out.status.success(), "exit must be 0");
    assert_eq!(out.stdout, b"Nothing to prune.\n");
}

/// (2) All blobs live (referenced by a tag): `Nothing to prune.\n`,
/// pre/post `count_blob_files` identical.
#[test]
fn prune_all_blobs_live_prints_nothing_to_prune() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let raw = tmp.path().join("input.raw");
    write_raw_fixture(&raw);

    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["import", raw.to_str().unwrap(), "myapp:base"])
        .assert()
        .success();

    let pre = count_blob_files(&g);

    let out = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["system", "prune"])
        .output()
        .unwrap();

    assert!(out.status.success(), "exit must be 0");
    assert_eq!(out.stdout, b"Nothing to prune.\n");
    assert_eq!(count_blob_files(&g), pre, "count must be unchanged");
}

/// (3) Import → rmi → prune: the orphans are already cleaned by rmi
/// (Phase 46 Plan 04 atomicity), so prune reports `Nothing to prune.\n`.
/// Regression guard against rmi/prune accounting drift.
#[test]
fn prune_after_import_then_rmi_reports_zero_orphans() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let raw = tmp.path().join("input.raw");
    write_raw_fixture(&raw);

    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["import", raw.to_str().unwrap(), "myapp:base"])
        .assert()
        .success();
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["rmi", "myapp:base"])
        .assert()
        .success();
    assert_eq!(
        count_blob_files(&g),
        0,
        "rmi must have already cleaned all blobs"
    );

    let out = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["system", "prune"])
        .output()
        .unwrap();

    assert!(out.status.success(), "exit must be 0");
    assert_eq!(out.stdout, b"Nothing to prune.\n");
}

/// (4) Shared-tag preservation: `tag a:1 b:1`, `rmi a:1`, `prune` —
/// blobs preserved because `b:1` still references them.
#[test]
fn prune_shared_tag_preservation() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let (mdigest, sdigest) = insert_manifest(&g, "docker.io/library/a:1", "aabb");

    // Add a second tag pointing at the same manifest.
    let db = FilesystemTagDb::open(&g).unwrap();
    db.set_tag("docker.io/library/b:1", &mdigest).unwrap();
    drop(db);

    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["rmi", "--force", "a:1"])
        .assert()
        .success();

    // Pre-prune sanity: the manifest + scute blobs survive (b:1 still
    // points at them).
    let bs = FilesystemBlobStore::new(&g);
    use pichi_storage::BlobStore;
    assert!(bs.blob_exists(&mdigest), "manifest must survive shared rmi");
    assert!(bs.blob_exists(&sdigest), "scute must survive shared rmi");

    let out = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["system", "prune"])
        .output()
        .unwrap();

    assert!(out.status.success(), "exit must be 0");
    assert_eq!(out.stdout, b"Nothing to prune.\n");
    assert!(bs.blob_exists(&mdigest), "manifest must survive prune");
    assert!(bs.blob_exists(&sdigest), "scute must survive prune");
}

/// (5) Headless-sidecar silent sweep: a single `<64hex>.verity` with no
/// source is reclaimed, but stdout is `Nothing to prune.\n` (D-prune-B3:
/// per-line records and `(N blobs)` count surface source-orphan blobs
/// only — headless sidecars are silent disk reclamation).
#[test]
fn prune_headless_sidecar_swept_silently() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let blobs = g.join("blobs").join("sha256");
    std::fs::create_dir_all(&blobs).unwrap();

    let stem = blobs.join("a".repeat(64));
    let headless = stem.with_extension("verity");
    std::fs::write(&headless, b"orphan-verity-bytes").unwrap();

    let out = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["system", "prune"])
        .output()
        .unwrap();

    assert!(out.status.success(), "exit must be 0");
    assert_eq!(out.stdout, b"Nothing to prune.\n");
    assert!(
        !headless.exists(),
        "headless .verity must be reclaimed silently"
    );
}

/// (6) --dry-run preserves files: manually seed a 64-hex orphan source
/// + `.verity`, run with `--dry-run`, assert files preserved + stdout
/// contains `sha256:` per-line and `Total reclaimed:` summary.
#[test]
fn prune_dry_run_preserves_files() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let blobs = g.join("blobs").join("sha256");
    std::fs::create_dir_all(&blobs).unwrap();

    let stem = blobs.join("c".repeat(64));
    let verity = stem.with_extension("verity");
    std::fs::write(&stem, b"orphan-source-bytes").unwrap();
    std::fs::write(&verity, b"orphan-verity-bytes").unwrap();
    let pre = count_blob_files(&g);

    let out = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["system", "prune", "--dry-run"])
        .output()
        .unwrap();

    assert!(out.status.success(), "exit must be 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("sha256:"),
        "dry-run stdout must contain a per-orphan sha256: line; got {stdout:?}"
    );
    assert!(
        stdout.contains("Total reclaimed:"),
        "dry-run stdout must contain a Total reclaimed: summary; got {stdout:?}"
    );
    assert_eq!(
        count_blob_files(&g),
        pre,
        "dry-run: count must be unchanged"
    );
    assert!(stem.exists(), "dry-run: source must be preserved");
    assert!(verity.exists(), "dry-run: .verity must be preserved");
}

/// (7) D-prune-DR1: --dry-run output is byte-identical to the real run.
/// Seed two equivalent graphroots (Tmp1 and Tmp2 with identical orphan
/// files + names), run --dry-run against Tmp1 and a real run against
/// Tmp2, assert `output1.stdout == output2.stdout` byte-equal. Tmp1's
/// blobs MUST still be present; Tmp2's MUST be gone.
#[test]
fn prune_dry_run_then_real_run_byte_identical_stdout() {
    let tmp1 = TempDir::new().unwrap();
    let tmp2 = TempDir::new().unwrap();
    let g1 = graphroot(&tmp1);
    let g2 = graphroot(&tmp2);
    for g in [&g1, &g2] {
        let blobs = g.join("blobs").join("sha256");
        std::fs::create_dir_all(&blobs).unwrap();
        let stem = blobs.join("d".repeat(64));
        std::fs::write(&stem, b"orphan-source-bytes").unwrap();
        std::fs::write(stem.with_extension("verity"), b"orphan-verity-bytes").unwrap();
    }

    let out1 = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp1.path())
        .args(["system", "prune", "--dry-run"])
        .output()
        .unwrap();
    let out2 = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp2.path())
        .args(["system", "prune"])
        .output()
        .unwrap();

    assert!(
        out1.status.success() && out2.status.success(),
        "both exit 0"
    );
    assert_eq!(
        out1.stdout, out2.stdout,
        "dry-run and real-run stdout MUST be byte-identical (D-prune-DR1)"
    );
    // Tmp1: dry-run must preserve files.
    assert!(g1.join("blobs/sha256").join("d".repeat(64)).exists());
    assert!(
        g1.join("blobs/sha256")
            .join(format!("{}.verity", "d".repeat(64)))
            .exists()
    );
    // Tmp2: real run must have removed them.
    assert!(!g2.join("blobs/sha256").join("d".repeat(64)).exists());
    assert!(
        !g2.join("blobs/sha256")
            .join(format!("{}.verity", "d".repeat(64)))
            .exists()
    );
}

/// (8) D-prune-B4: orphans are sorted by digest ASCII order. Two orphan
/// source blobs whose hex names start with `aaaa...` and `bbbb...`; the
/// `aaaa...` line MUST precede the `bbbb...` line in stdout.
#[test]
fn prune_sorts_orphans_by_digest_ascii() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let blobs = g.join("blobs").join("sha256");
    std::fs::create_dir_all(&blobs).unwrap();

    let aaaa = "a".repeat(64);
    let bbbb = "b".repeat(64);
    // Write bbbb FIRST so insertion-order ≠ ASCII order — proves the
    // sort actually runs (not just a happy coincidence of readdir order).
    std::fs::write(blobs.join(&bbbb), b"bytes-b").unwrap();
    std::fs::write(blobs.join(&aaaa), b"bytes-a").unwrap();

    let out = Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["system", "prune"])
        .output()
        .unwrap();

    assert!(out.status.success(), "exit must be 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let pos_a = stdout
        .find(&format!("sha256:{aaaa}"))
        .expect("aaaa line missing from stdout");
    let pos_b = stdout
        .find(&format!("sha256:{bbbb}"))
        .expect("bbbb line missing from stdout");
    assert!(
        pos_a < pos_b,
        "ASCII sort: aaaa line must precede bbbb line; got stdout={stdout:?}"
    );
}
