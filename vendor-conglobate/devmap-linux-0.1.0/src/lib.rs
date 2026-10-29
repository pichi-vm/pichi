// SPDX-License-Identifier: Apache-2.0

#![cfg_attr(docsrs, feature(doc_cfg))]
#![warn(missing_docs)]
#![doc = include_str!("../README.md")]

pub mod records;
pub mod targets;

mod abi;
mod control;
mod dev_id;
mod status;
mod table;

pub use control::Control;
pub use dev_id::DevId;
pub use status::Status;
pub use table::Table;
