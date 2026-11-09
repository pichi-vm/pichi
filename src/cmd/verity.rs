// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Carapace formatting policy around devmap's streaming dm-verity formatter.

#[cfg(test)]
use std::io::Cursor;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::num::NonZero;
use std::path::Path;

use devmap_core::{BlockSize, Geometry};
use devmap_verity::{Algorithm, Format as _, HashType, Header, Parameters, Salt};

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
    fn root_file(self, path: &Path) -> io::Result<[u8; 32]>;
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

    fn root_file(self, path: &Path) -> io::Result<[u8; 32]> {
        let workers = std::thread::available_parallelism().map_or(1, |n| n.get().min(8));
        let hash_size = u64::from(self.parameters.hash.bytes().get());
        let groups = self.parameters.data.count.get().div_ceil(hash_size / 32);
        // Bound first-level storage to 16 MiB. Small files and other algorithms
        // retain the existing streaming implementation.
        if workers <= 1
            || self.parameters.data.size.bytes().get() > 64 * 1024
            || self.parameters.algorithm != Algorithm::Sha256
            || self.parameters.hash_type != HashType::Normal
            || self.parameters.data_size()? < 16 * 1024 * 1024
            || groups
                .checked_mul(hash_size)
                .is_none_or(|size| size > 16 * 1024 * 1024)
        {
            return self.root(io::BufReader::new(std::fs::File::open(path)?));
        }
        ParallelRoot { header: self, path }.root(workers)
    }
}

/// Hash independent leaf blocks, then let the standard formatter reduce the
/// resulting first hash level. No cached hashes or roots are trusted.
struct ParallelRoot<'a> {
    header: Header,
    path: &'a Path,
}

