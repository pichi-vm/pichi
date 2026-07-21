// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Content-addressed blob storage. STORAGE-02, STORAGE-06, STORAGE-10.
//!
//! The CORE trait surface (`open_blob -> Box<dyn ReadSeek>`, `Send + Sync`)
//! is LOCKED from Phase 41 forward — the carapace device depends on
//! it. Additive methods that all impls trivially
//! satisfy may be added in later phases; Phase 46 Plan 01 added
//! `blob_path(&Digest) -> PathBuf` to support sidecar-path derivation
//! (D-08).

use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use tempfile::NamedTempFile;

use pichi_artifact::Digest;

use crate::ReadSeek;

/// Content-addressed blob storage trait. Implementations MUST be both
/// `Send` and `Sync` because v0.9's carapace device shares
/// `Arc<dyn BlobStore>` between vCPU threads.
pub trait BlobStore: Send + Sync {
    /// Atomically write `data` under `digest`. Idempotent: calling twice
    /// with the same digest is a no-op (content-addressed).
    fn put_blob(&self, digest: &Digest, data: &[u8]) -> Result<()>;
    /// Atomically place an EXISTING file at `digest` without loading its
    /// contents into memory. The caller is responsible for having already
    /// computed the digest over the file's bytes — implementations
    /// content-address by `digest` only and do NOT re-hash.
    ///
    /// On success, the source file at `src` no longer exists (it has
    /// been atomically renamed into the blob directory). On the
    /// idempotent fast-path (blob already present), `src` is removed.
    /// On error, `src` is left in place for the caller to clean up.
    ///
    /// `src` MUST live on the SAME filesystem as the blob directory
    /// (`<root>/blobs/sha256/`) so the underlying `rename(2)` does not
    /// fail with `EXDEV`. The recommended pattern is to create the
    /// source file via `tempfile::NamedTempFile::new_in(blob_dir_parent)`.
    ///
    /// Used by `pichi import` (BL-01) to stream multi-GB COW outputs
    /// directly to disk without ever holding the full blob in RAM.
    fn put_blob_from_path(&self, src: &Path, digest: &Digest) -> Result<()>;
    /// Return a directory that lives on the SAME filesystem as the blob
    /// directory and is suitable for staging temp files prior to
    /// `put_blob_from_path`. The directory is created on demand.
    ///
    /// Used by `pichi import` (BL-01) to find a place for streaming
    /// COW + verity output that won't fail `rename(2)` with `EXDEV`.
    fn scratch_dir(&self) -> Result<PathBuf>;
    /// Read the full blob into memory. Returns `Err` if not present.
    fn get_blob(&self, digest: &Digest) -> Result<Vec<u8>>;
    /// Open the blob as a seekable handle. Returns `Err` if not present.
    /// The handle is `Box<dyn ReadSeek>` — `ReadSeek: Read + Seek + Send`.
    fn open_blob(&self, digest: &Digest) -> Result<Box<dyn ReadSeek>>;
    /// Returns `true` iff the blob is present on disk.
    fn blob_exists(&self, digest: &Digest) -> bool;
    /// Delete the blob. Returns `Ok(true)` if it existed, `Ok(false)` if not.
    fn delete_blob(&self, digest: &Digest) -> Result<bool>;
    /// Return the on-disk path where this blob's bytes live (or would live).
    /// The path may not exist on disk yet; no I/O is performed.
    ///
    /// Used by Phase 46 callers to derive sidecar paths (`<src>.deflated`
    /// for `+zstd` layers, `<src>.verity` for every scute — D-01) and to
    /// unlink the source + sidecars together (D-08, `cmd::rmi`).
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
    /// `pichi_storage::sidecar::*_path(&blob_path)`. The returned path may
    /// not exist yet (no I/O performed); content-addressing means all
    /// on-disk paths are computable from the digest alone.
    pub fn blob_path(&self, digest: &Digest) -> PathBuf {
        self.blob_dir().join(digest.hex())
    }
}

