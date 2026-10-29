// SPDX-License-Identifier: Apache-2.0

//! Linux dm-verity target descriptions and activation policy.

use devmap_linux::{DevId, targets::Version};

use crate::Parameters;

mod builder;
mod codec;
mod info;
mod policy;

pub use builder::Builder;
pub use info::Info;
pub use policy::{CorruptionPolicy, IoErrorPolicy};

/// A validated Linux dm-verity table description.
///
/// Construct with default parameters through [`Verity::builder`], explicit
/// parameters through [`Parameters::builder`], or an on-disk header through
/// [`Header::builder`](crate::Header::builder). The root must come
/// from an independently trusted source. Activate in a read-only table.
///
/// Table parsing produces the same validated description. Read back the format
/// through [`Self::parameters`], the mapping inputs through [`Self::data`],
/// [`Self::hash`], and [`Self::root`], and activation settings through the methods
/// named after their builder counterparts. A target retains neither a header
/// UUID nor evidence that an on-disk header exists.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Verity {
    parameters: Parameters,
    data: DevId,
    hash: DevId,
    hash_start: u64,
    corruption_policy: CorruptionPolicy,
    io_error_policy: IoErrorPolicy,
    ignore_zero_blocks: bool,
    check_at_most_once: bool,
    try_verify_in_tasklet: bool,
    root_hash_sig_key_desc: Option<String>,
    root: Box<[u8]>,
    data_sectors: u64,
}

impl Verity {
    /// Borrows the hash-tree format used to construct or parse this target.
    pub const fn parameters(&self) -> &Parameters {
        &self.parameters
    }

    /// Returns the device containing the protected data.
    pub const fn data(&self) -> DevId {
        self.data
    }

    /// Returns the device containing the hash tree.
    pub const fn hash(&self) -> DevId {
        self.hash
    }

    /// Borrows the root digest, without hex encoding.
    ///
    /// Reading this value from a table does not establish its authenticity.
    pub fn root(&self) -> &[u8] {
        &self.root
    }

    /// Returns the tree offset in hash-sized blocks, as set by [`Builder::hash_start`].
    pub const fn hash_start(&self) -> u64 {
        self.hash_start
    }

    /// Returns the selected action for a hash mismatch.
    pub const fn corruption_policy(&self) -> CorruptionPolicy {
        self.corruption_policy
    }

    /// Returns the selected action for a backing-device I/O error.
    pub const fn io_error_policy(&self) -> IoErrorPolicy {
        self.io_error_policy
    }

    /// Returns whether blocks with the expected zero-block digest are returned as zeroes.
    pub const fn ignore_zero_blocks(&self) -> bool {
        self.ignore_zero_blocks
    }

    /// Returns whether data blocks are verified only on their first read.
    pub const fn check_at_most_once(&self) -> bool {
        self.check_at_most_once
    }

    /// Returns whether verification may run in a kernel bottom half.
    pub const fn try_verify_in_tasklet(&self) -> bool {
        self.try_verify_in_tasklet
    }

    /// Borrows the kernel key description selected by [`Builder::root_hash_sig_key_desc`].
    ///
    /// This is a key description, not the signature itself.
    pub fn root_hash_sig_key_desc(&self) -> Option<&str> {
        self.root_hash_sig_key_desc.as_deref()
    }

    /// Returns the full row length in 512-byte sectors.
    pub const fn data_sectors(&self) -> u64 {
        self.data_sectors
    }

    fn supports_version(&self, version: Version) -> bool {
        let release = (version.major, version.minor, version.patch);
        version.major == 1
            && (!matches!(
                self.corruption_policy,
                CorruptionPolicy::Ignore | CorruptionPolicy::Restart
            ) || release >= (1, 2, 0))
            && (!self.ignore_zero_blocks || release >= (1, 3, 0))
            && (!self.check_at_most_once || release >= (1, 4, 0))
            && (self.root_hash_sig_key_desc.is_none() || release >= (1, 5, 0))
            && (self.corruption_policy != CorruptionPolicy::Panic || release >= (1, 7, 0))
            && (!self.try_verify_in_tasklet || release >= (1, 9, 0))
            && (self.io_error_policy == IoErrorPolicy::Error || release >= (1, 10, 0))
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::num::NonZero;

    use devmap_linux::{targets::Parse, targets::Target};

    use super::*;

    #[test]
    #[allow(clippy::too_many_lines)] // Version-boundary fixtures for independent target options.
    fn version_gates_optional_verity_parameters() {
        let data = DevId::new(252, 1).unwrap();
        let hash = DevId::new(252, 2).unwrap();
        let base = Verity::builder(data, hash, NonZero::new(1).unwrap(), vec![0; 32])
            .unwrap()
            .build();

        let cases = [
            (
                Verity {
                    corruption_policy: CorruptionPolicy::Ignore,
                    ..base.clone()
                },
                [1, 1, 0],
                [1, 2, 0],
            ),
            (
                Verity {
                    corruption_policy: CorruptionPolicy::Restart,
                    ..base.clone()
                },
                [1, 1, 0],
                [1, 2, 0],
            ),
            (
                Verity {
                    ignore_zero_blocks: true,
                    ..base.clone()
                },
                [1, 2, 0],
                [1, 3, 0],
            ),
            (
                Verity {
                    check_at_most_once: true,
                    ..base.clone()
                },
                [1, 3, 0],
                [1, 4, 0],
            ),
            (
                Verity {
                    root_hash_sig_key_desc: Some("test-key".into()),
                    ..base.clone()
                },
                [1, 4, 0],
                [1, 5, 0],
            ),
            (
                Verity {
                    corruption_policy: CorruptionPolicy::Panic,
                    ..base.clone()
                },
                [1, 6, 0],
                [1, 7, 0],
            ),
            (
                Verity {
                    try_verify_in_tasklet: true,
                    ..base.clone()
                },
                [1, 8, 0],
                [1, 9, 0],
            ),
            (
                Verity {
                    io_error_policy: IoErrorPolicy::Restart,
                    ..base.clone()
                },
                [1, 9, 0],
                [1, 10, 0],
            ),
            (
                Verity {
                    io_error_policy: IoErrorPolicy::Panic,
                    ..base.clone()
                },
                [1, 9, 0],
                [1, 10, 0],
            ),
        ];

        assert!(
            base.encode(
                Version {
                    major: 1,
                    minor: 0,
                    patch: 0
                },
                &mut Vec::new()
            )
            .is_ok()
        );
        assert!(
            base.encode(
                Version {
                    major: 2,
                    minor: 0,
                    patch: 0
                },
                &mut Vec::new()
            )
            .is_err()
        );

        for (target, before, minimum) in cases {
            let [major, minor, patch] = before;
            let before = Version {
                major,
                minor,
                patch,
            };
            let [major, minor, patch] = minimum;
            let minimum = Version {
                major,
                minor,
                patch,
            };
            assert!(
                matches!(target.encode(before, &mut Vec::new()), Err(error) if error.kind() == io::ErrorKind::Unsupported)
            );
            assert!(target.encode(minimum, &mut Vec::new()).is_ok());

            let mut bytes = Vec::new();
            target.encode(minimum, &mut bytes).unwrap();
            let text = String::from_utf8(bytes).unwrap();
            let error = Verity::parse(&text, before).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
            assert_eq!(Verity::parse(&text, minimum).unwrap(), target);
        }
    }
}
