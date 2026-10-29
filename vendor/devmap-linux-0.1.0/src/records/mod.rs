// SPDX-License-Identifier: Apache-2.0

//! Borrowed records returned by device-mapper queries.
//!
//! Traverse replies with [`Records`]. Table and runtime queries yield a [`Row`]
//! whose [`Table`] or [`Info`] mode determines the target response type.

mod dependencies;
mod devices;
mod mode;
mod row;
mod rows;
mod versions;

pub(crate) use dependencies::Dependencies;
pub(crate) use devices::Devices;
pub use mode::{Info, Table};
pub use row::Row;
pub(crate) use rows::Rows;
pub(crate) use versions::Versions;

/// Fallible iteration over a reply whose items may borrow its storage.
///
/// Unlike [`Iterator`], each call to [`next`](Self::next) can lend data from
/// the reply itself. Finish using borrowed data before advancing again.
/// Owned items, such as dependency identifiers, can be retained directly.
///
/// # Examples
///
/// Copy names only when they must outlive the current record.
///
/// ```no_run
/// use devmap_linux::{Control, records::Records};
///
/// # fn example() -> std::io::Result<()> {
/// let mut reply = Control::open()?.devices::<4096>()?;
/// let mut names = Vec::new();
/// while let Some(record) = reply.next() {
///     let (name, _) = record?;
///     names.push(name.to_owned());
/// }
/// # Ok(())
/// # }
/// ```
pub trait Records {
    /// An item that may borrow the reply for `'a`.
    type Item<'a>
    where
        Self: 'a;

    /// Returns the next item, or `None` when traversal is complete.
    ///
    /// Crate-provided replies decode each record without allocation. Borrowed
    /// items remain valid until their last use, before another mutable access
    /// to the reply.
    ///
    /// # Errors
    ///
    /// Malformed records yield an error. After any decoding error or exhaustion,
    /// subsequent calls return `None`. Errors from separately parsing a returned
    /// [`Row`] do not end traversal.
    fn next(&mut self) -> Option<std::io::Result<Self::Item<'_>>>;
}