impl BlobStore for FilesystemBlobStore {
    fn put_blob(&self, digest: &Digest, data: &[u8]) -> Result<()> {
        let final_path = self.blob_path(digest);

        // Content-addressed fast path: if the blob already exists it is
        // immutable (same digest ⇒ same bytes).
        if final_path.exists() {
            return Ok(());
        }

        let parent: &Path = final_path
            .parent()
            .expect("blob_path always has a parent directory");
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create blob dir: {}", parent.display()))?;

        // CRITICAL: new_in(parent) — temp file lives on the SAME filesystem as
        // the final path. persist() uses rename(2) which fails with EXDEV if
        // source and target are on different filesystems (e.g., /tmp vs home).
        let mut tmp = NamedTempFile::new_in(parent)
            .with_context(|| format!("failed to create temp blob file in: {}", parent.display()))?;
        tmp.write_all(data)
            .with_context(|| format!("failed to write temp blob for {}", digest))?;
        tmp.flush()
            .with_context(|| format!("failed to flush temp blob for {}", digest))?;

        // persist() = atomic rename(2). On Linux this is POSIX-atomic.
        // If a concurrent writer beat us here (race), the file is the same
        // bytes — treat as success.
        match tmp.persist(&final_path) {
            Ok(_) => Ok(()),
            Err(e) => {
                if final_path.exists() {
                    // Race: another writer renamed first. Content identical.
                    return Ok(());
                }
                Err(anyhow::anyhow!(
                    "failed to persist blob {}: {}",
                    digest,
                    e.error
                ))
            }
        }
    }

    fn put_blob_from_path(&self, src: &Path, digest: &Digest) -> Result<()> {
        let final_path = self.blob_path(digest);

        // Content-addressed fast path: if the blob already exists it is
        // immutable (same digest ⇒ same bytes). Best-effort remove the
        // staging file so the caller doesn't leak it.
        if final_path.exists() {
            let _ = std::fs::remove_file(src);
            return Ok(());
        }

        let parent: &Path = final_path
            .parent()
            .expect("blob_path always has a parent directory");
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create blob dir: {}", parent.display()))?;

        // CRITICAL: `src` MUST live on the same filesystem as
        // `final_path` for rename(2) to succeed (would otherwise EXDEV).
        // Callers obtain a same-fs path via `scratch_dir()`.
        match std::fs::rename(src, &final_path) {
            Ok(()) => Ok(()),
            Err(e) => {
                if final_path.exists() {
                    // Race: another writer renamed first. Content
                    // identical (content-addressing). Best-effort
                    // remove the source we lost the race on.
                    let _ = std::fs::remove_file(src);
                    return Ok(());
                }
                Err(anyhow::anyhow!(
                    "failed to rename {} -> blob {}: {}",
                    src.display(),
                    digest,
                    e
                ))
            }
        }
    }

    fn scratch_dir(&self) -> Result<PathBuf> {
        let dir = self.root.join("scratch");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create scratch dir: {}", dir.display()))?;
        Ok(dir)
    }

    fn get_blob(&self, digest: &Digest) -> Result<Vec<u8>> {
        let path = self.blob_path(digest);
        std::fs::read(&path)
            .with_context(|| format!("blob not found: {} ({})", digest, path.display()))
    }

    fn open_blob(&self, digest: &Digest) -> Result<Box<dyn ReadSeek>> {
        let path = self.blob_path(digest);
        let file = File::open(&path)
            .with_context(|| format!("blob not found: {} ({})", digest, path.display()))?;
        // File implements Read + Seek + Send; the blanket impl in read_seek.rs
        // covers it automatically.
        Ok(Box::new(file))
    }

    fn blob_exists(&self, digest: &Digest) -> bool {
        self.blob_path(digest).exists()
    }

