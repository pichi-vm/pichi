// SPDX-License-Identifier: Apache-2.0

#![cfg_attr(docsrs, feature(doc_cfg))]
#![warn(missing_docs)]
#![doc = include_str!("../README.md")]

mod constraint;
#[cfg(target_os = "linux")]
mod discard;
mod exception;
mod header;
#[cfg(target_os = "linux")]
mod info;
#[cfg(target_os = "linux")]
mod merge;
#[cfg(target_os = "linux")]
mod origin;
#[cfg(target_os = "linux")]
mod parameters;
#[cfg(target_os = "linux")]
mod persistence;
#[cfg(target_os = "linux")]
mod persistent;
#[cfg(target_os = "linux")]
mod snapshot;

pub use constraint::Constraint;
#[cfg(target_os = "linux")]
#[cfg_attr(docsrs, doc(cfg(target_os = "linux")))]
pub use discard::Discard;
#[cfg(target_os = "linux")]
#[cfg_attr(docsrs, doc(cfg(target_os = "linux")))]
pub use info::Info;
#[cfg(target_os = "linux")]
#[cfg_attr(docsrs, doc(cfg(target_os = "linux")))]
pub use merge::Merge;
#[cfg(target_os = "linux")]
#[cfg_attr(docsrs, doc(cfg(target_os = "linux")))]
pub use origin::Origin;
#[cfg(target_os = "linux")]
#[cfg_attr(docsrs, doc(cfg(target_os = "linux")))]
pub use parameters::Parameters;
#[cfg(target_os = "linux")]
#[cfg_attr(docsrs, doc(cfg(target_os = "linux")))]
pub use persistence::Persistence;
#[cfg(target_os = "linux")]
#[cfg_attr(docsrs, doc(cfg(target_os = "linux")))]
pub use persistent::Persistent;
#[cfg(target_os = "linux")]
#[cfg_attr(docsrs, doc(cfg(target_os = "linux")))]
pub use snapshot::Snapshot;

pub use exception::Exception;
pub use header::Header;
