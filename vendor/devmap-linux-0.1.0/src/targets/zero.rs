// SPDX-License-Identifier: Apache-2.0

//! The Linux device-mapper `zero` target.

use std::io;

use super::Parse;
use super::{Target, Version};

/// Returns zeroes on reads and discards writes.
///
/// The table row supplies the mapped length; see the [table example](crate::Table#examples).
/// Table responses parse as `Zero`; runtime info is `()`.
///
/// Encoding and table parsing support target major version 1.
/// Table parsing accepts only an empty parameter string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Zero;

impl Target for Zero {
    const NAME: &'static str = "zero";

    type Table = Self;
    type Info = ();

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
        output.write_all(b"")
    }
}

impl Parse for Zero {
    fn parse(text: &str, version: Version) -> io::Result<Self> {
        version.require(Version {
            major: 1,
            minor: 0,
            patch: 0,
        })?;
        if text.is_empty() {
            Ok(Zero)
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected empty zero parameters",
            ))
        }
    }
}
