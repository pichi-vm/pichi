// SPDX-License-Identifier: Apache-2.0

use std::io;

use devmap_linux::targets::{Parse, Target, Version};

use crate::{Discard, Info, Parameters, Persistent};

/// A target description for merging a persistent snapshot into its origin.
///
/// Construct from [`Parameters<Persistent>`] with [`Self::from`]. Construction
/// does not start merging or change any mapping. `Merge` dereferences to its
/// parameters for field access. Table read-back produces `Merge` and runtime
/// information is [`Info`].
///
/// Activation requires the [kernel's handover procedure]: this target replaces
/// the origin target and takes over the existing snapshot's COW store. Once
/// merging starts, access through the old snapshot mapping fails with I/O errors.
/// The caller must coordinate the participating mappings and their suspension.
///
/// [kernel's handover procedure]: https://docs.kernel.org/admin-guide/device-mapper/snapshot.html
///
/// Overflow reporting requires target version 1.4.0; discard features require
/// 1.5.0. Only major version 1 is supported. These requirements are independent
/// of [`Snapshot`](crate::Snapshot)'s.
///
/// # Examples
///
/// Prepare a merge table using the merge target's version, not the snapshot's.
/// Loading and activating this table still require the handover above.
///
/// ```no_run
/// use devmap_core::Span;
/// use devmap_linux::{Control, Table, targets::Target};
/// use devmap_snapshot::{Merge, Parameters, Persistent};
///
/// fn merge_table(control: &Control, parameters: Parameters<Persistent>, sectors: u64)
///     -> std::io::Result<Table>
/// {
///     let merge = Merge::from(parameters);
///     Table::default().add(
///         Span::new(0, sectors), &merge, control.version(Merge::NAME)?,
///     )
/// }
/// ```
///
/// Parameters that allow transient persistence cannot directly construct a merge:
///
/// ```compile_fail
/// use devmap_snapshot::{Merge, Parameters, Persistence};
///
/// fn merge(parameters: Parameters<Persistence>) -> Merge {
///     Merge::from(parameters)
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Merge(Parameters<Persistent>);

impl Merge {
    const MIN_VERSION: Version = Version {
        major: 1,
        minor: 0,
        patch: 0,
    };

    fn required_version(&self) -> Version {
        let minor = if self.discard != Discard::Disabled {
            5
        } else if self.persistence == Persistent::Overflow {
            4
        } else {
            0
        };

        Version {
            minor,
            ..Self::MIN_VERSION
        }
    }
}

impl From<Parameters<Persistent>> for Merge {
    fn from(parameters: Parameters<Persistent>) -> Self {
        Self(parameters)
    }
}

impl std::ops::Deref for Merge {
    type Target = Parameters<Persistent>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Target for Merge {
    const NAME: &'static str = "snapshot-merge";

    type Table = Self;
    type Info = Info;

    /// Writes the merge parameters for the supplied merge-target version.
    ///
    /// # Errors
    ///
    /// Returns `Unsupported` when the version cannot support this configuration
    /// (see [`Merge`]). Destination errors are propagated and may leave partial
    /// output. Encoding does not validate the live mappings or perform handover.
    fn encode(
        &self,
        version: Version,
        output: &mut (impl std::io::Write + ?Sized),
    ) -> io::Result<()> {
        version.require(self.required_version())?;
        self.0.encode(output)
    }
}

impl Parse for Merge {
    /// Parses a merge table response into owned persistent parameters.
    ///
    /// # Errors
    ///
    /// Returns `InvalidData` for malformed parameters, including transient
    /// persistence, or `Unsupported` for an unsupported target configuration.
    /// Parsing neither checks device existence nor starts a merge.
    ///
    /// Unsupported major versions are rejected before parsing parameters.
    fn parse(text: &str, version: Version) -> io::Result<Self> {
        version.require(Self::MIN_VERSION)?;

        let target = Self(Parameters::parse(text)?);
        version.require(target.required_version())?;

        Ok(target)
    }
}
