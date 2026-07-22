// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `pichi rmi <ref>...` (LOCAL-03). Removes one or more tags; refcounts
//! blobs and unlinks orphans.
//!
//! Concurrency (T-42-02): the entire refcount-and-unlink sequence —
//! resolve target, scan still-referenced set, delete tag, unlink orphan
//! blobs — runs inside ONE `with_index_lock` window. The tag mutation uses
//! [`FilesystemTagDb::delete_tag_locked`] (no inner flock) so we do not
//! self-deadlock against the outer `with_index_lock` flock on the same
//! `index.json.lock` path. A concurrent `pichi tag` / `pichi pull` in
//! another process is forced to wait at the same flock and therefore
//! cannot pin a blob between our refcount scan and our unlink.
//!
//! Sidecar atomicity (Phase 46 D-08): every orphaned source blob's
//! `<src>.deflated` and `<src>.verity` sidecars (Phase 46 D-01 — no
//! `.roothash`; pichi never reads the roothash) are unlinked together with
//! the source inside the SAME `with_index_lock` window via
//! [`pichi_storage::sidecar::unlink_blob_with_sidecars`]. Implicit refcount
//! (D-01): if the source is alive, all sidecars are alive; if the source
//! is unlinked, all sidecars MUST be unlinked in the same operation. The
//! helper tolerates ENOENT for missing siblings (raw scutes per D-04 have
//! no `.deflated`; manifest blobs have no sidecars; partial-derivation
//! states per D-03 may be missing one or more sidecars).
//!
//! See `docs/threats.md` (T-42-02) and Plan 04 SUMMARY for the rationale.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::collections::HashSet;

use anyhow::{Context, Result, anyhow, bail};

use pichi_artifact::{Digest, Manifest, Reference};
use pichi_storage::{
    BlobSidecarExt, BlobStore, CacheLayout, FilesystemBlobStore, FilesystemTagDb, TagDb, TagEntry,
};

use crate::cli::RmiArgs;
use crate::config::Config;

/// `pichi rmi <ref>...` entry point — remove tags + refcount-aware blob
/// GC (LOCAL-03).
pub async fn run(args: RmiArgs, config: &Config) -> Result<()> {
    let layout = config.resolve_layout()?;
    for input in &args.references {
        rmi_one(input, args.force, &layout)
            .await
            .with_context(|| format!("removing {input}"))?;
    }
    Ok(())
}

async fn rmi_one(input: &str, force: bool, layout: &CacheLayout) -> Result<()> {
    let target_ref: Reference = input
        .parse()
        .with_context(|| format!("invalid reference: {input}"))?;
    let target_key = target_ref.to_string();

    // T-42-02: refcount, tag delete, AND blob unlink all happen inside ONE
    // index-lock window. The lock is dropped only after every blob unlink
    // returns. `delete_tag_locked` skips re-acquiring the flock so we don't
    // self-deadlock against the outer `with_index_lock`.
    let (target_digest, deleted) = layout.with_index_lock(|| async {
        let db = FilesystemTagDb::open(&layout.graphroot)?;
        let blob_store = FilesystemBlobStore::new(&layout.graphroot);

        // Resolve target.
        let target_digest = db
            .resolve_tag(&target_key)
            .await?
            .ok_or_else(|| anyhow!("ref not found in cache: {target_key}"))?;

        // --force check: other tags pointing at the SAME manifest digest?
        let all_tags = db.list_tags().await?;
        let other_pointers: Vec<&TagEntry> = all_tags
            .iter()
            .filter(|e| e.digest == target_digest && e.tag != target_key)
            .collect();
        if !other_pointers.is_empty() && !force {
            bail!(
                "manifest {} is referenced by other tags (use --force to remove this tag and preserve blobs): {}",
                target_digest,
                other_pointers
                    .iter()
                    .map(|e| e.tag.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        // Refcount: build the set of digests still referenced by ANY tag
        // EXCEPT the one we're about to delete.
        let mut still_referenced: HashSet<Digest> = HashSet::new();
        for entry in &all_tags {
            if entry.tag == target_key {
                continue; // skip the tag we're removing
            }
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

        // Build to_delete = target manifest's digest + its layer digests,
        // minus still_referenced.
        let mut candidates: HashSet<Digest> = HashSet::new();
        candidates.insert(target_digest.clone());
        if let Ok(bytes) = blob_store.get_blob(&target_digest).await {
            if let Ok(m) = Manifest::from_reader(bytes.as_slice()) {
                for layer in &m.layers {
                    if let Ok(d) = layer.digest_str().parse::<Digest>() {
                        candidates.insert(d);
                    }
                }
            }
        }

        let to_delete: HashSet<Digest> =
            candidates.difference(&still_referenced).cloned().collect();

        // Mutate state under the same lock window — order matters:
        // (1) drop the tag pointer first so any reader who acquires the lock
        // *after* we release it cannot re-resolve target_key, and
        // (2) only then unlink the orphan blobs. This matches the canonical
        // "refcount-then-unlink" pattern; both steps are observable as a
        // single atomic transition because they're inside the same flock.
        db.delete_tag_locked(&target_key)
            .await
            .with_context(|| format!("deleting tag {target_key}"))?;

        // Phase 46 D-08: every orphaned source blob's sidecars
        // (`<src>.deflated`, `<src>.verity`) are unlinked together with
        // the source inside the SAME with_index_lock window. The helper
        // tolerates ENOENT for each individual sibling:
        //   - raw `+identity` scutes have no `.deflated` (D-04)
        //   - manifest blobs have no sidecars (they are not scute layers)
        //   - partial-derivation states (D-03 mid-write crash) may be
        //     missing one or more sidecars
        // No `.roothash` sidecar exists — pichi never reads the roothash;
        // the publisher bakes `roothash=<hex>` into the PMI cmdline at
        // arma-build time. The `existed` check preserves the deleted-count
        // metric meaning (number of source blobs that actually got unlinked,
        // ignoring sidecar absence).
        let mut deleted = 0usize;
        for d in &to_delete {
            let blob_path = blob_store.blob_path(d);
            let existed = blob_path.exists();
            blob_path.unlink_with_sidecars()
                .await
                .with_context(|| format!("unlink blob+sidecars for {d}"))?;
            if existed {
                deleted += 1;
            }
        }

        Ok::<(Digest, usize), anyhow::Error>((target_digest, deleted))
    })
    .await?;

    log::info!(
        "removed tag {target_key} (manifest {target_digest}); unlinked {deleted} orphan blob(s)"
    );
    Ok(())
}
