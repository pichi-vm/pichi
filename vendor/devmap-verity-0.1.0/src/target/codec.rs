// SPDX-License-Identifier: Apache-2.0

use std::{borrow::Cow, io, num::NonZero};

use devmap_core::BlockSize;
use devmap_linux::{
    DevId,
    targets::{Parse, Target, Version},
};

use super::{CorruptionPolicy, Info, IoErrorPolicy, Verity};
use crate::{Constraint, HashType, Parameters, Salt};

impl Verity {
    fn hex(f: &mut (impl io::Write + ?Sized), bytes: &[u8]) -> io::Result<()> {
        for byte in bytes {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }

    fn chars(value: &str) -> impl Iterator<Item = char> + '_ {
        let mut chars = value.chars();
        std::iter::from_fn(move || {
            chars.next().map(|c| {
                if c == '\\' {
                    chars.next().unwrap_or(c)
                } else {
                    c
                }
            })
        })
    }

    fn unhex(value: &str) -> impl Iterator<Item = io::Result<u8>> + '_ {
        let mut chars = value.chars();
        std::iter::from_fn(move || {
            let first = chars.next()?;
            Some((|| {
                let high = first
                    .to_digit(16)
                    .filter(|_| first.is_ascii())
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "invalid verity hex digit")
                    })?;
                let last = chars.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "incomplete verity hex byte")
                })?;
                let low = last
                    .to_digit(16)
                    .filter(|_| last.is_ascii())
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "invalid verity hex digit")
                    })?;
                Ok((high * 16 + low) as u8)
            })())
        })
    }

    fn word(value: &str) -> Cow<'_, str> {
        if value.contains('\\') {
            Cow::Owned(Self::chars(value).collect())
        } else {
            Cow::Borrowed(value)
        }
    }

    fn token(f: &mut (impl io::Write + ?Sized), value: &str) -> io::Result<()> {
        for c in value.chars() {
            if c == '\\' || c.is_ascii_whitespace() {
                f.write_all(b"\\")?;
            }
            f.write_all(c.encode_utf8(&mut [0; 4]).as_bytes())?;
        }
        Ok(())
    }

    fn words(input: &str) -> io::Result<Vec<&str>> {
        if input.contains('\0') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "NUL in verity parameters",
            ));
        }
        let mut words = Vec::new();
        let mut start = None;
        let mut escaped = false;
        for (index, c) in input.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            if c.is_ascii_whitespace() {
                if let Some(start) = start.take() {
                    words.push(&input[start..index]);
                }
            } else {
                start.get_or_insert(index);
                escaped = c == '\\';
            }
        }
        if let Some(start) = start {
            words.push(&input[start..]);
        }
        Ok(words)
    }

    fn device(value: &str) -> io::Result<DevId> {
        if let Ok(id) = value.parse() {
            return Ok(id);
        }

        #[cfg(target_os = "linux")]
        {
            std::fs::metadata(value)?.try_into()
        }

        #[cfg(not(target_os = "linux"))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "device path resolution requires Linux",
            ))
        }
    }
}

impl Target for Verity {
    const NAME: &'static str = "verity";

    type Table = Self;
    type Info = Info;

