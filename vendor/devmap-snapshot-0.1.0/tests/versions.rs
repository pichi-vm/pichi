// SPDX-License-Identifier: Apache-2.0
#![cfg(target_os = "linux")]

use std::io;

use devmap_core::BlockSize;
use devmap_linux::{
    DevId,
    targets::{Parse, Target, Version},
};
use devmap_snapshot::{
    Discard, Info, Merge, Origin, Parameters, Persistence, Persistent, Snapshot,
};

#[test]
fn snapshot_versions_gate_overflow_and_discard_features() {
    let base = Parameters {
        origin: DevId::new(252, 1).unwrap(),
        cow: DevId::new(252, 2).unwrap(),
        persistence: Persistence::Persistent(Persistent::Standard),
        chunk_size: BlockSize::default(),
        discard: Discard::Disabled,
    };
    let overflow = Parameters {
        origin: DevId::new(252, 1).unwrap(),
        cow: DevId::new(252, 2).unwrap(),
        persistence: Persistence::Persistent(Persistent::Overflow),
        chunk_size: BlockSize::default(),
        discard: Discard::Disabled,
    };
    let mut discard = base;
    discard.discard = Discard::ZeroCow;

    for (target, before, minimum) in [
        (Snapshot::from(overflow), [1, 14, 0], [1, 15, 0]),
        (Snapshot::from(discard), [1, 15, 0], [1, 16, 0]),
    ] {
        let [major, minor, patch] = before;
        let before = Version {
            major,
            minor,
            patch,
        };
        let [major, minor, patch] = minimum;
        let minimum = Version {
            major,
            minor,
            patch,
        };
        let mut rejected = Vec::new();
        let encode_error = target.encode(before, &mut rejected).unwrap_err();
        assert_eq!(encode_error.kind(), io::ErrorKind::Unsupported);
        assert_eq!(rejected, []);
        assert!(target.encode(minimum, &mut Vec::new()).is_ok());

        let mut bytes = Vec::new();
        target.encode(minimum, &mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        let parse_error = Snapshot::parse(&text, before).unwrap_err();
        assert_eq!(parse_error.kind(), io::ErrorKind::Unsupported);
        assert_eq!(parse_error.to_string(), encode_error.to_string());
        assert!(parse_error.to_string().contains(&format!(
            "{}.{}.{}",
            minimum.major, minimum.minor, minimum.patch,
        )));
        assert!(parse_error.to_string().contains(&format!(
            "{}.{}.{}",
            before.major, before.minor, before.patch,
        )));
        assert_eq!(<Snapshot as Parse>::parse(&text, minimum).unwrap(), target);
    }

    assert!(
        Snapshot::from(base)
            .encode(
                Version {
                    major: 1,
                    minor: 0,
                    patch: 0
                },
                &mut Vec::new()
            )
            .is_ok()
    );
    assert!(
        Snapshot::from(base)
            .encode(
                Version {
                    major: 2,
                    minor: 0,
                    patch: 0
                },
                &mut Vec::new()
            )
            .is_err()
    );
}

#[test]
fn merge_versions_use_their_own_feature_boundaries() {
    let overflow = Merge::from(Parameters {
        origin: DevId::new(252, 1).unwrap(),
        cow: DevId::new(252, 2).unwrap(),
        persistence: Persistent::Overflow,
        chunk_size: BlockSize::default(),
        discard: Discard::Disabled,
    });
    let mut discard = Parameters {
        origin: DevId::new(252, 1).unwrap(),
        cow: DevId::new(252, 2).unwrap(),
        persistence: Persistent::Standard,
        chunk_size: BlockSize::default(),
        discard: Discard::Disabled,
    };
    discard.discard = Discard::Passdown;
    let discard = Merge::from(discard);

    for (target, before, minimum) in [
        (overflow, [1, 3, 0], [1, 4, 0]),
        (discard, [1, 4, 0], [1, 5, 0]),
    ] {
        let [major, minor, patch] = before;
        let before = Version {
            major,
            minor,
            patch,
        };
        let [major, minor, patch] = minimum;
        let minimum = Version {
            major,
            minor,
            patch,
        };
        let mut rejected = Vec::new();
        let encode_error = target.encode(before, &mut rejected).unwrap_err();
        assert_eq!(encode_error.kind(), io::ErrorKind::Unsupported);
        assert_eq!(rejected, []);
        assert!(target.encode(minimum, &mut Vec::new()).is_ok());

        let mut bytes = Vec::new();
        target.encode(minimum, &mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        let parse_error = Merge::parse(&text, before).unwrap_err();
        assert_eq!(parse_error.kind(), io::ErrorKind::Unsupported);
        assert_eq!(parse_error.to_string(), encode_error.to_string());
        assert!(parse_error.to_string().contains(&format!(
            "{}.{}.{}",
            minimum.major, minimum.minor, minimum.patch,
        )));
        assert!(parse_error.to_string().contains(&format!(
            "{}.{}.{}",
            before.major, before.minor, before.patch,
        )));
        assert_eq!(<Merge as Parse>::parse(&text, minimum).unwrap(), target);
    }
}

#[test]
fn target_major_versions_are_checked_before_parameters() {
    for major in [0, 2] {
        let version = Version {
            major,
            minor: 0,
            patch: 0,
        };
        assert_eq!(
            Snapshot::parse("malformed", version).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            Merge::parse("malformed", version).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }
    let version = Version {
        major: 1,
        minor: 0,
        patch: 0,
    };
    assert_eq!(
        Snapshot::parse("malformed", version).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(
        Merge::parse("malformed", version).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
}

#[test]
fn origin_and_info_reject_unknown_major_versions() {
    let origin = Origin {
        origin: DevId::new(252, 1).unwrap(),
    };
    let version = Version {
        major: 2,
        minor: 0,
        patch: 0,
    };
    assert!(
        matches!(origin.encode(version, &mut Vec::new()), Err(error) if error.kind() == io::ErrorKind::Unsupported)
    );
    assert_eq!(
        <Origin as Parse>::parse("252:1", version)
            .unwrap_err()
            .kind(),
        io::ErrorKind::Unsupported
    );
    assert_eq!(
        <Info as Parse>::parse("Invalid", version)
            .unwrap_err()
            .kind(),
        io::ErrorKind::Unsupported
    );
    assert_eq!(
        <Info as Parse>::parse(
            "Overflow",
            Version {
                major: 1,
                minor: 3,
                patch: 0
            }
        )
        .unwrap_err()
        .kind(),
        io::ErrorKind::Unsupported
    );
    assert_eq!(
        <Info as Parse>::parse(
            "Overflow",
            Version {
                major: 1,
                minor: 4,
                patch: 0
            }
        )
        .unwrap(),
        Info::Overflow
    );
}
