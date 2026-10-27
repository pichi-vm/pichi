// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    io::{Seek as _, SeekFrom, Write as _},
    num::NonZero,
};

use devmap_core::{BlockSize, Detect as _, Geometry};

#[derive(Debug, Clone, Copy)]
enum Constraint {}

impl devmap_core::Constraint for Constraint {
    const MIN: u32 = 9;
    const MAX: u32 = 12;
    const DEFAULT: u32 = 10;
}

#[test]
fn geometry_is_an_ordinary_generic_value() {
    let geometry = Geometry::<Constraint> {
        size: BlockSize::default(),
        count: NonZero::new(3).unwrap(),
    };
    let copied = geometry;
    assert_eq!(copied, geometry);
    assert!(format!("{geometry:?}").contains("Geometry"));
    let mut hash = DefaultHasher::new();
    geometry.hash(&mut hash);
    assert_ne!(hash.finish(), 0);
}

#[test]
fn regular_files_use_the_constraint_default_and_complete_extent() {
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(&vec![0; 3 * 1024]).unwrap();
    file.seek(SeekFrom::Start(1024)).unwrap();

    let size = BlockSize::<Constraint>::detect(&file).unwrap();
    let geometry = Geometry::<Constraint>::detect(&file).unwrap();

    assert_eq!(size.bytes().get(), 1024);
    assert_eq!(geometry.size, size);
    assert_eq!(geometry.count.get(), 3);
    assert_eq!(file.stream_position().unwrap(), 1024);
}

#[test]
fn invalid_extents_and_descriptor_types_are_rejected() {
    let empty = tempfile::tempfile().unwrap();
    assert_eq!(
        Geometry::<Constraint>::detect(&empty).unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );

    let mut misaligned = tempfile::tempfile().unwrap();
    misaligned.write_all(&vec![0; 1025]).unwrap();
    assert_eq!(
        Geometry::<Constraint>::detect(&misaligned)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
}