    fn encode(
        &self,
        version: Version,
        output: &mut (impl std::io::Write + ?Sized),
    ) -> io::Result<()> {
        if !self.supports_version(version) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "unsupported target version {}.{}.{}",
                    version.major, version.minor, version.patch
                ),
            ));
        }

        write!(
            output,
            "{} {} {} {} {} {} {} {} ",
            self.parameters.hash_type,
            self.data,
            self.hash,
            self.parameters.data.size.bytes(),
            self.parameters.hash.bytes(),
            self.parameters.data.count,
            self.hash_start,
            self.parameters.algorithm
        )?;
        Self::hex(output, &self.root)?;
        output.write_all(b" ")?;
        if self.parameters.salt.is_empty() {
            output.write_all(b"-")?;
        } else {
            Self::hex(output, self.parameters.salt.as_ref())?;
        }
        let count = usize::from(self.corruption_policy != CorruptionPolicy::Error)
            + usize::from(self.io_error_policy != IoErrorPolicy::Error)
            + usize::from(self.ignore_zero_blocks)
            + usize::from(self.check_at_most_once)
            + usize::from(self.try_verify_in_tasklet)
            + usize::from(self.root_hash_sig_key_desc.is_some()) * 2;
        if count == 0 {
            return Ok(());
        }
        write!(output, " {count}")?;
        match self.corruption_policy {
            CorruptionPolicy::Error => {}
            CorruptionPolicy::Ignore => output.write_all(b" ignore_corruption")?,
            CorruptionPolicy::Restart => output.write_all(b" restart_on_corruption")?,
            CorruptionPolicy::Panic => output.write_all(b" panic_on_corruption")?,
        }
        match self.io_error_policy {
            IoErrorPolicy::Error => {}
            IoErrorPolicy::Restart => output.write_all(b" restart_on_error")?,
            IoErrorPolicy::Panic => output.write_all(b" panic_on_error")?,
        }
        if self.ignore_zero_blocks {
            output.write_all(b" ignore_zero_blocks")?;
        }
        if self.check_at_most_once {
            output.write_all(b" check_at_most_once")?;
        }
        if self.try_verify_in_tasklet {
            output.write_all(b" try_verify_in_tasklet")?;
        }
        if let Some(key) = &self.root_hash_sig_key_desc {
            output.write_all(b" root_hash_sig_key_desc ")?;
            Self::token(output, key)?;
        }
        Ok(())
    }
}

