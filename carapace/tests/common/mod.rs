//! Test fixture wrapper.
//!
//! The fixture is built by `tests/fixtures/build_carapace.sh` — a pure
//! shell + python pipeline using `sgdisk`, `veritysetup`, `dmsetup`,
//! and `mkfs.ext4`. NO production-crate code is reused on the producer
//! side, which is what makes the assembler reading the fixture a true
//! differential test against cryptsetup's tooling and the kernel's own
//! dm-snapshot persistent-store implementation.
//!
//! This module is the thin Rust shim that drives the script (parses
//! stdout for trusted root hex), plus loop-attach and mount helpers.
//! Carapace itself no longer auto-loops images — `AttachedImage`
//! handles `losetup --partscan` so the kernel populates
//! `/sys/class/block/loopXpY` for sysfs-based partition discovery.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// Output of [`build_chain`].
pub struct BuiltChain {
    /// Path to the GPT image file.
    pub image: PathBuf,
    /// Trusted root hash, lowercase hex (the top scute's root).
    pub trusted_root_hex: String,
    /// Number of scutes (chain depth).
    pub scutes: usize,
}

/// Loop-attached fixture image. Drop runs `losetup -d`. Carapace itself
/// no longer auto-loops anything — the operator (here, the test
/// harness) owns the loop lifecycle so the kernel populates
/// `/sys/class/block/loopXpY` for the partition discovery path.
pub struct AttachedImage {
    pub loop_path: PathBuf,
}

