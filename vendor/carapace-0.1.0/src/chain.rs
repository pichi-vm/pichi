// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Chain walker. Given a trusted root, resolve the top scute by
//! PARTUUID, walk backward through salt prefixes (validating each
//! scute's parameters against the RDP whitelist), and stop at the
//! base scute — identified by its `digest_size` zero-byte salt
//! prefix sentinel.
//!
//! Output order: BASE → TOP. Activation builds the read stack
//! bottom-up, so this matches the consumer's natural iteration.
//!
//! Partition opening is delegated to the [`ChainResolver`] trait so the
//! decision logic (cycle / sentinel / salt-too-short) is
//! unit-testable with temporary files, without sysfs or block devices.

use crate::partition::ChainResolver;
use crate::verity::superblock::VERITY_SUPERBLOCK_SIZE;
use crate::verity::{Algorithm, ValidatedVeritySuperblock};
use crate::Scute;
use std::collections::HashSet;
use std::io;
use std::os::unix::fs::FileExt;

/// Walk the chain starting at `trusted_root` backward via salt prefixes,
/// returning an ordered list of open scutes in BASE → TOP
/// order.
///
/// This is the PARTUUID-discovery pass used by `FromGpt::from_gpt`. It
/// retains the files inspected during discovery; `assemble` later rereads and revalidates
/// every superblock independently.
pub(crate) fn walk_chain(
    trusted_root: &[u8],
    resolver: &dyn ChainResolver,
) -> io::Result<Vec<Scute>> {
    if trusted_root.len() < 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "trusted_root too short: {} bytes (need at least 32 for PARTUUID extraction)",
                trusted_root.len()
            ),
        ));
    }

    let mut scutes_top_down = Vec::new();
    let mut visited: HashSet<[u8; 16]> = HashSet::new();
    let mut current_root = [0u8; 64];
    let init_len = trusted_root.len().min(current_root.len());
    current_root[..init_len].copy_from_slice(&trusted_root[..init_len]);
    let current_root_len = trusted_root.len();
    let mut chain_algo: Option<Algorithm> = None;

    loop {
        let data_partuuid: [u8; 16] = current_root[..16].try_into().unwrap();
        let hash_partuuid: [u8; 16] = current_root[16..32].try_into().unwrap();

        if !visited.insert(hash_partuuid) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "chain walk failed: cycle: hash PARTUUID {} revisited at depth {}",
                    crate::util::hex_lower(&hash_partuuid),
                    scutes_top_down.len()
                ),
            ));
        }

        let hash = resolver
            .resolve_hash(&hash_partuuid)
            .map_err(|e| wrap_partition_not_found(e, scutes_top_down.len(), "hash"))?;
        let data = resolver
            .resolve_data(&data_partuuid)
            .map_err(|e| wrap_partition_not_found(e, scutes_top_down.len(), "data"))?;

        let mut sb_bytes = [0; VERITY_SUPERBLOCK_SIZE];
        hash.read_exact_at(&mut sb_bytes, 0)?;
        let superblock = ValidatedVeritySuperblock::parse(&sb_bytes, scutes_top_down.len())?;
        let digest_size = superblock.algorithm.digest_size();

        match chain_algo {
            None => {
                chain_algo = Some(superblock.algorithm);
                if current_root_len != digest_size {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "trusted_root is {} bytes but chain uses {} (digest_size {} bytes)",
                            current_root_len,
                            superblock.algorithm.name(),
                            digest_size
                        ),
                    ));
                }
                // current_root_len validated; lock is implicit (only first scute checked).
            }
            Some(expected) if superblock.algorithm != expected => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "chain walk failed: algorithm mismatch at depth {}: \
                         scute has {} but chain_params.algorithm = {}",
                        scutes_top_down.len(),
                        superblock.algorithm.name(),
                        expected.name()
                    ),
                ));
            }
            Some(_) => {}
        }

        let is_base = superblock.is_base();
        scutes_top_down.push(Scute { data, hash });

        if is_base {
            break;
        }

        let salt = superblock.full_salt();
        current_root[..digest_size].copy_from_slice(&salt[..digest_size]);
    }

    let mut scutes = scutes_top_down;
    scutes.reverse();
    Ok(scutes)
}

