// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Carapace formatting policy around devmap's streaming dm-verity formatter.

#[cfg(test)]
use std::io::Cursor;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::num::NonZero;

use devmap_core::{BlockSize, Geometry};
use devmap_verity::{Algorithm, Format as _, HashType, Header, Parameters, Salt};

pub(crate) const VERITY_FORMAT_VERSION: u32 = 1;
pub(crate) const VERITY_HASH_TYPE_NORMAL: u32 = 1;

/// Parameters used when producing SHA-256 format-1 carapaces.
#[derive(Debug, Clone)]
pub(crate) struct VerityParams {
    pub data_block_size: u32,
    pub hash_block_size: u32,
    pub salt: Vec<u8>,
    pub uuid: [u8; 16],
}

/// In-memory convenience output for callers that already hold the data.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct VerityOutput {
    pub blob: Vec<u8>,
    pub root_hash: [u8; 32],
}

impl VerityParams {
    /// Derive a header from a known COW byte length.
    pub(super) fn header(&self, data_size: u64) -> io::Result<Header> {
        let block_size = NonZero::new(self.data_block_size).ok_or(io::ErrorKind::InvalidInput)?;
        self.header_for_blocks(data_size.div_ceil(u64::from(block_size.get())))
    }

    /// Build a header from its declared `data_blocks` reconstruction input.
    pub(super) fn header_for_blocks(&self, data_blocks: u64) -> io::Result<Header> {
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid verity geometry or salt",
            )
        };
        let size = NonZero::new(self.data_block_size)
            .and_then(BlockSize::from_bytes)
            .ok_or_else(invalid)?;
        let hash = NonZero::new(self.hash_block_size)
            .and_then(BlockSize::from_bytes)
            .ok_or_else(invalid)?;
        let count = NonZero::new(data_blocks).ok_or_else(invalid)?;
        Ok(Header {
            uuid: self.uuid,
            parameters: Parameters {
                hash_type: HashType::Normal,
                algorithm: Algorithm::Sha256,
                salt: Salt::new(&self.salt).ok_or_else(invalid)?,
                data: Geometry { size, count },
                hash,
            },
        })
    }

    /// Format an in-memory COW; streaming callers should use `format_padded`.
    #[cfg(test)]
    pub(super) fn compute(&self, cow: &[u8]) -> io::Result<VerityOutput> {
        let mut output = Cursor::new(Vec::new());
        let size = cow.len() as u64;
        let root_hash = self.header(size)?.format_padded(cow, &mut output)?;
        Ok(VerityOutput {
            blob: output.into_inner(),
            root_hash,
        })
    }
}

/// Validate the declared data-block count and zero-pad only a short final block.
pub(crate) trait VerityFormat {
    fn format_padded<R: Read, W: Write + Seek>(self, data: R, hash: W) -> io::Result<[u8; 32]>;
    fn root<R: Read>(self, data: R) -> io::Result<[u8; 32]>;
}

impl VerityFormat for Header {
    fn format_padded<R: Read, W: Write + Seek>(self, data: R, hash: W) -> io::Result<[u8; 32]> {
        let block_size = u64::from(self.parameters.data.size.bytes().get());
        let remaining = self
            .parameters
            .data
            .count
            .get()
            .checked_mul(block_size)
            .ok_or(io::ErrorKind::InvalidInput)?;
        let mut data = PaddedReader {
            inner: data,
            remaining,
            block_size,
            eof: false,
        };
        let root = self.format(&mut data, hash)?;
        // devmap stops at the declared extent. Check EOF even for aligned input,
        // so extra decoded bytes and late transport errors aren't missed.
        data.check_eof()?;
        root.as_ref().try_into().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "carapace root must be 32 bytes",
            )
        })
    }

    fn root<R: Read>(self, data: R) -> io::Result<[u8; 32]> {
        self.format_padded(data, RootSink::default())
    }
}

struct PaddedReader<R> {
    inner: R,
    remaining: u64,
    block_size: u64,
    eof: bool,
}

impl<R: Read> PaddedReader<R> {
    fn check_eof(&mut self) -> io::Result<()> {
        if !self.eof {
            if self.inner.read(&mut [0])? != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "data exceeds declared data_blocks",
                ));
            }
            self.eof = true;
        }
        Ok(())
    }
}

