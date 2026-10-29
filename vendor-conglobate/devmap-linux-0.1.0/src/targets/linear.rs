// SPDX-License-Identifier: Apache-2.0

//! The Linux device-mapper `linear` target.

use std::io;

use super::{Parse, Target, Version};
use crate::DevId;

/// Maps a sector range onto a contiguous range of a backing device.
///
/// The table row supplies the mapped length. Table responses retain the device
/// and offset as `Linear`; runtime info is `()`.
/// Use [`Table::add`](crate::Table::add) to construct a mapping and
/// [`Row::parse`](crate::records::Row::parse) to inspect it.
///
/// Encoding and table parsing support target major version 1. Table parsing
/// accepts a `major:minor` device number and an offset, not a device path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Linear {
    /// Backing block device.
    pub device: DevId,
    /// Starting offset in the backing device, in 512-byte sectors.
    pub offset: u64,
}

impl Target for Linear {
    const NAME: &'static str = "linear";

    type Table = Self;
    type Info = ();

    fn encode(&self, version: Version, output: &mut (impl io::Write + ?Sized)) -> io::Result<()> {
        version.require(Version {
            major: 1,
            minor: 0,
            patch: 0,
        })?;
        write!(output, "{} {}", self.device, self.offset)
    }
}

impl Parse for Linear {
    fn parse(text: &str, version: Version) -> io::Result<Self> {
        version.require(Version {
            major: 1,
            minor: 0,
            patch: 0,
        })?;
        let mut fields = text.split_whitespace();
        let device = fields
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing linear device"))?
            .parse()?;
        let offset = fields
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing linear offset"))?
            .parse()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if fields.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected trailing linear parameters",
            ));
        }
        Ok(Self { device, offset })
    }
}
