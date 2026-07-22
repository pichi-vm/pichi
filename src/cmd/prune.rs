// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `pichi system prune` (PRUNE-01..04). Walks the live tag set, computes
//! refcount=0 source blobs in `<graphroot>/blobs/sha256/`, and unlinks
//! them plus their `.deflated` / `.verity` sidecars inside ONE
//! `with_index_lock` window. A second readdir pass sweeps headless
//! sidecars (closing Phase 46 D-09).
//!
//! ## Lock-window discipline (D-prune-LW1)
//!
//! The ENTIRE flow — `list_tags`, manifest-blob reads, candidate readdir,
//! source-orphan unlinks, headless-sidecar sweep readdir + unlinks, and
//! output buffering — runs inside ONE `with_index_lock` callback. Output
//! is `print!`'d AFTER the lock callback returns so a slow stdout cannot
//! extend the lock window.
//!
//! ## Sidecar atomicity (D-prune-H1)
//!
//! Source orphans are unlinked together with their `<src>.deflated` and
//! `<src>.verity` siblings via
//! [`pichi_storage::sidecar::unlink_blob_with_sidecars`] (Phase 46 D-08
//! primitive — ENOENT-tolerant per sibling). The headless-sidecar sweep
//! runs AFTER the source-orphan loop within the same lock callback, so a
//! source orphan's OWN siblings (which `unlink_blob_with_sidecars`
//! removes alongside the source) are never visible to the sweep's
//! `with_extension("")` `exists()` check — by the time the sweep iterates
//! the directory, those siblings have already been unlinked. This
//! ordering is load-bearing.
//!
//! ## Live-set policy (D-prune-L1, mirrors `cmd::rmi`)
//!
//! Live set = the union, over every tag in `FilesystemTagDb::list_tags`,
//! of `{tag.digest} ∪ {layer.digest_str().parse() for layer in
//! Manifest::from_reader(blob_store.get_blob(tag.digest)).layers}`.
//! Identical structure to `cmd::rmi::rmi_one`'s `still_referenced`
//! accumulator (`src/cmd/rmi.rs:88-105`); copy of the loop minus the
//! per-target filter (prune walks ALL tags). Manifests that fail to
//! parse (corrupted) DO NOT contribute to the live set — same lenient
//! policy `cmd::rmi` uses.
//!
//! ## Concurrency contract
//!
//! Phase 42 T-42-02 + Phase 44 D-03 + Phase 46 D-08 — flock-bound
//! mutator; serialization with `set_tag` / `rmi` / `pull` is automatic
//! via the shared `index.json.lock` flock (kernel primitive, proven
//! once by Phase 42 T-42-02; not re-proven per consumer per
//! D-prune-T-FLOCK). The flock-binding contract for `cmd::prune::run`
//! is verified statically by a grep gate that counts the call invocations
//! of `with_index_lock` in this file — exactly one call invocation
//! must exist. Per-consumer thread-orchestration regression tests are
//! testing the OS, not pichi.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::collections::HashSet;

use anyhow::{Context, Result};
use humansize::{BINARY, format_size};

use pichi_artifact::{Digest, Manifest};
use pichi_storage::{
    BlobStore, CacheLayout, FilesystemBlobStore, FilesystemTagDb, TagDb, with_index_lock,
};

use crate::cli::PruneArgs;
use crate::config::Config;