impl<R: Read> Read for PaddedReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let count = self.remaining.min(out.len() as u64) as usize;
        if count == 0 {
            return Ok(0);
        }
        if !self.eof {
            let read = self.inner.read(&mut out[..count])?;
            if read != 0 {
                self.remaining -= read as u64;
                return Ok(read);
            }
            if self.remaining >= self.block_size {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "data contains fewer blocks than declared data_blocks",
                ));
            }
            self.eof = true;
        }
        out[..count].fill(0);
        self.remaining -= count as u64;
        Ok(count)
    }
}

/// Discard formatted bytes while preserving the seek contract for root-only reads.
#[derive(Default)]
struct RootSink(u64);

impl Write for RootSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 += bytes.len() as u64;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for RootSink {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.0 = match from {
            SeekFrom::Start(position) => position,
            SeekFrom::Current(offset) => self
                .0
                .checked_add_signed(offset)
                .ok_or(io::ErrorKind::InvalidInput)?,
            SeekFrom::End(_) => return Err(io::ErrorKind::Unsupported.into()),
        };
        Ok(self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> VerityParams {
        VerityParams {
            data_block_size: 4096,
            hash_block_size: 4096,
            salt: vec![0; 32],
            uuid: [0; 16],
        }
    }

    #[test]
    fn padding_and_exact_length() {
        let p = params();
        let data = vec![0x5a; 4097];
        let mut padded = data.clone();
        padded.resize(8192, 0);
        let output = p.compute(&data).unwrap();
        assert_eq!(output.blob, p.compute(&padded).unwrap().blob);
        assert_eq!(
            output.root_hash,
            p.header(4097).unwrap().root(data.as_slice()).unwrap()
        );
        for size in [4096, 8193] {
            assert!(p.header(size).unwrap().root(data.as_slice()).is_err());
        }
        assert!(p.compute(&[]).is_err());
    }

    #[test]
    fn declared_blocks_allow_only_a_partial_final_block() {
        let p = params();
        for blocks in [1, 2] {
            for length in [0, 1, 4095, 4096, 4097, 8191, 8192, 8193] {
                let data = vec![0x5a; length];
                let result = p.header_for_blocks(blocks).unwrap().root(data.as_slice());
                let fits = match blocks {
                    1 => (1..=4096).contains(&length),
                    2 => (4097..=8192).contains(&length),
                    _ => unreachable!(),
                };
                assert_eq!(result.is_ok(), fits, "blocks={blocks}, bytes={length}");
            }
        }
        assert!(p.header_for_blocks(0).is_err());
        assert!(
            p.header_for_blocks(u64::MAX)
                .unwrap()
                .root(&b"x"[..])
                .is_err()
        );
    }

    #[test]
    fn matches_veritysetup_including_single_data_block() {
        use std::{fs, process::Command};
        if Command::new("veritysetup")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("veritysetup unavailable; skipping external compatibility check");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        for blocks in [1, 2, 128, 129] {
            let data = vec![0x5a; blocks * 4096];
            let p = params();
            let output = p.compute(&data).unwrap();
            let input = dir.path().join("data");
            let reference = dir.path().join(format!("hash-{blocks}"));
            fs::write(&input, &data).unwrap();
            let result = Command::new("veritysetup")
                .args([
                    "format",
                    "--format=1",
                    "--hash=sha256",
                    "--data-block-size=4096",
                    "--hash-block-size=4096",
                    "--uuid=00000000-0000-0000-0000-000000000000",
                ])
                .arg(format!("--salt={}", hex::encode(&p.salt)))
                .arg(&input)
                .arg(&reference)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            let stdout = String::from_utf8(result.stdout).unwrap();
            let root = stdout
                .lines()
                .find_map(|l| l.strip_prefix("Root hash:"))
                .unwrap()
                .trim();
            assert_eq!(hex::encode(output.root_hash), root, "{blocks} data blocks");
            assert_eq!(
                output.blob,
                fs::read(reference).unwrap(),
                "{blocks} data blocks"
            );
        }
    }
}
