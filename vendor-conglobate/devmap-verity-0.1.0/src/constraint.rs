// SPDX-License-Identifier: Apache-2.0

/// Block-size limits supported by verity target geometry.
#[derive(Debug, Clone, Copy)]
pub enum Constraint {}

impl devmap_core::Constraint for Constraint {
    const MIN: u32 = 9;
    const MAX: u32 = 19;
    const DEFAULT: u32 = 12;
}
