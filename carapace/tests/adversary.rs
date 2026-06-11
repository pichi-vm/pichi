//! Adversary scenarios that do NOT fall out implicitly from dm-verity
//! arithmetic and therefore need explicit witnesses:
//!
//!   Parser whitelist:
//!     1. Whitelist enforcement: base scute with a non-whitelisted
//!        algorithm.
//!     2. Parameter consistency: scute 2 with off-RDP block size while
//!        the base is RDP-compliant.
//!
//!   Walker control flow:
//!     3. Wrong trusted root: chain walk fails to resolve the top.
//!     4. Forge zero-prefix sentinel on a non-base scute → walker
//!        terminates the chain prematurely; dm-verity then catches the
//!        salt-mismatch at activation. Documents that "is_base() is
//!        not the security boundary; it's just a structural hint."
//!     5. Missing parent partition: walker reaches a non-base scute,
//!        tries to look up its parent, lookup fails → ChainWalkFailed.
//!        Documents that the walker fails closed when the chain points
//!        at a partition the kernel doesn't expose.
//!
//!   Implicit / not tested separately:
//!     - Extra spurious partitions (would be C7 in the SPEC adversary
//!       table). The walker is purely chain-driven via salt → PARTUUID
//!       lookup; it has no enumeration loop. Unrelated partitions are
//!       structurally invisible. Verified by inspection of
//!       chain::walk_chain — there is no `for partition in all` loop.
//!     - Cow byte mutation, verity tree mutation, partition swap,
//!       PARTUUID forgery: all caught by dm-verity activation (root
//!       hash mismatch on first read).

#![cfg(target_os = "linux")]

mod common;

use common::{build_chain, cleanup_dm, decode_hex, hex_lower, AttachedImage};
use serial_test::serial;
use std::path::PathBuf;
use std::process::Command;
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

fn binary() -> PathBuf {
    if let Some(p) = std::env::var_os("CARGO_BIN_EXE_carapace") {
        return PathBuf::from(p);
    }
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("target/debug/carapace");
    p
}

fn attach_fail(name: &str, root_hex: &str) -> (i32, String) {
    let out = Command::new(binary())
        .args(["attach", "--name", name, "--root", root_hex])
        .output()
        .expect("spawn carapace attach");
    let code = out.status.code().unwrap_or(-1);
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    (code, stderr)
}

#[test]
#[serial]
fn rejects_chain_with_blocklisted_algorithm_in_base_superblock() {
    if !root() {
        eprintln!("skip: requires root");
        return;
    }
    let dir = TempDir::new().unwrap();
    let img = dir.path().join("blocklist.img");
    let chain = build_chain(&img, 1);
    let name = format!("carapace-adv-blocklist-{}", std::process::id());
    cleanup_dm(&name);

    // Mutate the base verity superblock's algorithm field from
    // "sha256" to "md5" by overwriting bytes 32..64 of the superblock.
    // The base superblock lives at the start of scute 0's verity
    // partition. We don't know exactly where on disk that is without
    // reparsing the GPT — find it by scanning for "verity\0\0sha256"
    // and replacing in place.
    let mut buf = std::fs::read(&chain.image).unwrap();
    let needle = b"verity\0\0";
    let pos = buf
        .windows(needle.len())
        .position(|w| w == needle)
        .expect("verity signature in image");
    // signature is at pos..pos+8; algorithm is at pos+32..pos+64
    let alg_off = pos + 32;
    for b in &mut buf[alg_off..alg_off + 32] {
        *b = 0;
    }
    buf[alg_off..alg_off + 3].copy_from_slice(b"md5");
    std::fs::write(&chain.image, buf).unwrap();

    // losetup AFTER mutation so the kernel/sysfs see the mutated bytes.
    let _attached = AttachedImage::attach(&chain.image);
    let (code, stderr) = attach_fail(&name, &chain.trusted_root_hex);
    // Exit 2 = chain rejection (WhitelistViolation on algorithm).
    assert_eq!(
        code, 2,
        "expected chain-rejection exit code 2; stderr: {stderr}"
    );
    assert!(
        stderr.contains("algorithm") || stderr.contains("md5") || stderr.contains("whitelist"),
        "stderr must mention algorithm / md5 / whitelist; got: <<<{stderr}>>>"
    );
    cleanup_dm(&name);
}

