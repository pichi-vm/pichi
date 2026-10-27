// SPDX-License-Identifier: Apache-2.0
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fmt::Debug;

use devmap_linux::{targets::Parse, targets::Version};

const VERSION: Version = Version {
    major: 1,
    minor: 16,
    patch: 0,
};

fn row<T: Parse + Debug + PartialEq>(text: &str) {
    let parsed = T::parse(text, VERSION).unwrap();
    let fields: Vec<_> = text.split_whitespace().collect();
    for length in 0..fields.len() {
        let prefix = fields[..length].join(" ");
        assert!(
            T::parse(&prefix, VERSION).is_err(),
            "prefix {prefix:?} of {text:?}"
        );
    }
    assert_eq!(
        T::parse(&format!("{text} unexpected"), VERSION)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidData
    );
    assert_eq!(
        T::parse(&format!(" \t{}\n", fields.join("\t")), VERSION).unwrap(),
        parsed
    );
}

fn reject<T: Parse + Debug>(cases: &[&str]) {
    for text in cases {
        assert_eq!(
            T::parse(text, VERSION).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }
}

#[test]
fn chunk_sizes_convert_between_bytes_and_sectors() {
    use devmap_core::BlockSize;
    use devmap_linux::targets::Target as _;
    use devmap_snapshot::{Constraint, Merge, Snapshot};

    assert_eq!(BlockSize::<Constraint>::default().bytes().get(), 4096);
    assert!(BlockSize::<Constraint>::from_exponent(8).is_none());
    assert!(BlockSize::<Constraint>::from_exponent(31).is_none());
    for (sectors, bytes) in [(1, 512), (8, 4096), (2_097_152, 1 << 30)] {
        let text = format!("7:0 7:1 P {sectors}");
        let snapshot = Snapshot::parse(&text, VERSION).unwrap();
        let merge = Merge::parse(&text, VERSION).unwrap();
        assert_eq!(snapshot.chunk_size.bytes().get(), bytes);
        assert_eq!(merge.chunk_size, snapshot.chunk_size);
        let mut encoded = Vec::new();
        snapshot.encode(VERSION, &mut encoded).unwrap();
        assert_eq!(encoded, text.as_bytes());
        encoded.clear();
        merge.encode(VERSION, &mut encoded).unwrap();
        assert_eq!(encoded, text.as_bytes());
    }
    for sectors in [
        "0",
        "3",
        "4194304",
        "8388608",
        "4294967295",
        "4294967296",
        "-1",
        "x",
    ] {
        let text = format!("7:0 7:1 P {sectors}");
        reject::<Snapshot>(&[&text]);
        reject::<Merge>(&[&text]);
    }
}

#[test]
fn transient_persistence_is_only_accepted_by_snapshot() {
    use devmap_snapshot::{Merge, Persistence, Snapshot};
    let text = "7:0 7:1 N 32";
    let snapshot = Snapshot::parse(text, VERSION).unwrap();
    assert_eq!(snapshot.persistence, Persistence::Transient);
    assert_eq!(
        Merge::parse(text, VERSION).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
}

#[test]
fn table_parsers_preserve_required_optional_and_trailing_fields() {
    row::<devmap_snapshot::Snapshot>("7:0 7:1 PO 32");
}

#[test]
fn persistent_modes_preserve_parameters_across_targets() {
    use devmap_linux::targets::Target as _;
    use devmap_snapshot::{Merge, Persistence, Persistent, Snapshot};

    assert_eq!(Persistent::default(), Persistent::Standard);
    assert_eq!(
        Persistence::default(),
        Persistence::Persistent(Persistent::Standard)
    );

    for (token, mode) in [("P", Persistent::Standard), ("PO", Persistent::Overflow)] {
        let text = format!("7:0 7:1 {token} 32 2 discard_zeroes_cow discard_passdown_origin");
        let snapshot = Snapshot::parse(&text, VERSION).unwrap();
        assert_eq!(snapshot.persistence, Persistence::Persistent(mode));
        let merge = Merge::parse(&text, VERSION).unwrap();
        assert_eq!(Snapshot::from(*snapshot), snapshot);
        assert_eq!(Merge::from(*merge), merge);
        assert_eq!(merge.persistence, mode);
        assert_eq!(merge.origin, snapshot.origin);
        assert_eq!(merge.cow, snapshot.cow);
        assert_eq!(merge.chunk_size, snapshot.chunk_size);
        assert_eq!(merge.discard, snapshot.discard);

        let mut encoded = Vec::new();
        merge.encode(VERSION, &mut encoded).unwrap();
        assert_eq!(encoded, text.as_bytes());
    }
}

#[test]
fn status_parsers_preserve_required_optional_and_trailing_fields() {
    row::<devmap_snapshot::Info>("12/64 8");
}

#[test]
fn discard_modes_round_trip_and_reject_invalid_combinations() {
    use devmap_linux::targets::Target as _;
    use devmap_snapshot::{Discard, Merge, Snapshot};

    assert_eq!(Discard::default(), Discard::Disabled);
    for (suffix, discard) in [
        ("", Discard::Disabled),
        (" 1 discard_zeroes_cow", Discard::ZeroCow),
        (
            " 2 discard_zeroes_cow discard_passdown_origin",
            Discard::Passdown,
        ),
    ] {
        let text = format!("7:0 7:1 P 32{suffix}");
        let snapshot = Snapshot::parse(&text, VERSION).unwrap();
        let merge = Merge::parse(&text, VERSION).unwrap();
        assert_eq!(snapshot.discard, discard);
        assert_eq!(merge.discard, discard);
        let mut encoded = Vec::new();
        snapshot.encode(VERSION, &mut encoded).unwrap();
        assert_eq!(encoded, text.as_bytes());
        encoded.clear();
        merge.encode(VERSION, &mut encoded).unwrap();
        assert_eq!(encoded, text.as_bytes());
    }
    for suffix in [" 0", " 2 discard_passdown_origin discard_zeroes_cow"] {
        let text = format!("7:0 7:1 P 32{suffix}");
        let expected = if suffix == " 0" {
            Discard::Disabled
        } else {
            Discard::Passdown
        };
        assert_eq!(Snapshot::parse(&text, VERSION).unwrap().discard, expected);
        assert_eq!(Merge::parse(&text, VERSION).unwrap().discard, expected);
    }
    let invalid = [
        "7:0 7:1 P 32 1 discard_passdown_origin",
        "7:0 7:1 P 32 2 discard_zeroes_cow discard_zeroes_cow",
        "7:0 7:1 P 32 2 discard_passdown_origin discard_passdown_origin",
        "7:0 7:1 P 32 2 discard_zeroes_cow",
        "7:0 7:1 P 32 1 unknown",
    ];
    reject::<Snapshot>(&invalid);
    reject::<Merge>(&invalid);
}

#[test]
fn special_statuses_remain_whole_row_alternatives() {
    for text in ["Invalid", "Overflow", "Unknown"] {
        row::<devmap_snapshot::Info>(text);
    }
    let merge = devmap_snapshot::Info::parse(" Merge failed ", VERSION).unwrap();
    assert_eq!(merge, devmap_snapshot::Info::MergeFailed);
    reject::<devmap_snapshot::Info>(&["Merge", "Merge failed extra"]);
}

#[test]
fn usage_fields_do_not_gain_new_relationship_constraints() {
    row::<devmap_snapshot::Info>("0/0 0");
    reject::<devmap_snapshot::Info>(&[
        "1/2/3 0",
        "/2 0",
        "1/ 0",
        "-1/2 0",
        "18446744073709551616/2 0",
        "1/18446744073709551616 0",
    ]);
}
