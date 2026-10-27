// SPDX-License-Identifier: Apache-2.0

use core::{fmt, str::FromStr};
use std::io;

#[cfg(feature = "blake2")]
use blake2::{Blake2b, Blake2s};
#[cfg(any(
    feature = "sha1",
    feature = "sha2",
    feature = "sha3",
    feature = "ripemd",
    feature = "whirlpool",
    feature = "streebog",
    feature = "sm3",
    feature = "blake2"
))]
use digest::DynDigest;
#[cfg(feature = "blake2")]
use digest::consts::{U16, U20, U28, U32, U48, U64};
#[cfg(feature = "ripemd")]
use ripemd::Ripemd160;
#[cfg(feature = "sha1")]
use sha1::Sha1;
#[cfg(feature = "sha2")]
use sha2::{Sha224, Sha256, Sha384, Sha512};
#[cfg(feature = "sha3")]
use sha3::{Sha3_224, Sha3_256, Sha3_384, Sha3_512};
#[cfg(feature = "sm3")]
use sm3::Sm3;
#[cfg(feature = "streebog")]
use streebog::{Streebog256, Streebog512};
#[cfg(feature = "whirlpool")]
use whirlpool::Whirlpool;

/// A dm-verity hash algorithm.
///
/// The default is [`Sha256`](Self::Sha256). [`Display`](fmt::Display) and
/// [`AsRef<str>`] return the kernel spelling documented on each variant;
/// [`FromStr`] accepts exactly those case-sensitive spellings.
///
/// Parsing and displaying names do not require hash-family features. Building a
/// hash tree with an unavailable algorithm returns [`io::ErrorKind::Unsupported`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Algorithm {
    /// SHA-1 (`sha1`), with a 20-byte digest.
    Sha1,

    /// SHA-224 (`sha224`), with a 28-byte digest.
    Sha224,
    /// SHA-256 (`sha256`), with a 32-byte digest.
    #[default]
    Sha256,
    /// SHA-384 (`sha384`), with a 48-byte digest.
    Sha384,
    /// SHA-512 (`sha512`), with a 64-byte digest.
    Sha512,

    /// RIPEMD-160 (`rmd160`), with a 20-byte digest.
    Ripemd160,

    /// Whirlpool (`wp512`), with a 64-byte digest.
    Whirlpool,

    /// SHA3-224 (`sha3-224`), with a 28-byte digest.
    Sha3_224,
    /// SHA3-256 (`sha3-256`), with a 32-byte digest.
    Sha3_256,
    /// SHA3-384 (`sha3-384`), with a 48-byte digest.
    Sha3_384,
    /// SHA3-512 (`sha3-512`), with a 64-byte digest.
    Sha3_512,

    /// Streebog-256 (`streebog256`), with a 32-byte digest.
    Streebog256,
    /// Streebog-512 (`streebog512`), with a 64-byte digest.
    Streebog512,

    /// SM3 (`sm3`), with a 32-byte digest.
    Sm3,

    /// BLAKE2b with a 160-bit output (`blake2b-160`).
    Blake2b160,
    /// BLAKE2b with a 256-bit output (`blake2b-256`).
    Blake2b256,
    /// BLAKE2b with a 384-bit output (`blake2b-384`).
    Blake2b384,
    /// BLAKE2b with a 512-bit output (`blake2b-512`).
    Blake2b512,

    /// BLAKE2s with a 128-bit output (`blake2s-128`).
    Blake2s128,
    /// BLAKE2s with a 160-bit output (`blake2s-160`).
    Blake2s160,
    /// BLAKE2s with a 224-bit output (`blake2s-224`).
    Blake2s224,
    /// BLAKE2s with a 256-bit output (`blake2s-256`).
    Blake2s256,
}

impl Algorithm {
    /// Returns the digest length in bytes, including for the root digest.
    ///
    /// This excludes padding used in hash-tree blocks and does not require
    /// the algorithm's hash-family feature to be enabled.
    ///
    /// ```
    /// use devmap_verity::Algorithm;
    ///
    /// const ROOT_SIZE: usize = Algorithm::Sha256.digest_size();
    /// assert_eq!(ROOT_SIZE, 32);
    /// ```
    pub const fn digest_size(self) -> usize {
        match self {
            Self::Sha1 | Self::Ripemd160 | Self::Blake2b160 | Self::Blake2s160 => 20,
            Self::Sha224 | Self::Sha3_224 | Self::Blake2s224 => 28,
            Self::Sha256
            | Self::Sha3_256
            | Self::Streebog256
            | Self::Sm3
            | Self::Blake2b256
            | Self::Blake2s256 => 32,
            Self::Sha384 | Self::Sha3_384 | Self::Blake2b384 => 48,
            Self::Sha512
            | Self::Whirlpool
            | Self::Sha3_512
            | Self::Streebog512
            | Self::Blake2b512 => 64,
            Self::Blake2s128 => 16,
        }
    }

