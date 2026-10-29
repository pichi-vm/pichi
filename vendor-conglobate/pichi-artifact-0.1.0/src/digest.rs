// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Content-addressable digests. See crate-level docs for D-01.

use std::fmt;
use std::str::FromStr;

use sha2::{Digest as _, Sha256};
use thiserror::Error;

/// Typed digest sum-type. Per D-01.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Digest {
    /// SHA-256 (32 bytes).
    Sha256([u8; 32]),
}

/// Errors from parsing a digest string.
#[derive(Debug, Error)]
pub enum DigestParseError {
    /// Algorithm prefix not recognised (only `sha256:` is supported in v0.8).
    #[error("unknown algorithm prefix: expected 'sha256:', got '{0}'")]
    UnknownAlgorithm(String),
    /// Hex decoding failed.
    #[error("invalid hex in digest: {0}")]
    InvalidHex(#[from] hex::FromHexError),
    /// Hex string had wrong length for the algorithm.
    #[error("wrong digest length: expected {expected} hex chars, got {actual}")]
    WrongLength {
        /// Required length.
        expected: usize,
        /// Length actually seen.
        actual: usize,
    },
}

impl Digest {
    /// Hash arbitrary bytes with SHA-256, return a typed [`Digest`].
    pub fn from_bytes_sha256(data: &[u8]) -> Self {
        let hash = Sha256::digest(data);
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&hash);
        Self::Sha256(bytes)
    }

    /// Return the algorithm name string (for OCI media-type prefix matching).
    pub fn algo(&self) -> &'static str {
        match self {
            Self::Sha256(_) => "sha256",
        }
    }

    /// The raw 32-byte SHA-256 digest. Callers that need the bare bytes (e.g.
    /// dm-verity salt/uuid derivation) use this instead of re-hex-decoding the
    /// `Display` form.
    #[must_use]
    pub fn as_sha256_array(&self) -> [u8; 32] {
        match self {
            Self::Sha256(b) => *b,
        }
    }

    /// Return the lowercase hex string (64 chars for SHA-256).
    pub fn hex(&self) -> String {
        match self {
            Self::Sha256(b) => hex::encode(b),
        }
    }

    /// Return the raw bytes of the digest.
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Sha256(b) => b,
        }
    }
}

impl FromStr for Digest {
    type Err = DigestParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(hex_str) = s.strip_prefix("sha256:") {
            if hex_str.len() != 64 {
                return Err(DigestParseError::WrongLength {
                    expected: 64,
                    actual: hex_str.len(),
                });
            }
            let bytes = hex::decode(hex_str)?;
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes);
            Ok(Self::Sha256(arr))
        } else {
            let prefix = s.split(':').next().unwrap_or(s);
            Err(DigestParseError::UnknownAlgorithm(prefix.to_string()))
        }
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.algo(), self.hex())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known SHA-256 of "hello": 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
    const HELLO_HEX: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[test]
    fn from_bytes_sha256_known_value() {
        let d = Digest::from_bytes_sha256(b"hello");
        assert_eq!(d.hex(), HELLO_HEX);
    }

    #[test]
    fn from_str_round_trip() {
        let s = format!("sha256:{HELLO_HEX}");
        let d: Digest = s.parse().unwrap();
        assert_eq!(d.to_string(), s);
    }

    #[test]
    fn from_bytes_and_from_str_agree() {
        let by_bytes = Digest::from_bytes_sha256(b"hello");
        let by_str: Digest = format!("sha256:{HELLO_HEX}").parse().unwrap();
        assert_eq!(by_bytes, by_str);
    }

    #[test]
    fn display_round_trips() {
        let d = Digest::from_bytes_sha256(b"hello");
        let s = d.to_string();
        let d2: Digest = s.parse().unwrap();
        assert_eq!(d, d2);
    }

    #[test]
    fn algo_returns_sha256() {
        let d = Digest::from_bytes_sha256(b"x");
        assert_eq!(d.algo(), "sha256");
    }

    #[test]
    fn hex_length_is_64() {
        let d = Digest::from_bytes_sha256(b"x");
        assert_eq!(d.hex().len(), 64);
    }

    #[test]
    fn as_bytes_length_is_32() {
        let d = Digest::from_bytes_sha256(b"x");
        assert_eq!(d.as_bytes().len(), 32);
    }

    #[test]
    fn err_unknown_algorithm() {
        let err = "md5:abc".parse::<Digest>().unwrap_err();
        assert!(matches!(err, DigestParseError::UnknownAlgorithm(_)));
    }

    #[test]
    fn err_wrong_length() {
        let err = "sha256:tooshort".parse::<Digest>().unwrap_err();
        match err {
            DigestParseError::WrongLength { expected, actual } => {
                assert_eq!(expected, 64);
                assert_eq!(actual, 8);
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn err_invalid_hex() {
        // 64 chars but invalid hex (z is not valid hex)
        let bad_hex = "z".repeat(64);
        let err = format!("sha256:{bad_hex}").parse::<Digest>().unwrap_err();
        assert!(matches!(err, DigestParseError::InvalidHex(_)));
    }
}
