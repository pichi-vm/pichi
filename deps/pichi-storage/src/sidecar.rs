// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
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
//! The write/unlink helpers are async: the atomic temp-write-then-rename
//! (blocking, sync-only in the `tempfile` crate) runs in `spawn_blocking`,
//! and unlinks use `tokio::fs`. The path resolvers are pure and stay sync.
//!
//! ## Path scheme: `Path::with_extension`
//!
//! Both resolvers below rely on the source `<src>` having NO file extension
//! on its final component (it is a 64-char lowercase hex string — see
//! `FilesystemBlobStore::blob_path`). `Path::with_extension` REPLACES any
//! existing extension; the unit tests assert exact path strings to guard this.

use std::path::{Path, PathBuf};

/// Sidecar-path derivation + cleanup for a cached blob path. `Path` is foreign,
/// so these hang off a local extension trait (per the "extension trait when you
/// don't own the type" rule) — callers read `blob_path.verity_path()` and
/// `blob_path.unlink_with_sidecars().await`.
#[allow(async_fn_in_trait)] // used only via static dispatch on Path values
pub trait BlobSidecarExt {
    /// `<self>.deflated` — the decompressed-scute sidecar (only present for
    /// `+zstd` layers per D-04; raw scutes are their own deflated bytes).
    #[must_use]
    fn deflated_path(&self) -> PathBuf;
    /// `<self>.verity` — the dm-verity hash tree (byte-exact with
    /// `pichi_import::verity::compute`), exposed to the guest as the hash device.
    #[must_use]
    fn verity_path(&self) -> PathBuf;
    /// Unlink this blob path plus its `.deflated`/`.verity` siblings (D-08),
    /// tolerating a missing sibling (`NotFound` → `Ok`).
    async fn unlink_with_sidecars(&self) -> std::io::Result<()>;
}

impl BlobSidecarExt for Path {
    fn deflated_path(&self) -> PathBuf {
        self.with_extension("deflated")
    }

    fn verity_path(&self) -> PathBuf {
        self.with_extension("verity")
    }

    async fn unlink_with_sidecars(&self) -> std::io::Result<()> {
        fn ignore_enoent(r: std::io::Result<()>) -> std::io::Result<()> {
            match r {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                other => other,
            }
        }
        ignore_enoent(tokio::fs::remove_file(self).await)?;
        ignore_enoent(tokio::fs::remove_file(self.deflated_path()).await)?;
        ignore_enoent(tokio::fs::remove_file(self.verity_path()).await)?;
        Ok(())
    }
}

/// Atomically write `bytes` to `final_path` via a same-fs temp + `rename(2)`,
/// fully async (no blocking syscall on a runtime worker).
///
/// `scratch` MUST live on the SAME filesystem as `final_path`'s parent (the
/// underlying `rename(2)` fails with `EXDEV` otherwise); production callers
/// pass `BlobStore::scratch_dir().await?`.
///
/// # Errors
///
/// Returns `Err` if creating the temp file fails, writing fails, the `fsync`
/// fails, or the final rename fails (including EXDEV).
pub async fn write_sidecar_atomic(
    scratch: &Path,
    final_path: &Path,
    bytes: &[u8],
) -> anyhow::Result<()> {
    crate::atomic::write_atomic(scratch, final_path, bytes).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_use_with_extension_correctly() {
        let hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let src = PathBuf::from(format!("/tmp/abc/blobs/sha256/{hex}"));
        assert_eq!(
            src.deflated_path(),
            PathBuf::from(format!("/tmp/abc/blobs/sha256/{hex}.deflated")),
        );
        assert_eq!(
            src.verity_path(),
            PathBuf::from(format!("/tmp/abc/blobs/sha256/{hex}.verity")),
        );
    }

    #[tokio::test]
    async fn write_sidecar_atomic_creates_final_file_with_exact_bytes() {
        let dir = tempfile::TempDir::new().unwrap();
        let blobs = dir.path().join("blobs").join("sha256");
        let scratch = dir.path().join("scratch");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::create_dir_all(&scratch).unwrap();
        let final_path = blobs.join("deadbeef.verity");
        let bytes: &[u8] = b"hello-sidecar";
        write_sidecar_atomic(&scratch, &final_path, bytes)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&final_path).unwrap(), bytes);
    }

    #[tokio::test]
    async fn write_sidecar_atomic_succeeds_on_same_fs_scratch() {
        let dir = tempfile::TempDir::new().unwrap();
        let blobs = dir.path().join("blobs").join("sha256");
        let scratch = dir.path().join("scratch");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::create_dir_all(&scratch).unwrap();
        let final_path = blobs.join("cafebabe.deflated");
        write_sidecar_atomic(&scratch, &final_path, b"deflated bytes")
            .await
            .unwrap();
        assert!(final_path.exists());
    }

    #[tokio::test]
    async fn unlink_blob_with_sidecars_tolerates_missing_deflated() {
        let dir = tempfile::TempDir::new().unwrap();
        let src = dir.path().join("aaa");
        std::fs::write(&src, b"source").unwrap();
        std::fs::write(src.verity_path(), b"verity").unwrap();

        src.unlink_with_sidecars().await.unwrap();

        assert!(!src.exists(), "source should be unlinked");
        assert!(!src.verity_path().exists(), ".verity should be unlinked");
    }

    #[tokio::test]
    async fn unlink_blob_with_sidecars_is_idempotent_on_partial_state() {
        let dir = tempfile::TempDir::new().unwrap();
        let src = dir.path().join("bbb");
        std::fs::write(&src, b"source").unwrap();
        std::fs::write(src.deflated_path(), b"deflated").unwrap();

        src.unlink_with_sidecars().await.unwrap();
        assert!(!src.exists());
        assert!(!src.deflated_path().exists());

        src.unlink_with_sidecars().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unlink_blob_with_sidecars_propagates_non_enoent() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::TempDir::new().unwrap();
        let parent = dir.path().join("locked");
        std::fs::create_dir(&parent).unwrap();
        let src = parent.join("ccc");
        std::fs::write(&src, b"source").unwrap();
        std::fs::write(src.verity_path(), b"verity").unwrap();

        let original = std::fs::metadata(&parent).unwrap().permissions();
        let mut readonly = original.clone();
        readonly.set_mode(0o500);
        std::fs::set_permissions(&parent, readonly).unwrap();

        let result = src.unlink_with_sidecars().await;

        std::fs::set_permissions(&parent, original).unwrap();

        if rustix::process::geteuid().as_raw() != 0 {
            assert!(
                result.is_err(),
                "unlink_blob_with_sidecars should propagate EACCES on a read-only parent dir"
            );
        }
    }
}
