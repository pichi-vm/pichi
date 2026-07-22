// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Content-addressed blob storage. STORAGE-02, STORAGE-06, STORAGE-10.
//!
//! The trait is async: every method that touches the filesystem is an
//! `async fn` so callers never block a runtime worker. Simple reads/writes use
//! `tokio::fs`; the atomic tempfile-write-then-rename commit (which the
//! `tempfile` crate only exposes synchronously) runs inside `spawn_blocking`.
//! `open_blob` yields an async reader (`AsyncRead`) — callers stream blobs
//! without a blocking `Read` on the executor thread.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use tokio::io::AsyncRead;

use pichi_artifact::Digest;

/// An async, streaming blob handle returned by [`BlobStore::open_blob`].
pub type BlobReader = Box<dyn AsyncRead + Send + Unpin>;

/// Content-addressed blob storage trait. Implementations MUST be both `Send`
/// and `Sync` because the cache is shared across concurrent pull/push tasks.
#[async_trait]
pub trait BlobStore: Send + Sync {
    /// Atomically write `data` under `digest`. Idempotent: calling twice with
    /// the same digest is a no-op (content-addressed).
    async fn put_blob(&self, digest: &Digest, data: &[u8]) -> Result<()>;
    /// Atomically place an EXISTING file at `digest` without loading its
    /// contents into memory. The caller is responsible for having already
    /// computed the digest over the file's bytes — implementations
    /// content-address by `digest` only and do NOT re-hash.
    ///
    /// On success, the source file at `src` no longer exists (it has been
    /// atomically renamed into the blob directory). On the idempotent
    /// fast-path (blob already present), `src` is removed. On error, `src` is
    /// left in place for the caller to clean up.
    ///
    /// `src` MUST live on the SAME filesystem as the blob directory
    /// (`<root>/blobs/sha256/`) so the underlying `rename(2)` does not fail
    /// with `EXDEV`. The recommended pattern is to create the source file via
    /// `tempfile::NamedTempFile::new_in(scratch_dir())`.
    async fn put_blob_from_path(&self, src: &Path, digest: &Digest) -> Result<()>;
    /// Return a directory that lives on the SAME filesystem as the blob
    /// directory and is suitable for staging temp files prior to
    /// `put_blob_from_path`. The directory is created on demand.
    async fn scratch_dir(&self) -> Result<PathBuf>;
    /// Read the full blob into memory. Returns `Err` if not present.
    async fn get_blob(&self, digest: &Digest) -> Result<Vec<u8>>;
    /// Open the blob as an async streaming reader. Returns `Err` if not present.
    async fn open_blob(&self, digest: &Digest) -> Result<BlobReader>;
    /// Returns `true` iff the blob is present on disk.
    async fn blob_exists(&self, digest: &Digest) -> bool;
    /// Delete the blob. Returns `Ok(true)` if it existed, `Ok(false)` if not.
    async fn delete_blob(&self, digest: &Digest) -> Result<bool>;
    /// Return the on-disk path where this blob's bytes live (or would live).
    /// The path may not exist on disk yet; no I/O is performed, so this stays
    /// synchronous.
    ///
    /// Used to derive sidecar paths (`<src>.deflated` for `+zstd` layers,
    /// `<src>.verity` for every scute — D-01) and to unlink the source +
    /// sidecars together (D-08, `cmd::rmi`).
    ///
    /// Implementations MUST return a path with NO file extension on the final
    /// component (the sidecar helpers in `pichi_storage::sidecar` rely on
    /// `Path::with_extension` which strips any existing extension).
    fn blob_path(&self, digest: &Digest) -> PathBuf;
}

/// Filesystem-backed `BlobStore` storing blobs as `<root>/blobs/sha256/<hex>`.
///
/// Concurrent `put_blob` calls with the same digest are safe by construction:
/// `NamedTempFile::new_in(parent).persist(target)` performs an atomic
/// `rename(2)`. Last rename wins, and because the store is content-addressed
/// all concurrent writers produce identical bytes.
#[derive(Debug, Clone)]
pub struct FilesystemBlobStore {
    root: PathBuf,
}

