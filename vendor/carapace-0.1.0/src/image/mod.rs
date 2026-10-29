// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Plan an image-to-base-scute conversion, then stream its COW bytes.
//!
//! Import [`Plan`] or `AsyncPlan` (with the `tokio` feature) to construct a
//! [`Planned`] reader. Planning scans the source for nonzero chunks; streaming
//! rereads the selected chunks and emits a base-scute COW stream.
//!
//! The source must be an immutable, nonempty regular file of at most 64 GiB.
//! Do not mutate it or use duplicated handles during planning or reading.
//! A partial final chunk is zero-padded to 4096 bytes. Sparse holes are skipped
//! where supported; otherwise planning scans the entire source. Memory usage
//! grows with the number of nonzero chunks.
//!
//! [`Planned::size`] gives the exact padded COW length before streaming, for
//! example to configure a verity formatter. It excludes the separate hash tree.

#[cfg(feature = "tokio")]
mod asynchronous;
mod sync;
#[cfg(windows)]
mod windows;

use devmap_core::BlockSize;
use devmap_snapshot::{Exception, Header};
use std::ops::Range;
use std::{fs::File, io, num::NonZero};

const BLOCK: usize = 4096;
const BATCH: usize = 1024 * 1024;

/// Blocking construction of a planned image reader.
pub trait Plan: Sized {
    /// Scan the source and retain its exception metadata, without producing output.
    fn plan(image: File) -> io::Result<Self>;
}

/// Asynchronous construction of a planned image reader (requires Tokio).
#[cfg(feature = "tokio")]
pub trait AsyncPlan: Sized {
    /// Scan with Tokio I/O; sparse-extent queries use the blocking pool.
    /// Dropping the future discards the plan; an outstanding file operation may
    /// complete, but no whole-image background worker continues scanning.
    fn plan(image: tokio::fs::File) -> impl std::future::Future<Output = io::Result<Self>> + Send;
}

/// An image and its prepared exception metadata, exposed as a sequential COW reader.
///
/// Implements `Read` for `std::fs::File` and `AsyncRead` for `tokio::fs::File`.
/// After a source I/O error, further nonempty reads fail rather than resume a
/// partially emitted block. Empty reads are always harmless.
#[derive(Debug)]
pub struct Planned<R> {
    source: R,
    metadata: Metadata,
    chunk: u64,
    chunks: u64,
    block: Box<[u8; BLOCK]>,
    used: usize,
    available: usize,
    failed: bool,
    #[cfg(feature = "tokio")]
    loading: Loading,
}

#[cfg(feature = "tokio")]
#[derive(Debug, Clone, Copy)]
enum Loading {
    Idle,
    Seeking { want: usize },
    Reading { want: usize, filled: usize },
}

impl<R> Planned<R> {
    /// Number of nonzero source chunks represented by exception records.
    pub fn exceptions(&self) -> u64 {
        self.metadata.count
    }

    /// Total encoded COW size in bytes, including header, data, metadata, and
    /// all chunk padding. Excludes the separate verity hash tree.
    ///
    /// This remains constant as the reader is consumed and can be used to
    /// configure a verity formatter before reading.
    pub fn size(&self) -> u64 {
        self.chunks * BLOCK as u64
    }

    // Prepare the next output block. Data blocks return a source read request;
    // header/metadata blocks are already in the buffer. None also marks EOF.
    fn prepare_block(&mut self) -> Option<(u64, usize)> {
        if self.chunk == self.chunks {
            return None;
        }
        self.block.fill(0);
        let capacity = self.metadata.header.exceptions_per_chunk();
        if self.chunk == 0 {
            self.block[..16].copy_from_slice(&self.metadata.header.encode());
        } else {
            let area = (self.chunk - 1) / (capacity + 1);
            let slot = (self.chunk - 1) % (capacity + 1);
            if slot == 0 {
                self.block
                    .copy_from_slice(&self.metadata.areas[area as usize]);
            } else {
                let start = (slot as usize - 1) * 16;
                let mut record = [0; 16];
                record.copy_from_slice(&self.metadata.areas[area as usize][start..start + 16]);
                let offset = Exception::decode(record).old_chunk * BLOCK as u64;
                let want = (self.metadata.image_size - offset).min(BLOCK as u64) as usize;
                return Some((offset, want));
            }
        }
        self.ready();
        None
    }

    fn ready(&mut self) {
        self.used = 0;
        self.available = BLOCK;
        self.chunk += 1;
    }

