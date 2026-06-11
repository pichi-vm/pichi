// SPDX-License-Identifier: Apache-2.0

//! Integration tests for `pichi rmi` (LOCAL-03 / SC3 / T-42-02).

use std::collections::BTreeMap;

use assert_cmd::Command;
use pichi_artifact::{
    EmptyConfigDescriptor, Layer, MEDIA_TYPE_PICHI_ARTIFACT_V1, Manifest, ScuteAnnotations,
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

/// Insert a manifest with a unique scute digest based on `salt_seed`.
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
        config: EmptyConfigDescriptor::canonical(),
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
    bs.put_blob(&mdigest, &mbytes).unwrap();
    // Insert a fake scute blob too so `delete_blob` actually finds something.
    let parsed_scute: pichi_artifact::Digest = scute_digest.parse().unwrap();
    bs.put_blob(&parsed_scute, b"fake-scute-bytes").unwrap();
    let db = FilesystemTagDb::open(graphroot).unwrap();
    db.set_tag(tag, &mdigest).unwrap();
    (mdigest, parsed_scute)
}

#[test]
fn rmi_unique_manifest_unlinks_blobs() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let (mdigest, sdigest) = insert_manifest(&g, "docker.io/library/alpine:3", "aaaa");

    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["rmi", "alpine:3"])
        .assert()
        .success();

    let bs = FilesystemBlobStore::new(&g);
    assert!(!bs.blob_exists(&mdigest), "manifest blob must be unlinked");
    assert!(!bs.blob_exists(&sdigest), "scute blob must be unlinked");

    let db = FilesystemTagDb::open(&g).unwrap();
    assert_eq!(db.resolve_tag("docker.io/library/alpine:3").unwrap(), None);
}

#[test]
fn rmi_shared_manifest_blocked_without_force() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let (mdigest, _) = insert_manifest(&g, "docker.io/library/alpine:3", "bbbb");
    // Second tag pointing at the SAME manifest.
    let db = FilesystemTagDb::open(&g).unwrap();
    db.set_tag("docker.io/library/alpine:latest", &mdigest)
        .unwrap();

    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["rmi", "alpine:3"])
        .assert()
        .failure();

    // Both tags still resolve.
    let db = FilesystemTagDb::open(&g).unwrap();
    assert!(
        db.resolve_tag("docker.io/library/alpine:3")
            .unwrap()
            .is_some()
    );
    assert!(
        db.resolve_tag("docker.io/library/alpine:latest")
            .unwrap()
            .is_some()
    );
}

// =============================================================================
// Phase 46 Plan 04 (CACHE-04 / D-08): sidecar-atomicity integration tests.
//
// These tests exercise the round-trip invariant that `pichi rmi` unlinks
// every orphaned source blob TOGETHER with its `.deflated` and `.verity`
// sidecars (Phase 46 D-01) inside one `with_index_lock` window. The
// load-bearing assertion across all four tests is:
//
//     find blobs/sha256/ -type f  →  ZERO files after rmi (orphan case)
//
// We exercise both the import-produced sidecar shape (`<cow>.verity` only;
// no `.deflated` because import is identity-decompression per D-04) and
// the multi-tag refcount preservation case.
//
// Note: pull→rmi round-trip tests are deferred to the in-bin test module
// (`src/cmd/rmi.rs::tests`) because `pull_inner_with_registry` is `pub(crate)`
// and not reachable from `tests/cmd_rmi.rs`. The import-side tests here
// fully exercise `unlink_blob_with_sidecars` because the helper is source-
// agnostic — it acts on the on-disk sidecar shape, regardless of whether
// pull or import produced it.
// =============================================================================

/// Helper: fresh raw fixture (1 MiB, one non-zero chunk).
fn write_raw_fixture(path: &std::path::Path) {
    const CHUNK_BYTES: usize = 16 * 1024;
    const NUM_CHUNKS: usize = 64;
    let mut buf = vec![0u8; CHUNK_BYTES * NUM_CHUNKS];
    buf[5 * CHUNK_BYTES..6 * CHUNK_BYTES].fill(0xA1);
    std::fs::write(path, &buf).unwrap();
}

/// Helper: count files in `<graphroot>/blobs/sha256/`.
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

/// CACHE-04 / D-08: after rmi, blobs/sha256/ contains ZERO files for an
/// artifact produced by `pichi import`. Pre-rmi shape: `<cow>` + `<cow>.verity`
/// + `<manifest>` (3 entries; import is identity-decompression so no `.deflated`
/// per D-04). The `.verity` sidecar MUST be unlinked together with the cow
/// inside the same `with_index_lock` window.
#[test]
fn rmi_after_import_leaves_no_orphans() {
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

    // Pre-rmi: 3 entries (cow + .verity + manifest); explicitly assert NO
    // .deflated exists (D-04: identity-decompression import skips .deflated).
    let pre_count = count_blob_files(&g);
    assert_eq!(
        pre_count, 3,
        "pre-rmi: expected 3 entries (cow + manifest + .verity sidecar); got {pre_count}"
    );
    let blobs = g.join("blobs").join("sha256");
    let deflated_present = std::fs::read_dir(&blobs)
        .unwrap()
        .filter_map(Result::ok)
        .any(|e| e.file_name().to_string_lossy().ends_with(".deflated"));
    assert!(
        !deflated_present,
        "import is identity-decompression (D-04) — .deflated must NOT be written"
    );

    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["rmi", "myapp:base"])
        .assert()
        .success();

    let post_count = count_blob_files(&g);
    assert_eq!(
        post_count, 0,
        "post-rmi: blobs/sha256/ MUST be empty (load-bearing D-08 invariant); got {post_count} files",
    );
}

