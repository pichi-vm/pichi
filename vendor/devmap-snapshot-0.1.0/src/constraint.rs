// SPDX-License-Identifier: Apache-2.0

/// Snapshot chunk-size limits: powers of two from 512 bytes through 1 GiB.
///
/// The default is 4 KiB. Backing devices may impose additional alignment
/// requirements, checked by the kernel when loading the table.
#[derive(Debug, Clone, Copy)]
pub enum Constraint {}

impl devmap_core::Constraint for Constraint {
    const MIN: u32 = 9;
    const MAX: u32 = 30;
    const DEFAULT: u32 = 12;
}
