// SPDX-License-Identifier: Apache-2.0

/// Discard behavior for snapshot and merge targets.
///
/// Select through [`crate::Parameters::discard`]. The default is [`Self::Disabled`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Discard {
    /// Enables neither COW zeroing nor origin discard passdown.
    #[default]
    Disabled,
    /// Zeroes existing COW data covered by full-chunk discards.
    ///
    /// Enables `discard_zeroes_cow`. This does not reclaim COW exceptions or
    /// pass discards through to the origin.
    ZeroCow,
    /// Zeroes COW data and passes eligible discards to the underlying origin.
    ///
    /// Enables both `discard_zeroes_cow` and `discard_passdown_origin`.
    /// Origin discards bypass the snapshot-origin mapping's copy-out protection:
    /// this option can discard origin data, not just data in the snapshot.
    Passdown,
}
