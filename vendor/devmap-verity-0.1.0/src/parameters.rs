// SPDX-License-Identifier: Apache-2.0

use std::io;

use devmap_core::{BlockSize, Geometry};

use crate::{Algorithm, Constraint, HashType, Salt};

/// Hash-tree format independent of an on-disk header or device mapping.
///
/// A header adds a UUID to these parameters; a target adds backing devices,
/// a trusted root digest, and activation settings. Public fields allow editing
/// before construction. Size calculations and target builders reject
/// combinations whose extents cannot be represented.
///
/// On Linux, `Parameters::builder` constructs a headerless target;
/// `Header::builder` instead defaults the tree offset to one hash block,
/// leaving space for the header. A parsed target returns these same format
/// parameters without inferring whether a header is present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Parameters {
    /// The hash-tree convention.
    pub hash_type: HashType,

    /// The digest algorithm.
    pub algorithm: Algorithm,

    /// At most 256 salt bytes.
    pub salt: Salt,

    /// Geometry of the protected data extent.
    pub data: Geometry<Constraint>,

    /// Size of each hash-tree block.
    pub hash: BlockSize<Constraint>,
}

impl Parameters {
    /// Returns the size of the protected data extent in bytes.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] if the size exceeds `u64::MAX`.
    pub fn data_size(&self) -> io::Result<u64> {
        u64::from(self.data.size.bytes().get())
            .checked_mul(self.data.count.get())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "verity data extent overflows")
            })
    }

    /// Returns the size of the complete hash tree in bytes.
    ///
    /// This excludes any on-disk header and any space before the tree. Those
    /// are layout choices made by the formatter or target configuration.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] if the tree size cannot be
    /// represented as a `u64` byte count.
    pub fn tree_size(&self) -> io::Result<u64> {
        let hashes_per_block = self.hashes_per_block()?;
        let blocks = self
            .level_blocks(hashes_per_block)
            .try_fold(0u64, u64::checked_add)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "verity hash tree size overflows",
                )
            })?;
        blocks
            .checked_mul(u64::from(self.hash.bytes().get()))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "verity hash tree size overflows",
                )
            })
    }

    pub(crate) fn hashes_per_block(&self) -> io::Result<usize> {
        let capacity = self.hash.bytes().get() as usize / self.algorithm.digest_size();
        if capacity < 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "hash block cannot hold two digests",
            ));
        }
        Ok(1usize << capacity.ilog2())
    }

    pub(crate) fn level_blocks(&self, hashes_per_block: usize) -> impl Iterator<Item = u64> {
        let divisor = hashes_per_block as u64;
        std::iter::successors(Some(self.data.count.get()), move |blocks| {
            (*blocks > 1).then(|| blocks.div_ceil(divisor))
        })
        .skip(1)
    }
}