impl Parse for Verity {
    // Keep option counts, ordering, and conflict checks in one parser.
    #[allow(clippy::too_many_lines)]
    fn parse(input: &str, version: Version) -> io::Result<Self> {
        let raw = Self::words(input)?;
        let words: Vec<_> = raw.iter().map(|word| Self::word(word)).collect();
        if words.len() < 10 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "missing verity parameters",
            ));
        }
        let hash_type = match words[0].as_ref() {
            "0" => HashType::ChromeOs,
            "1" => HashType::Normal,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unknown verity hash type",
                ));
            }
        };
        let data = Self::device(&words[1])?;
        let hash = Self::device(&words[2])?;
        let count = NonZero::new(
            words[5]
                .parse::<u64>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        )
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "zero verity data-block count")
        })?;
        let algorithm = words[7].parse().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "unknown verity hash algorithm")
        })?;
        let data_block_size = NonZero::new(
            words[3]
                .parse::<u32>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        )
        .and_then(BlockSize::<Constraint>::from_bytes)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid verity data-block size")
        })?;
        let hash_block_size = NonZero::new(
            words[4]
                .parse::<u32>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        )
        .and_then(BlockSize::<Constraint>::from_bytes)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid verity hash-block size")
        })?;
        let root = Self::unhex(&words[8]).collect::<Result<Vec<_>, _>>()?;
        let salt = if words[9] == "-" {
            Salt::default()
        } else {
            let salt = Self::unhex(&words[9]).collect::<Result<Vec<_>, _>>()?;
            Salt::new(&salt)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "verity salt too long"))?
        };
        let parameters = Parameters {
            hash_type,
            algorithm,
            salt,
            data: devmap_core::Geometry {
                size: data_block_size,
                count,
            },
            hash: hash_block_size,
        };
        let mut builder = parameters
            .builder(data, hash, root)
            .map_err(|error| {
                if error.kind() == io::ErrorKind::InvalidInput {
                    io::Error::new(io::ErrorKind::InvalidData, error)
                } else {
                    error
                }
            })?
            .hash_start(
                words[6]
                    .parse()
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
            )
            .map_err(|error| {
                if error.kind() == io::ErrorKind::InvalidInput {
                    io::Error::new(io::ErrorKind::InvalidData, error)
                } else {
                    error
                }
            })?;
        if words.len() > 10 {
            let extra: usize = words[10]
                .parse()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            if extra != words.len() - 11 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "incorrect verity option count",
                ));
            }
        }
        let mut index = 11;
        let mut corruption = false;
        let mut io_error = false;
        let mut signature = false;
        while index < words.len() {
            let option = words[index].to_ascii_lowercase();
            index += 1;
            let mut value = || -> io::Result<&str> {
                let value = words.get(index).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "missing verity option value")
                })?;
                index += 1;
                Ok(value.as_ref())
            };
            match option.as_str() {
                "ignore_corruption" | "restart_on_corruption" | "panic_on_corruption" => {
                    if corruption {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "conflicting verity corruption policies",
                        ));
                    }
                    corruption = true;
                    builder = builder.corruption_policy(match option.as_str() {
                        "ignore_corruption" => CorruptionPolicy::Ignore,
                        "restart_on_corruption" => CorruptionPolicy::Restart,
                        _ => CorruptionPolicy::Panic,
                    });
                }
                "restart_on_error" | "panic_on_error" => {
                    if io_error {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "conflicting verity I/O error policies",
                        ));
                    }
                    io_error = true;
                    builder = builder.io_error_policy(if option == "restart_on_error" {
                        IoErrorPolicy::Restart
                    } else {
                        IoErrorPolicy::Panic
                    });
                }
                "ignore_zero_blocks" => builder = builder.ignore_zero_blocks(true),
                "check_at_most_once" => {
                    builder = builder.check_at_most_once(true).map_err(|error| {
                        if error.kind() == io::ErrorKind::InvalidInput {
                            io::Error::new(io::ErrorKind::InvalidData, error)
                        } else {
                            error
                        }
                    })?;
                }
                "try_verify_in_tasklet" => builder = builder.try_verify_in_tasklet(true),
                "root_hash_sig_key_desc" => {
                    if signature {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "duplicate verity signature option",
                        ));
                    }
                    signature = true;
                    builder = builder.root_hash_sig_key_desc(value()?).map_err(|error| {
                        if error.kind() == io::ErrorKind::InvalidInput {
                            io::Error::new(io::ErrorKind::InvalidData, error)
                        } else {
                            error
                        }
                    })?;
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unknown verity option",
                    ));
                }
            }
        }
        let target = builder.build();
        if !target.supports_version(version) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unsupported verity target version or configuration",
            ));
        }
        Ok(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing_preserves_io_errors_and_reclassifies_validation_errors() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing");
        let expected = std::fs::metadata(&missing).unwrap_err();
        let root = "00".repeat(32);
        let text = format!("1 {} 7:1 4096 4096 1 0 sha256 {root} -", missing.display());
        let error = Verity::parse(
            &text,
            Version {
                major: 1,
                minor: 13,
                patch: 0,
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), expected.kind());
        assert_eq!(error.raw_os_error(), expected.raw_os_error());

        for text in [
            "1 7:0 7:1 4096 4096 1 0 sha256 00 -".to_owned(),
            format!("1 7:0 7:1 4096 4096 1 {} sha256 {root} -", u64::MAX),
            format!("1 7:0 7:1 4096 4096 0 0 sha256 {root} -"),
        ] {
            assert_eq!(
                Verity::parse(
                    &text,
                    Version {
                        major: 1,
                        minor: 13,
                        patch: 0
                    }
                )
                .unwrap_err()
                .kind(),
                io::ErrorKind::InvalidData
            );
        }

        let file = tempfile::NamedTempFile::new().unwrap();
        let text = format!(
            "1 {} 7:1 4096 4096 1 0 sha256 {root} -",
            file.path().display()
        );
        assert_eq!(
            Verity::parse(
                &text,
                Version {
                    major: 1,
                    minor: 13,
                    patch: 0
                }
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