#[test]
#[serial]
fn rejects_chain_with_off_rdp_block_size_in_non_base_scute() {
    if !root() {
        eprintln!("skip: requires root");
        return;
    }
    let dir = TempDir::new().unwrap();
    let img = dir.path().join("rdp.img");
    let chain = build_chain(&img, 2);
    let name = format!("carapace-adv-rdp-{}", std::process::id());
    cleanup_dm(&name);

    // Mutate scute 1's data_block_size from 4096 to 8192. Find the
    // SECOND verity superblock (skip the base).
    let mut buf = std::fs::read(&chain.image).unwrap();
    let needle = b"verity\0\0";
    let positions: Vec<usize> = buf
        .windows(needle.len())
        .enumerate()
        .filter_map(|(i, w)| if w == needle { Some(i) } else { None })
        .collect();
    assert!(positions.len() >= 2, "two verity superblocks expected");
    // data_block_size lives at offset 64..68 in the superblock.
    let dbs_off = positions[1] + 64;
    buf[dbs_off..dbs_off + 4].copy_from_slice(&8192u32.to_le_bytes());
    std::fs::write(&chain.image, buf).unwrap();

    let _attached = AttachedImage::attach(&chain.image);
    let (code, stderr) = attach_fail(&name, &chain.trusted_root_hex);
    // Exit 2 = chain rejection (WhitelistViolation on data_block_size).
    assert_eq!(
        code, 2,
        "expected chain-rejection exit code 2; stderr: {stderr}"
    );
    assert!(
        stderr.contains("data_block_size")
            || stderr.contains("whitelist")
            || stderr.contains("8192"),
        "stderr must mention data_block_size / whitelist / 8192; got: <<<{stderr}>>>"
    );
    cleanup_dm(&name);
}

#[test]
#[serial]
fn rejects_attach_with_wrong_trusted_root() {
    if !root() {
        eprintln!("skip: requires root");
        return;
    }
    let dir = TempDir::new().unwrap();
    let img = dir.path().join("wrongroot.img");
    let chain = build_chain(&img, 2);
    let name = format!("carapace-adv-wrongroot-{}", std::process::id());
    cleanup_dm(&name);

    // Flip a single byte in the otherwise-valid trusted root. The
    // walker tries to look up a verity partition whose PARTUUID is
    // that mutated trusted_root[16..32], which doesn't exist.
    let mut bytes = decode_hex(&chain.trusted_root_hex).unwrap();
    bytes[20] ^= 0x55;
    let bad = hex_lower(&bytes);

    let _attached = AttachedImage::attach(&chain.image);
    let (code, stderr) = attach_fail(&name, &bad);
    // Exit 2 = chain rejection (ChainWalkFailed: PARTUUID not present).
    assert_eq!(
        code, 2,
        "expected chain-rejection exit code 2; stderr: {stderr}"
    );
    assert!(
        stderr.contains("partition not found")
            || stderr.contains("chain walk")
            || stderr.contains("trusted root"),
        "stderr must mention chain walk failure; got: <<<{stderr}>>>"
    );
    cleanup_dm(&name);
}