    fn delete_blob(&self, digest: &Digest) -> Result<bool> {
        let path = self.blob_path(digest);
        match std::fs::remove_file(&path) {
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
    use std::io::{Read as _, Seek as _, SeekFrom};
    use std::sync::Arc;

    fn make_store() -> (tempfile::TempDir, FilesystemBlobStore) {
        let dir = tempfile::TempDir::new().unwrap();
        let store = FilesystemBlobStore::new(dir.path());
        (dir, store)
    }

    // Test 1: put + get round-trip
    #[test]
    fn put_get_round_trip() {
        let (_dir, s) = make_store();
        let data = b"hello world";
        let digest = Digest::from_bytes_sha256(data);
        s.put_blob(&digest, data).unwrap();
        let got = s.get_blob(&digest).unwrap();
        assert_eq!(got, data);
    }

    // Test 2: put creates the correct on-disk path
    #[test]
    fn put_creates_correct_path() {
        let (dir, s) = make_store();
        let data = b"path-check";
        let digest = Digest::from_bytes_sha256(data);
        s.put_blob(&digest, data).unwrap();
        let expected = dir.path().join("blobs").join("sha256").join(digest.hex());
        assert!(expected.exists(), "expected blob at {}", expected.display());
    }

    // Test 3: put is idempotent
    #[test]
    fn put_is_idempotent() {
        let (_dir, s) = make_store();
        let data = b"idempotent";
        let digest = Digest::from_bytes_sha256(data);
        s.put_blob(&digest, data).unwrap();
        s.put_blob(&digest, data).unwrap(); // second call must be a no-op
        assert_eq!(s.get_blob(&digest).unwrap(), data);
    }

    // Test 4: open_blob returns a seekable handle
    #[test]
    fn open_blob_returns_seekable_handle() {
        let (_dir, s) = make_store();
        let data: Vec<u8> = (0u8..100).collect();
        let digest = Digest::from_bytes_sha256(&data);
        s.put_blob(&digest, &data).unwrap();
        let mut h = s.open_blob(&digest).unwrap();
        h.seek(SeekFrom::Start(50)).unwrap();
        let mut buf = [0u8; 10];
        h.read_exact(&mut buf).unwrap();
        assert_eq!(buf, data[50..60]);
    }

    // Test 5: blob_exists distinguishes present from absent
    #[test]
    fn blob_exists_present_and_absent() {
        let (_dir, s) = make_store();
        let d_present = Digest::from_bytes_sha256(b"x");
        let d_absent = Digest::from_bytes_sha256(b"y");
        s.put_blob(&d_present, b"x").unwrap();
        assert!(s.blob_exists(&d_present));
        assert!(!s.blob_exists(&d_absent));
    }

    // Test 6: delete_blob returns true for existing, false for missing
    #[test]
    fn delete_blob_returns_true_for_existing_false_for_missing() {
        let (_dir, s) = make_store();
        let d = Digest::from_bytes_sha256(b"to-delete");
        // Not yet stored — should return false.
        assert!(!s.delete_blob(&d).unwrap());
        s.put_blob(&d, b"to-delete").unwrap();
        // Now stored — should return true.
        assert!(s.delete_blob(&d).unwrap());
        // Gone — should not exist.
        assert!(!s.blob_exists(&d));
    }

    // Test 7: Box<dyn BlobStore> compiles (proves dyn-safety + Send + Sync)
    #[test]
    fn box_dyn_blobstore_compiles() {
        let dir = tempfile::TempDir::new().unwrap();
        // This line is the compile-time assertion: FilesystemBlobStore satisfies
        // the dyn-safe BlobStore trait with Send + Sync supertraits.
        let _store: Box<dyn BlobStore> = Box::new(FilesystemBlobStore::new(dir.path()));
    }

    // Test 8: get_blob on a missing digest returns Err with a descriptive message
    #[test]
    fn get_blob_missing_errors() {
        let (_dir, s) = make_store();
        let d = Digest::from_bytes_sha256(b"never-stored");
        let err = s.get_blob(&d).unwrap_err();
        assert!(
            err.to_string().contains("blob not found"),
            "expected 'blob not found' in error, got: {err}"
        );
    }

    // Test 9: put_blob_from_path atomically renames a staged file into
    // place and removes the source, without ever loading bytes into memory.
    #[test]
    fn put_blob_from_path_renames_atomically() {
        let (dir, s) = make_store();
        let scratch = s.scratch_dir().unwrap();
        let staging = scratch.join("staged.bin");
        let data: Vec<u8> = (0u8..=255).cycle().take(8 * 1024).collect();
        std::fs::write(&staging, &data).unwrap();
        let digest = Digest::from_bytes_sha256(&data);

        s.put_blob_from_path(&staging, &digest).unwrap();

        // Source is gone; final blob is in place with correct content.
        assert!(!staging.exists(), "staging file should have been renamed");
        let final_path = dir.path().join("blobs").join("sha256").join(digest.hex());
        assert!(
            final_path.exists(),
            "final blob missing at {}",
            final_path.display()
        );
        assert_eq!(std::fs::read(&final_path).unwrap(), data);
    }

    // Test 10: put_blob_from_path is idempotent — second call with the
    // same digest is a no-op, and the staging file is removed.
    #[test]
    fn put_blob_from_path_idempotent() {
        let (_dir, s) = make_store();
        let scratch = s.scratch_dir().unwrap();
        let data = b"idempotent-rename".to_vec();
        let digest = Digest::from_bytes_sha256(&data);

        let s1 = scratch.join("s1.bin");
        std::fs::write(&s1, &data).unwrap();
        s.put_blob_from_path(&s1, &digest).unwrap();
        assert!(!s1.exists());

        // Second time: the blob already exists; we provide a fresh
        // staging file which should be removed without touching the
        // existing blob.
        let s2 = scratch.join("s2.bin");
        std::fs::write(&s2, &data).unwrap();
        s.put_blob_from_path(&s2, &digest).unwrap();
        assert!(!s2.exists(), "second staging file should have been removed");
        assert_eq!(s.get_blob(&digest).unwrap(), data);
    }

    // Test 11: scratch_dir is on the same filesystem as the blob dir,
    // i.e. a rename between them succeeds. (The same-fs guarantee is
    // critical for `put_blob_from_path` not to fail with EXDEV.)
    #[test]
    fn scratch_dir_same_fs_as_blob_dir() {
        let (dir, s) = make_store();
        let scratch = s.scratch_dir().unwrap();
        let staging = scratch.join("crossfs-test.bin");
        std::fs::write(&staging, b"x").unwrap();
        let target_dir = dir.path().join("blobs").join("sha256");
        std::fs::create_dir_all(&target_dir).unwrap();
        let target = target_dir.join("crossfs-target");
        // If scratch and blobs are on different filesystems, rename
        // returns EXDEV. We require same-fs.
        std::fs::rename(&staging, &target).expect("scratch and blob dir must share a filesystem");
    }

    // Phase 46 Plan 01 Task 1: blob_path is exposed both as a `pub` inherent
    // (consumed by Plan 04 rmi against the concrete `FilesystemBlobStore`)
    // and through the `BlobStore` trait (consumed by Plan 02 pull through
    // `&dyn BlobStore`). The two paths MUST agree.
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

    // Test 12: 8 concurrent threads writing the same digest all succeed,
    // exactly one file remains, and no leftover temp files exist.
    // Covers STORAGE-10 (blob-write concurrent safety).
    #[test]
    fn concurrent_put_blob_same_digest() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(FilesystemBlobStore::new(dir.path()));
        let data: &[u8] = b"concurrent-write-payload";
        let digest = Digest::from_bytes_sha256(data);

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let s = Arc::clone(&store);
                let d = digest.clone();
                std::thread::spawn(move || s.put_blob(&d, data))
            })
            .collect();

        for h in handles {
            h.join().unwrap().expect("each put_blob must succeed");
        }

        // Exactly one final file with correct content.
        let final_path = dir.path().join("blobs").join("sha256").join(digest.hex());
        assert!(
            final_path.exists(),
            "final blob missing after concurrent puts"
        );
        assert_eq!(
            std::fs::read(&final_path).unwrap(),
            data,
            "final blob content mismatch"
        );

        // No leftover temp files in the blob directory.
        let blob_dir = final_path.parent().unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(blob_dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| {
                let name = e.file_name();
                let name_str = name.to_string_lossy();
                // Anything that is not the final blob hex is a leaked temp file.
                name_str != digest.hex()
            })
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
