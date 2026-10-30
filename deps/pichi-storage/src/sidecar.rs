// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Prepared files live in `<blob>.prepared/`. Encoded blob identity does not
//! identify its verity parameters: callers supply a reconstruction key for
//! trees and a padded byte length for decoded or partially aligned raw data.
//! Legacy flat sidecars are
//! ignored when reading, but retained in cleanup accounting.

use conglobate::artifact::Digest;
use std::path::{Path, PathBuf};

/// Paths and cleanup for files derived from a cached source blob.
#[allow(async_fn_in_trait)] // used only via static dispatch on Path values
pub trait BlobSidecarExt {
    #[must_use]
    fn prepared_dir(&self) -> PathBuf;
    #[must_use]
    fn deflated_path(&self, padded_bytes: u64) -> PathBuf;
    #[must_use]
    fn verity_path(&self, parameters: &Digest) -> PathBuf;
    /// List regular prepared files without following symlinks.
    fn prepared_files(&self) -> std::io::Result<Vec<PathBuf>>;
    /// Count prepared and legacy sidecars, excluding directory metadata.
    fn sidecar_bytes(&self) -> std::io::Result<u64>;
    /// Remove the source and all derived files, tolerating missing paths.
    async fn unlink_with_sidecars(&self) -> std::io::Result<()>;
}

impl BlobSidecarExt for Path {
    fn prepared_dir(&self) -> PathBuf {
        self.with_extension("prepared")
    }

    fn deflated_path(&self, padded_bytes: u64) -> PathBuf {
        self.prepared_dir().join(format!("{padded_bytes}.deflated"))
    }

    fn verity_path(&self, parameters: &Digest) -> PathBuf {
        self.prepared_dir()
            .join(format!("{}.verity", parameters.hex()))
    }

    fn prepared_files(&self) -> std::io::Result<Vec<PathBuf>> {
        let directory = self.prepared_dir();
        match std::fs::symlink_metadata(&directory) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
            Ok(metadata) if !metadata.is_dir() => {
                return Err(std::io::ErrorKind::InvalidData.into());
            }
            Ok(_) => (),
        }
        let mut files = Vec::new();
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                files.push(entry.path());
            }
        }
        Ok(files)
    }

    fn sidecar_bytes(&self) -> std::io::Result<u64> {
        let mut bytes = 0u64;
        for path in self.prepared_files()?.into_iter().chain([
            self.with_extension("verity"),
            self.with_extension("deflated"),
        ]) {
            match std::fs::symlink_metadata(path) {
                Ok(metadata) if metadata.is_file() => bytes = bytes.saturating_add(metadata.len()),
                Ok(_) => (),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error),
            }
        }
        Ok(bytes)
    }

    async fn unlink_with_sidecars(&self) -> std::io::Result<()> {
        for path in [
            self.to_owned(),
            self.with_extension("deflated"),
            self.with_extension("verity"),
        ] {
            match tokio::fs::remove_file(path).await {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                result => result?,
            }
        }
        match tokio::fs::remove_dir_all(self.prepared_dir()).await {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
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
    fn prepared_paths_include_reconstruction_inputs() {
        let hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let src = PathBuf::from(format!("/tmp/abc/blobs/sha256/{hex}"));
        let key = Digest::Sha256([0x11; 32]);
        assert_eq!(
            src.deflated_path(8192),
            src.with_extension("prepared").join("8192.deflated")
        );
        assert_eq!(
            src.verity_path(&key),
            src.with_extension("prepared")
                .join(format!("{}.verity", key.hex()))
        );
        assert_ne!(src.deflated_path(8192), src.deflated_path(4096));
        assert_ne!(
            src.verity_path(&key),
            src.verity_path(&Digest::Sha256([0x22; 32]))
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
        std::fs::write(src.with_extension("verity"), b"verity").unwrap();

        src.unlink_with_sidecars().await.unwrap();

        assert!(!src.exists(), "source should be unlinked");
        assert!(
            !src.with_extension("verity").exists(),
            ".verity should be unlinked"
        );
    }

    #[tokio::test]
    async fn unlink_blob_with_sidecars_is_idempotent_on_partial_state() {
        let dir = tempfile::TempDir::new().unwrap();
        let src = dir.path().join("bbb");
        std::fs::write(&src, b"source").unwrap();
        std::fs::write(src.with_extension("deflated"), b"deflated").unwrap();

        src.unlink_with_sidecars().await.unwrap();
        assert!(!src.exists());
        assert!(!src.with_extension("deflated").exists());

        src.unlink_with_sidecars().await.unwrap();
    }

    #[tokio::test]
    async fn counts_and_removes_all_preparation_variants_and_legacy_files() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("aaa");
        std::fs::create_dir(source.prepared_dir()).unwrap();
        std::fs::write(&source, [0; 11]).unwrap();
        for (path, size) in [
            (source.deflated_path(8192), 3),
            (source.deflated_path(5120), 5),
            (source.verity_path(&Digest::Sha256([1; 32])), 7),
            (source.verity_path(&Digest::Sha256([2; 32])), 13),
            (source.with_extension("verity"), 17),
            (source.with_extension("deflated"), 19),
        ] {
            std::fs::write(path, vec![0; size]).unwrap();
        }
        assert_eq!(source.prepared_files().unwrap().len(), 4);
        assert_eq!(source.sidecar_bytes().unwrap(), 64);
        source.unlink_with_sidecars().await.unwrap();
        source.unlink_with_sidecars().await.unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn prepared_listing_and_cleanup_do_not_follow_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let outside = external.path().join("keep");
        std::fs::write(&outside, b"untouched").unwrap();
        let source = dir.path().join("aaa");
        std::fs::create_dir(source.prepared_dir()).unwrap();
        symlink(&outside, source.prepared_dir().join("link.verity")).unwrap();
        assert!(source.prepared_files().unwrap().is_empty());
        assert_eq!(source.sidecar_bytes().unwrap(), 0);
        source.unlink_with_sidecars().await.unwrap();
        assert_eq!(std::fs::read(&outside).unwrap(), b"untouched");
        symlink(external.path(), source.prepared_dir()).unwrap();
        assert!(source.prepared_files().is_err());
        source.unlink_with_sidecars().await.unwrap();
        assert_eq!(std::fs::read(outside).unwrap(), b"untouched");
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
        std::fs::write(src.with_extension("verity"), b"verity").unwrap();

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
