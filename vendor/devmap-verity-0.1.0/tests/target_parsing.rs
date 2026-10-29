// SPDX-License-Identifier: Apache-2.0

#![cfg(target_os = "linux")]

use std::{io::ErrorKind, num::NonZero};

use devmap_core::{BlockSize, Geometry};
use devmap_linux::{
    DevId,
    targets::{Parse, Target, Version},
};
use devmap_verity::{
    Algorithm, HashType, Header, Parameters, Salt,
    target::{CorruptionPolicy, IoErrorPolicy, Verity},
};

const VERSION: Version = Version {
    major: 1,
    minor: 13,
    patch: 0,
};

#[test]
fn table_readback_preserves_construction_inputs() {
    let parameters = Parameters {
        hash_type: HashType::ChromeOs,
        algorithm: Algorithm::Sha512,
        salt: Salt::new(&[1, 0, 3]).unwrap(),
        data: Geometry {
            size: BlockSize::from_exponent(10).unwrap(),
            count: NonZero::new(17).unwrap(),
        },
        hash: BlockSize::from_exponent(12).unwrap(),
    };
    let data = DevId::new(7, 0).unwrap();
    let hash = DevId::new(7, 1).unwrap();
    let root = vec![0xab; 64];
    let target = parameters
        .builder(data, hash, root.clone())
        .unwrap()
        .hash_start(7)
        .unwrap()
        .corruption_policy(CorruptionPolicy::Restart)
        .io_error_policy(IoErrorPolicy::Panic)
        .ignore_zero_blocks(true)
        .check_at_most_once(true)
        .unwrap()
        .try_verify_in_tasklet(true)
        .root_hash_sig_key_desc("trusted root")
        .unwrap()
        .build();
    let mut encoded = Vec::new();
    target.encode(VERSION, &mut encoded).unwrap();
    let parsed = Verity::parse(std::str::from_utf8(&encoded).unwrap(), VERSION).unwrap();
    assert_eq!(parsed, target);
    assert_eq!(parsed.parameters(), &parameters);
    assert_eq!(parsed.data(), data);
    assert_eq!(parsed.hash(), hash);
    assert_eq!(parsed.root(), root);
    assert_eq!(parsed.hash_start(), 7);
    assert_eq!(parsed.corruption_policy(), CorruptionPolicy::Restart);
    assert_eq!(parsed.io_error_policy(), IoErrorPolicy::Panic);
    assert!(parsed.ignore_zero_blocks());
    assert!(parsed.check_at_most_once());
    assert!(parsed.try_verify_in_tasklet());
    assert_eq!(parsed.root_hash_sig_key_desc(), Some("trusted root"));
    assert_eq!(parsed.data_sectors(), 34);

    let rebuilt = parsed
        .parameters()
        .builder(parsed.data(), parsed.hash(), parsed.root())
        .unwrap()
        .hash_start(parsed.hash_start())
        .unwrap()
        .corruption_policy(parsed.corruption_policy())
        .io_error_policy(parsed.io_error_policy())
        .ignore_zero_blocks(parsed.ignore_zero_blocks())
        .check_at_most_once(parsed.check_at_most_once())
        .unwrap()
        .try_verify_in_tasklet(parsed.try_verify_in_tasklet())
        .root_hash_sig_key_desc(parsed.root_hash_sig_key_desc().unwrap())
        .unwrap()
        .build();
    assert_eq!(rebuilt, parsed);
}

#[test]
fn header_and_parameters_choose_only_the_default_offset() {
    let target = Verity::builder(
        DevId::new(7, 0).unwrap(),
        DevId::new(7, 1).unwrap(),
        NonZero::new(2).unwrap(),
        vec![0; 32],
    )
    .unwrap()
    .build();
    let parameters = *target.parameters();
    assert_eq!(
        parameters
            .builder(target.data(), target.hash(), target.root())
            .unwrap()
            .build(),
        target
    );
    assert_eq!(target.hash_start(), 0);
    assert_eq!(target.root_hash_sig_key_desc(), None);
    let header = Header {
        uuid: [0x5a; 16],
        parameters,
    };
    let backed = header
        .builder(target.data(), target.hash(), target.root())
        .unwrap()
        .build();
    assert_eq!(backed.parameters(), &parameters);
    assert_eq!(backed.hash_start(), 1);
    assert_eq!(
        header
            .builder(target.data(), target.hash(), target.root())
            .unwrap()
            .hash_start(0)
            .unwrap()
            .build(),
        target
    );
    assert_eq!(
        Header {
            uuid: [0xa5; 16],
            ..header
        }
        .builder(target.data(), target.hash(), target.root())
        .unwrap()
        .build(),
        backed
    );
    assert_eq!(
        parameters
            .builder(target.data(), target.hash(), vec![0; 31])
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
    let overflow = Parameters {
        data: Geometry {
            count: NonZero::new(u64::MAX).unwrap(),
            ..parameters.data
        },
        ..parameters
    };
    assert_eq!(
        overflow
            .builder(target.data(), target.hash(), target.root())
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
}

#[test]
fn hex_tokens_are_unescaped_exactly_once() {
    let root = "ab".repeat(32);
    let expected = Verity::parse(
        &format!("1 7:0 7:1 4096 4096 1 0 sha256 {root} ab"),
        VERSION,
    )
    .unwrap();
    let escaped = format!(r"1 7:0 7:1 4096 4096 1 0 sha256 \{root} \ab");
    assert_eq!(Verity::parse(&escaped, VERSION).unwrap(), expected);
    for (digest, salt) in [
        (format!(r"\\{root}"), "ab".to_owned()),
        (root, r"\\ab".to_owned()),
    ] {
        let text = format!("1 7:0 7:1 4096 4096 1 0 sha256 {digest} {salt}");
        assert_eq!(
            Verity::parse(&text, VERSION).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
    }
}

#[test]
fn malformed_table_parameters_are_invalid_data() {
    let root = "ab".repeat(32);
    let base = format!("1 7:0 7:1 4096 4096 1 0 sha256 {root} -");
    let mut cases = Vec::new();
    for (index, name, replacement) in [
        (0, "unknown hash type", "2"),
        (7, "unknown algorithm", "unknown"),
        (8, "non-hex root", "gg"),
        (8, "odd root length", "abc"),
        (9, "non-hex salt", "gg"),
        (9, "odd salt length", "abc"),
    ] {
        let mut fields: Vec<_> = base.split_whitespace().collect();
        fields[index] = replacement;
        cases.push((name, fields.join(" ")));
    }
    cases.push((
        "overlong salt",
        format!("{} {}", base.trim_end_matches('-'), "ab".repeat(257)),
    ));
    for (name, suffix) in [
        ("wrong option count", "2 ignore_zero_blocks"),
        ("invalid option count", "x ignore_zero_blocks"),
        ("missing signature value", "1 root_hash_sig_key_desc"),
        (
            "conflicting corruption policies",
            "2 ignore_corruption restart_on_corruption",
        ),
        (
            "conflicting I/O policies",
            "2 restart_on_error panic_on_error",
        ),
        ("unknown option", "1 unknown"),
        (
            "duplicate signature",
            "4 root_hash_sig_key_desc a root_hash_sig_key_desc b",
        ),
    ] {
        cases.push((name, format!("{base} {suffix}")));
    }
    assert!(Verity::parse(&base, VERSION).is_ok());
    for (name, text) in cases {
        assert_eq!(
            Verity::parse(&text, VERSION).unwrap_err().kind(),
            ErrorKind::InvalidData,
            "{name}"
        );
    }
}