impl ParallelRoot<'_> {
    fn root(self, workers: usize) -> io::Result<[u8; 32]> {
        use sha2::{Digest as _, Sha256};

        let mut header = self.header;
        let size = header.parameters.data.size.bytes().get() as usize;
        let hash_size = header.parameters.hash.bytes().get() as usize;
        let count = usize::try_from(header.parameters.data.count.get())
            .map_err(|_| io::ErrorKind::InvalidInput)?;
        let groups = count.div_ceil(hash_size / 32);
        let mut hashes = vec![0; groups * hash_size];
        let per_worker = groups.div_ceil(workers) * hash_size;
        std::thread::scope(|scope| -> io::Result<()> {
            let mut joins = Vec::new();
            for (index, output) in hashes.chunks_mut(per_worker).enumerate() {
                let first = index * per_worker / 32;
                let blocks = (count - first).min(output.len() / 32);
                let salt = header.parameters.salt;
                joins.push(std::thread::Builder::new().spawn_scoped(
                    scope,
                    move || -> io::Result<()> {
                        let mut input = io::BufReader::with_capacity(
                            256 * 1024,
                            std::fs::File::open(self.path)?,
                        );
                        input.seek(SeekFrom::Start((first as u64) * (size as u64)))?;
                        let mut block = vec![0; size];
                        for digest in output.chunks_mut(32).take(blocks) {
                            input.read_exact(&mut block)?;
                            let mut hash = Sha256::new();
                            hash.update(salt.as_ref());
                            hash.update(&block);
                            digest.copy_from_slice(&hash.finalize());
                        }
                        if first + blocks == count && input.read(&mut [0])? != 0 {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "data exceeds declared data_blocks",
                            ));
                        }
                        Ok(())
                    },
                )?);
            }
            for join in joins {
                join.join()
                    .map_err(|_| io::Error::other("verity worker panicked"))??;
            }
            Ok(())
        })?;
        header.parameters.data.size = header.parameters.hash;
        header.parameters.data.count = (groups as u64).try_into().unwrap();
        header.root(hashes.as_slice())
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

    #[test]
    fn parallel_file_roots_match_streaming_at_tree_boundaries() {
        for (data_size, hash_size, count) in [
            (512, 512, 2),
            (4096, 512, 15),
            (512, 512, 16),
            (4096, 512, 17),
            (512, 4096, 127),
            (4096, 4096, 128),
            (512, 4096, 129),
            (4096, 4096, 4097),
        ] {
            let mut params = params();
            params.data_block_size = data_size;
            params.hash_block_size = hash_size;
            params.salt = (0..47).collect();
            let header = params.header_for_blocks(count).unwrap();
            let data: Vec<_> = (0..data_size as usize * count as usize)
                .map(|index| (index.wrapping_mul(31) ^ (index >> 12)) as u8)
                .collect();
            let file = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(file.path(), &data).unwrap();
            let expected = header.root(data.as_slice()).unwrap();
            for workers in [1, 2, 4, 8] {
                assert_eq!(
                    ParallelRoot {
                        header,
                        path: file.path()
                    }
                    .root(workers)
                    .unwrap(),
                    expected
                );
            }
            assert_eq!(header.root_file(file.path()).unwrap(), expected);
        }
    }

    #[test]
    fn parallel_file_roots_reject_wrong_extents_and_detect_changed_blocks() {
        let header = params().header_for_blocks(129).unwrap();
        let data = vec![0x5a; 129 * 4096];
        let expected = header.root(data.as_slice()).unwrap();
        let file = tempfile::NamedTempFile::new().unwrap();
        for size in [0, data.len() - 4096, data.len() - 1, data.len() + 1] {
            std::fs::write(file.path(), vec![0x5a; size]).unwrap();
            assert!(
                ParallelRoot {
                    header,
                    path: file.path()
                }
                .root(8)
                .is_err()
            );
        }
        for index in [0, 4096 * 16, 4096 * 128, data.len() - 1] {
            let mut changed = data.clone();
            changed[index] ^= 1;
            std::fs::write(file.path(), changed).unwrap();
            assert_ne!(
                ParallelRoot {
                    header,
                    path: file.path()
                }
                .root(8)
                .unwrap(),
                expected
            );
        }
    }

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
        use sha2::{Digest as _, Sha256};

        // References from veritysetup 2.8.8, with data filled with 0x5a:
        // veritysetup format --format=1 --hash=sha256 --data-block-size=4096
        //   --hash-block-size=4096 --uuid=00000000-0000-0000-0000-000000000000
        //   --salt=<64 zero hex digits> data hash
        // Compare the root and SHA-256 of the entire hash file on every host,
        // including hosts where veritysetup cannot run.
        for (blocks, root, blob_sha256) in [
            (
                1,
                "0ecd152e0678bd6a24c4b752f6e6064547175b143fbd7057a95267e12abf16c1",
                "38369acb3def6df515ba26a2ca670baefd0b03e316ad8142333c8b8d347a9782",
            ),
            (
                2,
                "5d10312f31710534c00115205b2647cd4ef57fbcb1186095f4574094bbfc62fc",
                "7c0c806e706eaa3fa5e9ea28b67edfafc60c2042d51f0a99c2e8711742426f01",
            ),
            (
                128,
                "7db6b588ccee2515a070c54f4c8199a4e8a043e2522937e7ee0c1e9a096fd059",
                "552f3e199a58c0b0fbe4c622f9bfa379173fdf42d559d634762ff686268e59cb",
            ),
            (
                129,
                "8aed38a04074125290bdd43e524f6a37d2c5baaee8d2f67faab90bc985db361c",
                "d9cca0200faf892bb169a733028c091d315572f673a5d593bae476bb1d5e76ee",
            ),
        ] {
            let data = vec![0x5a; blocks * 4096];
            let p = params();
            let output = p.compute(&data).unwrap();
            assert_eq!(hex::encode(output.root_hash), root, "{blocks} data blocks");
            assert_eq!(
                hex::encode(Sha256::digest(&output.blob)),
                blob_sha256,
                "{blocks} data blocks"
            );
        }
    }
}
