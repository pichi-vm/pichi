// SPDX-License-Identifier: Apache-2.0

//! Marker types selecting which response a [`Row`](crate::records::Row) parses.
//!
//! [`Table`] selects [`Target::Table`]; [`Info`] selects [`Target::Info`].
//! These types have no values and need not be constructed by callers.

use crate::targets::{Parse, Target};

/// Selects table read-back from [`Control::table`](crate::Control::table).
///
/// Parsing produces [`Target::Table`], which may differ from the original input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Table {}

/// Selects runtime information from [`Control::info`](crate::Control::info).
///
/// Parsing produces [`Target::Info`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Info {}

mod private {
    use super::{Info, Parse, Table, Target};

    /// Associates a response mode with the corresponding type from a [`Target`].
    ///
    /// This trait is sealed; the supported modes are [`Table`] and [`Info`].
    pub trait Mode: 'static {
        /// Type produced for target family `T` in this response mode.
        type Parsed<T: Target>: Parse;
    }

    impl Mode for Table {
        type Parsed<T: Target> = T::Table;
    }

    impl Mode for Info {
        type Parsed<T: Target> = T::Info;
    }
}

pub(super) use private::Mode;