/// G4: forge the zero-prefix base sentinel on the TOP scute of a
/// 2-scute chain. The walker inspects salt[..digest_size] and treats
/// any all-zero prefix as base; this attack tries to truncate the
/// chain by making scute 1 LOOK like a base.
///
/// What MUST happen: dm-verity activation with the mutated salt
/// produces a hash mismatch (salt is part of the verity hash
/// computation), so DM_TABLE_LOAD fails and attach exits non-zero.
/// The is_base() check itself is not the security boundary — the
/// cryptographic chain is.
#[test]
#[serial]
fn rejects_chain_with_forged_zero_prefix_sentinel() {
    if !root() {
        eprintln!("skip: requires root");
        return;
    }
    let dir = TempDir::new().unwrap();
    let img = dir.path().join("forge-sentinel.img");
    let chain = build_chain(&img, 2);
    let name = format!("carapace-adv-forgesentinel-{}", std::process::id());
    cleanup_dm(&name);

    // Find the SECOND verity superblock (top scute). Mutate its
    // salt[..32] to all zeros, posing as the base sentinel. Salt
    // begins at offset 88 in the superblock; we zero bytes 88..120.
    let mut buf = std::fs::read(&chain.image).unwrap();
    let needle = b"verity\0\0";
    let positions: Vec<usize> = buf
        .windows(needle.len())
        .enumerate()
        .filter_map(|(i, w)| if w == needle { Some(i) } else { None })
        .collect();
    assert_eq!(positions.len(), 2, "two verity superblocks expected");
    let salt_off = positions[1] + 88;
    for b in &mut buf[salt_off..salt_off + 32] {
        *b = 0;
    }
    std::fs::write(&chain.image, buf).unwrap();

    let _attached = AttachedImage::attach(&chain.image);
    let (code, stderr) = attach_fail(&name, &chain.trusted_root_hex);
    assert_ne!(code, 0, "attach must fail (dm-verity hash mismatch)");
    // Pin the failure subsystem: must surface from dm/verity, not an
    // unrelated panic. Two legitimate channels: DM_TABLE_LOAD rejection
    // ("dm ioctl"), or a kernel-side hash mismatch on the first read
    // through the just-activated dm-verity device when the chunk_size
    // header is fetched ("Input/output error" / EIO).
    assert!(
        stderr.contains("dm ioctl") || stderr.contains("verity") || stderr.contains("Input/output"),
        "stderr must mention dm/verity/IO failure; got: <<<{stderr}>>>"
    );
    cleanup_dm(&name);
}

/// C6: a non-base scute references a parent PARTUUID that does not
/// exist in the kernel's GPT view. Construct this by mutating the
/// BASE scute's salt[..32] to non-zero, fictional bytes — the walker
/// then thinks the (one-and-only) scute is non-base, tries to look
/// up its non-existent parent, and must surface ChainWalkFailed.
#[test]
#[serial]
fn rejects_chain_when_parent_partition_missing() {
    if !root() {
        eprintln!("skip: requires root");
        return;
    }
    let dir = TempDir::new().unwrap();
    let img = dir.path().join("missing-parent.img");
    let chain = build_chain(&img, 1);
    let name = format!("carapace-adv-missingparent-{}", std::process::id());
    cleanup_dm(&name);

    // Mutate the (only) verity superblock's salt[..32] from zeros to
    // a fictional value the walker will then try to resolve.
    let mut buf = std::fs::read(&chain.image).unwrap();
    let needle = b"verity\0\0";
    let pos = buf.windows(needle.len()).position(|w| w == needle).unwrap();
    let salt_off = pos + 88;
    for (i, b) in buf[salt_off..salt_off + 32].iter_mut().enumerate() {
        *b = (i as u8).wrapping_add(0x37); // arbitrary non-zero pattern
    }
    std::fs::write(&chain.image, buf).unwrap();

    let _attached = AttachedImage::attach(&chain.image);
    let (code, stderr) = attach_fail(&name, &chain.trusted_root_hex);
    // Could surface as walker "not found" (exit 2 — chain rejection)
    // OR as dm-verity hash mismatch (exit 1 — DmIoctl) depending on
    // order. Both are correct failure modes; assert non-zero.
    assert_ne!(code, 0, "attach must fail");
    assert!(
        stderr.contains("partition not found")
            || stderr.contains("chain walk")
            || stderr.contains("dm ioctl"),
        "stderr must mention a structural failure; got: <<<{stderr}>>>"
    );
    cleanup_dm(&name);
}
