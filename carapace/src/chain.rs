//! Chain walker. Given a trusted root, resolve the top scute by
//! PARTUUID, walk backward through salt prefixes (validating each
//! scute's parameters against the RDP whitelist), and stop at the
//! base scute — identified by its `digest_size` zero-byte salt
//! prefix sentinel.
//!
//! Output order: BASE → TOP. Activation builds the read stack
//! bottom-up, so this matches the consumer's natural iteration.
//!
//! All I/O is funneled through the [`ChainResolver`] trait so the
//! decision logic (cycle / depth / sentinel / salt-too-short) is
//! unit-testable without sysfs or disk fixtures.

use crate::partition::ChainResolver;
use crate::verity::{Algorithm, ValidatedVeritySuperblock};
use crate::CarapaceError;
use std::collections::HashSet;

/// Hard cap on chain depth. Visited-set cycle detector fires for any
/// actual cycle; the limit is a catastrophic guard against a long-but-
/// not-cyclic adversarial chain.
pub(crate) const MAX_CHAIN_DEPTH: usize = 32;

/// Per-scute information the activation pipeline needs. Chain order is
/// implicit in the position of each scute within `ValidatedChain.scutes`
/// (BASE → TOP); no explicit `index` field is carried.
///
/// Cow / verity are carried as `(major, minor)` tuples — exactly what
/// `ResolvedPartition::dev_ref` returns and what activation feeds into
/// `TargetSpec::Linear` / `Snapshot` / `Verity`. We don't store the
/// full `ResolvedPartition` here because the activation path doesn't
/// need the partition's `/dev/...` path (the kernel-synchronous
/// `<maj>:<min>` form goes straight into the dm-table line). Avoids
/// cloning a `PathBuf` per scute during chain walk.
#[derive(Debug, Clone)]
pub(crate) struct ValidatedScute {
    /// `(major, minor)` of the cow partition.
    pub cow: (u32, u32),
    /// `(major, minor)` of the verity partition.
    pub verity: (u32, u32),
    /// Parsed + whitelist-validated superblock for the verity partition.
    pub superblock: ValidatedVeritySuperblock,
    /// THE root hash to feed dm-verity for this scute. CRITICAL-1: this
    /// is the *chain-computed* root, never derived from the scute's own
    /// superblock. For the top scute this equals the trusted cmdline
    /// root; for lower scutes it equals the next scute's salt prefix.
    pub root: Vec<u8>,
}

#[derive(Debug, Clone)]
pub(crate) struct ValidatedChain {
    /// Scutes in BASE → TOP order.
    pub scutes: Vec<ValidatedScute>,
}

