// SPDX-License-Identifier: Apache-2.0

use std::{fmt, io, str::FromStr};

use crate::Persistent;

/// Snapshot exception-store persistence.
///
/// Used by [`crate::Snapshot`]'s [`crate::Parameters`]. A [`crate::Merge`] uses
/// [`Persistent`] instead because transient snapshots cannot be merged.
/// Defaults to persistent metadata without overflow reporting
/// ([`Persistent::Standard`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Persistence {
    /// Metadata stored on the COW device for reactivation with the same backing devices.
    ///
    /// See [`Persistent`] for overflow reporting and durability considerations.
    Persistent(Persistent),
    /// In-memory metadata that does not survive reactivation (`N`).
    Transient,
}

impl Default for Persistence {
    fn default() -> Self {
        Self::Persistent(Persistent::default())
    }
}

/// Formats the mode as `P`, `PO`, or `N`.
impl fmt::Display for Persistence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Persistent(mode) => mode.fmt(f),
            Self::Transient => f.write_str("N"),
        }
    }
}

/// Parses a snapshot store mode.
impl FromStr for Persistence {
    type Err = io::Error;

    /// Accepts exactly `P`, `PO`, or `N`, without surrounding whitespace.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidData`] for any other text.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "N" => Ok(Self::Transient),
            _ => s.parse().map(Self::Persistent),
        }
    }
}
