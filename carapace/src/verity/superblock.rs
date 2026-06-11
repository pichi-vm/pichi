//! dm-verity superblock parser.
//!
//! Parse-and-validate in one step — `RawVeritySuperblock` is private; the
//! only path from `&[u8]` to a typed view is
//! [`ValidatedVeritySuperblock::parse`], which enforces the entire RDP
//! whitelist before returning. Downstream code never sees partially-
//! validated data (PITFALLS CRITICAL-2 / CRITICAL-3 prevention).
//!
//! On-disk layout (mirrors cryptsetup `lib/verity/verity.c`, total 512 B,
//! all numeric fields little-endian):
//!
//!   signature[8] | version u32 | hash_type u32 | uuid[16] | algorithm[32]
//!   | data_block_size u32 | hash_block_size u32 | data_blocks u64
//!   | salt_size u16 | _pad1[6] | salt[256] | _pad2[168]

use crate::verity::algorithm::{self, Algorithm};
use crate::CarapaceError;
use zerocopy::little_endian::{U16, U32, U64};
use zerocopy::{FromBytes, Immutable, KnownLayout};

pub(crate) const VERITY_SUPERBLOCK_SIZE: usize = 512;
pub(crate) const VERITY_SIGNATURE: [u8; 8] = *b"verity\0\0";

/// RDP-locked v1 chain parameters. Equality with these constants is
/// both the per-scute legality check and the chain-consistency check.
const RDP_DATA_BLOCK_SIZE: u32 = 4096;
const RDP_HASH_BLOCK_SIZE: u32 = 4096;
const RDP_HASH_TYPE: u32 = 1;
const RDP_VERSION: u32 = 1;

#[derive(FromBytes, KnownLayout, Immutable, Debug, Clone, Copy)]
#[repr(C)]
struct RawVeritySuperblock {
    signature: [u8; 8],
    version: U32,
    hash_type: U32,
    uuid: [u8; 16],
    algorithm: [u8; 32],
    data_block_size: U32,
    hash_block_size: U32,
    data_blocks: U64,
    salt_size: U16,
    _pad1: [u8; 6],
    salt: [u8; 256],
    _pad2: [u8; 168],
}

const _: () = assert!(core::mem::size_of::<RawVeritySuperblock>() == VERITY_SUPERBLOCK_SIZE);

#[derive(Debug, Clone, Copy)]
pub(crate) struct ValidatedVeritySuperblock {
    pub algorithm: Algorithm,
    pub data_blocks: u64,
    salt_size: u16,
    salt_buf: [u8; 256],
}

