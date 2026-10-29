// SPDX-License-Identifier: Apache-2.0

//! Semantic contents and fixed-record codec for a standard dm-verity header.

use std::{io, num::NonZero};

use devmap_core::{BlockSize, Geometry};

use crate::{Constraint, HashType, Parameters, Salt};

pub(crate) const RECORD_SIZE: usize = 512;

const SIGNATURE: [u8; 8] = *b"verity\0\0";
const ALGORITHM_SIZE: usize = 32;
const RESERVED_SIZE: usize = 6;
const SALT_SIZE: usize = 256;
const PADDING_SIZE: usize = 168;

/// Decoded metadata from a standard dm-verity on-disk header.
///
/// The UUID identifies the formatted hash volume but does not authenticate it.
/// Keep the root digest in independently trusted storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Header {
    /// Identifier recorded when the hash volume was formatted.
    pub uuid: [u8; 16],

    /// Hash-tree format and block geometry.
    pub parameters: Parameters,
}

impl Header {
    /// Returns the size of the padded header prefix in bytes.
    ///
    /// The prefix occupies one complete hash block: a 512-byte encoded header
    /// followed by padding through the block boundary.
    pub fn prefix_size(&self) -> u64 {
        u64::from(self.parameters.hash.bytes().get())
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
        self.parameters.tree_size()
    }

    /// Returns the size of the padded header prefix and hash tree in bytes.
    ///
    /// This is the complete extent written by the synchronous and asynchronous
    /// formatters. Headerless and custom-offset layouts should use
    /// [`Self::tree_size`] and account for their selected tree offset instead.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] if the complete size cannot be
    /// represented as a `u64` byte count.
    pub fn total_size(&self) -> io::Result<u64> {
        self.prefix_size()
            .checked_add(self.tree_size()?)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "verity hash volume size overflows",
                )
            })
    }

    /// Decodes and validates one standard 512-byte dm-verity header record.
    ///
    /// The record excludes padding through the hash-block boundary. Reading
    /// the record and positioning a hash stream are the caller's transport
    /// responsibilities.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidData`] when the record is malformed,
    /// noncanonical, or unsupported.
    pub fn decode(bytes: [u8; RECORD_SIZE]) -> io::Result<Self> {
        let invalid = |message| io::Error::new(io::ErrorKind::InvalidData, message);
        let mut input = bytes.as_slice();

        if input.field()? != SIGNATURE {
            return Err(invalid("invalid verity signature"));
        }
        if u32::from_le_bytes(input.field()?) != 1 {
            return Err(invalid("unsupported verity version"));
        }

        let hash_type = HashType::from_le_bytes(input.field()?)
            .ok_or_else(|| invalid("unsupported verity hash type"))?;
        let uuid = input.field()?;

        let name = input.field::<ALGORITHM_SIZE>()?;
        let end = name
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(name.len());
        if name[end..].iter().any(|byte| *byte != 0) {
            return Err(invalid("invalid hash algorithm encoding"));
        }
        let algorithm = std::str::from_utf8(&name[..end])
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
            .parse()?;

        let data_size = NonZero::new(u32::from_le_bytes(input.field()?))
            .and_then(BlockSize::<Constraint>::from_bytes)
            .ok_or_else(|| invalid("invalid verity data block size"))?;
        let hash_size = NonZero::new(u32::from_le_bytes(input.field()?))
            .and_then(BlockSize::<Constraint>::from_bytes)
            .ok_or_else(|| invalid("invalid verity hash block size"))?;
        let count = NonZero::<u64>::new(u64::from_le_bytes(input.field()?))
            .ok_or_else(|| invalid("verity data block count must be nonzero"))?;

        let salt_size = usize::from(u16::from_le_bytes(input.field()?));
        let reserved = input.field::<RESERVED_SIZE>()?;
        let salt_bytes = input.field::<SALT_SIZE>()?;
        let padding = input.field::<PADDING_SIZE>()?;
        debug_assert_eq!(input, []);

        if salt_size > SALT_SIZE {
            return Err(invalid("salt exceeds 256 bytes"));
        }
        if reserved
            .iter()
            .chain(&salt_bytes[salt_size..])
            .any(|byte| *byte != 0)
        {
            return Err(invalid("nonzero salt padding"));
        }
        if padding.iter().any(|byte| *byte != 0) {
            return Err(invalid("nonzero verity superblock padding"));
        }
        let salt =
            Salt::new(&salt_bytes[..salt_size]).ok_or_else(|| invalid("salt exceeds 256 bytes"))?;

        Ok(Self {
            uuid,
            parameters: Parameters {
                hash_type,
                algorithm,
                salt,
                data: Geometry {
                    size: data_size,
                    count,
                },
                hash: hash_size,
            },
        })
    }

    /// Encodes this value as one standard 512-byte dm-verity header record.
    ///
    /// The returned record excludes padding through the hash-block boundary.
    /// Writing the record and any required padding are the caller's transport
    /// responsibilities.
    pub fn encode(&self) -> [u8; RECORD_SIZE] {
        let mut bytes = [0; RECORD_SIZE];
        let mut output = bytes.as_mut_slice();

        let name = self.parameters.algorithm.as_ref().as_bytes();

        output.push(SIGNATURE);
        output.push(1u32.to_le_bytes());
        output.push(self.parameters.hash_type.to_le_bytes());
        output.push(self.uuid);
        output.push(name);
        output.skip(ALGORITHM_SIZE - name.len());
        output.push(self.parameters.data.size.bytes().get().to_le_bytes());
        output.push(self.parameters.hash.bytes().get().to_le_bytes());
        output.push(self.parameters.data.count.get().to_le_bytes());
        output.push((self.parameters.salt.len() as u16).to_le_bytes());
        output.skip(RESERVED_SIZE);
        output.push(self.parameters.salt.as_ref());
        output.skip(SALT_SIZE - self.parameters.salt.len());
        output.skip(PADDING_SIZE);

        debug_assert_eq!(output, []);
        bytes
    }
}

trait Push {
    fn push(&mut self, bytes: impl AsRef<[u8]>);
    fn skip(&mut self, length: usize);
}

impl Push for &mut [u8] {
    fn push(&mut self, bytes: impl AsRef<[u8]>) {
        let bytes = bytes.as_ref();
        let output = std::mem::take(self);
        output[..bytes.len()].copy_from_slice(bytes);
        *self = &mut output[bytes.len()..];
    }

    fn skip(&mut self, length: usize) {
        *self = &mut std::mem::take(self)[length..];
    }
}

trait Field {
    fn field<const N: usize>(&mut self) -> io::Result<[u8; N]>;
}

impl Field for &[u8] {
    fn field<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let (field, remaining) = std::mem::take(self)
            .split_first_chunk::<N>()
            .ok_or_else(|| io::Error::other("verity header fields exceed the 512-byte record"))?;
        *self = remaining;
        Ok(*field)
    }
}
