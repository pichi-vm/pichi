// SPDX-License-Identifier: Apache-2.0

use std::{fmt, io, str::FromStr};

/// Persistent exception-store modes accepted by both snapshot and merge targets.
///
/// Persistent metadata allows the snapshot to be reactivated using the same
/// backing devices. Selecting a mode does not itself flush or synchronize data.
/// Use directly in [`crate::Parameters`] for [`crate::Merge`], or wrap it in
/// [`crate::Persistence::Persistent`] for [`crate::Snapshot`].
/// Defaults to [`Self::Standard`], without overflow reporting.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Persistent {
    /// Persistent metadata without overflow reporting (`P`).
    #[default]
    Standard,
    /// Persistent metadata with overflow reporting (`PO`).
    ///
    /// Advertises that the caller handles [`crate::Info::Overflow`], distinct
    /// from [`crate::Info::Invalid`]. It does not increase the store's capacity
    /// or guarantee that the snapshot remains valid after every space failure.
    Overflow,
}

/// Formats the mode as `P` or `PO`.
impl fmt::Display for Persistent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Standard => "P",
            Self::Overflow => "PO",
        })
    }
}

/// Parses a persistent store mode.
impl FromStr for Persistent {
    type Err = io::Error;

    /// Accepts exactly `P` or `PO`, without surrounding whitespace.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidData`] for any other text, including the
    /// transient mode `N`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "P" => Ok(Self::Standard),
            "PO" => Ok(Self::Overflow),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected persistent snapshot mode",
            )),
        }
    }
}
