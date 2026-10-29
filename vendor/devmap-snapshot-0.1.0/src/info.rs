// SPDX-License-Identifier: Apache-2.0

use std::io;

use devmap_linux::targets::{Parse, Version};

/// The runtime status of a [`Snapshot`](crate::Snapshot) or
/// [`Merge`](crate::Merge).
///
/// Read through [`devmap_linux::Control::info`] and
/// [`devmap_linux::records::Row::parse`]. Usage counters and exceptional states
/// are alternatives: counters are unavailable when a state such as
/// [`Self::Overflow`] is reported. All counters use 512-byte sectors.
///
/// # Examples
///
/// Inspect snapshot and merge rows without changing their mappings.
///
/// ```no_run
/// use devmap_linux::{Control, DevId, records::Records, targets::Target};
/// use devmap_snapshot::{Info, Merge, Snapshot};
///
/// fn inspect(control: &Control, id: DevId) -> std::io::Result<()> {
///     let mut rows = control.info::<4096>(id)?;
///     while let Some(row) = rows.next() {
///         let row = row?;
///         let info = match row.name() {
///             Snapshot::NAME => row.parse::<Snapshot>(control.version(Snapshot::NAME)?)?,
///             Merge::NAME => row.parse::<Merge>(control.version(Merge::NAME)?)?,
///             _ => continue,
///         };
///         match info {
///             Info::Usage { allocated_sectors, total_sectors, .. } =>
///                 println!("COW usage: {allocated_sectors}/{total_sectors} sectors"),
///             Info::Overflow => eprintln!("Snapshot overflow: writes are rejected"),
///             Info::Invalid => eprintln!("Snapshot is invalid"),
///             Info::MergeFailed => eprintln!("Snapshot merge failed"),
///             _ => eprintln!("No usage counters available: {info:?}"),
///         }
///     }
///     Ok(())
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Info {
    /// Copy-on-write store usage for the snapshot or merge.
    Usage {
        /// Allocated store space, including metadata, in 512-byte sectors.
        allocated_sectors: u64,
        /// Total store capacity in 512-byte sectors.
        ///
        /// Capacity alone does not determine snapshot validity; handle the
        /// exceptional states separately and monitor usage before space runs out.
        total_sectors: u64,
        /// Metadata space in 512-byte sectors, counted within
        /// `allocated_sectors`.
        metadata_sectors: u64,
    },

    /// The snapshot is invalid and cannot serve snapshot I/O.
    Invalid,

    /// A [`Merge`](crate::Merge) failed; do not assume merging completed.
    MergeFailed,

    /// A write to the snapshot could not allocate a COW exception.
    ///
    /// Further snapshot writes are rejected. This is distinct from invalidation:
    /// reads can still succeed while the snapshot remains valid. See
    /// [`crate::Persistent::Overflow`] for enabling this report.
    Overflow,

    /// The store provides no usage counters.
    Unknown,
}

impl Parse for Info {
    /// Parses usage counters or an exceptional state from a target response.
    ///
    /// Supply the version of the row's actual target. This shared parser receives
    /// no target identity, so it accepts `Overflow` from version 1.4.0, the
    /// [`Merge`](crate::Merge) threshold. It cannot enforce
    /// [`Snapshot`](crate::Snapshot)'s stricter 1.15.0 threshold.
    ///
    /// # Errors
    ///
    /// Returns `InvalidData` for malformed or extra fields. Returns `Unsupported`
    /// for a major version other than 1, or an `Overflow` response with a version
    /// earlier than 1.4.0.
    fn parse(text: &str, version: Version) -> io::Result<Self> {
        version.require(Version {
            major: 1,
            minor: 0,
            patch: 0,
        })?;

        // The sentinels replace the entire line, and "Merge failed" is two
        // tokens, so match the trimmed line before tokenizing at all.
        let info = match text.trim() {
            "Invalid" => Info::Invalid,
            "Merge failed" => Info::MergeFailed,
            "Overflow" => Info::Overflow,
            "Unknown" => Info::Unknown,
            _ => {
                let mut fields = text.split_whitespace();
                let (allocated, total) = fields
                    .next()
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "missing snapshot usage")
                    })?
                    .split_once('/')
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "missing snapshot usage separator",
                        )
                    })?;
                let allocated_sectors = allocated
                    .parse()
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                let total_sectors = total
                    .parse()
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                let metadata_sectors = fields
                    .next()
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "missing snapshot metadata usage",
                        )
                    })?
                    .parse()
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                if fields.next().is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unexpected trailing snapshot status",
                    ));
                }
                Info::Usage {
                    allocated_sectors,
                    total_sectors,
                    metadata_sectors,
                }
            }
        };
        if info == Self::Overflow
            && version
                < (Version {
                    major: 1,
                    minor: 4,
                    patch: 0,
                })
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unsupported snapshot target version or configuration",
            ));
        }
        Ok(info)
    }
}
