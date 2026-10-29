// SPDX-License-Identifier: Apache-2.0

use std::io::{self, Write};

use devmap_core::Span;
use zerocopy::IntoBytes;

use super::{
    DevId,
    abi::{Command, Header, TargetSpec},
    targets::{Target, Version},
};

/// A table describing how a device maps its sectors to targets.
///
/// Build an empty table with [`Default`], append rows with [`add`](Self::add),
/// then submit it with [`Control::load`](super::Control::load).
///
/// Rows occupy an inline buffer of `N` bytes (4096 by default). Capacity includes
/// row headers, parameters, and padding, but excludes the ioctl header.
/// Larger capacities increase the size of this value.
///
/// # Examples
///
/// Construct a 4 MiB zero mapping. This builds a table but does not create,
/// load, or activate a device.
///
/// ```no_run
/// use devmap_core::Span;
/// use devmap_linux::{Control, Table, targets::Zero};
///
/// # fn example() -> std::io::Result<()> {
/// let control = Control::open()?;
/// let version = control.version("zero")?;
/// let table = Table::<4096>::default()
///     .add(Span::new(0, 8192), &Zero, version)?;
/// # Ok(())
/// # }
/// ```
pub struct Table<const N: usize = 4096> {
    data: [u8; N],
    count: u32,
    used: usize,
}

impl<const N: usize> Table<N> {
    /// Appends a target covering a range of 512-byte sectors.
    ///
    /// Supply the target version from [`Control::version`](super::Control::version).
    /// Encoding does not query the kernel. The kernel checks the complete mapping,
    /// including row ordering, gaps, lengths, and backing devices, during
    /// [`Control::load`](super::Control::load).
    ///
    /// This consumes the table, including on error; a failed append cannot be retried
    /// on the same value. See the [construction example](Table#examples).
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` for invalid target names, non-UTF-8 parameters,
    /// embedded NULs, or row sizes/counts that cannot be represented.
    /// Returns `WriteZero` when the row and padding do not fit.
    /// Encoder errors, including `Unsupported` for unsupported configurations,
    /// are propagated unchanged.
    pub fn add<T: Target>(
        mut self,
        span: Span<u64>,
        target: &T,
        version: Version,
    ) -> io::Result<Self> {
        let spec = TargetSpec::new(span, T::NAME)?;
        let capacity = N - self.used;
        let mut output = &mut self.data[self.used..];
        output.write_all(spec.as_bytes())?;
        target.encode(version, &mut output)?;
        let written = capacity - output.len();
        output.write_all(&[0])?;
        let padding = (written + 1).next_multiple_of(8) - (written + 1);
        output.write_all(&[0; 7][..padding])?;
        let params = &self.data[self.used + TargetSpec::SIZE..self.used + written];
        std::str::from_utf8(params)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        if params.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "target parameters contain NUL",
            ));
        }
        let row_len = written + 1 + padding;
        let next =
            u32::try_from(row_len).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let spec = spec.with_next(next);
        self.data[self.used..self.used + TargetSpec::SIZE].copy_from_slice(spec.as_bytes());
        self.used += row_len;
        self.count = self
            .count
            .checked_add(1)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        Ok(self)
    }

    pub(super) fn into_command(self, dev: DevId, read_only: bool) -> Command<N> {
        let header = Header::by_dev(dev.to_kernel())
            .with_target_count(self.count)
            .with_readonly(read_only);
        Command::from_parts(header, self.data)
    }
}

/// Creates an empty table with space for `N` bytes of rows.
impl<const N: usize> Default for Table<N> {
    fn default() -> Self {
        Self {
            data: [0; N],
            count: 0,
            used: 0,
        }
    }
}

impl<const N: usize> std::fmt::Debug for Table<N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Table")
            .field("capacity", &N)
            .field("used", &self.used)
            .field("count", &self.count)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::Zero;

    enum Parameters {
        Text(&'static [u8]),
        IgnoreOverflow,
        Fail,
    }

    impl Target for Parameters {
        const NAME: &'static str = "linear";

        type Table = String;
        type Info = String;

        fn encode(&self, _: Version, output: &mut (impl Write + ?Sized)) -> io::Result<()> {
            match self {
                Self::Text(text) => output.write_all(text)?,
                Self::IgnoreOverflow => {
                    let _ = output.write_all(b"too long for this buffer");
                }
                Self::Fail => return Err(io::Error::from_raw_os_error(28)),
            }
            Ok(())
        }
    }
    const VERSION: Version = Version {
        major: 1,
        minor: 0,
        patch: 0,
    };

    #[test]
    fn rows_are_aligned_and_capacity_errors_come_from_writes() {
        let table = Table::<105>::default()
            .add(Span::new(0, 8), &Parameters::Text(b"252:5 00"), VERSION)
            .unwrap()
            .add(Span::new(8, 8), &Zero, VERSION)
            .unwrap();
        assert_eq!(table.used, 104);
        assert_eq!(table.count, 2);
        assert_eq!(&table.data[20..24], &56u32.to_ne_bytes());
        assert_eq!(&table.data[76..80], &48u32.to_ne_bytes());
        assert_eq!(&table.data[40..56], b"252:5 00\0\0\0\0\0\0\0\0");
        assert_eq!(
            table
                .add(Span::new(16, 8), &Zero, VERSION)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WriteZero
        );
        assert_eq!(
            Table::<47>::default()
                .add(Span::new(0, 8), &Zero, VERSION)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WriteZero
        );
    }

    #[test]
    fn parameters_validate_text_and_preserve_encoder_errors() {
        let error = Table::<64>::default()
            .add(Span::new(0, 8), &Parameters::Text(b"x\0y"), VERSION)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        for bytes in [&b"\xff"[..], &b"\xc3"[..]] {
            let error = Table::<64>::default()
                .add(Span::new(0, 8), &Parameters::Text(bytes), VERSION)
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
        Table::<64>::default()
            .add(
                Span::new(0, 8),
                &Parameters::Text("café".as_bytes()),
                VERSION,
            )
            .unwrap();
        let error = Table::<48>::default()
            .add(Span::new(0, 8), &Parameters::IgnoreOverflow, VERSION)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WriteZero);
        let error = Table::<64>::default()
            .add(Span::new(0, 8), &Parameters::Fail, VERSION)
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(28));
        let error = Table::<64>::default()
            .add(
                Span::new(0, 8),
                &Zero,
                Version {
                    major: 0,
                    minor: 0,
                    patch: 0,
                },
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    }
}
