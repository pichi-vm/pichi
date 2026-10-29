// SPDX-License-Identifier: Apache-2.0

use devmap_linux::targets::Zero;
use devmap_linux::{targets::Parse, targets::Version};

const VERSION: Version = Version {
    major: 1,
    minor: 0,
    patch: 0,
};

#[test]
fn zero_accepts_empty_parameters() {
    assert_eq!(Zero::parse("", VERSION).unwrap(), Zero);
}

#[test]
fn zero_rejects_non_empty_params() {
    assert!(Zero::parse("junk", VERSION).is_err());
}