impl FilesystemBlobStore {
    /// Create a new store rooted at `root`. Directories are created lazily on
    /// first `put_blob`; the constructor does not touch disk.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn blob_dir(&self) -> PathBuf {
        self.root.join("blobs").join("sha256")
    }

    /// Return the on-disk path `<root>/blobs/sha256/<digest.hex()>`.
    ///
    /// Public so `cmd::rmi` (Phase 46 D-08) can derive sidecar paths via
    /// `pichi_storage::sidecar::*_path(&blob_path)`. The returned path may not
    /// exist yet (no I/O performed); content-addressing means all on-disk
    /// paths are computable from the digest alone.
    pub fn blob_path(&self, digest: &Digest) -> PathBuf {
        self.blob_dir().join(digest.hex())
    }
}

#[async_trait]
impl BlobStore for FilesystemBlobStore {
    async fn put_blob(&self, digest: &Digest, data: &[u8]) -> Result<()> {
        let final_path = self.blob_path(digest);

        // Content-addressed fast path: if the blob already exists it is
        // immutable (same digest ⇒ same bytes).
        if tokio::fs::try_exists(&final_path).await.unwrap_or(false) {
            return Ok(());
        }

        let parent = final_path
            .parent()
            .expect("blob_path always has a parent directory");
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create blob dir: {}", parent.display()))?;

        // Atomic write via a same-fs temp + rename(2), fully async.
        crate::atomic::write_atomic(parent, &final_path, data)
            .await
            .with_context(|| format!("failed to write blob {digest}"))
    }

    async fn put_blob_from_path(&self, src: &Path, digest: &Digest) -> Result<()> {
        let final_path = self.blob_path(digest);

        // Content-addressed fast path: blob already present ⇒ immutable.
        // Best-effort remove the staging file so the caller doesn't leak it.
        if tokio::fs::try_exists(&final_path).await.unwrap_or(false) {
            let _ = tokio::fs::remove_file(src).await;
            return Ok(());
        }

        let parent = final_path
            .parent()
            .expect("blob_path always has a parent directory")
            .to_path_buf();
        tokio::fs::create_dir_all(&parent)
            .await
            .with_context(|| format!("failed to create blob dir: {}", parent.display()))?;

        // CRITICAL: `src` MUST live on the same filesystem as `final_path` for
        // rename(2) to succeed (would otherwise EXDEV). Callers obtain a
        // same-fs path via `scratch_dir()`.
        match tokio::fs::rename(src, &final_path).await {
            Ok(()) => Ok(()),
            // Race: another writer renamed first. Content identical.
            Err(_) if final_path.exists() => {
                let _ = tokio::fs::remove_file(src).await;
                Ok(())
            }
            Err(e) => Err(anyhow::anyhow!(
                "failed to rename {} -> blob {}: {}",
                src.display(),
                digest,
                e
            )),
        }
    }

