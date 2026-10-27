// SPDX-License-Identifier: Apache-2.0

use std::num::NonZero;

use devmap_core::{BlockSize, Constraint, General};

#[derive(Debug)]
enum Restricted {}

impl Constraint for Restricted {
    const MIN: u32 = 9;
    const MAX: u32 = 19;
    const DEFAULT: u32 = 12;
}

#[test]
fn constructs_from_bytes_and_exponents() {
    for exponent in 0..u32::BITS {
        let bytes = NonZero::new(1u32 << exponent).unwrap();
        let from_bytes = BlockSize::<General>::from_bytes(bytes).unwrap();
        let from_exponent = BlockSize::<General>::from_exponent(exponent).unwrap();

        assert_eq!(from_bytes, from_exponent);
        assert_eq!(from_bytes.bytes(), bytes);
        assert_eq!(from_bytes.exponent(), exponent);
    }

    assert!(BlockSize::<General>::from_exponent(u32::BITS).is_none());
    for bytes in [3, 511, 513, u32::MAX] {
        assert!(BlockSize::<General>::from_bytes(NonZero::new(bytes).unwrap()).is_none());
    }
}

#[test]
fn constraints_are_inclusive() {
    assert!(BlockSize::<Restricted>::from_exponent(8).is_none());
    assert_eq!(
        BlockSize::<Restricted>::from_exponent(9)
            .unwrap()
            .bytes()
            .get(),
        512
    );
    assert_eq!(
        BlockSize::<Restricted>::from_exponent(19)
            .unwrap()
            .bytes()
            .get(),
        512 * 1024
    );
    assert!(BlockSize::<Restricted>::from_exponent(20).is_none());
}

#[test]
fn defaults_and_ordering_use_exponents() {
    let size = BlockSize::<Restricted>::default();
    assert_eq!(size.exponent(), Restricted::DEFAULT);
    assert_eq!(size.bytes().get(), 4096);
    assert!(BlockSize::<Restricted>::from_exponent(9).unwrap() < size);
}

#[test]
fn converts_between_constraints() {
    let accepted = BlockSize::<General>::from_exponent(12).unwrap();
    let converted: BlockSize<Restricted> = accepted.convert().unwrap();
    assert_eq!(converted.exponent(), 12);

    let rejected = BlockSize::<General>::from_exponent(8).unwrap();
    assert!(rejected.convert::<Restricted>().is_none());
    assert_eq!(accepted.convert::<General>().unwrap(), accepted);
}

#[test]
fn block_sizes_are_copyable() {
    const fn copy<T: Copy>() {}
    copy::<BlockSize>();
    copy::<BlockSize<Restricted>>();
}
