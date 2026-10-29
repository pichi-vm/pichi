// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Hash algorithm registry. Hardcoded dispatch over the v1 spec
//! whitelist: `sha256` (REQUIRED) + `sha512` (OPTIONAL).
//!
//! No `Default` impl — an unknown algorithm string is always
//! `io::ErrorKind::InvalidData`, never a silent fallback
//! under the specification’s Parameter Whitelist.

use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Algorithm {
    Sha256,
    Sha512,
}

impl Algorithm {
    #[cfg(feature = "gpt")]
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
            Self::Sha512 => "sha512",
        }
    }

    /// Digest size in bytes.
    pub(crate) fn digest_size(self) -> usize {
        match self {
            Self::Sha256 => 32,
            Self::Sha512 => 64,
        }
    }
}

/// Parse an algorithm name as written in the verity superblock.
/// NUL trimming is the caller's responsibility.
pub(crate) fn parse(name: &str) -> io::Result<Algorithm> {
    match name {
        "sha256" => Ok(Algorithm::Sha256),
        "sha512" => Ok(Algorithm::Sha512),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported algorithm: {other}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_whitelisted_algorithms() {
        assert_eq!(parse("sha256").unwrap(), Algorithm::Sha256);
        assert_eq!(parse("sha512").unwrap(), Algorithm::Sha512);
    }

    #[test]
    fn rejects_non_whitelisted() {
        for n in [
            "", "md5", "sha1", "sha224", "sha384", "sha3-256", "blake3", "sha2",
        ] {
            let e = parse(n).unwrap_err();
            assert_eq!(
                e.kind(),
                io::ErrorKind::InvalidData,
                "{n} should be rejected with InvalidData"
            );
        }
    }

    #[test]
    fn digest_sizes_are_canonical() {
        assert_eq!(Algorithm::Sha256.digest_size(), 32);
        assert_eq!(Algorithm::Sha512.digest_size(), 64);
    }
}
