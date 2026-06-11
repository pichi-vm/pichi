//! Hash algorithm registry. Hardcoded dispatch over the v1 spec
//! whitelist: `sha256` (REQUIRED) + `sha512` (OPTIONAL).
//!
//! No `Default` impl — an unknown algorithm string is always
//! [`CarapaceError::UnsupportedAlgorithm`], never a silent fallback
//! (PITFALLS CRITICAL-7 prevention).

use crate::CarapaceError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Algorithm {
    Sha256,
    Sha512,
}

impl Algorithm {
    pub fn name(self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
            Self::Sha512 => "sha512",
        }
    }

    /// Digest size in bytes. With sha384/sha3 dropped from the v1
    /// whitelist, `digest_size == slot_size` is now an invariant.
    pub fn digest_size(self) -> usize {
        match self {
            Self::Sha256 => 32,
            Self::Sha512 => 64,
        }
    }
}

/// Parse an algorithm name as written in the verity superblock.
/// NUL trimming is the caller's responsibility.
pub(crate) fn parse(name: &str) -> Result<Algorithm, CarapaceError> {
    match name {
        "sha256" => Ok(Algorithm::Sha256),
        "sha512" => Ok(Algorithm::Sha512),
        other => Err(CarapaceError::UnsupportedAlgorithm(other.to_string())),
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
            assert!(matches!(
                parse(n),
                Err(CarapaceError::UnsupportedAlgorithm(_))
            ));
        }
    }

    #[test]
    fn digest_sizes_are_canonical() {
        assert_eq!(Algorithm::Sha256.digest_size(), 32);
        assert_eq!(Algorithm::Sha512.digest_size(), 64);
    }
}
