// SPDX-License-Identifier: Apache-2.0
#![cfg(target_os = "linux")]

use devmap_linux::{DevId, targets::Parse, targets::Target, targets::Version};
use devmap_snapshot::{Merge, Origin, Snapshot};

#[test]
fn supported_kernel_targets_have_expected_names() {
    assert_eq!(Snapshot::NAME, "snapshot");
    assert_eq!(Origin::NAME, "snapshot-origin");
    assert_eq!(Merge::NAME, "snapshot-merge");
    assert_eq!(
        Origin::parse(
            "252:1",
            Version {
                major: 1,
                minor: 0,
                patch: 0
            }
        )
        .unwrap(),
        Origin {
            origin: DevId::new(252, 1).unwrap()
        }
    );
}