impl ValidatedVeritySuperblock {
    /// Parse a 512-byte superblock buffer and enforce the full v1 RDP
    /// whitelist.
    ///
    /// `scute_index` is the chain position used for error attribution.
    /// Every scute carries a `digest_size`-byte salt prefix; the base
    /// scute uses `digest_size` zero bytes as its prefix (the no-parent
    /// sentinel; see [`Self::is_base`]). The parser enforces
    /// `salt_size >= digest_size` uniformly.
    pub fn parse(bytes: &[u8], scute_index: usize) -> Result<Self, CarapaceError> {
        if bytes.len() < VERITY_SUPERBLOCK_SIZE {
            return Err(CarapaceError::SuperblockInvalid {
                scute_index,
                reason: format!("need {VERITY_SUPERBLOCK_SIZE} bytes, got {}", bytes.len()),
            });
        }
        let raw = RawVeritySuperblock::ref_from_bytes(&bytes[..VERITY_SUPERBLOCK_SIZE]).map_err(
            |_| CarapaceError::SuperblockInvalid {
                scute_index,
                reason: "alignment / size".into(),
            },
        )?;

        if raw.signature != VERITY_SIGNATURE {
            return Err(CarapaceError::SuperblockInvalid {
                scute_index,
                reason: format!("signature {:?} != {:?}", raw.signature, VERITY_SIGNATURE),
            });
        }

        // -- whitelist (RDP equality on every scute) ------------------
        let version = raw.version.get();
        if version != RDP_VERSION {
            return Err(CarapaceError::WhitelistViolation {
                scute_index,
                field: "version",
                value: version.to_string(),
            });
        }
        let hash_type = raw.hash_type.get();
        if hash_type != RDP_HASH_TYPE {
            return Err(CarapaceError::WhitelistViolation {
                scute_index,
                field: "hash_type",
                value: hash_type.to_string(),
            });
        }
        let dbs = raw.data_block_size.get();
        if dbs != RDP_DATA_BLOCK_SIZE {
            return Err(CarapaceError::WhitelistViolation {
                scute_index,
                field: "data_block_size",
                value: dbs.to_string(),
            });
        }
        let hbs = raw.hash_block_size.get();
        if hbs != RDP_HASH_BLOCK_SIZE {
            return Err(CarapaceError::WhitelistViolation {
                scute_index,
                field: "hash_block_size",
                value: hbs.to_string(),
            });
        }

        // Algorithm: parse cstring then check whitelist via algorithm::parse.
        let alg_str =
            cstr_to_str(&raw.algorithm).map_err(|e| CarapaceError::SuperblockInvalid {
                scute_index,
                reason: format!("algorithm field: {e}"),
            })?;
        let algorithm =
            algorithm::parse(alg_str).map_err(|_| CarapaceError::WhitelistViolation {
                scute_index,
                field: "algorithm",
                value: alg_str.to_string(),
            })?;
        let digest_size = algorithm.digest_size();

        // Salt size: digest_size <= salt_size <= 256. Every scute
        // carries a digest_size-byte prefix (parent root, or zero
        // sentinel for the base — distinguished post-parse via
        // `is_base()`).
        let salt_size = raw.salt_size.get();
        if salt_size as usize > raw.salt.len() {
            return Err(CarapaceError::SuperblockInvalid {
                scute_index,
                reason: format!(
                    "salt_size {salt_size} > salt field length {}",
                    raw.salt.len()
                ),
            });
        }
        if (salt_size as usize) < digest_size {
            return Err(CarapaceError::WhitelistViolation {
                scute_index,
                field: "salt_size",
                value: format!("{salt_size} < digest_size {digest_size}"),
            });
        }

        let data_blocks = raw.data_blocks.get();
        if data_blocks == 0 {
            // A zero-data-block scute would map to an empty dm-verity
            // device. The spec doesn't define semantics for it and the
            // kernel rejects the resulting `0 0 verity ...` table line
            // with EINVAL — but rejecting at parse time gives a clean
            // SuperblockInvalid instead of a downstream DmIoctl failure
            // and avoids any arithmetic on a zero (length, sectors).
            return Err(CarapaceError::SuperblockInvalid {
                scute_index,
                reason: "data_blocks must be > 0".into(),
            });
        }
        // Activation computes `data_blocks * VERITY_BLOCK_SIZE_BYTES`
        // (then divides by 512 to get sectors) when sizing the
        // dm-verity table line and the top alias. Overflow is
        // theoretically reachable with adversarial data_blocks (~2^52
        // for sha256 RDP) — far above any realistic carapace, but we
        // validate at the data boundary so no downstream code has to
        // worry about it.
        if data_blocks
            .checked_mul(RDP_DATA_BLOCK_SIZE as u64)
            .is_none()
        {
            return Err(CarapaceError::SuperblockInvalid {
                scute_index,
                reason: format!("data_blocks {data_blocks} * {RDP_DATA_BLOCK_SIZE} overflows u64"),
            });
        }

        Ok(Self {
            algorithm,
            data_blocks,
            salt_size,
            salt_buf: raw.salt,
        })
    }

    /// Full salt as fed to dm-verity hashing. Length = `salt_size`.
    pub fn full_salt(&self) -> &[u8] {
        &self.salt_buf[..self.salt_size as usize]
    }

    /// True iff this scute is the chain's base — detected by the
    /// `digest_size`-byte zero-prefix sentinel in the salt
    /// (PITFALLS CRITICAL-4: prefix length is `digest_size`, never
    /// `salt_size`). The chain walker uses this as the termination
    /// signal; no out-of-band marker (GPT attribute, manifest entry,
    /// etc.) is needed.
    pub fn is_base(&self) -> bool {
        self.salt_buf[..self.algorithm.digest_size()]
            .iter()
            .all(|&b| b == 0)
    }
}

