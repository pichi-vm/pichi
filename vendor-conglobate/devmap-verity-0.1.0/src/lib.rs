// SPDX-License-Identifier: Apache-2.0

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::similar_names
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![warn(missing_docs)]
#![doc = include_str!("../README.md")]

#[cfg(target_os = "linux")]
#[cfg_attr(docsrs, doc(cfg(target_os = "linux")))]
pub mod target;

mod algorithm;
mod constraint;
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
mod format;
mod hash_type;
mod header;
mod parameters;
mod salt;

pub use algorithm::Algorithm;
pub use constraint::Constraint;
pub use hash_type::HashType;
pub use header::Header;
pub use parameters::Parameters;
pub use salt::Salt;

#[cfg(all(
    feature = "tokio",
    any(
        feature = "sha1",
        feature = "sha2",
        feature = "sha3",
        feature = "ripemd",
        feature = "whirlpool",
        feature = "streebog",
        feature = "sm3",
        feature = "blake2"
    )
))]
#[cfg_attr(
    docsrs,
    doc(cfg(all(
        feature = "tokio",
        any(
            feature = "sha1",
            feature = "sha2",
            feature = "sha3",
            feature = "ripemd",
            feature = "whirlpool",
            feature = "streebog",
            feature = "sm3",
            feature = "blake2"
        )
    )))
)]
pub use format::AsyncFormat;
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
pub use format::Format;