/// CACHE-04: --pmi artifact rmi (Plan 03 + PMI layer). Pre-rmi: 4 entries
/// (cow + .verity + pmi + manifest). The PMI layer has no sidecars
/// (Pitfall 6 — PMI has no verity tree). Post-rmi: ZERO files.
#[test]
fn rmi_after_import_with_pmi_leaves_no_orphans() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let raw = tmp.path().join("input.raw");
    write_raw_fixture(&raw);

    // Synthetic PMI bytes — opaque per Phase 43 D-06; pichi never parses
    // the PMI format. A 4 KiB zero-padded buffer is sufficient.
    let pmi = tmp.path().join("input.pmi");
    std::fs::write(&pmi, vec![0u8; 4096]).unwrap();

    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args([
            "import",
            "--pmi",
            pmi.to_str().unwrap(),
            raw.to_str().unwrap(),
            "myapp:pmi",
        ])
        .assert()
        .success();

    let pre_count = count_blob_files(&g);
    assert_eq!(
        pre_count, 4,
        "pre-rmi --pmi: expected 4 (cow + manifest + pmi + .verity); got {pre_count}"
    );

    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["rmi", "myapp:pmi"])
        .assert()
        .success();

    let post_count = count_blob_files(&g);
    assert_eq!(
        post_count, 0,
        "post-rmi --pmi: blobs/sha256/ MUST be empty; got {post_count}"
    );
}

/// CACHE-04: refcount preservation under sidecar atomicity. Two tags pointing
/// at the same manifest; rmi one (with --force). The cow + .verity sidecar
/// + manifest MUST be PRESERVED because tag2 still references them. This
/// guards against the bug where unlink_blob_with_sidecars is called on a
/// still-referenced blob (which would silently remove sidecars even though
/// the source is preserved by refcount logic).
#[test]
fn rmi_with_other_tag_preserves_source_and_sidecars() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let raw = tmp.path().join("input.raw");
    write_raw_fixture(&raw);

    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["import", raw.to_str().unwrap(), "tag1:v"])
        .assert()
        .success();

    // Add a second tag pointing at the same manifest.
    let db = FilesystemTagDb::open(&g).unwrap();
    let manifest_digest = db
        .resolve_tag("docker.io/library/tag1:v")
        .unwrap()
        .expect("tag1 should resolve");
    db.set_tag("docker.io/library/tag2:v", &manifest_digest)
        .unwrap();

    let pre_count = count_blob_files(&g);
    assert_eq!(pre_count, 3, "pre-rmi: expected 3 entries");

    // rmi tag1 with --force (required because another tag references the
    // same manifest).
    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["rmi", "--force", "tag1:v"])
        .assert()
        .success();

    // tag1 gone, tag2 still resolves, all 3 files (cow + .verity + manifest)
    // STILL present.
    let db = FilesystemTagDb::open(&g).unwrap();
    assert!(
        db.resolve_tag("docker.io/library/tag1:v")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        db.resolve_tag("docker.io/library/tag2:v").unwrap(),
        Some(manifest_digest)
    );
    let post_count = count_blob_files(&g);
    assert_eq!(
        post_count, 3,
        "post-rmi --force with shared tag: blobs+sidecars MUST be preserved (refcount integrity); got {post_count}"
    );
}

/// CACHE-04 / Pitfall 3: rmi tolerates an artifact whose source blob has
/// NO sidecars at all (the legacy pre-Phase-46 case, or the
/// fixture-inserted-fake-blob case from `rmi_unique_manifest_unlinks_blobs`).
/// The helper's ENOENT tolerance must NOT cause rmi to fail when sidecars
/// are absent.
#[test]
fn rmi_tolerates_source_blob_without_sidecars() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let (mdigest, sdigest) = insert_manifest(&g, "docker.io/library/legacy:v", "dddd");

    // Sanity check: NO sidecars exist for the fixture-inserted blobs.
    let bs = FilesystemBlobStore::new(&g);
    let scute_path = bs.blob_path(&sdigest);
    assert!(
        !scute_path.with_extension("verity").exists(),
        "fixture must NOT have a .verity sidecar (legacy shape)"
    );
    assert!(
        !scute_path.with_extension("deflated").exists(),
        "fixture must NOT have a .deflated sidecar (legacy shape)"
    );

    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["rmi", "legacy:v"])
        .assert()
        .success();

    // Both source blobs unlinked; ENOENT on missing sidecars was tolerated.
    assert!(!bs.blob_exists(&mdigest), "manifest must be unlinked");
    assert!(!bs.blob_exists(&sdigest), "scute must be unlinked");
}

#[test]
fn rmi_force_preserves_shared_blobs() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let (mdigest, sdigest) = insert_manifest(&g, "docker.io/library/alpine:3", "cccc");
    let db = FilesystemTagDb::open(&g).unwrap();
    db.set_tag("docker.io/library/alpine:latest", &mdigest)
        .unwrap();

    Command::cargo_bin("pichi")
        .unwrap()
        .env("XDG_DATA_HOME", tmp.path())
        .args(["rmi", "--force", "alpine:3"])
        .assert()
        .success();

    let bs = FilesystemBlobStore::new(&g);
    assert!(
        bs.blob_exists(&mdigest),
        "shared manifest blob must be preserved"
    );
    assert!(
        bs.blob_exists(&sdigest),
        "shared scute blob must be preserved"
    );

    let db = FilesystemTagDb::open(&g).unwrap();
    assert_eq!(db.resolve_tag("docker.io/library/alpine:3").unwrap(), None);
    assert!(
        db.resolve_tag("docker.io/library/alpine:latest")
            .unwrap()
            .is_some()
    );
}
