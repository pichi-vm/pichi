// SPDX-License-Identifier: Apache-2.0

use std::io;

use devmap_linux::DevId;
use devmap_linux::targets::{Parse, Target, Version};

/// A target that preserves snapshot contents when the origin is written.
///
/// Reads pass through to the underlying device. Before overwriting a chunk,
/// the kernel preserves its original contents in the stores of snapshots that
/// still need it. Direct writes to the underlying device bypass this protection.
///
/// Add this description to a [`devmap_linux::Table`]; constructing it does not
/// create a mapping. Coordinate activation and suspension with the dependent
/// snapshots as described in the [kernel snapshot documentation].
///
/// Supports target major version 1. Table read-back parses as `Origin`;
/// the runtime response is empty and parses as `()`.
///
/// [kernel snapshot documentation]: https://docs.kernel.org/admin-guide/device-mapper/snapshot.html
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Origin {
    /// The underlying device, not the device-mapper mapping using this target.
    pub origin: DevId,
}

impl Target for Origin {
    const NAME: &'static str = "snapshot-origin";

    type Table = Self;
    type Info = ();

    /// Writes the underlying device identifier as target parameters.
    ///
    /// # Errors
    ///
    /// Returns `Unsupported` for a target major version other than 1.
    /// Destination errors are propagated and may leave partial output.
    fn encode(
        &self,
        version: Version,
        output: &mut (impl std::io::Write + ?Sized),
    ) -> io::Result<()> {
        version.require(Version {
            major: 1,
            minor: 0,
            patch: 0,
        })?;
        write!(output, "{}", self.origin)
    }
}

impl Parse for Origin {
    /// Parses an origin target's table response without opening its device.
    ///
    /// # Errors
    ///
    /// Returns `Unsupported` for a target major version other than 1, or
    /// `InvalidData` for a missing, malformed, or extra parameter.
    fn parse(text: &str, version: Version) -> io::Result<Self> {
        version.require(Version {
            major: 1,
            minor: 0,
            patch: 0,
        })?;

        let mut it = text.split_whitespace();
        let origin = it
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing snapshot origin"))?
            .parse()?;
        if it.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected trailing snapshot-origin parameters",
            ));
        }
        Ok(Origin { origin })
    }
}