impl AttachedImage {
    /// `losetup --partscan` the image. Partscan is synchronous: by the
    /// time `losetup` returns, the kernel has parsed the GPT and
    /// populated `/sys/class/block/loopXpN/uevent` with `PARTUUID=...`.
    /// No udev round-trip required.
    pub fn attach(image: &Path) -> Self {
        let out = Command::new("losetup")
            .args(["--show", "-f", "--partscan"])
            .arg(image)
            .output()
            .expect("spawn losetup");
        if !out.status.success() {
            panic!(
                "losetup --partscan {} failed: {:?}\nstderr=<<<{}>>>",
                image.display(),
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let loop_path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
        Self { loop_path }
    }
}

impl Drop for AttachedImage {
    fn drop(&mut self) {
        // Best-effort. The dm stack above may still be holding the
        // loop; in that case losetup -d races (the kernel detaches
        // when the last reference drops). Tests that care about
        // post-detach loop state use `loop_bound_to` to poll.
        let _ = Command::new("losetup")
            .arg("-d")
            .arg(&self.loop_path)
            .status();
    }
}

impl BuiltChain {
    /// Files that should be visible at the root of the assembled
    /// filesystem, with their expected contents. Mirrors what
    /// `tests/fixtures/build_carapace.sh` writes per scute.
    ///
    /// scute_i.txt contains "scute<i> content" for every i in [0, N).
    /// scute0.txt is overwritten on every scute > 0 with
    /// "modified by scute<i>", so the surviving content is the one
    /// written by the topmost scute.
    pub fn expected_files(&self) -> Vec<(String, String)> {
        let mut files: Vec<(String, String)> = Vec::with_capacity(self.scutes);
        for i in 0..self.scutes {
            files.push((format!("scute{i}.txt"), format!("scute{i} content\n")));
        }
        if self.scutes > 1 {
            // scute 0's content gets overwritten by the topmost scute.
            files[0].1 = format!("modified by scute{}\n", self.scutes - 1);
        }
        files
    }
}

/// Build a carapace image with `n_scutes` scutes (>= 1).
///
/// Spawns `tests/fixtures/build_carapace.sh` and parses its stdout.
/// Panics on script failure (test-only, not production).
pub fn build_chain(image: &Path, n_scutes: usize) -> BuiltChain {
    assert!((1..=16).contains(&n_scutes));

    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/build_carapace.sh");
    let out = Command::new(&script)
        .arg(image)
        .arg(n_scutes.to_string())
        .output()
        .unwrap_or_else(|e| panic!("spawn {}: {e}", script.display()));
    if !out.status.success() {
        panic!(
            "fixture builder failed: status={:?}\nstderr=<<<{}>>>",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let trusted_root_hex = String::from_utf8_lossy(&out.stdout)
        .lines()
        .last()
        .unwrap_or("")
        .trim()
        .to_string();
    if trusted_root_hex.len() != 64 {
        panic!(
            "fixture builder returned {} bytes; expected 64 hex chars\nstderr=<<<{}>>>",
            trusted_root_hex.len(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    BuiltChain {
        image: image.to_path_buf(),
        trusted_root_hex,
        scutes: n_scutes,
    }
}

pub fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(s, "{b:02x}").expect("write to String never fails");
    }
    s
}

pub fn decode_hex(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return Err(format!("odd hex length: {}", s.len()));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for i in (0..s.len()).step_by(2) {
        let b = u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string())?;
        out.push(b);
    }
    Ok(out)
}

/// Poll `losetup -l` until no loop is bound to `image_path`. Returns
/// true iff a loop is still bound after `timeout`. Tests use this for
/// the post-detach assertion (LOOP_CLR_FD is acked synchronously but
/// completed asynchronously; single-shot losetup races on Ubuntu).
pub fn loop_bound_to(image: &Path, timeout: std::time::Duration) -> bool {
    let needle = image.to_string_lossy().to_string();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let bound = match Command::new("losetup").arg("-l").output() {
            Ok(o) => String::from_utf8_lossy(&o.stdout).contains(&needle),
            Err(_) => false,
        };
        if !bound {
            return false;
        }
        if std::time::Instant::now() >= deadline {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Mount the assembled mapper device read-only, run `f` against the
/// mountpoint, then unmount. Panics on mount/unmount failure.
pub fn with_mounted<F, R>(mapper: &Path, f: F) -> R
where
    F: FnOnce(&Path) -> R,
{
    let mnt = tempfile::tempdir().expect("tempdir for mount");
    let status = Command::new("mount")
        .args(["-o", "ro"])
        .arg(mapper)
        .arg(mnt.path())
        .status()
        .expect("spawn mount");
    if !status.success() {
        panic!(
            "mount {} -> {} failed: {status:?}",
            mapper.display(),
            mnt.path().display()
        );
    }
    let result = f(mnt.path());
    let umount_status = Command::new("umount")
        .arg(mnt.path())
        .status()
        .expect("spawn umount");
    if !umount_status.success() {
        // Try lazy unmount before giving up; tempdir drop will retry rm.
        let _ = Command::new("umount").args(["-l"]).arg(mnt.path()).status();
    }
    result
}

/// Best-effort dm cleanup for a top-level name (used in test
/// teardown to avoid polluting subsequent tests). Probe count must
/// match the production walker's `chain::MAX_CHAIN_DEPTH` so a test
/// using a long fixture can't leak devices the cleanup never visits.
/// Hardcoded to 32 here because integration tests can only see the
/// crate's `pub` surface and `MAX_CHAIN_DEPTH` is `pub(crate)`. Drift
/// risk is bounded: the constant rarely changes, and lib unit tests
/// would break long before the drift mattered.
const CLEANUP_PROBE_DEPTH: usize = 32;

pub fn cleanup_dm(name: &str) {
    let _ = Command::new("dmsetup")
        .args(["remove", "-f", name])
        .output();
    for i in (0..CLEANUP_PROBE_DEPTH).rev() {
        for prefix in ["s", "v"] {
            let _ = Command::new("dmsetup")
                .args(["remove", "-f", &format!("{name}-{prefix}{i}")])
                .output();
        }
    }
    let _ = Command::new("dmsetup")
        .args(["remove", "-f", &format!("{name}-z0")])
        .output();
}
