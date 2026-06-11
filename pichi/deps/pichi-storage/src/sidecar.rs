// SPDX-License-Identifier: Apache-2.0

//! Sidecar-next-to-source helpers for derived blobs (Phase 46 D-01, D-03, D-08).
//!
//! Phase 46 introduces two derived files alongside cached source blobs
//! (`<graphroot>/blobs/sha256/<hex>`):
//!
//! - `<src>.deflated` — decompressed scute bytes (only written for `+zstd`
//!   layers per D-04; for raw scutes the source IS the deflated bytes)
//! - `<src>.verity`   — dm-verity hash tree (byte-exact
//!   `tools/import::verity::compute` format) — carapace exposes this as the
//!   guest's hash device so dm-verity activation can read pre-computed hashes
//!
//! No `.roothash` sidecar: pichi never reads the roothash. The publisher
//! bakes `roothash=<hex>` into the PMI cmdline at arma-build time; the
//! guest reads it from cmdline at boot. pichi is dumb storage.
//!
//! ## Implicit refcount model (D-01)
//!
//! Derived files share the source blob's filename stem. The on-disk
//! invariant is: if `<src>` is alive, all `<src>.*` are alive; if `<src>`
//! is unlinked, all `<src>.*` MUST be unlinked in the same operation
//! (D-08, [`unlink_blob_with_sidecars`]). Refcount is implicit — there is
//! no separate index file for sidecars.
//!
//! ## Lock ordering (RESEARCH.md Pitfall 7)
//!
//! `pichi pull` and `pichi import` write source + sidecars OUTSIDE the
//! `with_index_lock` window (so multi-GB pulls do not block other commands).
//! `pichi rmi` and `pichi system prune` unlink the source + ALL sidecars
//! INSIDE the lock window. The asymmetry is sound because the deletion set
//! is bounded by the just-deleted manifest's layers, which by definition
//! exclude any in-flight pulls.
//!
//! ## Path scheme: `Path::with_extension`
//!
//! Both resolvers below rely on the source `<src>` having NO file
//! extension on its final component (it is a 64-char lowercase hex string —
//! see `FilesystemBlobStore::blob_path`). `Path::with_extension` REPLACES
//! any existing extension; if a future change introduces extensions on the
//! base blob name, the resolvers would silently produce the wrong paths.
//! The unit tests below assert exact path strings to guard against this.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

/// Path of the decompressed-scute sidecar for a source blob at `blob_path`.
///
/// Returns `<blob_path>.deflated`. The file may not exist (raw `+identity`
/// layers skip writing this sidecar per D-04 — carapace's read path is
/// `if exists(.deflated) then mmap that else mmap <src>`).
#[must_use]
pub fn deflated_path(blob_path: &Path) -> PathBuf {
    blob_path.with_extension("deflated")
}

/// Path of the dm-verity hash-tree sidecar for a source blob at `blob_path`.
///
/// Returns `<blob_path>.verity`. The file is byte-exact with
/// `tools/import::verity::compute` output. Carapace exposes this to the
/// guest as the dm-verity hash device.
#[must_use]
pub fn verity_path(blob_path: &Path) -> PathBuf {
    blob_path.with_extension("verity")
}

/// Atomically write `bytes` to `final_path` via a temp file in `scratch`.
///
/// Uses `tempfile::NamedTempFile::new_in(scratch).persist(final_path)`,
/// which on Linux is a POSIX-atomic `rename(2)`.
///
/// # Same-filesystem requirement
///
/// `scratch` MUST live on the SAME filesystem as `final_path`'s parent
/// directory — `rename(2)` fails with `EXDEV` across filesystems and the
/// helper returns `Err`. Callers in production pass
/// `BlobStore::scratch_dir()?`, which is guaranteed same-fs as the blob
/// directory (see `FilesystemBlobStore::scratch_dir`'s rustdoc and the
/// `scratch_dir_same_fs_as_blob_dir` regression test in
/// `blob_store::tests`). On EXDEV, `NamedTempFile`'s `Drop` cleans up the
/// temp file (RESEARCH Pitfall 1).
///
/// # Errors
///
/// Returns `Err` if creating the temp file fails, writing fails, the
/// `fsync` fails, or the final rename fails (including EXDEV).
pub fn write_sidecar_atomic(scratch: &Path, final_path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut tmp = tempfile::NamedTempFile::new_in(scratch).with_context(|| {
        format!(
            "creating sidecar temp file in {} (same-fs as final path required)",
            scratch.display()
        )
    })?;
    std::io::Write::write_all(&mut tmp, bytes)
        .with_context(|| format!("writing sidecar bytes for {}", final_path.display()))?;
    tmp.as_file()
        .sync_all()
        .with_context(|| format!("fsync sidecar temp for {}", final_path.display()))?;
    tmp.persist(final_path).map_err(|e| {
        anyhow::anyhow!(
            "failed to persist sidecar {}: {}",
            final_path.display(),
            e.error
        )
    })?;
    Ok(())
}

