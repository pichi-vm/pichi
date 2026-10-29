// SPDX-License-Identifier: Apache-2.0

use std::io;

use super::Version;

/// Parses a target response using the installed kernel target version.
///
/// Implementations describe either table parameters or runtime information.
/// Use them as [`Target::Table`] or [`Target::Info`]; [`Row::parse`](crate::records::Row::parse)
/// selects the appropriate type.
///
/// The `()` implementation accepts empty or whitespace-only text. The
/// [`String`] implementation copies text without interpreting it.
/// Both ignore the version.
pub trait Parse: Sized {
    /// Parses response parameters for `version`.
    ///
    /// # Errors
    ///
    /// Implementations return `InvalidData` for malformed parameters and
    /// `Unsupported` for unsupported versions or configurations.
    /// Errors from I/O performed while resolving parameters must be preserved.
    fn parse(text: &str, version: Version) -> io::Result<Self>;
}

/// Copies the response text without validation beyond its UTF-8 type.
impl Parse for String {
    fn parse(text: &str, _: Version) -> io::Result<Self> {
        Ok(text.to_owned())
    }
}

/// Accepts empty or whitespace-only parameters, independently of target version.
impl Parse for () {
    fn parse(text: &str, _: Version) -> io::Result<Self> {
        if text.trim().is_empty() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected empty target parameters",
            ))
        }
    }
}

/// A target that can be encoded into a device-mapper table.
///
/// [`Table::add`](crate::Table::add) supplies the sector range and calls
/// [`encode`](Self::encode) to write this target's construction parameters.
///
/// [`Table`](Self::Table) describes table read-back;
/// [`Info`](Self::Info) describes runtime information. Read-back may normalize,
/// omit, or redact inputs. Use `Self` for the table response only when it retains
/// everything needed to construct the target.
///
/// # Implementing a target
///
/// This minimal implementation illustrates the kernel's parameterless `error`
/// target. Both responses are empty; the built-in equivalent is
/// [`targets::Error`](crate::targets::Error).
///
/// ```
/// use std::io;
/// use devmap_linux::{targets::Target, targets::Version};
///
/// struct AlwaysFail;
///
/// impl Target for AlwaysFail {
///     const NAME: &'static str = "error";
///     type Table = ();
///     type Info = ();
///
///     fn encode(
///         &self,
///         version: Version,
///         _output: &mut (impl io::Write + ?Sized),
///     ) -> io::Result<()> {
///         if version.major != 1 {
///             return Err(io::ErrorKind::Unsupported.into());
///         }
///         Ok(())
///     }
/// }
/// ```
pub trait Target: Sized {
    /// The kernel target name.
    ///
    /// Must be nonempty, shorter than 16 bytes, and contain neither NUL nor
    /// ASCII whitespace.
    const NAME: &'static str;

    /// Writes construction parameters for the installed target version.
    ///
    /// # Implementor requirements
    ///
    /// Write UTF-8 directly to `output`, without NULs or alignment padding.
    /// Propagate destination errors unchanged. On failure, partial output may
    /// remain and must be discarded by the caller.
    ///
    /// # Errors
    ///
    /// Return `Unsupported` if the configuration cannot be expressed for `version`.
    /// Successful encoding does not guarantee that the kernel accepts the mapping;
    /// backing devices and target-specific constraints are checked during loading.
    fn encode(
        &self,
        version: Version,
        output: &mut (impl std::io::Write + ?Sized),
    ) -> io::Result<()>;

    /// The parsed table response returned through [`Control::table`](crate::Control::table).
    ///
    /// This describes what the kernel reports, not necessarily the original input.
    /// Use `()` when the kernel reports no parameters.
    type Table: Parse;

    /// The runtime response returned through [`Control::info`](crate::Control::info).
    ///
    /// Use `()` when the kernel reports no runtime fields.
    type Info: Parse;
}
