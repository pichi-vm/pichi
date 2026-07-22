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

/// Path of the decompressed-scute sidecar for a source blob at `blob_path`.
///
/// Returns `<blob_path>.deflated`. The file may not exist (raw `+identity`
/// layers skip writing this sidecar per D-04).
#[must_use]
pub fn deflated_path(blob_path: &Path) -> PathBuf {
    blob_path.with_extension("deflated")
}

/// Path of the dm-verity hash-tree sidecar for a source blob at `blob_path`.
///
/// Returns `<blob_path>.verity`, byte-exact with `pichi_import::verity::compute`
/// output. Carapace exposes this to the guest as the dm-verity hash device.
#[must_use]
pub fn verity_path(blob_path: &Path) -> PathBuf {
    blob_path.with_extension("verity")
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

/// Unlink a source blob plus its `.deflated` and `.verity` sidecars (D-08).
///
/// Removes (in this order): `<blob_path>`, `<blob_path>.deflated`,
/// `<blob_path>.verity`. Each `remove_file` translates `NotFound` to `Ok(())`
/// so missing siblings do not propagate (raw scutes lack `.deflated`;
/// partial-derivation states may lack a sidecar; a second call is a no-op).
/// Other errors (permission denied, EBUSY, etc.) propagate.
///
/// # Errors
///
/// Returns the first non-ENOENT `io::Error` encountered.
pub async fn unlink_blob_with_sidecars(blob_path: &Path) -> std::io::Result<()> {
    fn ignore_enoent(r: std::io::Result<()>) -> std::io::Result<()> {
        match r {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
    ignore_enoent(tokio::fs::remove_file(blob_path).await)?;
    ignore_enoent(tokio::fs::remove_file(deflated_path(blob_path)).await)?;
    ignore_enoent(tokio::fs::remove_file(verity_path(blob_path)).await)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_use_with_extension_correctly() {
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
        std::fs::write(verity_path(&src), b"verity").unwrap();

        unlink_blob_with_sidecars(&src).await.unwrap();

        assert!(!src.exists(), "source should be unlinked");
        assert!(!verity_path(&src).exists(), ".verity should be unlinked");
    }

    #[tokio::test]
    async fn unlink_blob_with_sidecars_is_idempotent_on_partial_state() {
        let dir = tempfile::TempDir::new().unwrap();
        let src = dir.path().join("bbb");
        std::fs::write(&src, b"source").unwrap();
        std::fs::write(deflated_path(&src), b"deflated").unwrap();

        unlink_blob_with_sidecars(&src).await.unwrap();
        assert!(!src.exists());
        assert!(!deflated_path(&src).exists());

        unlink_blob_with_sidecars(&src).await.unwrap();
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
        std::fs::write(verity_path(&src), b"verity").unwrap();

        let original = std::fs::metadata(&parent).unwrap().permissions();
        let mut readonly = original.clone();
        readonly.set_mode(0o500);
        std::fs::set_permissions(&parent, readonly).unwrap();

        let result = unlink_blob_with_sidecars(&src).await;

        std::fs::set_permissions(&parent, original).unwrap();

        if rustix::process::geteuid().as_raw() != 0 {
            assert!(
                result.is_err(),
                "unlink_blob_with_sidecars should propagate EACCES on a read-only parent dir"
            );
        }
    }
}
