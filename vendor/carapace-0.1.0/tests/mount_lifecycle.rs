// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! End-to-end assemble + mount + teardown against a real layered ext4 fixture.
//!
//! Each test:
//!   1. Builds a fixture via `common::build_chain` (shell pipeline:
//!      `sgdisk` + `veritysetup` + `dmsetup` writable-snapshot dogfood
//!      — no production-crate code is reused on the producer side).
//!   2. `losetup --partscan` the image (kernel populates
//!      `/sys/class/block/loopXpY/uevent` with PARTUUIDs synchronously).
//!   3. Calls `carapace::Chain::from_gpt(&bytes)?.assemble()` (library API).
//!   4. Mounts `/dev/dm-{minor}` read-only and verifies the expected
//!      per-scute files exist with the right content (proves cross-scute
//!      composition end to end, not just byte parsing).
//!   5. Unmounts, then drops the device file — DM_DEFERRED_REMOVE
//!      cascades the teardown.
//!   6. Drops `AttachedImage` → `losetup -d`. Verifies the image is no
//!      longer loop-bound.
//!
//! All tests `#[serial]` (global dm + loop namespace).

#![cfg(all(target_os = "linux", feature = "gpt"))]

mod common;

use common::{
    assemble_from_hex, build_chain, carapace_devices_present, cleanup_dm, loop_bound_to,
    with_mounted, AttachedImage,
};
use serial_test::serial;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;

fn is_root() -> bool {
    match std::fs::read_to_string("/proc/self/status") {
        Ok(s) => s.lines().any(|l| {
            l.strip_prefix("Uid:")
                .and_then(|rest| rest.split_whitespace().next())
                .is_some_and(|euid| euid == "0")
        }),
        Err(_) => false,
    }
}

/// Run the full assemble -> mount -> verify -> umount -> drop loop and
/// assert that every per-scute file is visible at the expected content.
fn assemble_mount_verify_teardown(test_label: &str, n_scutes: usize) {
    if !is_root() {
        eprintln!("skip: requires root");
        return;
    }
    let dir = TempDir::new().unwrap();
    let img = dir.path().join(format!("{test_label}.img"));
    let chain = build_chain(&img, n_scutes);

    // Pre-test cleanup of any leftover CARAPACE-* devices.
    cleanup_dm();

    // losetup --partscan exposes the image's GPT partitions to the
    // kernel; sysfs gets PARTUUID entries synchronously. Chain::from_gpt
    // discovers them from /sys/class/block.
    let attached = AttachedImage::attach(&chain.image);

    let removed = assemble_from_hex(&chain.trusted_root_hex)
        .unwrap_or_else(|e| panic!("assemble failed: {e}"));

    let minor = carapace::DevId::try_from(removed.metadata().unwrap())
        .unwrap()
        .minor();
    let dev_path = PathBuf::from(format!("/dev/dm-{minor}"));
    assert!(
        dev_path.exists(),
        "{} must exist after assemble",
        dev_path.display()
    );

    // Mount the assembled device read-only and verify per-scute files.
    // This exercises the full layered composition, not just byte parsing.
    with_mounted(&dev_path, |mnt| {
        for (filename, expected) in chain.expected_files() {
            let path = mnt.join(&filename);
            let actual = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            assert_eq!(actual, expected, "content mismatch for {filename}");
        }
    });

    // Drop the device file: DM_DEFERRED_REMOVE cascades the teardown.
    drop(removed);

    // Wait for the chain to tear down.
    assert!(
        !carapace_devices_present(Duration::from_secs(5)),
        "CARAPACE-* devices must be gone after dropping the device file"
    );

    drop(attached); // losetup -d
    assert!(
        !loop_bound_to(&chain.image, Duration::from_secs(2)),
        "no loop must be bound to image after teardown"
    );
}

#[test]
#[serial]
fn assemble_then_teardown_one_scute_chain() {
    assemble_mount_verify_teardown("one", 1);
}

#[test]
#[serial]
fn assemble_then_teardown_two_scute_chain() {
    assemble_mount_verify_teardown("two", 2);
}

#[test]
#[serial]
fn assemble_then_teardown_three_scute_chain() {
    assemble_mount_verify_teardown("three", 3);
}