/// Re-wrap a `NotFound` from partition resolution into `InvalidData` with
/// a chain-walk context message.
fn wrap_partition_not_found(e: io::Error, depth: usize, role: &'static str) -> io::Error {
    if e.kind() == io::ErrorKind::NotFound {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "chain walk failed at depth {depth}: {role} partition not found: {}",
                e
            ),
        )
    } else {
        e
    }
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verity::superblock::{VERITY_SIGNATURE, VERITY_SUPERBLOCK_SIZE};
    use std::collections::HashMap;
    use std::fs::File;

    fn synth_sb(salt_size: u16, salt_prefix: &[u8]) -> [u8; VERITY_SUPERBLOCK_SIZE] {
        synth_sb_alg("sha256", salt_size, salt_prefix)
    }

    fn synth_sb_alg(alg: &str, salt_size: u16, salt_prefix: &[u8]) -> [u8; VERITY_SUPERBLOCK_SIZE] {
        let mut buf = [0u8; VERITY_SUPERBLOCK_SIZE];
        buf[..8].copy_from_slice(&VERITY_SIGNATURE);
        buf[8..12].copy_from_slice(&1u32.to_le_bytes()); // version
        buf[12..16].copy_from_slice(&1u32.to_le_bytes()); // hash_type
        let alg_bytes = alg.as_bytes();
        buf[32..32 + alg_bytes.len()].copy_from_slice(alg_bytes);
        buf[64..68].copy_from_slice(&4096u32.to_le_bytes()); // data_block_size
        buf[68..72].copy_from_slice(&4096u32.to_le_bytes()); // hash_block_size
        buf[72..80].copy_from_slice(&1u64.to_le_bytes()); // data_blocks
        buf[80..82].copy_from_slice(&salt_size.to_le_bytes());
        let n = (salt_size as usize).min(salt_prefix.len());
        buf[88..88 + n].copy_from_slice(&salt_prefix[..n]);
        buf
    }

    #[derive(Default)]
    struct MockResolver {
        datas: HashMap<[u8; 16], File>,
        hashes: HashMap<[u8; 16], File>,
    }

    impl MockResolver {
        fn with_data(mut self, partuuid: [u8; 16]) -> Self {
            self.datas.insert(partuuid, tempfile::tempfile().unwrap());
            self
        }
        fn with_hash(mut self, partuuid: [u8; 16], sb: &[u8; VERITY_SUPERBLOCK_SIZE]) -> Self {
            let file = tempfile::tempfile().unwrap();
            file.write_all_at(sb, 0).unwrap();
            self.hashes.insert(partuuid, file);
            self
        }
    }

    impl ChainResolver for MockResolver {
        fn resolve_data(&self, p: &[u8; 16]) -> io::Result<File> {
            self.datas
                .get(p)
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?
                .try_clone()
        }
        fn resolve_hash(&self, p: &[u8; 16]) -> io::Result<File> {
            self.hashes
                .get(p)
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?
                .try_clone()
        }
    }

    fn root_from(data: u8, hash: u8) -> [u8; 32] {
        let mut r = [0u8; 32];
        r[..16].fill(data);
        r[16..].fill(hash);
        r
    }

    #[test]
    fn discovery_retains_inspected_files_without_changing_offsets() {
        use std::io::{Seek, SeekFrom};
        use std::os::unix::fs::MetadataExt;

        let mut resolver = MockResolver::default()
            .with_data([0xAA; 16])
            .with_hash([0xBB; 16], &synth_sb(32, &[0; 32]));
        resolver
            .hashes
            .get_mut(&[0xBB; 16])
            .unwrap()
            .seek(SeekFrom::Start(71))
            .unwrap();
        let hash_inode = resolver.hashes[&[0xBB; 16]].metadata().unwrap().ino();
        let mut scutes = walk_chain(&root_from(0xAA, 0xBB), &resolver).unwrap();
        drop(resolver);
        assert_eq!(scutes[0].hash.metadata().unwrap().ino(), hash_inode);
        assert_eq!(scutes[0].hash.stream_position().unwrap(), 71);
        let mut bytes = [0; VERITY_SUPERBLOCK_SIZE];
        scutes[0].hash.read_exact_at(&mut bytes, 0).unwrap();
        assert_eq!(bytes, synth_sb(32, &[0; 32]));
        // Discovery's successful validation must not be reused by assembly.
        scutes[0].hash.write_all_at(b"badmagic", 0).unwrap();
        let chain = crate::Chain {
            root: root_from(0xAA, 0xBB).into(),
            scutes,
        };
        assert_eq!(
            crate::Assemble::assemble(&chain).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn walks_one_scute_base_chain() {
        let resolver = MockResolver::default()
            .with_data([0xAA; 16])
            .with_hash([0xBB; 16], &synth_sb(32, &[0u8; 32]));
        let scutes = walk_chain(&root_from(0xAA, 0xBB), &resolver).unwrap();
        assert_eq!(scutes.len(), 1);
    }

    #[test]
    fn walks_two_scute_chain_in_base_to_top_order() {
        let mut top_salt = [0u8; 32];
        top_salt[..16].copy_from_slice(&[0xAA; 16]);
        top_salt[16..32].copy_from_slice(&[0xBB; 16]);
        let resolver = MockResolver::default()
            .with_data([0xAA; 16])
            .with_hash([0xBB; 16], &synth_sb(32, &[0u8; 32]))
            .with_data([0xCC; 16])
            .with_hash([0xDD; 16], &synth_sb(32, &top_salt));
        let scutes = walk_chain(&root_from(0xCC, 0xDD), &resolver).unwrap();
        assert_eq!(scutes.len(), 2);
        // BASE is first (the one with zero-prefix salt).
        let mut salt = [1; 32];
        scutes[0].hash.read_exact_at(&mut salt, 88).unwrap();
        assert_eq!(salt, [0; 32]);
        scutes[1].hash.read_exact_at(&mut salt, 88).unwrap();
        assert_eq!(salt, top_salt);
    }

    #[test]
    fn rejects_chain_with_missing_hash_partition() {
        let resolver = MockResolver::default().with_data([0xAA; 16]);
        let r = walk_chain(&root_from(0xAA, 0xBB), &resolver);
        match r {
            Err(e) => {
                assert_eq!(e.kind(), io::ErrorKind::InvalidData, "got: {e}");
                assert!(
                    e.to_string().contains("hash partition not found"),
                    "got: {e}"
                );
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn rejects_chain_with_missing_data_partition() {
        let resolver = MockResolver::default().with_hash([0xBB; 16], &synth_sb(32, &[0u8; 32]));
        let r = walk_chain(&root_from(0xAA, 0xBB), &resolver);
        match r {
            Err(e) => {
                assert_eq!(e.kind(), io::ErrorKind::InvalidData, "got: {e}");
                assert!(
                    e.to_string().contains("data partition not found"),
                    "got: {e}"
                );
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn rejects_cycle() {
        let mut self_salt = [0u8; 32];
        self_salt[..16].copy_from_slice(&[0xAA; 16]);
        self_salt[16..32].copy_from_slice(&[0xBB; 16]);
        let resolver = MockResolver::default()
            .with_data([0xAA; 16])
            .with_hash([0xBB; 16], &synth_sb(32, &self_salt));
        let r = walk_chain(&root_from(0xAA, 0xBB), &resolver);
        match r {
            Err(e) => {
                assert_eq!(e.kind(), io::ErrorKind::InvalidData, "got: {e}");
                assert!(e.to_string().contains("cycle"), "got: {e}");
            }
            other => panic!("expected cycle error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_base_with_short_salt() {
        let resolver = MockResolver::default()
            .with_data([0xAA; 16])
            .with_hash([0xBB; 16], &synth_sb(16, &[0xFF; 16]));
        let r = walk_chain(&root_from(0xAA, 0xBB), &resolver);
        assert!(
            matches!(r, Err(ref e) if e.kind() == io::ErrorKind::InvalidData),
            "got: {r:?}"
        );
    }

    #[test]
    fn trusted_root_too_short_is_invalid_input_error() {
        let resolver = MockResolver::default();
        let e = walk_chain(&[0u8; 16], &resolver).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn walks_one_scute_sha512_base_chain() {
        let resolver = MockResolver::default()
            .with_data([0xAA; 16])
            .with_hash([0xBB; 16], &synth_sb_alg("sha512", 64, &[0u8; 64]));

        let mut trusted_root = [0u8; 64];
        trusted_root[..16].fill(0xAA);
        trusted_root[16..32].fill(0xBB);
        for (i, b) in trusted_root[32..].iter_mut().enumerate() {
            *b = 0x40 + i as u8;
        }

        let scutes = walk_chain(&trusted_root, &resolver).unwrap();
        assert_eq!(scutes.len(), 1);
    }

    #[test]
    fn rejects_sha512_chain_with_32_byte_trusted_root() {
        let resolver = MockResolver::default()
            .with_data([0xAA; 16])
            .with_hash([0xBB; 16], &synth_sb_alg("sha512", 64, &[0u8; 64]));
        let r = walk_chain(&root_from(0xAA, 0xBB), &resolver);
        match r {
            Err(e) => {
                assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "got: {e}");
                assert!(
                    e.to_string().contains("sha512") && e.to_string().contains("64"),
                    "expected message citing sha512 + 64; got {e}"
                );
            }
            other => panic!("expected InvalidInput error, got {other:?}"),
        }
    }

    #[test]
    fn walks_chains_longer_than_32_scutes_and_rejects_cycles() {
        let mut resolver = MockResolver::default();
        for i in 0..64u8 {
            let data = [i; 16];
            let hash = [0x80 + i; 16];
            let mut salt = [0u8; 32];
            if i < 63 {
                salt[..16].fill(i + 1);
                salt[16..32].fill(0x80 + i + 1);
            }
            resolver = resolver
                .with_data(data)
                .with_hash(hash, &synth_sb(32, &salt));
        }
        let root = root_from(0, 0x80);
        let scutes = walk_chain(&root, &resolver).unwrap();
        assert_eq!(scutes.len(), 64);
        for (index, scute) in scutes.iter().enumerate() {
            let mut salt = [0u8; 32];
            scute.hash.read_exact_at(&mut salt, 88).unwrap();
            let expected = if index == 0 {
                [0; 32]
            } else {
                root_from((64 - index) as u8, (0xc0 - index) as u8)
            };
            assert_eq!(salt, expected);
        }

        // Turn the base into a link back to the top after the long walk.
        resolver.hashes[&[0xbf; 16]]
            .write_all_at(&root, 88)
            .unwrap();
        let error = walk_chain(&root, &resolver).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("cycle"), "{error}");
    }

    #[test]
    fn rejects_chain_with_mixed_algorithm_across_scutes() {
        let mut top_salt = [0u8; 32];
        top_salt[..16].copy_from_slice(&[0xAA; 16]);
        top_salt[16..32].copy_from_slice(&[0xBB; 16]);
        let resolver = MockResolver::default()
            // Base — sha512 (mismatch). 64-byte salt with zero prefix.
            .with_data([0xAA; 16])
            .with_hash([0xBB; 16], &synth_sb_alg("sha512", 64, &[0u8; 64]))
            // Top — sha256, points at base via top_salt[..32].
            .with_data([0xCC; 16])
            .with_hash([0xDD; 16], &synth_sb_alg("sha256", 32, &top_salt));
        let r = walk_chain(&root_from(0xCC, 0xDD), &resolver);
        match r {
            Err(e) => {
                assert_eq!(e.kind(), io::ErrorKind::InvalidData, "got: {e}");
                assert!(
                    e.to_string().contains("sha512") && e.to_string().contains("sha256"),
                    "got {e}"
                );
            }
            other => panic!("expected algorithm mismatch error, got {other:?}"),
        }
    }
    #[test]
    fn rejects_overlong_trusted_roots() {
        let resolver = MockResolver::default()
            .with_data([0xAA; 16])
            .with_hash([0xBB; 16], &synth_sb(32, &[0; 32]));
        for length in [33, 64, 65] {
            let mut root = root_from(0xAA, 0xBB).to_vec();
            root.resize(length, 0);
            assert_eq!(
                walk_chain(&root, &resolver).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }
}
