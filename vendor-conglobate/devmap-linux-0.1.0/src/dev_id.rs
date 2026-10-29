// SPDX-License-Identifier: Apache-2.0

use std::io;
use std::{fmt, str::FromStr};

/// A Linux block-device identifier, written as `major:minor`.
///
/// Identifies a device without opening it or keeping it alive. Obtain one from
/// [`Control::create`](crate::Control::create), device enumeration, or block-device
/// metadata. This type can identify backing devices as well as device-mapper devices.
///
/// # Examples
///
/// ```
/// use devmap_linux::DevId;
///
/// # fn main() -> std::io::Result<()> {
/// let id: DevId = "252:5".parse()?;
/// assert_eq!((id.major(), id.minor()), (252, 5));
/// assert_eq!(id.to_string(), "252:5");
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct DevId {
    major: u32,
    minor: u32,
}

impl DevId {
    /// Constructs an identifier from major and minor numbers.
    ///
    /// Returns `None` if `major` exceeds 4095 (12 bits) or `minor` exceeds
    /// 1,048,575 (20 bits). This does not check whether the device exists.
    pub const fn new(major: u32, minor: u32) -> Option<Self> {
        if major <= 0xfff && minor <= 0x000f_ffff {
            Some(Self { major, minor })
        } else {
            None
        }
    }

    /// Decode the kernel's 32-bit device number stored in a 64-bit ioctl field.
    ///
    /// Only the low 32 bits are significant. This is not the libc `dev_t`
    /// encoding, which can represent larger major and minor numbers.
    #[allow(clippy::cast_possible_truncation)]
    pub(crate) fn from_kernel(value: u64) -> Self {
        let dev = value as u32;
        Self {
            major: (dev >> 8) & 0xfff,
            minor: (dev & 0xff) | ((dev >> 12) & 0x000f_ff00),
        }
    }

    /// Encodes the device number for a device-mapper ioctl's 64-bit field.
    pub(crate) fn to_kernel(self) -> u64 {
        u64::from((self.minor & 0xff) | (self.major << 8) | ((self.minor >> 8) << 20))
    }

    /// Returns the major device number.
    pub const fn major(self) -> u32 {
        self.major
    }

    /// Returns the minor device number.
    pub const fn minor(self) -> u32 {
        self.minor
    }
}

#[cfg(target_os = "linux")]
#[cfg_attr(docsrs, doc(cfg(target_os = "linux")))]
/// Extracts the identifier of a block device.
///
/// # Errors
///
/// Returns `InvalidInput` for non-block-device metadata and `InvalidData`
/// if the device number exceeds the supported encoding.
impl TryFrom<std::fs::Metadata> for DevId {
    type Error = io::Error;

    fn try_from(metadata: std::fs::Metadata) -> io::Result<Self> {
        use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
        if !metadata.file_type().is_block_device() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a block device",
            ));
        }
        let encoded = u32::try_from(metadata.rdev())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(Self::from_kernel(u64::from(encoded)))
    }
}

#[cfg(target_os = "linux")]
#[cfg_attr(docsrs, doc(cfg(target_os = "linux")))]
/// Reads a block device's identifier, consuming and closing the file.
///
/// To retain the file, convert its [metadata](std::fs::File::metadata) instead.
///
/// # Errors
///
/// Propagates metadata I/O errors. Returns `InvalidInput` for non-block devices
/// and `InvalidData` for an unsupported device-number encoding.
impl TryFrom<std::fs::File> for DevId {
    type Error = io::Error;

    fn try_from(file: std::fs::File) -> io::Result<Self> {
        file.metadata()?.try_into()
    }
}

/// Parses decimal `major:minor` numbers without checking device existence.
///
/// # Errors
///
/// Returns `InvalidData` for malformed numbers or components outside the
/// ranges accepted by [`DevId::new`].
impl FromStr for DevId {
    type Err = io::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (major, minor) = value.split_once(':').ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "missing device-number separator",
            )
        })?;
        let major = major
            .parse()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let minor = minor
            .parse()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Self::new(major, minor)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "device number out of range"))
    }
}

/// Formats the identifier as decimal `major:minor`.
impl fmt::Display for DevId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.major, self.minor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_id_matches_known_bit_layout() {
        // Pin the packing against concrete constants, not just a round trip
        // (which can't catch an encode/decode pair that share the same bug).
        // 252 = 0xfc, minor 5 -> 0xfc05 in the classic packed encoding.
        assert_eq!(DevId::new(252, 5).unwrap().to_kernel(), 0xfc05);
        assert_eq!(DevId::from_kernel(0xfc05), DevId::new(252, 5).unwrap());

        // A minor large enough to spill into the high bits [31:20].
        // major=1 -> [19:8], minor=0x12345 -> low 8 at [7:0], high 12 at [31:20].
        assert_eq!(DevId::new(1, 0x1_2345).unwrap().to_kernel(), 0x1230_0145);
        assert_eq!(
            DevId::from_kernel(0x1230_0145),
            DevId::new(1, 0x1_2345).unwrap()
        );
    }

    #[test]
    fn from_dev_t_truncates_high_64_bits() {
        // from_dev_t operates on the low 32 bits only; garbage above bit 31
        // in the kernel-returned u64 must not leak into the result.
        assert_eq!(
            DevId::from_kernel(0xffff_ffff_0000_fc05),
            DevId::new(252, 5).unwrap()
        );
    }
}
