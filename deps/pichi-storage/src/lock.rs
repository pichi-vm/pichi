// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Cross-platform advisory file-lock helpers. The async path uses `fs4`'s
//! portable `try_lock` (`flock(2)` on Unix / `LockFileEx` on Windows) polled
//! with a short async backoff, so acquiring a contended lock never parks a
//! runtime worker and never needs `spawn_blocking` — there is no OS-level
//! truly-async file lock (locks don't integrate with epoll/IOCP), so
//! try-then-yield is the non-blocking primitive. The sync `lock_exclusive` /
//! `with_advisory_lock` helpers remain for non-async callers and tests.

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::time::Duration;

use anyhow::Context as _;
use fs4::AsyncFileExt as _;
use fs4::TryLockError;

use crate::layout::CacheLayout;

/// Poll interval when an advisory lock is contended. Uncontended acquisition
/// takes the first `try_lock` (no sleep); contention retries at this cadence.
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Take an exclusive advisory lock on `path` without blocking a runtime
/// worker. The lock lives on the returned [`tokio::fs::File`]'s open file
/// description, so holding the guard across `.await` keeps the lock; dropping
/// it (closing the FD) releases it.
///
/// Uses `fs4::try_lock` (portable) and an async backoff on contention — no
/// blocking syscall parks a thread and no `spawn_blocking` is involved.
pub(crate) async fn lock_exclusive_async(path: &Path) -> anyhow::Result<tokio::fs::File> {
    let mut opts = tokio::fs::OpenOptions::new();
    opts.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        opts.mode(0o600); // tokio::fs::OpenOptions::mode is inherent on unix
    }
    let file = opts
        .open(path)
        .await
        .with_context(|| format!("failed to open lock file: {}", path.display()))?;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) => tokio::time::sleep(LOCK_POLL_INTERVAL).await,
            Err(TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("failed to lock: {}", path.display()));
            }
        }
    }
}

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
    // Non-blocking acquire; the guard holds the flock across `op().await`.
    let guard = lock_exclusive_async(&lock_path).await?;
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
