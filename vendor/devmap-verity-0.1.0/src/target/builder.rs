// SPDX-License-Identifier: Apache-2.0

use std::{io, num::NonZero};

use devmap_core::{BlockSize, Geometry};
use devmap_linux::DevId;

use super::{CorruptionPolicy, IoErrorPolicy, Verity};
use crate::{Algorithm, HashType, Header, Parameters, Salt};

/// Configures a Linux dm-verity target.
///
/// Create a builder with [`Verity::builder`] for a headerless tree or
/// [`Header::builder`] for a tree described by an on-disk header.
/// Use [`Parameters::builder`] for an explicit headerless format.
#[derive(Debug, Clone)]
pub struct Builder {
    parameters: Parameters,
    data: DevId,
    hash: DevId,
    hash_start: u64,
    root: Box<[u8]>,
    corruption_policy: CorruptionPolicy,
    io_error_policy: IoErrorPolicy,
    ignore_zero_blocks: bool,
    check_at_most_once: bool,
    try_verify_in_tasklet: bool,
    root_hash_sig_key_desc: Option<String>,
    data_sectors: u64,
    tree_sectors: u64,
}

impl Builder {
    pub(super) fn new(
        data: DevId,
        hash: DevId,
        data_blocks: NonZero<u64>,
        root: impl Into<Box<[u8]>>,
    ) -> io::Result<Self> {
        Self::with_parameters(
            &Parameters {
                hash_type: HashType::default(),
                algorithm: Algorithm::default(),
                salt: Salt::default(),
                data: Geometry {
                    size: BlockSize::default(),
                    count: data_blocks,
                },
                hash: BlockSize::default(),
            },
            data,
            hash,
            0,
            root.into(),
        )
    }

    fn with_parameters(
        parameters: &Parameters,
        data: DevId,
        hash: DevId,
        hash_start: u64,
        root: Box<[u8]>,
    ) -> io::Result<Self> {
        let data_sectors = parameters.data_size()? / 512;
        let tree_sectors = parameters.tree_size()? / 512;
        if root.len() != parameters.algorithm.digest_size() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "root digest length does not match the hash algorithm",
            ));
        }

        Self {
            parameters: *parameters,
            data,
            hash,
            hash_start: 0,
            root,
            corruption_policy: CorruptionPolicy::Error,
            io_error_policy: IoErrorPolicy::Error,
            ignore_zero_blocks: false,
            check_at_most_once: false,
            try_verify_in_tasklet: false,
            root_hash_sig_key_desc: None,
            data_sectors,
            tree_sectors,
        }
        .hash_start(hash_start)
    }

    /// Changes the tree offset in hash-sized blocks.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] if the resulting hash extent
    /// overflows.
    pub fn hash_start(mut self, value: u64) -> io::Result<Self> {
        let block_sectors = u64::from(self.parameters.hash.bytes().get() / 512);
        value
            .checked_mul(block_sectors)
            .and_then(|start| start.checked_add(self.tree_sectors))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "verity hash extent overflows")
            })?;
        self.hash_start = value;
        Ok(self)
    }

    /// Selects corruption handling.
    #[must_use]
    pub const fn corruption_policy(mut self, value: CorruptionPolicy) -> Self {
        self.corruption_policy = value;
        self
    }

    /// Selects I/O-error handling.
    #[must_use]
    pub const fn io_error_policy(mut self, value: IoErrorPolicy) -> Self {
        self.io_error_policy = value;
        self
    }

    /// Selects authenticated zero-block handling.
    #[must_use]
    pub const fn ignore_zero_blocks(mut self, value: bool) -> Self {
        self.ignore_zero_blocks = value;
        self
    }

    /// Selects first-read-only verification.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] if the data extent contains
    /// more blocks than the kernel can track for this mode.
    pub fn check_at_most_once(mut self, value: bool) -> io::Result<Self> {
        if value && self.parameters.data.count.get() > i32::MAX as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "too many data blocks for check_at_most_once",
            ));
        }
        self.check_at_most_once = value;
        Ok(self)
    }

    /// Requests kernel bottom-half verification.
    #[must_use]
    pub const fn try_verify_in_tasklet(mut self, value: bool) -> Self {
        self.try_verify_in_tasklet = value;
        self
    }

    /// Selects a kernel root-signature key.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] for an empty description or
    /// one containing a NUL byte.
    pub fn root_hash_sig_key_desc(mut self, value: impl Into<String>) -> io::Result<Self> {
        let value = value.into();
        if value.is_empty() || value.contains('\0') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid signature key description",
            ));
        }
        self.root_hash_sig_key_desc = Some(value);
        Ok(self)
    }

    /// Converts this valid configuration into an owned target description.
    pub fn build(self) -> Verity {
        Verity {
            parameters: self.parameters,
            data: self.data,
            hash: self.hash,
            hash_start: self.hash_start,
            corruption_policy: self.corruption_policy,
            io_error_policy: self.io_error_policy,
            ignore_zero_blocks: self.ignore_zero_blocks,
            check_at_most_once: self.check_at_most_once,
            try_verify_in_tasklet: self.try_verify_in_tasklet,
            root_hash_sig_key_desc: self.root_hash_sig_key_desc,
            root: self.root,
            data_sectors: self.data_sectors,
        }
    }
}