    #[cfg(any(
        feature = "sha1",
        feature = "sha2",
        feature = "sha3",
        feature = "ripemd",
        feature = "whirlpool",
        feature = "streebog",
        feature = "sm3",
        feature = "blake2"
    ))]
    #[allow(unreachable_patterns)]
    pub(crate) fn hasher(self) -> Option<Box<dyn DynDigest + Send + Sync>> {
        match self {
            #[cfg(feature = "sha1")]
            Self::Sha1 => Some(Box::new(Sha1::default())),
            #[cfg(feature = "sha2")]
            Self::Sha224 => Some(Box::new(Sha224::default())),
            #[cfg(feature = "sha2")]
            Self::Sha256 => Some(Box::new(Sha256::default())),
            #[cfg(feature = "sha2")]
            Self::Sha384 => Some(Box::new(Sha384::default())),
            #[cfg(feature = "sha2")]
            Self::Sha512 => Some(Box::new(Sha512::default())),
            #[cfg(feature = "ripemd")]
            Self::Ripemd160 => Some(Box::new(Ripemd160::default())),
            #[cfg(feature = "whirlpool")]
            Self::Whirlpool => Some(Box::new(Whirlpool::default())),
            #[cfg(feature = "sha3")]
            Self::Sha3_224 => Some(Box::new(Sha3_224::default())),
            #[cfg(feature = "sha3")]
            Self::Sha3_256 => Some(Box::new(Sha3_256::default())),
            #[cfg(feature = "sha3")]
            Self::Sha3_384 => Some(Box::new(Sha3_384::default())),
            #[cfg(feature = "sha3")]
            Self::Sha3_512 => Some(Box::new(Sha3_512::default())),
            #[cfg(feature = "streebog")]
            Self::Streebog256 => Some(Box::new(Streebog256::default())),
            #[cfg(feature = "streebog")]
            Self::Streebog512 => Some(Box::new(Streebog512::default())),
            #[cfg(feature = "sm3")]
            Self::Sm3 => Some(Box::new(Sm3::default())),
            #[cfg(feature = "blake2")]
            Self::Blake2b160 => Some(Box::new(Blake2b::<U20>::default())),
            #[cfg(feature = "blake2")]
            Self::Blake2b256 => Some(Box::new(Blake2b::<U32>::default())),
            #[cfg(feature = "blake2")]
            Self::Blake2b384 => Some(Box::new(Blake2b::<U48>::default())),
            #[cfg(feature = "blake2")]
            Self::Blake2b512 => Some(Box::new(Blake2b::<U64>::default())),
            #[cfg(feature = "blake2")]
            Self::Blake2s128 => Some(Box::new(Blake2s::<U16>::default())),
            #[cfg(feature = "blake2")]
            Self::Blake2s160 => Some(Box::new(Blake2s::<U20>::default())),
            #[cfg(feature = "blake2")]
            Self::Blake2s224 => Some(Box::new(Blake2s::<U28>::default())),
            #[cfg(feature = "blake2")]
            Self::Blake2s256 => Some(Box::new(Blake2s::<U32>::default())),
            _ => None,
        }
    }
}

impl FromStr for Algorithm {
    type Err = io::Error;

    /// Parses a kernel algorithm name without trimming whitespace or accepting aliases.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidData`] for an unrecognized name.
    fn from_str(name: &str) -> Result<Self, Self::Err> {
        match name {
            "sha1" => Ok(Self::Sha1),
            "sha224" => Ok(Self::Sha224),
            "sha256" => Ok(Self::Sha256),
            "sha384" => Ok(Self::Sha384),
            "sha512" => Ok(Self::Sha512),
            "rmd160" => Ok(Self::Ripemd160),
            "wp512" => Ok(Self::Whirlpool),
            "sha3-224" => Ok(Self::Sha3_224),
            "sha3-256" => Ok(Self::Sha3_256),
            "sha3-384" => Ok(Self::Sha3_384),
            "sha3-512" => Ok(Self::Sha3_512),
            "streebog256" => Ok(Self::Streebog256),
            "streebog512" => Ok(Self::Streebog512),
            "sm3" => Ok(Self::Sm3),
            "blake2b-160" => Ok(Self::Blake2b160),
            "blake2b-256" => Ok(Self::Blake2b256),
            "blake2b-384" => Ok(Self::Blake2b384),
            "blake2b-512" => Ok(Self::Blake2b512),
            "blake2s-128" => Ok(Self::Blake2s128),
            "blake2s-160" => Ok(Self::Blake2s160),
            "blake2s-224" => Ok(Self::Blake2s224),
            "blake2s-256" => Ok(Self::Blake2s256),

            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported hash algorithm",
            )),
        }
    }
}

impl AsRef<str> for Algorithm {
    fn as_ref(&self) -> &str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha224 => "sha224",
            Self::Sha256 => "sha256",
            Self::Sha384 => "sha384",
            Self::Sha512 => "sha512",
            Self::Ripemd160 => "rmd160",
            Self::Whirlpool => "wp512",
            Self::Sha3_224 => "sha3-224",
            Self::Sha3_256 => "sha3-256",
            Self::Sha3_384 => "sha3-384",
            Self::Sha3_512 => "sha3-512",
            Self::Streebog256 => "streebog256",
            Self::Streebog512 => "streebog512",
            Self::Sm3 => "sm3",
            Self::Blake2b160 => "blake2b-160",
            Self::Blake2b256 => "blake2b-256",
            Self::Blake2b384 => "blake2b-384",
            Self::Blake2b512 => "blake2b-512",
            Self::Blake2s128 => "blake2s-128",
            Self::Blake2s160 => "blake2s-160",
            Self::Blake2s224 => "blake2s-224",
            Self::Blake2s256 => "blake2s-256",
        }
    }
}

impl fmt::Display for Algorithm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_ref().fmt(f)
    }
}
