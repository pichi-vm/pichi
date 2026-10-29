// SPDX-License-Identifier: Apache-2.0

use std::{io, num::NonZero};

use devmap_core::BlockSize;
use devmap_linux::DevId;

use crate::{Constraint, Discard};

/// Backing devices and storage policy for a snapshot or merge target.
///
/// Use [`crate::Persistence`] for [`crate::Snapshot`] or
/// [`crate::Persistent`] for [`crate::Merge`]. See the
/// [snapshot table example](crate#construct-a-snapshot-table) for construction.
///
/// These values do not open devices or validate their sizes and relationships.
/// The target version is checked during encoding; device and table constraints
/// are checked by the kernel when loading. The mapped sector range belongs to
/// the [`devmap_linux::Table`], not these parameters.
///
/// Writes to a live origin must pass through its [`crate::Origin`] mapping,
/// or the [`crate::Merge`] mapping after handover.
/// Loading or unloading a snapshot requires suspending the corresponding origin
/// or merge mapping; see the [kernel snapshot documentation] for coordination.
///
/// [kernel snapshot documentation]: https://docs.kernel.org/admin-guide/device-mapper/snapshot.html
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Parameters<P> {
    /// The underlying origin device, not the mapping containing [`crate::Origin`].
    pub origin: DevId,
    /// The dedicated copy-on-write device holding metadata and changed chunks.
    ///
    /// For a new snapshot, reserve disposable storage for this purpose. A merge
    /// must use the existing snapshot's store, preserving its contents.
    pub cow: DevId,
    /// Whether exception metadata survives reactivation and reports overflow.
    pub persistence: P,
    /// The unit of copy-on-write storage, expressed as a byte-based block size.
    ///
    /// The table codec converts this to and from 512-byte sectors.
    pub chunk_size: BlockSize<Constraint>,
    /// How snapshot discards affect the COW store and underlying origin.
    pub discard: Discard,
}

impl<P> Parameters<P> {
    // Shared grammar; snapshot and snapshot-merge have different version gates.
    pub(crate) fn encode(&self, output: &mut (impl io::Write + ?Sized)) -> io::Result<()>
    where
        P: std::fmt::Display,
    {
        write!(
            output,
            "{} {} {} {}",
            self.origin,
            self.cow,
            self.persistence,
            self.chunk_size.bytes().get() / 512
        )?;
        match self.discard {
            Discard::Disabled => Ok(()),
            Discard::ZeroCow => output.write_all(b" 1 discard_zeroes_cow"),
            Discard::Passdown => output.write_all(b" 2 discard_zeroes_cow discard_passdown_origin"),
        }
    }

    pub(crate) fn parse(s: &str) -> io::Result<Self>
    where
        P: std::str::FromStr<Err = io::Error>,
    {
        let mut fields = s.split_whitespace();
        let origin = fields
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing snapshot origin"))?
            .parse::<DevId>()?;
        let cow = fields
            .next()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "missing snapshot copy-on-write device",
                )
            })?
            .parse::<DevId>()?;
        let persistence = fields
            .next()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "missing snapshot persistence")
            })?
            .parse()?;
        let chunk_size = fields
            .next()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "missing snapshot chunk size")
            })?
            .parse::<u32>()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
            .checked_mul(512)
            .and_then(NonZero::new)
            .and_then(BlockSize::from_bytes)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid snapshot chunk size")
            })?;
        let mut zeroes = false;
        let mut passdown = false;
        if let Some(count) = fields.next() {
            let count: usize = count
                .parse()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            for _ in 0..count {
                match fields.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "missing snapshot option")
                })? {
                    "discard_zeroes_cow" if !zeroes => zeroes = true,
                    "discard_passdown_origin" if !passdown => passdown = true,
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "unknown or duplicate snapshot option",
                        ));
                    }
                }
            }
        }
        if fields.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected trailing snapshot parameters",
            ));
        }
        let discard = match (zeroes, passdown) {
            (false, false) => Discard::Disabled,
            (true, false) => Discard::ZeroCow,
            (true, true) => Discard::Passdown,
            (false, true) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "discard passdown requires discard_zeroes_cow",
                ));
            }
        };
        Ok(Self {
            origin,
            cow,
            persistence,
            chunk_size,
            discard,
        })
    }
}
