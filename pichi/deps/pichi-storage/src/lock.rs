// SPDX-License-Identifier: Apache-2.0

//! `flock(2)` helpers using `rustix::fs::flock`.
//!
//! Used by the Phase 44 partial-download path; defined in Phase 41 so the
//! foundation is stable. Phase 41's `BlobStore` does not require flock
//! because content-addressed atomic rename is sufficient (see RESEARCH
//! §Focus Area 3 "What to lock in Phase 41").

use std::path::Path;

use anyhow::Context as _;
use rustix::fd::OwnedFd;
use rustix::fs::{FlockOperation, Mode as RustixMode, OFlags, flock, open};

use crate::layout::CacheLayout;

/// Take a blocking exclusive flock on `path`. Lock is released when the
/// returned `OwnedFd` is dropped.
///
/// Creates `path` (and only `path` — caller ensures parent dir exists)
/// with mode 0600 if it does not exist.
pub fn lock_exclusive(path: &Path) -> anyhow::Result<OwnedFd> {
    let fd = open(
        path,
        OFlags::CREATE | OFlags::RDWR,
        RustixMode::from_raw_mode(0o600),
    )
    .with_context(|| format!("failed to open lock file: {}", path.display()))?;
    flock(&fd, FlockOperation::LockExclusive)
        .with_context(|| format!("failed to take exclusive flock on: {}", path.display()))?;
    Ok(fd)
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
pub fn with_index_lock<F, R>(layout: &CacheLayout, op: F) -> anyhow::Result<R>
where
    F: FnOnce() -> anyhow::Result<R>,
{
    let lock_path = layout.graphroot.join("index.json.lock");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    with_advisory_lock(&lock_path, op)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::fs::FlockOperation;

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

        // Second non-blocking attempt should fail immediately.
        let fd = open(
            &lock_path,
            OFlags::CREATE | OFlags::RDWR,
            RustixMode::from_raw_mode(0o600),
        )
        .unwrap();
        let err = flock(&fd, FlockOperation::NonBlockingLockExclusive).unwrap_err();
        // EWOULDBLOCK / EAGAIN expected.
        let raw = err.raw_os_error();
        assert!(
            raw == libc::EWOULDBLOCK || raw == libc::EAGAIN,
            "expected EWOULDBLOCK/EAGAIN, got {raw}"
        );
    }
}
