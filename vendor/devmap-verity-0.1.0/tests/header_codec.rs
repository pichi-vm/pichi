// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{io::ErrorKind, num::NonZero};

use devmap_core::{BlockSize, Geometry};
use devmap_verity::{Algorithm, Constraint, HashType, Header, Parameters, Salt};

const VERSION: std::ops::Range<usize> = 8..12;
const HASH_TYPE: std::ops::Range<usize> = 12..16;
const ALGORITHM: std::ops::Range<usize> = 32..64;
const DATA_BLOCK_SIZE: std::ops::Range<usize> = 64..68;
const HASH_BLOCK_SIZE: std::ops::Range<usize> = 68..72;
const DATA_BLOCKS: std::ops::Range<usize> = 72..80;
const SALT_SIZE: std::ops::Range<usize> = 80..82;
const RESERVED: std::ops::Range<usize> = 82..88;
const SALT: std::ops::Range<usize> = 88..344;
const PADDING: std::ops::Range<usize> = 344..512;

fn header(algorithm: Algorithm, hash_type: HashType, salt: &[u8]) -> Header {
    Header {
        uuid: [0x5a; 16],
        parameters: Parameters {
            hash_type,
            algorithm,
            salt: Salt::new(salt).unwrap(),
            data: Geometry {
                size: BlockSize::<Constraint>::from_exponent(9).unwrap(),
                count: NonZero::new(17).unwrap(),
            },
            hash: BlockSize::<Constraint>::from_exponent(19).unwrap(),
        },
    }
}

fn assert_invalid(name: &str, record: &[u8; 512]) {
    let error = Header::decode(*record).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidData, "{name}: {error}");
}

#[test]
fn every_algorithm_round_trips_without_its_hash_feature() {
    let algorithms = [
        Algorithm::Sha1,
        Algorithm::Sha224,
        Algorithm::Sha256,
        Algorithm::Sha384,
        Algorithm::Sha512,
        Algorithm::Ripemd160,
        Algorithm::Whirlpool,
        Algorithm::Sha3_224,
        Algorithm::Sha3_256,
        Algorithm::Sha3_384,
        Algorithm::Sha3_512,
        Algorithm::Streebog256,
        Algorithm::Streebog512,
        Algorithm::Sm3,
        Algorithm::Blake2b160,
        Algorithm::Blake2b256,
        Algorithm::Blake2b384,
        Algorithm::Blake2b512,
        Algorithm::Blake2s128,
        Algorithm::Blake2s160,
        Algorithm::Blake2s224,
        Algorithm::Blake2s256,
    ];

    for algorithm in algorithms {
        let expected = header(algorithm, HashType::Normal, &[1, 2, 3]);
        assert_eq!(Header::decode(expected.encode()).unwrap(), expected);
    }
}

#[test]
fn hash_types_and_salt_boundaries_round_trip() {
    let empty = header(Algorithm::Sha256, HashType::ChromeOs, &[]);
    assert_eq!(Header::decode(empty.encode()).unwrap(), empty);

    let salt = [0x5a; 256];
    let full = header(Algorithm::Sha512, HashType::Normal, &salt);
    assert_eq!(Header::decode(full.encode()).unwrap(), full);
}

#[test]
fn tree_size_excludes_the_header_and_tracks_every_level() {
    let mut header = Header {
        uuid: [0; 16],
        parameters: Parameters {
            hash_type: HashType::Normal,
            algorithm: Algorithm::Sha256,
            salt: Salt::default(),
            data: Geometry {
                size: BlockSize::default(),
                count: NonZero::new(1).unwrap(),
            },
            hash: BlockSize::default(),
        },
    };

    assert_eq!(header.prefix_size(), 4096);
    assert_eq!(header.tree_size().unwrap(), 0);
    assert_eq!(header.total_size().unwrap(), 4096);

    header.parameters.data.count = NonZero::new(128).unwrap();
    assert_eq!(header.tree_size().unwrap(), 4096);
    assert_eq!(header.total_size().unwrap(), 2 * 4096);

    header.parameters.data.count = NonZero::new(129).unwrap();
    assert_eq!(header.tree_size().unwrap(), 3 * 4096);
    assert_eq!(header.total_size().unwrap(), 4 * 4096);
}

#[test]
fn overflowing_tree_size_is_rejected() {
    let mut header = header(Algorithm::Sha256, HashType::Normal, &[]);
    header.parameters.data.count = NonZero::new(u64::MAX).unwrap();

    let error = header.tree_size().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert_eq!(
        header.total_size().unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
}

#[test]
fn total_size_checks_the_prefix_addition() {
    let mut header = header(Algorithm::Sha512, HashType::Normal, &[]);
    header.parameters.hash = BlockSize::from_exponent(9).unwrap();
    header.parameters.data.count = NonZero::new(252_201_579_132_747_753).unwrap();

    assert_eq!(header.tree_size().unwrap(), u64::MAX - 511);
    assert_eq!(
        header.total_size().unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
}

#[test]
fn malformed_structural_fields_are_rejected() {
    let valid = header(Algorithm::Sha256, HashType::Normal, &[1, 2, 3]).encode();
    let mut cases = Vec::new();

    let mut record = valid;
    record[0] ^= 1;
    cases.push(("signature", record));

    let mut record = valid;
    record[VERSION].copy_from_slice(&2_u32.to_le_bytes());
    cases.push(("version", record));

    let mut record = valid;
    record[HASH_TYPE].copy_from_slice(&2_u32.to_le_bytes());
    cases.push(("hash type", record));

    let mut record = valid;
    record[ALGORITHM].fill(0);
    record[32..39].copy_from_slice(b"unknown");
    cases.push(("unsupported algorithm", record));

    let mut record = valid;
    record[32] = 0xff;
    cases.push(("non-UTF-8 algorithm", record));

    let mut record = valid;
    record[39] = b'x';
    cases.push(("nonzero algorithm padding", record));

    let mut record = valid;
    record[DATA_BLOCK_SIZE].copy_from_slice(&1000_u32.to_le_bytes());
    cases.push(("data block size", record));

    let mut record = valid;
    record[HASH_BLOCK_SIZE].copy_from_slice(&0_u32.to_le_bytes());
    cases.push(("hash block size", record));

    let mut record = valid;
    record[DATA_BLOCKS].copy_from_slice(&0_u64.to_le_bytes());
    cases.push(("data block count", record));

    let mut record = valid;
    record[SALT_SIZE].copy_from_slice(&257_u16.to_le_bytes());
    cases.push(("salt length", record));

    let mut record = valid;
    record[RESERVED.start] = 1;
    cases.push(("reserved bytes", record));

    let mut record = valid;
    record[SALT.start + 3] = 1;
    cases.push(("salt padding", record));

    let mut record = valid;
    record[PADDING.start] = 1;
    cases.push(("record padding", record));

    for (name, record) in cases {
        assert_invalid(name, &record);
    }
}
