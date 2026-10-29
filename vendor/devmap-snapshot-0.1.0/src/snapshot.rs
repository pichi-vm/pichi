// SPDX-License-Identifier: Apache-2.0

use std::io;

use devmap_linux::targets::{Parse, Target, Version};

use crate::{Discard, Info, Parameters, Persistence, Persistent};

/// A target description for a writable copy-on-write snapshot.
///
/// Construct from [`Parameters<Persistence>`] with [`Self::from`], then add it to
/// a [`devmap_linux::Table`]. Construction does not create or activate a mapping;
/// see the [table example](crate#construct-a-snapshot-table) and the operational
/// requirements on [`Parameters`].
///
/// Table read-back produces another `Snapshot`. It dereferences to
/// [`Parameters<Persistence>`] for field access. Runtime information is [`Info`].
/// Read either through
/// [`devmap_linux::records::Row::parse`].
///
/// Only major version 1 is supported. Overflow reporting requires target
/// version 1.15.0; discard features require 1.16.0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Snapshot(Parameters<Persistence>);

impl Snapshot {
    const MIN_VERSION: Version = Version {
        major: 1,
        minor: 0,
        patch: 0,
    };

    fn required_version(&self) -> Version {
        let minor = if self.discard != Discard::Disabled {
            16
        } else if self.persistence == Persistence::Persistent(Persistent::Overflow) {
            15
        } else {
            0
        };
        Version {
            minor,
            ..Self::MIN_VERSION
        }
    }
}

impl From<Parameters<Persistence>> for Snapshot {
    fn from(parameters: Parameters<Persistence>) -> Self {
        Self(parameters)
    }
}

impl std::ops::Deref for Snapshot {
    type Target = Parameters<Persistence>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Target for Snapshot {
    const NAME: &'static str = "snapshot";

    type Table = Self;
    type Info = Info;

    /// Writes the snapshot's parameters for the supplied target version.
    ///
    /// # Errors
    ///
    /// Returns `Unsupported` when the version cannot support this configuration
    /// (see [`Snapshot`]). Destination errors are propagated and may leave partial
    /// output. Device availability and mapping geometry are checked when loading.
    fn encode(
        &self,
        version: Version,
        output: &mut (impl std::io::Write + ?Sized),
    ) -> io::Result<()> {
        version.require(self.required_version())?;
        self.0.encode(output)
    }
}

impl Parse for Snapshot {
    /// Parses a snapshot table response into owned parameters.
    ///
    /// # Errors
    ///
    /// Returns `InvalidData` for malformed parameters or `Unsupported` for a
    /// configuration not supported by the supplied target version. Device
    /// identifiers are parsed without checking that the devices exist.
    ///
    /// Unsupported major versions are rejected before parsing parameters.
    fn parse(text: &str, version: Version) -> io::Result<Self> {
        version.require(Self::MIN_VERSION)?;
        let target = Self(Parameters::parse(text)?);
        version.require(target.required_version())?;
        Ok(target)
    }
}