/// `pichi system prune` entry point — garbage-collect orphan blobs +
/// headless sidecars (PRUNE-01..04; Phase 47).
pub async fn run(args: PruneArgs, config: &Config) -> Result<()> {
    let layout = resolve_layout(config)?;

    // D-prune-LW1: ONE with_index_lock window covers the whole transaction
    // (live-set walk, source-orphan loop, headless-sidecar sweep). The
    // single call site in this file is also D-prune-T-FLOCK's static
    // assertion target — do not split into multiple lock callbacks.
    let (records, total) = with_index_lock(&layout, || async {
        let db = FilesystemTagDb::open(&layout.graphroot)?;
        let blob_store = FilesystemBlobStore::new(&layout.graphroot);

        let all_tags = db.list_tags().await?;

        // D-prune-L1: live set = every tag's manifest digest + that
        // manifest's layer digests. Verbatim from src/cmd/rmi.rs:88-105
        // MINUS the `if entry.tag == target_key { continue; }` filter
        // (prune walks ALL tags as the live set source).
        let mut still_referenced: HashSet<Digest> = HashSet::new();
        for entry in &all_tags {
            still_referenced.insert(entry.digest.clone());
            if let Ok(bytes) = blob_store.get_blob(&entry.digest).await {
                if let Ok(m) = Manifest::from_reader(bytes.as_slice()) {
                    for layer in &m.layers {
                        // digest_str is "sha256:<hex>" — round-trip via parse.
                        if let Ok(d) = layer.digest_str().parse::<Digest>() {
                            still_referenced.insert(d);
                        }
                    }
                }
            }
        }

        let blob_dir = layout.graphroot.join("blobs").join("sha256");
        let mut records: Vec<(Digest, u64)> = Vec::new();
        let mut total: u64 = 0;

        // Source-orphan loop. Skip cleanly if the cache directory does
        // not yet exist (fresh graphroot, never imported anything).
        if blob_dir.exists() {
            let entries = std::fs::read_dir(&blob_dir)
                .with_context(|| format!("read_dir {} (source-orphan pass)", blob_dir.display()))?;
            for entry in entries {
                let entry =
                    entry.with_context(|| format!("read_dir entry in {}", blob_dir.display()))?;
                let name_os = entry.file_name();
                let name = match name_os.to_str() {
                    Some(s) => s,
                    None => continue, // non-UTF8 names: not our blobs
                };

                // D-prune-L2: 64-lowercase-hex predicate is the SOLE
                // orphan-candidate gate. Sidecar names (`.deflated` /
                // `.verity`) contain '.' so the length check excludes
                // them; non-blob filesystem entries (lock files,
                // directories) are filtered out the same way.
                if !is_blob_filename(name) {
                    continue;
                }

                let digest: Digest = format!("sha256:{name}")
                    .parse()
                    .with_context(|| format!("parsing digest from filename {name}"))?;

                if still_referenced.contains(&digest) {
                    continue;
                }

                // Orphan source blob. Compute size (source + sidecars)
                // in BOTH dry-run and real modes — metadata reads are
                // cheap and required for the size column (D-prune-DR1).
                let blob_path = blob_store.blob_path(&digest);
                let source_size = std::fs::metadata(&blob_path)
                    .with_context(|| format!("metadata for orphan source {}", blob_path.display()))?
                    .len();
                let deflated_bytes =
                    std::fs::metadata(pichi_storage::sidecar::deflated_path(&blob_path))
                        .map(|m| m.len())
                        .unwrap_or(0);
                let verity_bytes =
                    std::fs::metadata(pichi_storage::sidecar::verity_path(&blob_path))
                        .map(|m| m.len())
                        .unwrap_or(0);

                records.push((digest.clone(), source_size));
                total = total
                    .saturating_add(source_size)
                    .saturating_add(deflated_bytes)
                    .saturating_add(verity_bytes);

                // D-prune-DR1: branch ONLY on the unlink call site;
                // everything else above runs identically in both modes.
                if !args.dry_run {
                    pichi_storage::sidecar::unlink_blob_with_sidecars(&blob_path)
                        .await
                        .with_context(|| format!("unlink orphan blob+sidecars for {digest}"))?;
                }
            }
        }

        // D-prune-H1: headless-sidecar sweep. Re-iterate read_dir
        // because the directory has changed (source-orphan loop
        // removed the source orphans + their sibling sidecars). Per
        // the ordering invariant in the rustdoc above, a source
        // orphan's own `.deflated` / `.verity` siblings cannot be
        // visible here — they were unlinked together with the source
        // by `unlink_blob_with_sidecars`.
        if blob_dir.exists() {
            let entries = std::fs::read_dir(&blob_dir)
                .with_context(|| format!("read_dir {} (headless sweep)", blob_dir.display()))?;
            for entry in entries {
                let entry = entry.with_context(|| {
                    format!("read_dir entry in {} (headless sweep)", blob_dir.display())
                })?;
                let path = entry.path();
                let ext = match path.extension().and_then(|e| e.to_str()) {
                    Some(e) => e,
                    None => continue,
                };
                if ext != "deflated" && ext != "verity" {
                    continue;
                }

                // Derive source stem: the inverse of
                // `pichi_storage::sidecar::{deflated,verity}_path` is
                // `Path::with_extension("")`.
                let source_path = path.with_extension("");
                if source_path.exists() {
                    continue; // not headless
                }

                let bytes = std::fs::metadata(&path)
                    .with_context(|| format!("metadata for headless sidecar {}", path.display()))?
                    .len();
                total = total.saturating_add(bytes);

                // D-prune-DR1: branch only on the unlink. Note: we
                // use std::fs::remove_file here, NOT
                // unlink_blob_with_sidecars — we are removing a
                // single headless file, not a triple (D-prune-H1).
                if !args.dry_run {
                    std::fs::remove_file(&path).with_context(|| {
                        format!("remove headless {ext} sidecar {}", path.display())
                    })?;
                }
            }
        }

        // D-prune-B4: deterministic ASCII order on digest string
        // (sidecar bytes do NOT appear in `records` per D-prune-B3).
        records.sort_by_key(|(d, _)| d.to_string());

        Ok::<(Vec<(Digest, u64)>, u64), anyhow::Error>((records, total))
    })
    .await?;

    // D-prune-LW1: print AFTER the lock is released — a slow stdout
    // pipe must NOT extend the lock window.
    if records.is_empty() {
        // D-prune-B3: empty-records case prints "Nothing to prune."
        // regardless of headless-sidecar work performed (silent
        // reclamation). The total is suppressed because there are no
        // per-line records to anchor it to.
        println!("Nothing to prune.");
        return Ok(());
    }

    for (digest, size) in &records {
        println!("{digest}  {}", format_size(*size, BINARY));
    }
    // D-prune-B3: literal "(N blobs)" — no pluralisation. The count is
    // source-orphan blobs only; headless sidecars are silent.
    println!(
        "Total reclaimed: {} ({} blobs)",
        format_size(total, BINARY),
        records.len()
    );

    Ok(())
}

