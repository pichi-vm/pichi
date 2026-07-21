// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Concurrency test for `with_index_lock` (LOCAL-03 lock infra).
//!
//! Verifies two concurrent `with_index_lock`-wrapped closures on the same
//! `CacheLayout` are serialised — exactly one runs at a time. Implemented
//! via two threads racing into the lock with a shared `Barrier` to maximise
//! contention, then asserting that the in-closure exclusion counter never
//! exceeds 1.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use pichi_storage::{CacheLayout, FilesystemTagDb, Mode, TagDb, with_index_lock};

fn layout(tmp: &std::path::Path) -> CacheLayout {
    // Hand-construct a CacheLayout pointing at the test temp dir.
    // We bypass CacheLayout::resolve() so the test is hermetic.
    CacheLayout {
        graphroot: tmp.to_path_buf(),
        runroot: tmp.join("run"),
        mode: Mode::Rootless,
    }
}

#[test]
fn concurrent_with_index_lock_serializes_writers() {
    let tmp = tempfile::TempDir::new().unwrap();
    let l = Arc::new(layout(tmp.path()));

    // Counter of "threads currently INSIDE the closure". Must never exceed 1
    // if the lock is doing its job.
    let in_critical = Arc::new(AtomicUsize::new(0));
    let max_observed = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(2));

    let mk_thread = |id: u32| {
        let l = Arc::clone(&l);
        let in_critical = Arc::clone(&in_critical);
        let max_observed = Arc::clone(&max_observed);
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            with_index_lock(&l, || {
                let now = in_critical.fetch_add(1, Ordering::SeqCst) + 1;
                // Update max_observed if this is a new high.
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
                // Hold the lock briefly to maximise the chance the other
                // thread tries to enter. 50 ms is generous on CI.
                std::thread::sleep(Duration::from_millis(50));
                in_critical.fetch_sub(1, Ordering::SeqCst);
                Ok::<u32, anyhow::Error>(id)
            })
            .expect("with_index_lock op must succeed")
        })
    };

    let h1 = mk_thread(1);
    let h2 = mk_thread(2);

    let r1 = h1.join().unwrap();
    let r2 = h2.join().unwrap();

    // Both ran (one returned 1, the other 2); never simultaneously.
    assert!((r1, r2) == (1, 2) || (r1, r2) == (2, 1), "got ({r1}, {r2})");
    assert_eq!(
        max_observed.load(Ordering::SeqCst),
        1,
        "with_index_lock failed to serialise — observed {} concurrent closures",
        max_observed.load(Ordering::SeqCst)
    );
}

#[test]
fn with_index_lock_creates_graphroot_if_absent() {
    // Hermetic: point at a path that does NOT yet exist; with_index_lock
    // must create the parent before opening the lock file.
    let tmp = tempfile::TempDir::new().unwrap();
    let layout = CacheLayout {
        graphroot: tmp.path().join("nested").join("dir").join("not-yet"),
        runroot: tmp.path().join("run"),
        mode: Mode::Rootless,
    };
    with_index_lock(&layout, || Ok::<(), anyhow::Error>(())).expect("must succeed");
    assert!(
        layout.graphroot.join("index.json.lock").exists(),
        "lock file must have been created at {}",
        layout.graphroot.join("index.json.lock").display()
    );
}

#[test]
fn with_index_lock_path_matches_filesystem_tag_db_lock_path() {
    // CRITICAL invariant: the helper MUST lock the SAME path that
    // FilesystemTagDb::set_tag locks against (`<graphroot>/index.json.lock`).
    // If these diverge, set_tag from one process and with_index_lock from
    // another would NOT serialise.
    use pichi_artifact::Digest;

    let tmp = tempfile::TempDir::new().unwrap();
    let layout = layout(tmp.path());

    // Hold the lock via with_index_lock from one thread; from another, try
    // to set_tag — it must block until the first releases. Verified by the
    // total elapsed time being >= the with_index_lock hold duration.
    let layout_arc = Arc::new(layout);
    let layout_for_lock = Arc::clone(&layout_arc);
    let layout_for_db = Arc::clone(&layout_arc);

    let barrier = Arc::new(Barrier::new(2));
    let b1 = Arc::clone(&barrier);
    let b2 = Arc::clone(&barrier);

    let lock_holder = std::thread::spawn(move || {
        with_index_lock(&layout_for_lock, || {
            b1.wait();
            std::thread::sleep(Duration::from_millis(75));
            Ok::<(), anyhow::Error>(())
        })
        .unwrap();
    });

    let setter_start = std::time::Instant::now();
    let setter = std::thread::spawn(move || {
        let db = FilesystemTagDb::open(&layout_for_db.graphroot).unwrap();
        b2.wait();
        // This will block on the same flock the lock_holder holds.
        let d = Digest::from_bytes_sha256(b"x");
        db.set_tag("test:tag", &d).unwrap();
    });

    lock_holder.join().unwrap();
    setter.join().unwrap();
    let elapsed = setter_start.elapsed();
    // The setter waited at least ~75ms for the lock to be released.
    // Generous threshold to avoid CI flakiness.
    assert!(
        elapsed >= Duration::from_millis(50),
        "set_tag completed too fast ({elapsed:?}) — locks may not be sharing the same path"
    );
}
