// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Cross-platform advisory file-lock helpers built on `std::fs::File`'s
//! native locking (`File::lock` / `try_lock` / `unlock`, stable since Rust
//! 1.89). These map to `flock(2)` on Unix and `LockFileEx` on Windows, so
//! the cache is inter-process-safe on every supported OS without any
//! platform-specific code here.
//!
//! Used by the Phase 44 partial-download path; defined in Phase 41 so the
//! foundation is stable. Phase 41's `BlobStore` does not require locking
//! because content-addressed atomic rename is sufficient (see RESEARCH
//! §Focus Area 3 "What to lock in Phase 41").

use std::fs::{File, OpenOptions};
use std::path::Path;

use anyhow::Context as _;

use crate::layout::CacheLayout;

/// Take a blocking exclusive lock on `path`. The lock is released when the
/// returned [`File`] is dropped (closing the handle).
///
/// Creates `path` (and only `path` — caller ensures parent dir exists),
/// with mode 0600 on Unix, if it does not exist. On Windows the file must
/// be opened for read or write to be lockable, which `read(true)` ensures.
pub fn lock_exclusive(path: &Path) -> anyhow::Result<File> {
    let mut opts = OpenOptions::new();
    opts.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let file = opts
        .open(path)
        .with_context(|| format!("failed to open lock file: {}", path.display()))?;
    file.lock()
        .with_context(|| format!("failed to take exclusive lock on: {}", path.display()))?;
    Ok(file)
}

/// Run `op` while holding an exclusive flock on `path`. The lock is
/// released after `op` returns (success or failure).
pub fn with_advisory_lock<F, R>(path: &Path, op: F) -> anyhow::Result<R>
where
    F: FnOnce() -> anyhow::Result<R>,
{
    let _guard = lock_exclusive(path)?;
    op()
}

/// Run `op` while holding the cache's index.json advisory lock.
///
/// Lock path: `<layout.graphroot>/index.json.lock` — the SAME path that
/// [`crate::FilesystemTagDb::set_tag`] / `delete_tag` lock against. This
/// means a `with_index_lock`-wrapped closure that ALSO calls `set_tag`
/// or `delete_tag` from the SAME process WILL deadlock (separate FDs to
/// the same file from one process block each other under `flock(2)`).
///
/// **Intended use** (per Plan 05 `cmd::rmi`): wrap a multi-step refcount
/// computation that uses the lock-free reads ([`crate::TagDb::list_tags`]
/// / [`crate::TagDb::resolve_tag`] / [`crate::BlobStore::get_blob`]). Any
/// `set_tag` / `delete_tag` call MUST happen OUTSIDE the closure (after
/// it returns).
///
/// Per D-22 / LOCAL-03 / Pitfall 4 in 42-RESEARCH.md: this serialises two
/// concurrent `pichi rmi` calls so they cannot both compute refcounts
/// before either commits the deletion.
pub async fn with_index_lock<F, Fut, R>(layout: &CacheLayout, op: F) -> anyhow::Result<R>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<R>>,
{
    let lock_path = layout.graphroot.join("index.json.lock");
    if let Some(parent) = lock_path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    // Acquire the flock off-runtime — it may block on a competing process.
    // The lock lives on the open file description, so holding the returned
    // `File` guard across `op().await` keeps it held regardless of which
    // worker thread the task resumes on; dropping it (closing the FD) releases.
    let lp = lock_path.clone();
    let guard = tokio::task::spawn_blocking(move || lock_exclusive(&lp))
        .await
        .context("index lock task panicked")??;
    let result = op().await;
    drop(guard);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::TryLockError;

    #[test]
    fn lock_then_release_works() {
        let dir = tempfile::TempDir::new().unwrap();
        let lock_path = dir.path().join("test.lock");
        let result = with_advisory_lock(&lock_path, || Ok::<_, anyhow::Error>(42)).unwrap();
        assert_eq!(result, 42);
        assert!(lock_path.exists());
    }

    #[test]
    fn second_nonblocking_lock_fails_while_first_held() {
        let dir = tempfile::TempDir::new().unwrap();
        let lock_path = dir.path().join("contended.lock");

        // First lock — held until the end of this test.
        let _first = lock_exclusive(&lock_path).unwrap();

        // Second non-blocking attempt should report contention.
        let second = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        match second.try_lock() {
            Err(TryLockError::WouldBlock) => {}
            other => panic!("expected Err(WouldBlock), got {other:?}"),
        }
    }
}