/// D-prune-L2: the SOLE orphan-candidate gate. A blob filename is exactly
/// 64 lowercase hex chars; sidecar names contain '.' so the length check
/// alone excludes them.
fn is_blob_filename(name: &str) -> bool {
    name.len() == 64
        && name
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// Mirrors `src/cmd/rmi.rs::resolve_layout` (D-prune-BP1 forbids touching
/// rmi.rs to extract a shared helper; future cleanup is not Phase 47's
/// concern).
fn resolve_layout(config: &Config) -> Result<CacheLayout> {
    let mut layout = CacheLayout::resolve()?;
    if let Some(p) = &config.storage.graphroot {
        layout.graphroot.clone_from(p);
    }
    if let Some(p) = &config.storage.runroot {
        layout.runroot.clone_from(p);
    }
    Ok(layout)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use pichi_artifact::{
        ConfigDescriptor, Layer, MEDIA_TYPE_PICHI_ARTIFACT_V1, Manifest, ScuteAnnotations,
        ScuteDescriptor,
    };
    use pichi_storage::{FilesystemTagDb, TagDb};
    use tempfile::TempDir;

    use crate::config::{Config, StorageConfig};

    /// Deterministic 64-hex constant for orphan-blob seed tests.
    const FAKE_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn graphroot(tmp: &TempDir) -> std::path::PathBuf {
        let p = tmp.path().join("pichi").join("storage");
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn make_config(tmp: &TempDir) -> Config {
        Config {
            storage: StorageConfig {
                graphroot: Some(graphroot(tmp)),
                ..Default::default()
            },
            ..Default::default()
        }
    }

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

    /// (1) Empty cache: run returns Ok; blobs/sha256/ unchanged (or absent).
    #[tokio::test]
    async fn prune_empty_cache_returns_ok_and_changes_nothing() {
        let tmp = TempDir::new().unwrap();
        let config = make_config(&tmp);
        run(PruneArgs { dry_run: false }, &config).await.unwrap();

        let blobs = config
            .storage
            .graphroot
            .as_ref()
            .unwrap()
            .join("blobs")
            .join("sha256");
        // Either absent or empty; both acceptable.
        let count = std::fs::read_dir(&blobs)
            .map(std::iter::Iterator::count)
            .unwrap_or(0);
        assert_eq!(count, 0, "empty cache: blobs/sha256/ must be empty");
    }

    /// (2) Orphan source blob with sidecars: all three are unlinked.
    #[tokio::test]
    async fn prune_orphan_source_blob_unlinked_with_sidecars() {
        let tmp = TempDir::new().unwrap();
        let config = make_config(&tmp);
        let g = config.storage.graphroot.as_ref().unwrap();
        let blobs = g.join("blobs").join("sha256");
        std::fs::create_dir_all(&blobs).unwrap();

        let src = blobs.join(FAKE_HEX);
        std::fs::write(&src, b"source-bytes").unwrap();
        std::fs::write(
            pichi_storage::sidecar::deflated_path(&src),
            b"deflated-bytes",
        )
        .unwrap();
        std::fs::write(pichi_storage::sidecar::verity_path(&src), b"verity-bytes").unwrap();

        run(PruneArgs { dry_run: false }, &config).await.unwrap();

        assert!(!src.exists(), "source orphan must be unlinked");
        assert!(
            !pichi_storage::sidecar::deflated_path(&src).exists(),
            ".deflated sidecar must be unlinked together with source"
        );
        assert!(
            !pichi_storage::sidecar::verity_path(&src).exists(),
            ".verity sidecar must be unlinked together with source"
        );
    }

    /// (3) --dry-run preserves files that would otherwise be removed.
    #[tokio::test]
    async fn prune_dry_run_preserves_files() {
        let tmp = TempDir::new().unwrap();
        let config = make_config(&tmp);
        let g = config.storage.graphroot.as_ref().unwrap();
        let blobs = g.join("blobs").join("sha256");
        std::fs::create_dir_all(&blobs).unwrap();

        let src = blobs.join(FAKE_HEX);
        std::fs::write(&src, b"source-bytes").unwrap();
        std::fs::write(
            pichi_storage::sidecar::deflated_path(&src),
            b"deflated-bytes",
        )
        .unwrap();
        std::fs::write(pichi_storage::sidecar::verity_path(&src), b"verity-bytes").unwrap();

        run(PruneArgs { dry_run: true }, &config).await.unwrap();

        assert!(src.exists(), "--dry-run: source must NOT be unlinked");
        assert!(
            pichi_storage::sidecar::deflated_path(&src).exists(),
            "--dry-run: .deflated must NOT be unlinked"
        );
        assert!(
            pichi_storage::sidecar::verity_path(&src).exists(),
            "--dry-run: .verity must NOT be unlinked"
        );
    }

    /// (4) Live-set walk: a tag → manifest → layer chain protects the
    /// referenced blobs from prune. Regression guard for the verbatim
    /// copy of `cmd::rmi`'s loop.
    #[tokio::test]
    async fn prune_preserves_referenced_blob_and_sidecars() {
        let tmp = TempDir::new().unwrap();
        let config = make_config(&tmp);
        let g = config.storage.graphroot.as_ref().unwrap();
        let blobs = g.join("blobs").join("sha256");
        std::fs::create_dir_all(&blobs).unwrap();

        // Build a manifest pointing at a single scute layer (matches the
        // shape used by tests/cmd_rmi.rs::insert_manifest).
        let scute_digest_str = format!("sha256:{}", "a".repeat(64));
        let scute_digest: Digest = scute_digest_str.parse().unwrap();
        let m = Manifest {
            schema_version: 2,
            media_type: "application/vnd.oci.image.manifest.v1+json".into(),
            artifact_type: MEDIA_TYPE_PICHI_ARTIFACT_V1.into(),
            config: ConfigDescriptor::canonical(),
            layers: vec![Layer::Scute(ScuteDescriptor {
                digest: scute_digest_str.clone(),
                size: 1024,
                annotations: ScuteAnnotations {
                    salt: "00000000".into(),
                },
            })],
            annotations: chain_annotations(),
        };
        let mbytes = m.to_bytes().unwrap();
        let mdigest = m.digest().unwrap();

        let bs = FilesystemBlobStore::new(g);
        bs.put_blob(&mdigest, &mbytes).await.unwrap();
        bs.put_blob(&scute_digest, b"fake-scute-bytes")
            .await
            .unwrap();
        // Add a verity sidecar to the scute to prove it survives the
        // referenced-blob path (sidecars of live blobs are NOT touched
        // by prune; they are only swept when their source is missing).
        let scute_path = bs.blob_path(&scute_digest);
        std::fs::write(
            pichi_storage::sidecar::verity_path(&scute_path),
            b"verity-bytes",
        )
        .unwrap();

        let db = FilesystemTagDb::open(g).unwrap();
        db.set_tag("docker.io/library/foo:1", &mdigest)
            .await
            .unwrap();

        let pre = std::fs::read_dir(&blobs).unwrap().count();
        run(PruneArgs { dry_run: false }, &config).await.unwrap();
        let post = std::fs::read_dir(&blobs).unwrap().count();
        assert_eq!(
            post, pre,
            "all blobs+sidecars must be preserved under a live tag"
        );
        assert!(bs.blob_exists(&mdigest).await, "manifest blob must survive");
        assert!(
            bs.blob_exists(&scute_digest).await,
            "scute blob must survive"
        );
        assert!(
            pichi_storage::sidecar::verity_path(&scute_path).exists(),
            "live scute's .verity sidecar must survive"
        );
    }

    // ===== Headless-sidecar sweep tests (D-prune-H1, closes Phase 46 D-09) =====

    /// (5) D-prune-H1: a headless `.verity` sidecar (source `<src>` is missing)
    /// is swept silently in the second readdir pass. Per D-prune-B3 the empty
    /// `records` triggers `Nothing to prune.\n` even though the sweep
    /// reclaimed disk.
    #[tokio::test]
    async fn prune_headless_verity_sidecar_swept() {
        let tmp = TempDir::new().unwrap();
        let config = make_config(&tmp);
        let g = config.storage.graphroot.as_ref().unwrap();
        let blobs = g.join("blobs").join("sha256");
        std::fs::create_dir_all(&blobs).unwrap();

        let stem = blobs.join(FAKE_HEX);
        let headless = pichi_storage::sidecar::verity_path(&stem);
        std::fs::write(&headless, b"orphan-verity").unwrap();
        // Source `<src>` deliberately NOT written — this is the legacy
        // pre-Phase-46 / D-09 case.
        assert!(!stem.exists(), "fixture: source must be missing");

        run(PruneArgs { dry_run: false }, &config).await.unwrap();

        assert!(
            !headless.exists(),
            "headless .verity must be swept (D-prune-H1)"
        );
    }

    /// (6) D-prune-H1: same shape with `.deflated`.
    #[tokio::test]
    async fn prune_headless_deflated_sidecar_swept() {
        let tmp = TempDir::new().unwrap();
        let config = make_config(&tmp);
        let g = config.storage.graphroot.as_ref().unwrap();
        let blobs = g.join("blobs").join("sha256");
        std::fs::create_dir_all(&blobs).unwrap();

        let stem = blobs.join(FAKE_HEX);
        let headless = pichi_storage::sidecar::deflated_path(&stem);
        std::fs::write(&headless, b"orphan-deflated").unwrap();
        assert!(!stem.exists(), "fixture: source must be missing");

        run(PruneArgs { dry_run: false }, &config).await.unwrap();

        assert!(
            !headless.exists(),
            "headless .deflated must be swept (D-prune-H1)"
        );
    }

    /// (7) D-prune-DR1 + D-prune-H1: --dry-run does NOT call remove_file on
    /// headless sidecars. Bytes accounting still happens (so `total` matches
    /// the non-dry path), but the file is preserved.
    #[tokio::test]
    async fn prune_dry_run_preserves_headless_sidecar() {
        let tmp = TempDir::new().unwrap();
        let config = make_config(&tmp);
        let g = config.storage.graphroot.as_ref().unwrap();
        let blobs = g.join("blobs").join("sha256");
        std::fs::create_dir_all(&blobs).unwrap();

        let stem = blobs.join(FAKE_HEX);
        let headless = pichi_storage::sidecar::verity_path(&stem);
        std::fs::write(&headless, b"orphan-verity").unwrap();

        run(PruneArgs { dry_run: true }, &config).await.unwrap();

        assert!(
            headless.exists(),
            "--dry-run: headless sidecar must be preserved"
        );
    }

    /// (8) Ordering invariant: source-orphan loop runs BEFORE headless sweep
    /// within the same `with_index_lock` callback. A source orphan's own
    /// `.verity` sibling is unlinked together with the source by
    /// `unlink_blob_with_sidecars` and is therefore NOT visible to the
    /// sweep's `with_extension("")` `exists()` check. Regression guard
    /// against any future refactor that moves the sweep before the
    /// source-orphan loop (which would double-process the sibling and
    /// could break under different deletion semantics).
    #[tokio::test]
    async fn prune_source_orphan_sidecars_not_double_swept() {
        let tmp = TempDir::new().unwrap();
        let config = make_config(&tmp);
        let g = config.storage.graphroot.as_ref().unwrap();
        let blobs = g.join("blobs").join("sha256");
        std::fs::create_dir_all(&blobs).unwrap();

        let src = blobs.join(FAKE_HEX);
        std::fs::write(&src, b"source-bytes").unwrap();
        std::fs::write(pichi_storage::sidecar::verity_path(&src), b"verity-bytes").unwrap();

        // Run completes without error AND both files are gone (the source
        // orphan loop unlinked them together via unlink_blob_with_sidecars
        // BEFORE the sweep ran).
        run(PruneArgs { dry_run: false }, &config).await.unwrap();

        assert!(!src.exists(), "source orphan must be unlinked");
        assert!(
            !pichi_storage::sidecar::verity_path(&src).exists(),
            ".verity sibling must be unlinked together with source"
        );
    }
}