    async fn scratch_dir(&self) -> Result<PathBuf> {
        let dir = self.root.join("scratch");
        tokio::fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("failed to create scratch dir: {}", dir.display()))?;
        Ok(dir)
    }

    async fn get_blob(&self, digest: &Digest) -> Result<Vec<u8>> {
        let path = self.blob_path(digest);
        tokio::fs::read(&path)
            .await
            .with_context(|| format!("blob not found: {} ({})", digest, path.display()))
    }

    async fn open_blob(&self, digest: &Digest) -> Result<BlobReader> {
        let path = self.blob_path(digest);
        let file = tokio::fs::File::open(&path)
            .await
            .with_context(|| format!("blob not found: {} ({})", digest, path.display()))?;
        Ok(Box::new(file))
    }

    async fn blob_exists(&self, digest: &Digest) -> bool {
        tokio::fs::try_exists(self.blob_path(digest))
            .await
            .unwrap_or(false)
    }

    async fn delete_blob(&self, digest: &Digest) -> Result<bool> {
        let path = self.blob_path(digest);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).with_context(|| format!("failed to delete blob: {}", path.display())),
        }
    }

    fn blob_path(&self, digest: &Digest) -> PathBuf {
        // Delegate to the inherent (same body — duplicated for trait dispatch).
        self.blob_dir().join(digest.hex())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::AsyncReadExt as _;

    fn make_store() -> (tempfile::TempDir, FilesystemBlobStore) {
        let dir = tempfile::TempDir::new().unwrap();
        let store = FilesystemBlobStore::new(dir.path());
        (dir, store)
    }

    // Test 1: put + get round-trip
    #[tokio::test]
    async fn put_get_round_trip() {
        let (_dir, s) = make_store();
        let data = b"hello world";
        let digest = Digest::from_bytes_sha256(data);
        s.put_blob(&digest, data).await.unwrap();
        let got = s.get_blob(&digest).await.unwrap();
        assert_eq!(got, data);
    }

    // Test 2: put creates the correct on-disk path
    #[tokio::test]
    async fn put_creates_correct_path() {
        let (dir, s) = make_store();
        let data = b"path-check";
        let digest = Digest::from_bytes_sha256(data);
        s.put_blob(&digest, data).await.unwrap();
        let expected = dir.path().join("blobs").join("sha256").join(digest.hex());
        assert!(expected.exists(), "expected blob at {}", expected.display());
    }

    // Test 3: put is idempotent
    #[tokio::test]
    async fn put_is_idempotent() {
        let (_dir, s) = make_store();
        let data = b"idempotent";
        let digest = Digest::from_bytes_sha256(data);
        s.put_blob(&digest, data).await.unwrap();
        s.put_blob(&digest, data).await.unwrap(); // second call must be a no-op
        assert_eq!(s.get_blob(&digest).await.unwrap(), data);
    }

    // Test 4: open_blob returns a readable async handle
    #[tokio::test]
    async fn open_blob_returns_readable_handle() {
        let (_dir, s) = make_store();
        let data: Vec<u8> = (0u8..100).collect();
        let digest = Digest::from_bytes_sha256(&data);
        s.put_blob(&digest, &data).await.unwrap();
        let mut h = s.open_blob(&digest).await.unwrap();
        let mut buf = Vec::new();
        h.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, data);
    }

    // Test 5: blob_exists distinguishes present from absent
    #[tokio::test]
    async fn blob_exists_present_and_absent() {
        let (_dir, s) = make_store();
        let d_present = Digest::from_bytes_sha256(b"x");
        let d_absent = Digest::from_bytes_sha256(b"y");
        s.put_blob(&d_present, b"x").await.unwrap();
        assert!(s.blob_exists(&d_present).await);
        assert!(!s.blob_exists(&d_absent).await);
    }

    // Test 6: delete_blob returns true for existing, false for missing
    #[tokio::test]
    async fn delete_blob_returns_true_for_existing_false_for_missing() {
        let (_dir, s) = make_store();
        let d = Digest::from_bytes_sha256(b"to-delete");
        assert!(!s.delete_blob(&d).await.unwrap());
        s.put_blob(&d, b"to-delete").await.unwrap();
        assert!(s.delete_blob(&d).await.unwrap());
        assert!(!s.blob_exists(&d).await);
    }

    // Test 7: Box<dyn BlobStore> compiles (proves dyn-safety + Send + Sync)
    #[tokio::test]
    async fn box_dyn_blobstore_compiles() {
        let dir = tempfile::TempDir::new().unwrap();
        let _store: Box<dyn BlobStore> = Box::new(FilesystemBlobStore::new(dir.path()));
    }

    // Test 8: get_blob on a missing digest returns Err with a descriptive message
    #[tokio::test]
    async fn get_blob_missing_errors() {
        let (_dir, s) = make_store();
        let d = Digest::from_bytes_sha256(b"never-stored");
        let err = s.get_blob(&d).await.unwrap_err();
        assert!(
            err.to_string().contains("blob not found"),
            "expected 'blob not found' in error, got: {err}"
        );
    }

    // Test 9: put_blob_from_path atomically renames a staged file into place
    // and removes the source, without ever loading bytes into memory.
    #[tokio::test]
    async fn put_blob_from_path_renames_atomically() {
        let (dir, s) = make_store();
        let scratch = s.scratch_dir().await.unwrap();
        let staging = scratch.join("staged.bin");
        let data: Vec<u8> = (0u8..=255).cycle().take(8 * 1024).collect();
        std::fs::write(&staging, &data).unwrap();
        let digest = Digest::from_bytes_sha256(&data);

        s.put_blob_from_path(&staging, &digest).await.unwrap();

        assert!(!staging.exists(), "staging file should have been renamed");
        let final_path = dir.path().join("blobs").join("sha256").join(digest.hex());
        assert!(
            final_path.exists(),
            "final blob missing at {}",
            final_path.display()
        );
        assert_eq!(std::fs::read(&final_path).unwrap(), data);
    }

    // Test 10: put_blob_from_path is idempotent — second call with the same
    // digest is a no-op, and the staging file is removed.
    #[tokio::test]
    async fn put_blob_from_path_idempotent() {
        let (_dir, s) = make_store();
        let scratch = s.scratch_dir().await.unwrap();
        let data = b"idempotent-rename".to_vec();
        let digest = Digest::from_bytes_sha256(&data);

        let s1 = scratch.join("s1.bin");
        std::fs::write(&s1, &data).unwrap();
        s.put_blob_from_path(&s1, &digest).await.unwrap();
        assert!(!s1.exists());

        let s2 = scratch.join("s2.bin");
        std::fs::write(&s2, &data).unwrap();
        s.put_blob_from_path(&s2, &digest).await.unwrap();
        assert!(!s2.exists(), "second staging file should have been removed");
        assert_eq!(s.get_blob(&digest).await.unwrap(), data);
    }

    // Test 11: scratch_dir is on the same filesystem as the blob dir.
    #[tokio::test]
    async fn scratch_dir_same_fs_as_blob_dir() {
        let (dir, s) = make_store();
        let scratch = s.scratch_dir().await.unwrap();
        let staging = scratch.join("crossfs-test.bin");
        std::fs::write(&staging, b"x").unwrap();
        let target_dir = dir.path().join("blobs").join("sha256");
        std::fs::create_dir_all(&target_dir).unwrap();
        let target = target_dir.join("crossfs-target");
        std::fs::rename(&staging, &target).expect("scratch and blob dir must share a filesystem");
    }

    // blob_path is exposed both as a `pub` inherent and through the trait; the
    // two paths MUST agree.
    #[test]
    fn blob_path_inherent_returns_expected_shape() {
        let store = FilesystemBlobStore::new("/tmp/abc");
        let digest = Digest::from_bytes_sha256(b"hello");
        let p = store.blob_path(&digest);
        assert_eq!(
            p,
            std::path::PathBuf::from(format!("/tmp/abc/blobs/sha256/{}", digest.hex()))
        );
    }

    #[test]
    fn blob_path_via_trait_returns_same_path() {
        let store: Box<dyn BlobStore> = Box::new(FilesystemBlobStore::new("/tmp/xyz"));
        let digest = Digest::from_bytes_sha256(b"world");
        let p = store.blob_path(&digest);
        assert_eq!(
            p,
            std::path::PathBuf::from(format!("/tmp/xyz/blobs/sha256/{}", digest.hex()))
        );
    }

    // Test 12: 8 concurrent tasks writing the same digest all succeed, exactly
    // one file remains, and no leftover temp files exist (STORAGE-10).
    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_put_blob_same_digest() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(FilesystemBlobStore::new(dir.path()));
        let data: &'static [u8] = b"concurrent-write-payload";
        let digest = Digest::from_bytes_sha256(data);

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let s = Arc::clone(&store);
                let d = digest.clone();
                tokio::spawn(async move { s.put_blob(&d, data).await })
            })
            .collect();

        for h in handles {
            h.await.unwrap().expect("each put_blob must succeed");
        }

        let final_path = dir.path().join("blobs").join("sha256").join(digest.hex());
        assert!(
            final_path.exists(),
            "final blob missing after concurrent puts"
        );
        assert_eq!(std::fs::read(&final_path).unwrap(), data);

        let blob_dir = final_path.parent().unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(blob_dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy() != digest.hex())
            .collect();
        assert!(
            leftovers.is_empty(),
            "leftover files in blob dir: {:?}",
            leftovers
                .iter()
                .map(std::fs::DirEntry::file_name)
                .collect::<Vec<_>>()
        );
    }
}
