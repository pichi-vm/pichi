// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Concurrency test for `with_index_lock` (LOCAL-03 lock infra).
//!
//! Verifies two concurrent `with_index_lock`-wrapped closures on the same
//! `CacheLayout` are serialised — exactly one runs at a time. Implemented via
//! two tasks racing into the lock with a shared `Barrier` to maximise
//! contention, then asserting that the in-closure exclusion counter never
//! exceeds 1.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use pichi_storage::{CacheLayout, FilesystemTagDb, Mode, TagDb};
use tokio::sync::Barrier;

fn layout(tmp: &std::path::Path) -> CacheLayout {
    CacheLayout {
        graphroot: tmp.to_path_buf(),
        runroot: tmp.join("run"),
        mode: Mode::Rootless,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_with_index_lock_serializes_writers() {
    let tmp = tempfile::TempDir::new().unwrap();
    let l = Arc::new(layout(tmp.path()));

    let in_critical = Arc::new(AtomicUsize::new(0));
    let max_observed = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(2));

    let mk_task = |id: u32| {
        let l = Arc::clone(&l);
        let in_critical = Arc::clone(&in_critical);
        let max_observed = Arc::clone(&max_observed);
        let barrier = Arc::clone(&barrier);
        tokio::spawn(async move {
            barrier.wait().await;
            l.with_index_lock(|| async {
                let now = in_critical.fetch_add(1, Ordering::SeqCst) + 1;
                let mut prev = max_observed.load(Ordering::SeqCst);
                while now > prev {
                    match max_observed.compare_exchange(
                        prev,
                        now,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    ) {
                        Ok(_) => break,
                        Err(p) => prev = p,
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                in_critical.fetch_sub(1, Ordering::SeqCst);
                Ok::<u32, anyhow::Error>(id)
            })
            .await
            .expect("with_index_lock op must succeed")
        })
    };

    // Spawn BOTH before awaiting either — the 2-party barrier requires both
    // tasks to reach it, so awaiting one before spawning the other deadlocks.
    let t1 = mk_task(1);
    let t2 = mk_task(2);
    let r1 = t1.await.unwrap();
    let r2 = t2.await.unwrap();

    assert!((r1, r2) == (1, 2) || (r1, r2) == (2, 1), "got ({r1}, {r2})");
    assert_eq!(
        max_observed.load(Ordering::SeqCst),
        1,
        "with_index_lock failed to serialise — observed {} concurrent closures",
        max_observed.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn with_index_lock_creates_graphroot_if_absent() {
    let tmp = tempfile::TempDir::new().unwrap();
    let layout = CacheLayout {
        graphroot: tmp.path().join("nested").join("dir").join("not-yet"),
        runroot: tmp.path().join("run"),
        mode: Mode::Rootless,
    };
    layout
        .with_index_lock(|| async { Ok::<(), anyhow::Error>(()) })
        .await
        .expect("must succeed");
    assert!(
        layout.graphroot.join("index.json.lock").exists(),
        "lock file must have been created at {}",
        layout.graphroot.join("index.json.lock").display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_index_lock_path_matches_filesystem_tag_db_lock_path() {
    // CRITICAL invariant: the helper MUST lock the SAME path that
    // FilesystemTagDb::set_tag locks against (`<graphroot>/index.json.lock`).
    use pichi_artifact::Digest;

    let tmp = tempfile::TempDir::new().unwrap();
    let layout_arc = Arc::new(layout(tmp.path()));
    let layout_for_lock = Arc::clone(&layout_arc);
    let layout_for_db = Arc::clone(&layout_arc);

    let barrier = Arc::new(Barrier::new(2));
    let b1 = Arc::clone(&barrier);
    let b2 = Arc::clone(&barrier);

    let lock_holder = tokio::spawn(async move {
        layout_for_lock
            .with_index_lock(|| async {
                b1.wait().await;
                tokio::time::sleep(Duration::from_millis(75)).await;
                Ok::<(), anyhow::Error>(())
            })
            .await
            .unwrap();
    });

    let setter_start = std::time::Instant::now();
    let setter = tokio::spawn(async move {
        let db = FilesystemTagDb::open(&layout_for_db.graphroot).unwrap();
        b2.wait().await;
        let d = Digest::from_bytes_sha256(b"x");
        db.set_tag("test:tag", &d).await.unwrap();
    });

    lock_holder.await.unwrap();
    setter.await.unwrap();
    let elapsed = setter_start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(50),
        "set_tag completed too fast ({elapsed:?}) — locks may not be sharing the same path"
    );
}
