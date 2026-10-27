// SPDX-License-Identifier: Apache-2.0

use std::{
    fmt,
    hash::{Hash, Hasher},
    num::NonZero,
};

use crate::{BlockSize, Constraint, General};

/// A nonempty extent expressed as a block size and block count.
pub struct Geometry<C: Constraint = General> {
    /// Size of one block.
    pub size: BlockSize<C>,
    /// Number of blocks in the extent.
    pub count: NonZero<u64>,
}

impl<C: Constraint> Clone for Geometry<C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<C: Constraint> Copy for Geometry<C> {}

impl<C: Constraint> fmt::Debug for Geometry<C> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Geometry")
            .field("size", &self.size)
            .field("count", &self.count)
            .finish()
    }
}

impl<C: Constraint> PartialEq for Geometry<C> {
    fn eq(&self, other: &Self) -> bool {
        self.size == other.size && self.count == other.count
    }
}

impl<C: Constraint> Eq for Geometry<C> {}

impl<C: Constraint> Hash for Geometry<C> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.size.hash(state);
        self.count.hash(state);
    }
}
