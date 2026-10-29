// SPDX-License-Identifier: Apache-2.0

#![warn(missing_docs)]
#![doc = include_str!("../README.md")]

mod block_size;
mod detect;
mod geometry;
mod span;

pub use block_size::BlockSize;
pub use detect::Detect;
pub use geometry::Geometry;
pub use span::Span;

/// Defines the permitted base-two exponents for a [`BlockSize`].
///
/// Implementations must choose a `DEFAULT` between `MIN` and `MAX` that can
/// be represented as a nonzero `u32` byte count.
/// An invalid default fails compilation when [`BlockSize::default`] is instantiated.
pub trait Constraint {
    /// The default base-two exponent.
    const DEFAULT: u32;

    /// The smallest permitted base-two exponent.
    const MIN: u32 = 0;

    /// The largest permitted base-two exponent.
    const MAX: u32;
}

/// The complete range of block sizes representable in a `u32` byte count.
#[derive(Debug, Clone, Copy)]
pub enum General {}

impl Constraint for General {
    const MAX: u32 = u32::BITS - 1;
    const DEFAULT: u32 = 12;
}
