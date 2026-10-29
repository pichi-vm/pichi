// SPDX-License-Identifier: Apache-2.0

use std::io;

use devmap_linux::{targets::Parse, targets::Version};

/// Runtime corruption status and number of FEC-corrected blocks.
///
/// Target versions before 1.13 report only the corruption flag; later
/// versions also report the FEC correction count or `-` when FEC is disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Info {
    /// Whether any hash mismatch has occurred.
    pub corrupted: bool,
    /// Corrected blocks, or `None` if FEC is disabled.
    pub fec_corrected: Option<u64>,
}

impl Parse for Info {
    fn parse(text: &str, version: Version) -> io::Result<Self> {
        version.require(Version {
            major: 1,
            minor: 0,
            patch: 0,
        })?;

        let status: Self = if version
            < (Version {
                major: 1,
                minor: 13,
                patch: 0,
            }) {
            let corrupted = match text.trim() {
                "C" => true,
                "V" => false,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid legacy verity status",
                    ));
                }
            };
            Self {
                corrupted,
                fec_corrected: None,
            }
        } else {
            let mut fields = text.split_whitespace();
            let corrupted = match fields.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "missing verity corruption status",
                )
            })? {
                "C" => true,
                "V" => false,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid verity corruption status",
                    ));
                }
            };
            let fec_corrected = match fields.next().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "missing verity FEC status")
            })? {
                "-" => None,
                count => Some(
                    count
                        .parse()
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
                ),
            };
            if fields.next().is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected trailing verity status",
                ));
            }
            Self {
                corrupted,
                fec_corrected,
            }
        };

        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versioned_info_parser_matches_the_kernel_status_grammar() {
        for status in ["V", "C"] {
            assert_eq!(
                <Info as Parse>::parse(
                    status,
                    Version {
                        major: 1,
                        minor: 12,
                        patch: 0
                    },
                )
                .unwrap(),
                Info {
                    corrupted: status == "C",
                    fec_corrected: None,
                }
            );
            assert_eq!(
                <Info as Parse>::parse(
                    status,
                    Version {
                        major: 1,
                        minor: 13,
                        patch: 0
                    },
                )
                .unwrap_err()
                .kind(),
                io::ErrorKind::InvalidData
            );
        }

        assert_eq!(
            <Info as Parse>::parse(
                "C 3",
                Version {
                    major: 1,
                    minor: 13,
                    patch: 0
                }
            )
            .unwrap(),
            Info {
                corrupted: true,
                fec_corrected: Some(3)
            }
        );
        assert_eq!(
            <Info as Parse>::parse(
                "V -",
                Version {
                    major: 1,
                    minor: 13,
                    patch: 0
                }
            )
            .unwrap(),
            Info::default()
        );
        assert_eq!(
            <Info as Parse>::parse(
                "C 3",
                Version {
                    major: 1,
                    minor: 12,
                    patch: 0
                }
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            <Info as Parse>::parse(
                "V -",
                Version {
                    major: 2,
                    minor: 0,
                    patch: 0
                }
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::Unsupported
        );
    }
}