    fn copy(&mut self, output: &mut [u8]) -> usize {
        let n = output.len().min(self.available - self.used);
        output[..n].copy_from_slice(&self.block[self.used..self.used + n]);
        self.used += n;
        n
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[derive(Debug)]
struct Metadata {
    header: Header,
    areas: Vec<[u8; BLOCK]>,
    count: u64,
    image_size: u64,
    read_bytes: u64,
}

impl Metadata {
    fn new(metadata: &std::fs::Metadata) -> io::Result<Self> {
        if !metadata.is_file() || metadata.len() == 0 {
            return Err(invalid("expected a nonempty regular image file"));
        }
        if metadata.len() > crate::ZERO_COUNT_SECTORS * 512 {
            return Err(invalid("image exceeds the 64 GiB virtual origin"));
        }
        let chunk_size = NonZero::new(BLOCK as u32)
            .and_then(BlockSize::from_bytes)
            .ok_or_else(|| invalid("invalid scute chunk size"))?;
        Ok(Self {
            header: Header {
                valid: true,
                chunk_size,
            },
            areas: vec![[0; BLOCK]],
            count: 0,
            image_size: metadata.len(),
            read_bytes: 0,
        })
    }

    fn inspect(&mut self, position: u64, bytes: &[u8]) -> io::Result<()> {
        self.read_bytes += bytes.len() as u64;
        let capacity = self.header.exceptions_per_chunk();
        for (index, chunk) in bytes.chunks(BLOCK).enumerate() {
            if chunk.iter().all(|b| *b == 0) {
                continue;
            }
            let area = self.count / capacity;
            let slot = self.count % capacity;
            let record = Exception {
                old_chunk: position / BLOCK as u64 + index as u64,
                new_chunk: self.header.metadata_chunk_index(area)? + 1 + slot,
            };
            let start = slot as usize * 16;
            self.areas[area as usize][start..start + 16].copy_from_slice(&record.encode());
            self.count += 1;
            if self.count.is_multiple_of(capacity) {
                self.areas.push([0; BLOCK]);
            }
        }
        Ok(())
    }

    fn finish<R>(self, source: R) -> Planned<R> {
        // One header chunk, all data chunks, and the retained metadata chunks.
        // `inspect` includes an empty terminator area when the last area is full.
        // The validated source size bounds this count and its byte size.
        let chunks = 1 + self.count + self.areas.len() as u64;
        Planned {
            source,
            metadata: self,
            chunks,
            chunk: 0,
            block: Box::new([0; BLOCK]),
            used: 0,
            available: 0,
            failed: false,
            #[cfg(feature = "tokio")]
            loading: Loading::Idle,
        }
    }
}

/// Internal safe extension for querying source data extents.
///
/// Returns the next possibly allocated range within `position..size`, or
/// `size..size` when none remains. Unsupported filesystems return the remaining
/// interval for sequential scanning. Allocated ranges may still contain zeros.
/// The source must remain unchanged while planning; Unix queries move its cursor.
trait SparseFile {
    fn next_extent(&self, position: u64, size: u64) -> io::Result<Range<u64>>;
}

// Query only at extent boundaries, never once per chunk in a hole. Filesystems
// may conservatively report holes as data; the content scan remains authoritative.
#[cfg(any(target_os = "linux", target_os = "macos"))]
impl SparseFile for File {
    fn next_extent(&self, position: u64, size: u64) -> io::Result<Range<u64>> {
        use rustix::{
            fs::{seek, SeekFrom},
            io::Errno,
        };
        if position >= size {
            return Ok(size..size);
        }
        let unsupported = |e| matches!(e, Errno::INVAL | Errno::OPNOTSUPP | Errno::NOSYS);
        let start = match seek(self, SeekFrom::Data(position)) {
            Ok(start) => start,
            Err(Errno::NXIO) => return Ok(size..size),
            Err(e) if unsupported(e) => return Ok(position..size),
            Err(e) => return Err(e.into()),
        };
        let end = match seek(self, SeekFrom::Hole(start)) {
            Ok(end) => end.min(size),
            Err(e) if unsupported(e) => return Ok(position..size),
            Err(e) => return Err(e.into()),
        };
        if start < position || end <= start || start >= size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid or changing sparse extent",
            ));
        }
        Ok(start..end)
    }
}

// Other platforms scan the source; omission of zero chunks remains identical.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
impl SparseFile for File {
    fn next_extent(&self, position: u64, size: u64) -> io::Result<Range<u64>> {
        Ok(position.min(size)..size)
    }
}

#[cfg(test)]
mod tests;