impl Verity {
    /// Starts a headerless target with default hashing, block sizes, and policy.
    ///
    /// The hash tree starts at hash block zero.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] if the root length does not
    /// match the default algorithm or the resulting layout overflows.
    pub fn builder(
        data: DevId,
        hash: DevId,
        data_blocks: NonZero<u64>,
        root: impl Into<Box<[u8]>>,
    ) -> io::Result<Builder> {
        Builder::new(data, hash, data_blocks, root)
    }
}

impl Header {
    /// Starts a target builder with this header's format metadata.
    ///
    /// The hash tree defaults to block one, immediately after a standard
    /// header block. The UUID is not a target-table parameter and is not retained.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] if the header and root do not
    /// describe a valid kernel target or the resulting layout overflows.
    pub fn builder(
        &self,
        data: DevId,
        hash: DevId,
        root: impl Into<Box<[u8]>>,
    ) -> io::Result<Builder> {
        Builder::with_parameters(&self.parameters, data, hash, 1, root.into())
    }
}

impl Parameters {
    /// Starts a headerless target builder with these hash-tree parameters.
    ///
    /// The tree defaults to hash block zero. Use [`Builder::hash_start`] for
    /// another offset. The root digest must come from independently trusted storage.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] if the root length does not match
    /// the algorithm or the resulting layout overflows.
    pub fn builder(
        &self,
        data: DevId,
        hash: DevId,
        root: impl Into<Box<[u8]>>,
    ) -> io::Result<Builder> {
        Builder::with_parameters(self, data, hash, 0, root.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn devices() -> (DevId, DevId) {
        (DevId::new(252, 1).unwrap(), DevId::new(252, 2).unwrap())
    }

    fn header(data_blocks: u64) -> Header {
        Header {
            uuid: [0; 16],
            parameters: Parameters {
                hash_type: HashType::default(),
                algorithm: Algorithm::default(),
                salt: Salt::default(),
                data: Geometry {
                    size: BlockSize::default(),
                    count: NonZero::new(data_blocks).unwrap(),
                },
                hash: BlockSize::default(),
            },
        }
    }

    #[test]
    fn default_and_custom_tree_offsets_are_preserved() {
        let (data, hash) = devices();
        let headerless = Verity::builder(data, hash, NonZero::new(2).unwrap(), vec![0; 32])
            .unwrap()
            .build();
        assert_eq!(headerless.hash_start, 0);

        let header_backed = header(2).builder(data, hash, vec![0; 32]).unwrap().build();
        assert_eq!(header_backed.hash_start, 1);

        let custom = header(2)
            .builder(data, hash, vec![0; 32])
            .unwrap()
            .hash_start(7)
            .unwrap()
            .build();
        assert_eq!(custom.hash_start, 7);
    }

    #[test]
    fn tree_offset_extent_uses_checked_sector_arithmetic() {
        let (data, hash) = devices();
        let builder = Verity::builder(data, hash, NonZero::new(2).unwrap(), vec![0; 32]).unwrap();

        // Two data blocks produce one 4096-byte tree block: eight sectors.
        let maximum = (u64::MAX - 8) / 8;
        assert!(builder.clone().hash_start(maximum).is_ok());

        let error = builder.hash_start(maximum + 1).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