/// Unlink a source blob plus its `.deflated` and `.verity` sidecars (D-08).
///
/// Removes (in this order): `<blob_path>`, `<blob_path>.deflated`,
/// `<blob_path>.verity`. Each individual `remove_file` call translates
/// `io::ErrorKind::NotFound` to `Ok(())` so missing siblings do not propagate:
///
/// - Raw `+identity` scutes have no `.deflated` (D-04).
/// - Partial-derivation states (D-03 step 4 mid-write crash) may be
///   missing one or more sidecars.
/// - A second invocation against an already-cleaned blob is a no-op.
///
/// Other errors (permission denied, EBUSY, etc.) propagate to the caller.
/// The caller is responsible for the surrounding `with_index_lock` window
/// (`pichi rmi` and `pichi system prune` both wrap their orphan-walk in
/// one).
///
/// # Errors
///
/// Returns the first non-ENOENT `io::Error` encountered.
pub fn unlink_blob_with_sidecars(blob_path: &Path) -> std::io::Result<()> {
    fn ignore_enoent(r: std::io::Result<()>) -> std::io::Result<()> {
        match r {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
    ignore_enoent(std::fs::remove_file(blob_path))?;
    ignore_enoent(std::fs::remove_file(deflated_path(blob_path)))?;
    ignore_enoent(std::fs::remove_file(verity_path(blob_path)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D-01: the sidecar resolvers MUST use `Path::with_extension`
    /// (not `format!` / `Path::join`) so they tolerate a future change
    /// in the source blob's parent directory layout. Asserts exact path
    /// strings against the canonical `<root>/blobs/sha256/<hex>` shape.
    #[test]
    fn paths_use_with_extension_correctly() {
        // 64-char lowercase hex, mirrors a real digest.
        let hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let src = PathBuf::from(format!("/tmp/abc/blobs/sha256/{hex}"));
        assert_eq!(
            deflated_path(&src),
            PathBuf::from(format!("/tmp/abc/blobs/sha256/{hex}.deflated")),
        );
        assert_eq!(
            verity_path(&src),
            PathBuf::from(format!("/tmp/abc/blobs/sha256/{hex}.verity")),
        );
    }

    /// D-03 step 4: write_sidecar_atomic creates the final file with EXACT
    /// caller-provided bytes via `tempfile::NamedTempFile::persist` (=
    /// rename(2)). Mirrors the `FilesystemBlobStore::put_blob` shape from
    /// blob_store.rs:91-133.
    #[test]
    fn write_sidecar_atomic_creates_final_file_with_exact_bytes() {
        let dir = tempfile::TempDir::new().unwrap();
        // Mirror the production layout: `<root>/blobs/sha256/<hex>.verity`
        // with a same-fs scratch directory at `<root>/scratch/`.
        let blobs = dir.path().join("blobs").join("sha256");
        let scratch = dir.path().join("scratch");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::create_dir_all(&scratch).unwrap();
        let final_path = blobs.join("deadbeef.verity");
        let bytes: &[u8] = b"hello-sidecar";
        write_sidecar_atomic(&scratch, &final_path, bytes).unwrap();
        assert_eq!(std::fs::read(&final_path).unwrap(), bytes);
    }

    /// Smoke-test for Pitfall 1 (RESEARCH.md): when scratch and final_path
    /// share a filesystem (the production case via
    /// `FilesystemBlobStore::scratch_dir`), persist succeeds. We can't
    /// portably FORCE EXDEV in a unit test (test tmpdir + /tmp may share
    /// a filesystem on Linux's default tmpfs), so this is a positive
    /// smoke test: a fresh same-fs scratch + final must succeed.
    #[test]
    fn write_sidecar_atomic_succeeds_on_same_fs_scratch() {
        let dir = tempfile::TempDir::new().unwrap();
        let blobs = dir.path().join("blobs").join("sha256");
        let scratch = dir.path().join("scratch");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::create_dir_all(&scratch).unwrap();
        let final_path = blobs.join("cafebabe.deflated");
        write_sidecar_atomic(&scratch, &final_path, b"deflated bytes").unwrap();
        assert!(final_path.exists());
    }

    /// D-04: raw scutes have no `.deflated` sidecar. unlink_blob_with_sidecars
    /// MUST tolerate the missing file and remove every other sibling.
    #[test]
    fn unlink_blob_with_sidecars_tolerates_missing_deflated() {
        let dir = tempfile::TempDir::new().unwrap();
        let src = dir.path().join("aaa");
        std::fs::write(&src, b"source").unwrap();
        std::fs::write(verity_path(&src), b"verity").unwrap();
        // Note: NO .deflated written — simulating a raw scute per D-04.

        unlink_blob_with_sidecars(&src).unwrap();

        assert!(!src.exists(), "source should be unlinked");
        assert!(!verity_path(&src).exists(), ".verity should be unlinked");
    }

    /// D-03: a partial-derivation state (only some sidecars present) is
    /// fully cleaned by unlink_blob_with_sidecars without error. Idempotent
    /// when called twice.
    #[test]
    fn unlink_blob_with_sidecars_is_idempotent_on_partial_state() {
        let dir = tempfile::TempDir::new().unwrap();
        let src = dir.path().join("bbb");
        std::fs::write(&src, b"source").unwrap();
        std::fs::write(deflated_path(&src), b"deflated").unwrap();
        // No .verity — a partial derivation crash mid-write.

        // First call removes what's present.
        unlink_blob_with_sidecars(&src).unwrap();
        assert!(!src.exists());
        assert!(!deflated_path(&src).exists());

        // Second call against an already-clean blob is a no-op (every
        // remove_file returns ENOENT, all swallowed by ignore_enoent).
        unlink_blob_with_sidecars(&src).unwrap();
    }

    /// T-46-02 (threat model): non-ENOENT errors propagate to the caller.
    /// We can't force every kind of `io::Error` portably, but a chmod 0o500
    /// on the parent directory denies `unlink(2)` on Linux — the helper
    /// MUST surface that as Err rather than swallowing.
    #[cfg(unix)]
    #[test]
    fn unlink_blob_with_sidecars_propagates_non_enoent() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::TempDir::new().unwrap();
        let parent = dir.path().join("locked");
        std::fs::create_dir(&parent).unwrap();
        let src = parent.join("ccc");
        std::fs::write(&src, b"source").unwrap();
        std::fs::write(verity_path(&src), b"verity").unwrap();

        // Strip write permission from the parent so unlink(2) returns EACCES.
        // Note: root ignores DAC; if this test ever runs as uid 0 the assert
        // will fail. The pichi project does not run tests as root.
        let original = std::fs::metadata(&parent).unwrap().permissions();
        let mut readonly = original.clone();
        readonly.set_mode(0o500);
        std::fs::set_permissions(&parent, readonly).unwrap();

        let result = unlink_blob_with_sidecars(&src);

        // ALWAYS restore perms so TempDir::Drop can clean up, even if the
        // assertion below fails.
        std::fs::set_permissions(&parent, original).unwrap();

        // Skip the assertion when running as root (DAC bypass): EACCES
        // would not have been raised. Detect via the Err path being absent.
        if rustix::process::geteuid().as_raw() != 0 {
            assert!(
                result.is_err(),
                "unlink_blob_with_sidecars should propagate EACCES on a read-only parent dir"
            );
        }
    }
}
