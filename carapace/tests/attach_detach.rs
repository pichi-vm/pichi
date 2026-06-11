//! End-to-end attach + detach against a real layered ext4 fixture.
//!
//! Each test:
//!   1. Builds a fixture via `common::build_chain` (shell pipeline:
//!      `sgdisk` + `veritysetup` + `dmsetup` writable-snapshot dogfood
//!      — no production-crate code is reused on the producer side).
//!   2. `losetup --partscan` the image (kernel populates
//!      `/sys/class/block/loopXpY/uevent` with PARTUUIDs synchronously).
//!   3. Runs `carapace attach --name=... --root=...` (no --storage —
//!      partition discovery is sysfs-driven).
//!   4. Mounts /dev/mapper/<name> read-only as ext4 and verifies the
//!      expected per-scute files exist with the right content (proves
//!      cross-layer composition end to end, not just byte parsing).
//!   5. Runs `carapace detach --name=...`.
//!   6. Drops `AttachedImage` → `losetup -d`. Verifies the image is no
//!      longer loop-bound.
//!
//! All tests `#[serial]` (global dm + loop namespace).

#![cfg(target_os = "linux")]

mod common;

use assert_cmd_lite::Carapace;
use common::{build_chain, cleanup_dm, loop_bound_to, with_mounted, AttachedImage};
use serial_test::serial;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;

fn root() -> bool {
    match std::fs::read_to_string("/proc/self/status") {
        Ok(s) => s.lines().any(|l| {
            l.strip_prefix("Uid:")
                .and_then(|rest| rest.split_whitespace().next())
                .map(|euid| euid == "0")
                .unwrap_or(false)
        }),
        Err(_) => false,
    }
}

fn name_for(test: &str) -> String {
    format!("carapace-test-{}-{}", test, std::process::id())
}

/// Run the full attach -> mount -> verify -> umount -> detach loop and
/// assert that every per-scute file is visible at the expected content.
fn attach_mount_verify_detach(test_label: &str, n_scutes: usize) {
    if !root() {
        eprintln!("skip: requires root");
        return;
    }
    let dir = TempDir::new().unwrap();
    let img = dir.path().join(format!("{test_label}.img"));
    let chain = build_chain(&img, n_scutes);
    let name = name_for(test_label);
    cleanup_dm(&name);

    // losetup --partscan exposes the image's GPT partitions to the
    // kernel; sysfs gets PARTUUID entries synchronously. carapace
    // attach discovers them from /sys/class/block.
    let attached = AttachedImage::attach(&chain.image);

    let mapper = Carapace::attach(&name, &chain.trusted_root_hex);
    let mapper_str = mapper.to_string_lossy();
    assert!(
        mapper_str.starts_with("/dev/dm-"),
        "attach should print the kernel-synchronous /dev/dm-<minor> path, got {mapper_str}"
    );
    assert!(
        mapper.exists(),
        "{} must exist after attach",
        mapper.display()
    );

    // Mount the assembled device read-only as ext4 and verify per-scute
    // files. This exercises the full layered composition, not just the
    // GPT/verity/snapshot byte parsing.
    with_mounted(&mapper, |mnt| {
        for (filename, expected) in chain.expected_files() {
            let path = mnt.join(&filename);
            let actual = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            assert_eq!(actual, expected, "content mismatch for {filename}");
        }
    });

    Carapace::detach(&name);
    assert!(!PathBuf::from(format!("/dev/mapper/{name}")).exists());
    drop(attached); // losetup -d
    assert!(
        !loop_bound_to(&chain.image, Duration::from_secs(2)),
        "no loop must be bound to image after detach"
    );
}

#[test]
#[serial]
fn attach_then_detach_one_scute_chain() {
    attach_mount_verify_detach("one", 1);
}

#[test]
#[serial]
fn attach_then_detach_two_scute_chain() {
    attach_mount_verify_detach("two", 2);
}

#[test]
#[serial]
fn attach_then_detach_three_scute_chain() {
    attach_mount_verify_detach("three", 3);
}

// ----- Inline command runner — no assert_cmd dep -----

mod assert_cmd_lite {
    use std::path::PathBuf;
    use std::process::Command;

    pub struct Carapace;
    impl Carapace {
        fn binary() -> PathBuf {
            // Prefer CARGO_BIN_EXE_carapace if set (cargo test injects it).
            if let Some(p) = std::env::var_os("CARGO_BIN_EXE_carapace") {
                return PathBuf::from(p);
            }
            // Fallback: assume target/debug/carapace.
            let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            p.push("target/debug/carapace");
            p
        }

        pub fn attach(name: &str, root_hex: &str) -> PathBuf {
            let out = Command::new(Self::binary())
                .args(["attach", "--name", name, "--root", root_hex])
                .output()
                .expect("spawn carapace attach");
            if !out.status.success() {
                panic!(
                    "carapace attach failed: status={:?} stderr=<<<{}>>>",
                    out.status,
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            PathBuf::from(stdout)
        }

        pub fn detach(name: &str) {
            let out = Command::new(Self::binary())
                .args(["detach", "--name", name])
                .output()
                .expect("spawn carapace detach");
            if !out.status.success() {
                panic!(
                    "carapace detach failed: status={:?} stderr=<<<{}>>>",
                    out.status,
                    String::from_utf8_lossy(&out.stderr)
                );
            }
        }
    }
}