/// Walk the chain starting at `trusted_root` (the top scute's root,
/// supplied out-of-band — kernel cmdline / signed UKI / etc.) backward
/// via salt prefixes. Each step:
///
/// 1. Resolve `current_root[..16]` (cow PARTUUID) and
///    `current_root[16..32]` (verity PARTUUID) via `resolver`. The
///    verity resolver also reads the 4 KiB superblock area in the
///    same call.
/// 2. Parse + whitelist-validate the superblock; enforce
///    chain-consistency on `algorithm` (spec §275 — every scute MUST
///    use the chain_params.algorithm declared by the base).
/// 3. If `salt_prefix == [0; digest_size]` (the base sentinel),
///    terminate. Otherwise the next walk target is the parent's
///    `digest_size`-byte salt prefix (full 32 for sha256, full 64 for
///    sha512).
///
/// `trusted_root` MUST be at least 32 bytes (PARTUUID extraction). For
/// sha512 chains it MUST additionally be ≥64 bytes (full digest);
/// validated after the top scute's superblock declares the algorithm.
pub(crate) fn walk_chain(
    trusted_root: &[u8],
    resolver: &dyn ChainResolver,
) -> Result<ValidatedChain, CarapaceError> {
    if trusted_root.len() < 32 {
        return Err(CarapaceError::Usage(format!(
            "trusted_root too short: {} bytes (need at least 32 for PARTUUID extraction)",
            trusted_root.len()
        )));
    }

    let mut scutes_top_down: Vec<ValidatedScute> = Vec::new();
    let mut visited: HashSet<[u8; 16]> = HashSet::new();
    // Variable-length root buffer kept on the stack: 32 bytes for
    // sha256 chains, 64 for sha512 (= the largest whitelisted
    // digest_size). Initialized from the operator's trusted_root;
    // `current_root_len` shrinks to `digest_size` after we read the
    // top scute's superblock. Per-iter parent updates `copy_from_slice`
    // into the same buffer — no Vec reallocation per chain step.
    let mut current_root = [0u8; 64];
    let init_len = trusted_root.len().min(current_root.len());
    current_root[..init_len].copy_from_slice(&trusted_root[..init_len]);
    let mut current_root_len = init_len;
    // Chain algorithm — None until we read the top scute, then locked.
    // Every subsequent scute MUST report this same value.
    let mut chain_algo: Option<Algorithm> = None;

    loop {
        if scutes_top_down.len() >= MAX_CHAIN_DEPTH {
            return Err(CarapaceError::ChainWalkFailed {
                depth: scutes_top_down.len(),
                reason: format!("exceeded MAX_CHAIN_DEPTH ({MAX_CHAIN_DEPTH})"),
            });
        }

        // PARTUUIDs always derive from the first 32 bytes of the root
        // regardless of digest_size (fixed convention, spec §312-313).
        let cow_partuuid: [u8; 16] = current_root[..16].try_into().unwrap();
        let verity_partuuid: [u8; 16] = current_root[16..32].try_into().unwrap();

        if !visited.insert(verity_partuuid) {
            return Err(CarapaceError::ChainWalkFailed {
                depth: scutes_top_down.len(),
                reason: format!(
                    "cycle: verity PARTUUID {} revisited",
                    crate::util::hex_lower(&verity_partuuid)
                ),
            });
        }

        let (verity, sb_bytes) = resolver
            .resolve_verity(&verity_partuuid)
            .map_err(|e| wrap_partition_not_found(e, scutes_top_down.len(), "verity"))?;
        let cow = resolver
            .resolve_cow(&cow_partuuid)
            .map_err(|e| wrap_partition_not_found(e, scutes_top_down.len(), "cow"))?;

        let superblock = ValidatedVeritySuperblock::parse(&sb_bytes, scutes_top_down.len())?;
        let digest_size = superblock.algorithm.digest_size();

        // chain_params consistency (spec §275: every scute MUST declare
        // identical chain_params).
        //
        // - data_block_size / hash_block_size / hash_type / version
        //   are RDP-locked LITERALS — every per-scute parse() must
        //   equal the RDP constant or it errors out via
        //   WhitelistViolation. Equality with a constant on every
        //   scute is, transitively, equality across scutes. No
        //   explicit per-pair check is needed.
        //
        // - algorithm is the only chain_param that is ALLOWED to vary
        //   between distinct chains (sha256 and sha512 are both
        //   whitelisted), so it's the only one we have to lock at the
        //   top scute and re-check on every subsequent scute. Done
        //   below.
        //
        // If the whitelist ever grows to allow alternative values for
        // any of the LITERAL fields, this proof breaks and explicit
        // per-pair consistency checks must be added.
        match chain_algo {
            None => {
                chain_algo = Some(superblock.algorithm);
                // Validate that the operator's trusted_root is wide
                // enough for the chain's digest_size. Catches the
                // sha512-with-32-byte-root misconfiguration before
                // dm-verity activation would silently truncate.
                if current_root_len < digest_size {
                    return Err(CarapaceError::Usage(format!(
                        "trusted_root is {} bytes but chain uses {} (digest_size {} bytes)",
                        current_root_len,
                        superblock.algorithm.name(),
                        digest_size
                    )));
                }
                current_root_len = digest_size;
            }
            Some(expected) if superblock.algorithm != expected => {
                return Err(CarapaceError::WhitelistViolation {
                    scute_index: scutes_top_down.len(),
                    field: "algorithm",
                    value: format!(
                        "{} (chain_params.algorithm = {})",
                        superblock.algorithm.name(),
                        expected.name()
                    ),
                });
            }
            Some(_) => {}
        }

        let is_base = superblock.is_base();

        scutes_top_down.push(ValidatedScute {
            cow: cow.dev_ref(),
            verity: verity.dev_ref(),
            superblock,
            // ValidatedScute.root is owned (Vec<u8>) for the activation
            // path's lifetime; copy out of the stack buffer.
            root: current_root[..current_root_len].to_vec(),
        });

        if is_base {
            break;
        }

        // Walk to parent: salt_prefix is `digest_size` bytes (the
        // parent's full root). The parser already enforces
        // salt_size >= digest_size, so this slice is in bounds.
        let salt = scutes_top_down.last().unwrap().superblock.full_salt();
        current_root[..digest_size].copy_from_slice(&salt[..digest_size]);
        current_root_len = digest_size;
    }

    // Reverse to BASE → TOP. Position in `scutes` is the chain index.
    // In-place reverse avoids the allocation that
    // `into_iter().rev().collect()` would do.
    let mut scutes = scutes_top_down;
    scutes.reverse();
    Ok(ValidatedChain { scutes })
}

