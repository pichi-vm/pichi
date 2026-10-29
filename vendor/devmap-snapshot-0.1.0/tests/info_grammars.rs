// SPDX-License-Identifier: Apache-2.0
#![cfg(target_os = "linux")]

//! Supported targets' `STATUSTYPE_INFO` grammar, parsed from a
//! kernel-realistic line.
//!
//! These are pure parse tests — no ioctl, no root — kept in one file so
//! the fixture lines sit side by side and can be checked against the
//! kernel's `.status` callbacks as a set. Per-target behaviour that needs
//! a live device is covered by the other integration tests.
//!
//! Every fixture is the shape the kernel actually emits, taken from the
//! `STATUSTYPE_INFO` branch of each target's status callback.

use devmap_linux::{targets::Parse, targets::Version};
use devmap_snapshot as snapshot;

const VERSION: Version = Version {
    major: 1,
    minor: 16,
    patch: 0,
};

#[test]
fn snapshot_reports_usage_or_a_state_keyword() {
    let info: snapshot::Info = snapshot::Info::parse("20480/524288 2048", VERSION).unwrap();
    assert_eq!(
        <snapshot::Info as Parse>::parse(
            "20480/524288 2048",
            Version {
                major: 1,
                minor: 16,
                patch: 0
            }
        )
        .unwrap(),
        info
    );
    assert_eq!(
        info,
        snapshot::Info::Usage {
            allocated_sectors: 20480,
            total_sectors: 524_288,
            metadata_sectors: 2048,
        }
    );

    // The keywords replace every numeric field, and "Merge failed" is two
    // tokens — which is why the parser matches the line before splitting.
    for (line, expected) in [
        ("Invalid", snapshot::Info::Invalid),
        ("Merge failed", snapshot::Info::MergeFailed),
        ("Overflow", snapshot::Info::Overflow),
        ("Unknown", snapshot::Info::Unknown),
    ] {
        assert_eq!(snapshot::Info::parse(line, VERSION).unwrap(), expected);
    }
}
