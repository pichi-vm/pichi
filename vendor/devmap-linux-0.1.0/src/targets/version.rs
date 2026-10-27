// SPDX-License-Identifier: Apache-2.0

use std::{fmt, io};

/// Version reported by the kernel for one device-mapper target type.
///
/// These are target-specific numbers, not a semantic-versioning promise.
/// Compare versions only within the same target type.
/// [`Display`](fmt::Display) writes the version as `major.minor.patch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    /// Major version.
    pub major: u32,
    /// Minor version.
    pub minor: u32,
    /// Patch version.
    pub patch: u32,
}

impl Version {
    /// Requires the same major version and a release at least as recent as `minimum`.
    ///
    /// Both versions must refer to the same target type. A higher major version
    /// is rejected rather than assumed compatible.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::Unsupported`] if the major versions differ or
    /// this version is older than `minimum`. The error includes both versions.
    pub fn require(self, minimum: Self) -> io::Result<()> {
        if self.major == minimum.major && self >= minimum {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "unsupported target version {self}; expected at least {minimum} within major version {}",
                    minimum.major,
                ),
            ))
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requirements_check_major_minor_and_patch() {
        let minimum = Version {
            major: 1,
            minor: 4,
            patch: 2,
        };
        for (major, minor, patch, accepted) in [
            (1, 4, 2, true),
            (1, 4, 3, true),
            (1, 5, 0, true),
            (1, 4, 1, false),
            (1, 3, u32::MAX, false),
            (0, u32::MAX, u32::MAX, false),
            (2, 0, 0, false),
        ] {
            let version = Version {
                major,
                minor,
                patch,
            };
            let result = version.require(minimum);
            assert_eq!(version.to_string(), format!("{major}.{minor}.{patch}"));
            assert_eq!(result.is_ok(), accepted, "{version:?}");
            if let Err(error) = result {
                assert_eq!(error.kind(), io::ErrorKind::Unsupported);
                assert!(error.to_string().contains("1.4.2"));
                assert!(
                    error
                        .to_string()
                        .contains(&format!("{major}.{minor}.{patch}"))
                );
            }
        }
    }
}
