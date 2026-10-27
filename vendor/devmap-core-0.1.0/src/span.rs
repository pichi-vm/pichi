// SPDX-License-Identifier: Apache-2.0

/// A contiguous extent described by its starting unit and length.
///
/// `Span` does not assign a unit to either value. Callers must establish one;
/// for example, device-mapper rows use sectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span<T> {
    /// Offset of the first unit in the extent.
    pub start: T,
    /// Number of units in the extent.
    pub length: T,
}

impl<T> Span<T> {
    /// Creates a span from its starting unit and length.
    pub const fn new(start: T, length: T) -> Self {
        Self { start, length }
    }
}
