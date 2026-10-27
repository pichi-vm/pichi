// SPDX-License-Identifier: Apache-2.0

//! Target definitions, versioned response parsing, and built-in mapping targets.
//!
//! Implement [`Target`] and [`Parse`] to support another kernel target. [`Version`]
//! identifies the installed target implementation.
//!
//! Add a target to a [`Table`](crate::Table), then load and activate it through
//! [`Control`](crate::Control). See the [table example](crate::Table#examples).
//! Read responses with [`Row::parse`](crate::records::Row::parse).

mod error;
mod linear;
mod target;
mod version;
mod zero;

pub use error::Error;
pub use linear::Linear;
pub use target::{Parse, Target};
pub use version::Version;
pub use zero::Zero;