/// Read a NUL-terminated ASCII string out of a fixed-length field.
///
/// A field with no NUL byte is malformed: the producer is required by
/// the dm-verity superblock format to NUL-terminate within the
/// allotted length. Falling back to the full slice would let trailing
/// garbage past a missing terminator masquerade as the string, which
/// the algorithm whitelist would then reject by name — same outcome,
/// but the wrong-shaped error. Reject explicitly so the operator sees
/// "missing NUL" rather than "unknown algorithm `sha256<garbage>`".
fn cstr_to_str(buf: &[u8]) -> Result<&str, &'static str> {
    let end = buf
        .iter()
        .position(|&b| b == 0)
        .ok_or("missing NUL terminator")?;
    std::str::from_utf8(&buf[..end]).map_err(|_| "non-UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a 512-byte superblock with the given fields. Useful for
    /// unit tests; production parsing reads from disk.
    fn synth(
        algorithm: &str,
        version: u32,
        hash_type: u32,
        data_block_size: u32,
        hash_block_size: u32,
        salt_size: u16,
        salt_prefix: &[u8],
    ) -> [u8; 512] {
        let mut buf = [0u8; 512];
        buf[..8].copy_from_slice(&VERITY_SIGNATURE);
        buf[8..12].copy_from_slice(&version.to_le_bytes());
        buf[12..16].copy_from_slice(&hash_type.to_le_bytes());
        // uuid 16..32: zeros
        let alg_bytes = algorithm.as_bytes();
        buf[32..32 + alg_bytes.len()].copy_from_slice(alg_bytes);
        buf[64..68].copy_from_slice(&data_block_size.to_le_bytes());
        buf[68..72].copy_from_slice(&hash_block_size.to_le_bytes());
        buf[72..80].copy_from_slice(&1u64.to_le_bytes()); // data_blocks
        buf[80..82].copy_from_slice(&salt_size.to_le_bytes());
        // _pad1 82..88
        buf[88..88 + salt_prefix.len()].copy_from_slice(salt_prefix);
        buf
    }

    #[test]
    fn parses_canonical_sha256_non_base() {
        let prefix = [0xAA; 32];
        let sb = synth("sha256", 1, 1, 4096, 4096, 32, &prefix);
        let v = ValidatedVeritySuperblock::parse(&sb, 0).unwrap();
        assert_eq!(v.algorithm, Algorithm::Sha256);
        assert_eq!(v.full_salt().len(), 32);
        assert_eq!(&v.full_salt()[..32], &prefix[..]);
        assert!(!v.is_base(), "non-zero prefix means non-base");
    }

    #[test]
    fn parses_sha256_base_with_zero_prefix_and_random_suffix() {
        // 32 zeros (prefix) + 32 random (suffix) = 64-byte salt.
        let mut salt = [0u8; 64];
        for (i, b) in salt[32..].iter_mut().enumerate() {
            *b = (i + 1) as u8;
        }
        let sb = synth("sha256", 1, 1, 4096, 4096, 64, &salt);
        let v = ValidatedVeritySuperblock::parse(&sb, 0).unwrap();
        assert!(v.is_base(), "all-zero prefix is the base sentinel");
        assert_eq!(v.full_salt().len(), 64);
    }

    #[test]
    fn parses_sha512_with_suffix() {
        let mut prefix = [0u8; 80];
        for (i, b) in prefix.iter_mut().enumerate() {
            *b = i as u8;
        }
        let sb = synth("sha512", 1, 1, 4096, 4096, 80, &prefix);
        let v = ValidatedVeritySuperblock::parse(&sb, 1).unwrap();
        assert_eq!(v.algorithm, Algorithm::Sha512);
        assert_eq!(v.full_salt().len(), 80);
        assert_eq!(v.algorithm.digest_size(), 64);
        assert!(!v.is_base());
    }

    #[test]
    fn rejects_blocklisted_algorithms() {
        for alg in ["md5", "sha1", "sha224", "sha384", "sha3-256", "blake3"] {
            let prefix = [0xAA; 32];
            let sb = synth(alg, 1, 1, 4096, 4096, 32, &prefix);
            let r = ValidatedVeritySuperblock::parse(&sb, 0);
            assert!(
                matches!(
                    r,
                    Err(CarapaceError::WhitelistViolation {
                        field: "algorithm",
                        ..
                    })
                ),
                "{alg} should be rejected, got {r:?}"
            );
        }
    }

    #[test]
    fn rejects_off_rdp_block_size() {
        let prefix = [0xAA; 32];
        for dbs in [512u32, 1024, 2048, 8192, 0, 7] {
            let sb = synth("sha256", 1, 1, dbs, 4096, 32, &prefix);
            assert!(matches!(
                ValidatedVeritySuperblock::parse(&sb, 0),
                Err(CarapaceError::WhitelistViolation {
                    field: "data_block_size",
                    ..
                })
            ));
        }
    }

    #[test]
    fn rejects_hash_type_zero() {
        let prefix = [0xAA; 32];
        let sb = synth("sha256", 1, 0, 4096, 4096, 32, &prefix);
        assert!(matches!(
            ValidatedVeritySuperblock::parse(&sb, 0),
            Err(CarapaceError::WhitelistViolation {
                field: "hash_type",
                ..
            })
        ));
    }

    #[test]
    fn rejects_version_zero() {
        let prefix = [0xAA; 32];
        let sb = synth("sha256", 0, 1, 4096, 4096, 32, &prefix);
        assert!(matches!(
            ValidatedVeritySuperblock::parse(&sb, 0),
            Err(CarapaceError::WhitelistViolation {
                field: "version",
                ..
            })
        ));
    }

    #[test]
    fn rejects_short_salt_uniformly() {
        // sha256 needs salt_size >= 32 for any scute (base or non-base).
        let sb = synth("sha256", 1, 1, 4096, 4096, 0, &[]);
        assert!(matches!(
            ValidatedVeritySuperblock::parse(&sb, 0),
            Err(CarapaceError::WhitelistViolation {
                field: "salt_size",
                ..
            })
        ));
        // sha512 needs salt_size >= 64.
        let sb = synth("sha512", 1, 1, 4096, 4096, 32, &[0; 32]);
        assert!(matches!(
            ValidatedVeritySuperblock::parse(&sb, 1),
            Err(CarapaceError::WhitelistViolation {
                field: "salt_size",
                ..
            })
        ));
    }

    #[test]
    fn rejects_wrong_signature() {
        let mut sb = synth("sha256", 1, 1, 4096, 4096, 32, &[0xAA; 32]);
        sb[0..8].copy_from_slice(b"badmagic");
        assert!(matches!(
            ValidatedVeritySuperblock::parse(&sb, 0),
            Err(CarapaceError::SuperblockInvalid { scute_index: 0, .. })
        ));
    }

    #[test]
    fn rejects_data_blocks_overflow_on_block_size_multiply() {
        // data_blocks = u64::MAX would overflow when multiplied by
        // RDP_DATA_BLOCK_SIZE (4096) in the activation path's
        // length-in-bytes calc. Reject at parse so no downstream code
        // can hit the overflow.
        let mut sb = synth("sha256", 1, 1, 4096, 4096, 32, &[0xAA; 32]);
        sb[72..80].copy_from_slice(&u64::MAX.to_le_bytes());
        let r = ValidatedVeritySuperblock::parse(&sb, 0);
        assert!(
            matches!(&r, Err(CarapaceError::SuperblockInvalid { reason, .. }) if reason.contains("overflow")),
            "expected SuperblockInvalid('...overflow...'), got {r:?}"
        );
    }

    #[test]
    fn rejects_data_blocks_zero() {
        // synth() hardcodes data_blocks = 1; override at offset 72.
        let mut sb = synth("sha256", 1, 1, 4096, 4096, 32, &[0xAA; 32]);
        sb[72..80].copy_from_slice(&0u64.to_le_bytes());
        let r = ValidatedVeritySuperblock::parse(&sb, 0);
        assert!(
            matches!(&r, Err(CarapaceError::SuperblockInvalid { reason, .. }) if reason.contains("data_blocks")),
            "expected SuperblockInvalid('...data_blocks...'), got {r:?}"
        );
    }

    #[test]
    fn rejects_algorithm_field_without_nul_terminator() {
        // synth() lays "sha256" at offset 32 then leaves the rest of
        // the 32-byte field as zero — that's well-formed. To exercise
        // the no-NUL path, fill all 32 bytes of the algorithm field
        // with a non-NUL byte. The producer would never emit this; an
        // adversary or corruption could.
        let mut sb = synth("sha256", 1, 1, 4096, 4096, 32, &[0xAA; 32]);
        for b in &mut sb[32..64] {
            *b = b'x';
        }
        let r = ValidatedVeritySuperblock::parse(&sb, 0);
        assert!(
            matches!(&r, Err(CarapaceError::SuperblockInvalid { reason, .. }) if reason.contains("NUL")),
            "expected SuperblockInvalid('...NUL...'), got {r:?}"
        );
    }
}