fn wrap_partition_not_found(e: CarapaceError, depth: usize, role: &'static str) -> CarapaceError {
    match e {
        CarapaceError::PartitionNotFound { partuuid } => CarapaceError::ChainWalkFailed {
            depth,
            reason: format!("{role} partition not found: PARTUUID {partuuid}"),
        },
        other => other,
    }
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partition::ResolvedPartition;
    use crate::verity::superblock::{VERITY_SIGNATURE, VERITY_SUPERBLOCK_SIZE};
    use std::collections::HashMap;
    use std::path::PathBuf;

    /// Build a 4 KiB block whose first 512 bytes are a synthetic
    /// verity superblock, the rest zero-padded. `salt_prefix` is
    /// copied into salt bytes 0..len; `salt_size` is set accordingly.
    /// Defaults to sha256; use [`synth_sb_alg`] to test sha512.
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
        // salt at 88..(88+salt_size)
        let n = (salt_size as usize).min(salt_prefix.len());
        buf[88..88 + n].copy_from_slice(&salt_prefix[..n]);
        buf
    }

    /// Mock implementation of [`ChainResolver`] backed by an in-memory
    /// HashMap keyed by raw PARTUUID. `with_verity` registers a verity
    /// partition + its synthetic superblock; `with_cow` registers a
    /// cow partition. Lookups for absent PARTUUIDs return
    /// [`CarapaceError::PartitionNotFound`].
    #[derive(Default)]
    struct MockResolver {
        cows: HashMap<[u8; 16], ResolvedPartition>,
        veritys: HashMap<[u8; 16], (ResolvedPartition, [u8; VERITY_SUPERBLOCK_SIZE])>,
    }

    impl MockResolver {
        fn part(label: &str, minor: u32) -> ResolvedPartition {
            ResolvedPartition {
                path: PathBuf::from(format!("/dev/mock-{label}")),
                major: 252,
                minor,
            }
        }
        fn with_cow(mut self, partuuid: [u8; 16]) -> Self {
            self.cows.insert(partuuid, Self::part("cow", 0));
            self
        }
        fn with_verity(mut self, partuuid: [u8; 16], sb: [u8; VERITY_SUPERBLOCK_SIZE]) -> Self {
            self.veritys.insert(partuuid, (Self::part("verity", 1), sb));
            self
        }
    }

    impl ChainResolver for MockResolver {
        fn resolve_cow(&self, p: &[u8; 16]) -> Result<&ResolvedPartition, CarapaceError> {
            self.cows
                .get(p)
                .ok_or_else(|| CarapaceError::PartitionNotFound {
                    partuuid: format!("{p:02x?}"),
                })
        }
        fn resolve_verity(
            &self,
            p: &[u8; 16],
        ) -> Result<(&ResolvedPartition, [u8; VERITY_SUPERBLOCK_SIZE]), CarapaceError> {
            self.veritys
                .get(p)
                .map(|(part, sb)| (part, *sb))
                .ok_or_else(|| CarapaceError::PartitionNotFound {
                    partuuid: format!("{p:02x?}"),
                })
        }
    }

    fn root_from(cow: u8, verity: u8) -> [u8; 32] {
        let mut r = [0u8; 32];
        r[..16].fill(cow);
        r[16..].fill(verity);
        r
    }

    #[test]
    fn walks_one_scute_base_chain() {
        // Top == base: salt_prefix is 32 zero bytes (the no-parent
        // sentinel).
        let resolver = MockResolver::default()
            .with_cow([0xAA; 16])
            .with_verity([0xBB; 16], synth_sb(32, &[0u8; 32]));
        let chain = walk_chain(&root_from(0xAA, 0xBB), &resolver).unwrap();
        assert_eq!(chain.scutes.len(), 1);
        assert!(chain.scutes[0].superblock.is_base());
    }

    #[test]
    fn walks_two_scute_chain_in_base_to_top_order() {
        // Top scute's salt[..32] = base's (cow_partuuid || verity_partuuid).
        let mut top_salt = [0u8; 32];
        top_salt[..16].copy_from_slice(&[0xAA; 16]);
        top_salt[16..32].copy_from_slice(&[0xBB; 16]);
        let resolver = MockResolver::default()
            .with_cow([0xAA; 16])
            .with_verity([0xBB; 16], synth_sb(32, &[0u8; 32]))
            .with_cow([0xCC; 16])
            .with_verity([0xDD; 16], synth_sb(32, &top_salt));
        let chain = walk_chain(&root_from(0xCC, 0xDD), &resolver).unwrap();
        assert_eq!(chain.scutes.len(), 2);
        assert!(chain.scutes[0].superblock.is_base(), "scutes[0] is base");
        assert!(
            !chain.scutes[1].superblock.is_base(),
            "scutes[1] is non-base"
        );
        // CRITICAL-1 lock: top scute's root equals the trusted root.
        assert_eq!(chain.scutes[1].root, root_from(0xCC, 0xDD).to_vec());
    }

    #[test]
    fn rejects_chain_with_missing_verity_partition() {
        // Cow exists, verity does not.
        let resolver = MockResolver::default().with_cow([0xAA; 16]);
        let r = walk_chain(&root_from(0xAA, 0xBB), &resolver);
        match r {
            Err(CarapaceError::ChainWalkFailed { reason, .. }) => {
                assert!(
                    reason.contains("verity partition not found"),
                    "got: {reason}"
                );
            }
            other => panic!("expected ChainWalkFailed, got {other:?}"),
        }
    }

    #[test]
    fn rejects_chain_with_missing_cow_partition() {
        // Verity exists, cow does not.
        let resolver = MockResolver::default().with_verity([0xBB; 16], synth_sb(32, &[0u8; 32]));
        let r = walk_chain(&root_from(0xAA, 0xBB), &resolver);
        match r {
            Err(CarapaceError::ChainWalkFailed { reason, .. }) => {
                assert!(reason.contains("cow partition not found"), "got: {reason}");
            }
            other => panic!("expected ChainWalkFailed, got {other:?}"),
        }
    }

    #[test]
    fn rejects_cycle() {
        // Self-loop: a non-base scute whose salt[..32] points at itself.
        let mut self_salt = [0u8; 32];
        self_salt[..16].copy_from_slice(&[0xAA; 16]);
        self_salt[16..32].copy_from_slice(&[0xBB; 16]);
        let resolver = MockResolver::default()
            .with_cow([0xAA; 16])
            .with_verity([0xBB; 16], synth_sb(32, &self_salt));
        let r = walk_chain(&root_from(0xAA, 0xBB), &resolver);
        match r {
            Err(CarapaceError::ChainWalkFailed { reason, .. }) => {
                assert!(reason.contains("cycle"), "got: {reason}");
            }
            other => panic!("expected cycle ChainWalkFailed, got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_base_with_short_salt() {
        // A scute with non-zero salt prefix (so not base) but
        // salt_size < 32 (so we can't extract the parent PARTUUIDs).
        // The parser's own salt-size whitelist now requires
        // salt_size >= digest_size, so this hits the parser first;
        // either way it's a clean validation failure.
        let resolver = MockResolver::default()
            .with_cow([0xAA; 16])
            .with_verity([0xBB; 16], synth_sb(16, &[0xFF; 16]));
        let r = walk_chain(&root_from(0xAA, 0xBB), &resolver);
        assert!(
            matches!(
                r,
                Err(CarapaceError::WhitelistViolation {
                    field: "salt_size",
                    ..
                }) | Err(CarapaceError::ChainWalkFailed { .. })
            ),
            "got: {r:?}"
        );
    }

    #[test]
    fn trusted_root_too_short_is_usage_error() {
        let resolver = MockResolver::default();
        assert!(matches!(
            walk_chain(&[0u8; 16], &resolver),
            Err(CarapaceError::Usage(_))
        ));
    }

    #[test]
    fn walks_one_scute_sha512_base_chain() {
        // sha512 base: salt prefix is 64 zero bytes (sentinel), then
        // optional builder suffix. Trusted root must be 64 bytes.
        let resolver = MockResolver::default()
            .with_cow([0xAA; 16])
            .with_verity([0xBB; 16], synth_sb_alg("sha512", 64, &[0u8; 64]));

        let mut trusted_root = [0u8; 64];
        trusted_root[..16].fill(0xAA);
        trusted_root[16..32].fill(0xBB);
        // 32..64 holds the upper half of the digest — for the top
        // scute these bytes are baked into trusted_root by the operator.
        for (i, b) in trusted_root[32..].iter_mut().enumerate() {
            *b = 0x40 + i as u8;
        }

        let chain = walk_chain(&trusted_root, &resolver).unwrap();
        assert_eq!(chain.scutes.len(), 1);
        assert_eq!(
            chain.scutes[0].root.len(),
            64,
            "sha512 chain root must be 64 bytes (full digest), not 32"
        );
        assert_eq!(chain.scutes[0].root, trusted_root.to_vec());
    }

    #[test]
    fn rejects_sha512_chain_with_32_byte_trusted_root() {
        // Operator misconfiguration: chain advertises sha512 but the
        // trusted_root supplied is only 32 bytes. Walker should refuse
        // before dm-verity activation would silently truncate.
        let resolver = MockResolver::default()
            .with_cow([0xAA; 16])
            .with_verity([0xBB; 16], synth_sb_alg("sha512", 64, &[0u8; 64]));
        let r = walk_chain(&root_from(0xAA, 0xBB), &resolver);
        match r {
            Err(CarapaceError::Usage(msg)) => {
                assert!(
                    msg.contains("sha512") && msg.contains("64"),
                    "expected Usage citing sha512 + 64; got {msg}"
                );
            }
            other => panic!("expected Usage error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_chain_exceeding_max_depth() {
        // Non-cyclic chain longer than MAX_CHAIN_DEPTH. Each scute's
        // salt[..32] points to a distinct deeper scute — no cycle, no
        // base sentinel reached within the limit. The walker should
        // surface the depth-limit guard, not loop forever.
        let mut resolver = MockResolver::default();
        for i in 0..MAX_CHAIN_DEPTH as u8 {
            let cow = [i; 16];
            let verity = [0x80 + i; 16];
            let mut salt = [0u8; 32];
            salt[..16].fill(i + 1);
            salt[16..32].fill(0x80 + i + 1);
            resolver = resolver
                .with_cow(cow)
                .with_verity(verity, synth_sb(32, &salt));
        }
        let mut root = [0u8; 32];
        root[..16].fill(0);
        root[16..32].fill(0x80);
        match walk_chain(&root, &resolver) {
            Err(CarapaceError::ChainWalkFailed { reason, depth }) => {
                assert!(
                    reason.contains("MAX_CHAIN_DEPTH"),
                    "expected depth-limit error, got: {reason}"
                );
                assert_eq!(depth, MAX_CHAIN_DEPTH);
            }
            other => panic!("expected ChainWalkFailed depth-limit, got {other:?}"),
        }
    }

    #[test]
    fn rejects_chain_with_mixed_algorithm_across_scutes() {
        // Top scute is sha256; non-base scute is sha512. Spec §275:
        // every scute MUST equal chain_params.algorithm. Walker locks
        // the algorithm at the top scute and enforces equality.
        let mut top_salt = [0u8; 32];
        top_salt[..16].copy_from_slice(&[0xAA; 16]);
        top_salt[16..32].copy_from_slice(&[0xBB; 16]);
        let resolver = MockResolver::default()
            // Base — sha512 (mismatch). 64-byte salt with zero prefix.
            .with_cow([0xAA; 16])
            .with_verity([0xBB; 16], synth_sb_alg("sha512", 64, &[0u8; 64]))
            // Top — sha256, points at base via top_salt[..32].
            .with_cow([0xCC; 16])
            .with_verity([0xDD; 16], synth_sb_alg("sha256", 32, &top_salt));
        let r = walk_chain(&root_from(0xCC, 0xDD), &resolver);
        match r {
            Err(CarapaceError::WhitelistViolation { field, value, .. }) => {
                assert_eq!(field, "algorithm");
                assert!(
                    value.contains("sha512") && value.contains("sha256"),
                    "got {value}"
                );
            }
            other => panic!("expected WhitelistViolation, got {other:?}"),
        }
    }
}
