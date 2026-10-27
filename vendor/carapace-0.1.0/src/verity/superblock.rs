// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Shared verity header decoding followed by Carapace's stricter profile.

use crate::verity::algorithm::{self, Algorithm};
use devmap_verity::{HashType, Header};
use std::io;

pub(crate) const VERITY_SUPERBLOCK_SIZE: usize = 512;
#[cfg(test)]
pub(crate) const VERITY_SIGNATURE: [u8; 8] = *b"verity\0\0";

#[derive(Debug, Clone, Copy)]
pub(crate) struct ValidatedVeritySuperblock {
    pub algorithm: Algorithm,
    pub header: Header,
}

impl ValidatedVeritySuperblock {
    /// Decode the standard format, then enforce the Carapace v1 whitelist.
    pub(crate) fn parse(bytes: &[u8], scute_index: usize) -> io::Result<Self> {
        let invalid = |reason: String| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("scute {scute_index}: {reason}"),
            )
        };
        let record = bytes
            .get(..VERITY_SUPERBLOCK_SIZE)
            .ok_or_else(|| invalid("short verity superblock".into()))?;
        let header = Header::decode(record.try_into().unwrap())
            .map_err(|error| invalid(error.to_string()))?;
        let algorithm = algorithm::parse(header.parameters.algorithm.as_ref())
            .map_err(|error| invalid(error.to_string()))?;
        if header.parameters.hash_type != HashType::Normal {
            return Err(invalid("hash_type must be 1".into()));
        }
        if header.parameters.data.size.bytes().get() != 4096
            || header.parameters.hash.bytes().get() != 4096
        {
            return Err(invalid("data and hash block sizes must be 4096".into()));
        }
        if header.parameters.salt.len() < algorithm.digest_size() {
            return Err(invalid("salt is shorter than the algorithm digest".into()));
        }
        if header
            .parameters
            .data
            .count
            .get()
            .checked_mul(4096)
            .is_none()
        {
            return Err(invalid("data_blocks * 4096 overflows u64".into()));
        }
        Ok(Self { algorithm, header })
    }

    pub(crate) fn full_salt(&self) -> &[u8] {
        &self.header.parameters.salt
    }

    pub(crate) fn is_base(&self) -> bool {
        self.full_salt()[..self.algorithm.digest_size()]
            .iter()
            .all(|&b| b == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let alg_bytes = algorithm.as_bytes();
        buf[32..32 + alg_bytes.len()].copy_from_slice(alg_bytes);
        buf[64..68].copy_from_slice(&data_block_size.to_le_bytes());
        buf[68..72].copy_from_slice(&hash_block_size.to_le_bytes());
        buf[72..80].copy_from_slice(&1u64.to_le_bytes()); // data_blocks
        buf[80..82].copy_from_slice(&salt_size.to_le_bytes());
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
                matches!(r, Err(ref e) if e.kind() == io::ErrorKind::InvalidData),
                "{alg} should be rejected with InvalidData, got {r:?}"
            );
        }
    }

    #[test]
    fn rejects_off_rdp_block_size() {
        let prefix = [0xAA; 32];
        for dbs in [512u32, 1024, 2048, 8192, 0, 7] {
            let sb = synth("sha256", 1, 1, dbs, 4096, 32, &prefix);
            let e = ValidatedVeritySuperblock::parse(&sb, 0).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn rejects_hash_type_zero() {
        let prefix = [0xAA; 32];
        let sb = synth("sha256", 1, 0, 4096, 4096, 32, &prefix);
        let e = ValidatedVeritySuperblock::parse(&sb, 0).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_version_zero() {
        let prefix = [0xAA; 32];
        let sb = synth("sha256", 0, 1, 4096, 4096, 32, &prefix);
        let e = ValidatedVeritySuperblock::parse(&sb, 0).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_short_salt_uniformly() {
        let sb = synth("sha256", 1, 1, 4096, 4096, 0, &[]);
        let e = ValidatedVeritySuperblock::parse(&sb, 0).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        // sha512 needs salt_size >= 64.
        let sb = synth("sha512", 1, 1, 4096, 4096, 32, &[0; 32]);
        let e = ValidatedVeritySuperblock::parse(&sb, 1).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_wrong_signature() {
        let mut sb = synth("sha256", 1, 1, 4096, 4096, 32, &[0xAA; 32]);
        sb[0..8].copy_from_slice(b"badmagic");
        let e = ValidatedVeritySuperblock::parse(&sb, 0).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_data_blocks_overflow_on_block_size_multiply() {
        let mut sb = synth("sha256", 1, 1, 4096, 4096, 32, &[0xAA; 32]);
        sb[72..80].copy_from_slice(&u64::MAX.to_le_bytes());
        let r = ValidatedVeritySuperblock::parse(&sb, 0);
        assert!(
            matches!(&r, Err(e) if e.kind() == io::ErrorKind::InvalidData && e.to_string().contains("overflow")),
            "expected InvalidData('...overflow...'), got {r:?}"
        );
    }

    #[test]
    fn rejects_data_blocks_zero() {
        let mut sb = synth("sha256", 1, 1, 4096, 4096, 32, &[0xAA; 32]);
        sb[72..80].copy_from_slice(&0u64.to_le_bytes());
        let r = ValidatedVeritySuperblock::parse(&sb, 0);
        assert!(
            matches!(&r, Err(e) if e.kind() == io::ErrorKind::InvalidData && e.to_string().contains("count")),
            "expected InvalidData('...data_blocks...'), got {r:?}"
        );
    }

    #[test]
    fn rejects_algorithm_field_without_nul_terminator() {
        let mut sb = synth("sha256", 1, 1, 4096, 4096, 32, &[0xAA; 32]);
        for b in &mut sb[32..64] {
            *b = b'x';
        }
        let r = ValidatedVeritySuperblock::parse(&sb, 0);
        assert!(
            matches!(&r, Err(e) if e.kind() == io::ErrorKind::InvalidData && e.to_string().contains("algorithm")),
            "expected InvalidData('...NUL...'), got {r:?}"
        );
    }
    #[test]
    fn rejects_noncanonical_padding() {
        for offset in [82, 120, 344, 511] {
            let mut bytes = synth("sha256", 1, 1, 4096, 4096, 32, &[0; 32]);
            bytes[offset] = 1;
            let error = ValidatedVeritySuperblock::parse(&bytes, 2).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("scute 2"));
        }
    }
}
