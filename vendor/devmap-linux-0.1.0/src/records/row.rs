// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::io;
use std::marker::PhantomData;

use devmap_core::Span;

use super::mode::Mode;
use crate::targets::{Parse, Target, Version};

/// A borrowed sector range and target response from a device-mapper table.
///
/// Returned by [`Control::table`](crate::Control::table) or
/// [`Control::info`](crate::Control::info). The mode selects whether
/// [`parse`](Self::parse) returns [`Target::Table`] or [`Target::Info`].
/// Parameters are validated and parsed only when requested, so rows can be
/// skipped using [`name`](Self::name). Debug output redacts parameters.
///
/// # Examples
///
/// Inspect the backing device and offset of each linear row.
///
/// ```no_run
/// use devmap_linux::{Control, DevId, records::Records, targets::Linear};
///
/// fn inspect(control: &Control, id: DevId) -> std::io::Result<()> {
///     let mut rows = control.table::<4096>(id)?;
///     while let Some(row) = rows.next() {
///         let row = row?;
///         if row.name() == "linear" {
///             let mapping = row.parse::<Linear>(control.version(row.name())?)?;
///             println!("{} at sector {}", mapping.device, mapping.offset);
///         }
///     }
///     Ok(())
/// }
/// ```
pub struct Row<'a, M> {
    span: Span<u64>,
    type_name: &'a str,
    params: &'a [u8],
    _mode: PhantomData<M>,
}

impl<'a, M> Row<'a, M> {
    pub(super) fn new(span: Span<u64>, type_name: &'a str, params: &'a [u8]) -> Self {
        Self {
            span,
            type_name,
            params,
            _mode: PhantomData,
        }
    }
}

impl<M> Row<'_, M> {
    /// Returns the covered range in 512-byte sectors.
    pub fn span(&self) -> Span<u64> {
        self.span
    }

    /// Returns the kernel target name, for selecting a target parser.
    pub fn name(&self) -> &str {
        self.type_name
    }
}

impl<M: Mode> Row<'_, M> {
    /// Parses this row as the response associated with target `T`.
    ///
    /// Supply the version from [`Control::version`](crate::Control::version) for
    /// this row's target. A version can be reused for rows of the same target type.
    /// Parsers may perform I/O, for example to resolve device paths.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` if the target name differs from [`T::NAME`](Target::NAME),
    /// and `InvalidData` for non-UTF-8 parameters. Parser errors are propagated,
    /// including `InvalidData` for malformed parameters and `Unsupported` for
    /// unsupported target versions or configurations.
    pub fn parse<T: Target>(&self, version: Version) -> io::Result<M::Parsed<T>> {
        if self.type_name != T::NAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("expected target {}, found {}", T::NAME, self.type_name),
            ));
        }
        let params = std::str::from_utf8(self.params)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        <M::Parsed<T> as Parse>::parse(params, version)
    }
}

impl<M> fmt::Debug for Row<'_, M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Row")
            .field("span", &self.span)
            .field("type_name", &self.type_name)
            .field("params", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        records::mode::{Info, Table},
        targets::Version,
        targets::Zero,
    };

    #[test]
    fn parsing_uses_the_supplied_version_without_a_control() {
        let row = Row::<Table> {
            span: Span::new(0, 8),
            type_name: "zero",
            params: b"",
            _mode: PhantomData,
        };
        assert_eq!(
            row.parse::<Zero>(Version {
                major: 1,
                minor: 0,
                patch: 0
            })
            .unwrap(),
            Zero
        );
        assert_eq!(
            row.parse::<Zero>(Version {
                major: 0,
                minor: 0,
                patch: 0
            })
            .unwrap_err()
            .kind(),
            io::ErrorKind::Unsupported
        );
        let row = Row::<Table> {
            type_name: "other",
            ..row
        };
        assert_eq!(
            row.parse::<Zero>(Version {
                major: 1,
                minor: 0,
                patch: 0
            })
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidInput
        );
        let row = Row::<Info> {
            span: Span::new(0, 8),
            type_name: "zero",
            params: b"",
            _mode: PhantomData,
        };
        row.parse::<Zero>(Version {
            major: 1,
            minor: 0,
            patch: 0,
        })
        .unwrap();
    }

    #[test]
    fn row_debug_redacts_borrowed_parameters() {
        let row = Row::<Table> {
            span: Span::new(0, 8),
            type_name: "other",
            params: b"secret-key",
            _mode: PhantomData,
        };
        let debug = format!("{row:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("secret-key"));
    }
}
