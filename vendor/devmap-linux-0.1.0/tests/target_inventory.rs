// SPDX-License-Identifier: Apache-2.0

use devmap_linux::{
    targets::Target,
    targets::{Error, Linear, Zero},
};

#[test]
fn basic_targets_have_the_expected_kernel_names() {
    assert_eq!(Zero::NAME, "zero");
    assert_eq!(Linear::NAME, "linear");
    assert_eq!(Error::NAME, "error");
}
