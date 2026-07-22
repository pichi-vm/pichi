// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! dm-snapshot persistent COW writer. Byte-exact per
//! `drivers/md/dm-snap-persistent.c` v6.6 LTS — see RESEARCH.md §2 for the
//! authoritative spec citations.
//!
//! All multi-byte fields are little-endian (`__le32` / `__le64`). All
//! `chunk` indices are *chunk* indices (chunk = `chunk_size_sectors *
//! SECTOR_SIZE` bytes), NOT sector indices — see RESEARCH §2 trap #1.
//!
//! Per CONTEXT D-05, kernel-side acceptance of this format is deferred to
//! v0.9 boot tests; the round-trip self-test below (`#[cfg(test)] mod
//! cow_reader` + `tests::round_trip_byte_exact`) is the only Phase 43 gate.

use std::io::{Read, Seek, SeekFrom, Write};

use anyhow::{Context as _, Result, bail};

/// "SnAp" little-endian — `dm-snap-persistent.c:59` `SNAP_MAGIC`.
pub const SNAP_MAGIC: u32 = 0x7041_6e53;
/// `dm-snap-persistent.c:64` `SNAPSHOT_DISK_VERSION`.
pub const SNAPSHOT_DISK_VERSION: u32 = 1;
/// `dm-snap-persistent.c:66` `NUM_SNAPSHOT_HDR_CHUNKS`.
pub const NUM_SNAPSHOT_HDR_CHUNKS: u64 = 1;
/// 512 bytes per sector (kernel-wide).
pub const SECTOR_SIZE: u32 = 512;
/// `dm-snap-persistent.c:21` `DM_CHUNK_SIZE_DEFAULT_SECTORS` = 32 sectors = 16 KiB.
pub const DEFAULT_CHUNK_SIZE_SECTORS: u32 = 32;
/// `sizeof(struct disk_exception)` = 16 bytes (`__le64 old_chunk; __le64 new_chunk;`).
pub const DISK_EXCEPTION_SIZE: usize = 16;
/// Header is 16 bytes — `magic | valid | version | chunk_size`, four `__le32`.
const DISK_HEADER_SIZE: usize = 16;
/// IMPORT-07: chunk size MUST be >= 8 sectors per kernel constraint.
pub const MIN_CHUNK_SIZE_SECTORS: u32 = 8;

/// Validate IMPORT-07: power-of-two AND >= 8 sectors.
///
/// Returns `Err` (with descriptive message) if `sectors` is zero, not a
/// power of two, or < `MIN_CHUNK_SIZE_SECTORS` (= 8).
///
/// Upper bound is enforced at the CLI boundary in Plan 04 (per CONTEXT
/// "Deferred Ideas" the planner picks 2048 sectors = 1 MiB).
pub fn validate_chunk_size(sectors: u32) -> Result<()> {
    if sectors == 0 {
        bail!("--chunk-size must be > 0 (got 0)");
    }
    if !sectors.is_power_of_two() {
        bail!("--chunk-size must be a power of two (got {sectors})");
    }
    if sectors < MIN_CHUNK_SIZE_SECTORS {
        bail!(
            "--chunk-size must be >= {MIN_CHUNK_SIZE_SECTORS} sectors per kernel constraint \
             (got {sectors})"
        );
    }
    Ok(())
}

/// Write a dm-snapshot persistent COW from `input`.
///
/// # Layout (per RESEARCH §2 lines 141–154)
///
/// ```text
/// chunk 0:                                  header (16-byte disk_header + zero pad to chunk_size)
/// chunk 1:                                  metadata area 0  (exceptions_per_area entries)
/// chunks 2 .. 2 + exceptions_per_area - 1:  data area 0
/// chunk (1 + exc_per_area + 1):             metadata area 1
/// ...
/// ```
///
/// Where `exceptions_per_area = chunk_bytes / 16`. Each exception's
/// `new_chunk` field IS the cow chunk index where the exception's data
/// lives; allocator walks `next_free` forward by 1 chunk per exception
/// and bumps past metadata chunks via `skip_metadata` (RESEARCH §2 lines
/// 156–181).
///
/// # Pitfalls handled (RESEARCH §2 traps 1–6)
///
/// - Trap 1: chunk vs sector units — `chunk_size` field is sectors;
///   `old_chunk` / `new_chunk` are chunks.
/// - Trap 2: `new_chunk == 0` is sentinel; first valid `new_chunk` is 2
///   (after `next_free=1` is bumped past the first metadata chunk).
/// - Trap 3: zero each metadata chunk before writing entries so trailing
///   slots naturally form sentinels.
/// - Trap 4: if an area fills exactly, write a zero metadata chunk at the
///   NEXT area location too.
/// - Trap 5: always `to_le_bytes()`.
/// - Trap 6: `valid = 1` always.
pub fn write(input: &[u8], chunk_size_sectors: u32) -> Result<Vec<u8>> {
    // Hard runtime check (NOT debug_assert!) — `cow` is `pub mod` and
    // `write` is `pub fn`, so Phase 44's pull path (per CONTEXT D-03) or
    // any external caller can reach this without going through lib::run's
    // pre-validation. A release-mode caller passing 0 would otherwise
    // panic with `attempt to divide by zero` deep inside this function.
    validate_chunk_size(chunk_size_sectors).context("cow::write: chunk_size_sectors")?;

    let chunk_bytes = (chunk_size_sectors as usize) * (SECTOR_SIZE as usize);
    let exceptions_per_area = chunk_bytes / DISK_EXCEPTION_SIZE;
    let area_stride_chunks = (exceptions_per_area + 1) as u64; // 1 metadata + N data

    // Walk the input in chunk_bytes units. For each non-zero source
    // chunk, record its (old_chunk, new_chunk) pair. `new_chunk` =
    // `next_free` (in chunk units), bumped past any metadata-chunk slot
    // it would land on.
    let total_input_chunks = input.len().div_ceil(chunk_bytes);
    let mut exceptions: Vec<(u64, u64)> = Vec::new();
    // next_free starts at NUM_SNAPSHOT_HDR_CHUNKS = 1; the first call to
    // bump_past_metadata() skips it to 2 (chunk 1 is the first metadata area),
    // so the first valid new_chunk is 2 — per RESEARCH §2 trap #2.
    let mut next_free: u64 = NUM_SNAPSHOT_HDR_CHUNKS;

    for src_idx in 0..total_input_chunks {
        let off = src_idx * chunk_bytes;
        let end = (off + chunk_bytes).min(input.len());
        let src_chunk = &input[off..end];
        // IMPORT-03 / dm-zero origin: skip all-zero chunks.
        if src_chunk.iter().all(|&b| b == 0) {
            continue;
        }
        // skip_metadata BEFORE allocating: bump next_free if it would
        // land on a metadata chunk slot (per dm-snap-persistent.c:275-282).
        next_free = bump_past_metadata(next_free, area_stride_chunks);
        let new_chunk = next_free;
        next_free += 1;
        exceptions.push((src_idx as u64, new_chunk));
    }

    // Compute total cow size in chunks: enough to cover all `new_chunk`
    // slots PLUS, if the last area filled exactly, one trailing zeroed
    // metadata chunk (trap 4).
    let max_new_chunk = exceptions.iter().map(|(_, nc)| *nc).max().unwrap_or(0);
    let mut total_chunks = max_new_chunk + 1; // need at least up to the last data chunk

    // Trap 4: if the last area filled to capacity, ensure the next area's
    // metadata chunk (which we leave zeroed) is in the cow so the reader
    // sees the sentinel there.
    if !exceptions.is_empty() && exceptions.len().is_multiple_of(exceptions_per_area) {
        let n_areas_used = (exceptions.len() / exceptions_per_area) as u64;
        let next_area_metadata = NUM_SNAPSHOT_HDR_CHUNKS + n_areas_used * area_stride_chunks;
        if next_area_metadata + 1 > total_chunks {
            total_chunks = next_area_metadata + 1;
        }
    }

    // Need at least chunk 0 (header) + chunk 1 (metadata area 0).
    if total_chunks < 2 {
        total_chunks = 2;
    }

    let total_bytes = (total_chunks as usize) * chunk_bytes;
    let mut cow = vec![0u8; total_bytes];

    // Write disk_header at offset 0 (chunk 0). The rest of chunk 0 stays
    // zeroed (already zero-initialized via `vec![0u8; ...]`).
    // Trap 5: to_le_bytes() for every field.
    // Trap 6: valid = 1.
    debug_assert!(
        chunk_bytes >= DISK_HEADER_SIZE,
        "chunk_bytes must be >= DISK_HEADER_SIZE"
    );
    cow[0..4].copy_from_slice(&SNAP_MAGIC.to_le_bytes());
    cow[4..8].copy_from_slice(&1u32.to_le_bytes()); // valid = 1
    cow[8..12].copy_from_slice(&SNAPSHOT_DISK_VERSION.to_le_bytes());
    cow[12..16].copy_from_slice(&chunk_size_sectors.to_le_bytes());

    // Write exception entries into metadata areas. Each metadata area
    // chunk starts already zero-filled (so trailing slots beyond the
    // last entry are sentinels — trap 3).
    for (i, (old_chunk, new_chunk)) in exceptions.iter().enumerate() {
        let area = (i / exceptions_per_area) as u64;
        let slot_in_area = i % exceptions_per_area;
        let metadata_chunk = NUM_SNAPSHOT_HDR_CHUNKS + area * area_stride_chunks;
        let metadata_off = (metadata_chunk as usize) * chunk_bytes;
        let entry_off = metadata_off + slot_in_area * DISK_EXCEPTION_SIZE;
        cow[entry_off..entry_off + 8].copy_from_slice(&old_chunk.to_le_bytes());
        cow[entry_off + 8..entry_off + 16].copy_from_slice(&new_chunk.to_le_bytes());
    }

    // Write each non-zero source chunk's data into its `new_chunk` slot.
    for (old_chunk, new_chunk) in &exceptions {
        let src_off = (*old_chunk as usize) * chunk_bytes;
        let src_end = (src_off + chunk_bytes).min(input.len());
        let copy_len = src_end - src_off;
        let dst_off = (*new_chunk as usize) * chunk_bytes;
        cow[dst_off..dst_off + copy_len].copy_from_slice(&input[src_off..src_end]);
        // If the last source chunk is short, the rest of the cow chunk
        // stays zero (already zero-initialized).
    }

    Ok(cow)
}

/// Metadata returned by [`write_streaming`] — describes the COW that
/// was written to the output sink (which the caller still owns).
///
/// Used by `pichi import` to size the manifest's `cow_size` annotation
/// and to compute the COW digest in a separate streaming pass over the
/// just-written file.
#[derive(Debug, Clone, Copy)]
pub struct CowStreamMeta {
    /// Total length of the COW blob, in bytes (`total_chunks * chunk_bytes`).
    /// The caller MUST `set_len` the output to this value if the underlying
    /// sink does not already track its own length (`write_streaming` does
    /// this for `Seek + Write` sinks via a final write at the last byte).
    pub total_bytes: u64,
    /// Number of input chunks read.
    pub input_chunks: u64,
    /// Number of non-zero input chunks (i.e. exception entries written).
    pub exception_count: u64,
    /// Chunk size in 512-byte sectors (echoed back from the caller for
    /// downstream consumers).
    pub chunk_size_sectors: u32,
}

/// Streaming version of [`write`] for BL-01 / T-43-01 mitigation.
///
/// Walks `input` exactly once in `chunk_bytes`-sized buffers and writes
/// the dm-snapshot persistent COW directly to `output` via seeks. Memory
/// footprint is bounded:
///
/// - one read buffer of `chunk_bytes` (≤ 1 MiB)
/// - the full exception list (16 bytes per non-zero input chunk; for a
///   10 GiB sparse input at default 16 KiB chunks the worst case is
///   ~10 MiB)
///
/// The output is written sparsely (metadata chunks are LEFT AS HOLES;
/// they read back as zeros — exactly the `disk_exception` sentinel
/// pattern the kernel reader expects).
///
/// On success the file at `output` will be exactly
/// [`CowStreamMeta::total_bytes`] bytes long. On error the partial
/// contents of `output` are left for the caller to clean up (typically
/// by dropping a `tempfile::NamedTempFile`).
///
/// # Layout, traps, and pitfalls
///
/// Identical to the in-memory [`write`]; see its docs for the byte
/// layout and RESEARCH §2 trap citations. The streaming variant uses
/// the SAME `bump_past_metadata` allocator and produces byte-identical
/// output for the same input (verified by the
/// `streaming_matches_in_memory` round-trip test).
pub fn write_streaming<R: Read, W: Write + Seek>(
    input: &mut R,
    output: &mut W,
    chunk_size_sectors: u32,
) -> Result<CowStreamMeta> {
    write_streaming_impl(&mut SequentialReader { input }, output, chunk_size_sectors)
}

/// Sparse-aware [`write_streaming`] for real files. Uses `SEEK_DATA` /
/// `SEEK_HOLE` to skip whole chunks that lie inside a sparse gap, so a
/// multi-GB hole in a raw disk image costs a couple of `lseek`s instead of
/// reading — and zero-scanning — every byte. The COW output is byte-identical
/// to [`write_streaming`] over the same logical content (holes and explicit
/// zeros both elide to no exception); only the read cost differs.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn write_streaming_sparse<W: Write + Seek>(
    input: &std::fs::File,
    input_len: u64,
    output: &mut W,
    chunk_size_sectors: u32,
) -> Result<CowStreamMeta> {
    let mut reader = SparseReader {
        file: input,
        len: input_len,
        pos: 0,
        data_start: 0,
        data_end: 0,
    };
    write_streaming_impl(&mut reader, output, chunk_size_sectors)
}

/// One logical input chunk's disposition, produced by a [`ChunkReader`].
enum ChunkKind {
    /// End of input.
    Eof,
    /// The whole chunk is a sparse gap — treat as all-zero; no read performed.
    /// Only the Unix sparse reader produces this; on other targets the fallback
    /// reader yields only `Data`/`Eof`, so the variant is never constructed.
    #[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
    Hole,
    /// The chunk was read into the caller's buffer (zero-padded on a short
    /// final read).
    Data,
}

/// Source of fixed-size input chunks for [`write_streaming_impl`]. The caller
/// zero-fills the buffer before each call, so a short final read is naturally
/// zero-padded and a `Hole` needs no work.
trait ChunkReader {
    fn next_chunk(&mut self, buf: &mut [u8]) -> Result<ChunkKind>;
}

/// Plain sequential reader — every non-EOF chunk is `Data`; all-zero chunks are
/// still elided downstream by the impl's zero check. Byte-identical to the
/// pre-refactor `write_streaming`.
struct SequentialReader<'a, R: Read> {
    input: &'a mut R,
}

impl<R: Read> ChunkReader for SequentialReader<'_, R> {
    fn next_chunk(&mut self, buf: &mut [u8]) -> Result<ChunkKind> {
        let mut filled = 0usize;
        while filled < buf.len() {
            let n = self
                .input
                .read(&mut buf[filled..])
                .context("cow::write_streaming: read input")?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        Ok(if filled == 0 {
            ChunkKind::Eof
        } else {
            ChunkKind::Data
        })
    }
}

/// Sparse-aware reader over a real file (Unix `SEEK_DATA` / `SEEK_HOLE`). Holds
/// the current data extent `[data_start, data_end)`; chunks fully before
/// `data_start` are gaps, chunks overlapping the extent are read positionally.
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct SparseReader<'a> {
    file: &'a std::fs::File,
    len: u64,
    pos: u64,
    data_start: u64,
    data_end: u64,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl ChunkReader for SparseReader<'_> {
    fn next_chunk(&mut self, buf: &mut [u8]) -> Result<ChunkKind> {
        use std::os::unix::fs::FileExt as _;
        let chunk_bytes = buf.len() as u64;
        if self.pos >= self.len {
            return Ok(ChunkKind::Eof);
        }
        // Re-probe the data extent once we've walked past the last one.
        if self.pos >= self.data_end {
            match rustix::fs::seek(self.file, rustix::fs::SeekFrom::Data(self.pos)) {
                Ok(data) => {
                    self.data_start = data;
                    self.data_end =
                        rustix::fs::seek(self.file, rustix::fs::SeekFrom::Hole(data))
                            .map_err(|e| anyhow::anyhow!("cow: SEEK_HOLE at {data}: {e}"))?;
                }
                // No data at/after `pos` — the rest of the file is a hole.
                Err(rustix::io::Errno::NXIO) => {
                    self.data_start = self.len;
                    self.data_end = self.len;
                }
                Err(e) => return Err(anyhow::anyhow!("cow: SEEK_DATA at {}: {e}", self.pos)),
            }
        }
        let chunk_end = (self.pos + chunk_bytes).min(self.len);
        if chunk_end <= self.data_start {
            // Whole chunk precedes the next data extent — a gap. Skip it.
            self.pos += chunk_bytes;
            Ok(ChunkKind::Hole)
        } else {
            // Chunk overlaps real data; read it (any hole bytes read as zeros).
            let want = (chunk_end - self.pos) as usize;
            self.file
                .read_exact_at(&mut buf[..want], self.pos)
                .with_context(|| format!("cow: read_at offset {}", self.pos))?;
            self.pos += chunk_bytes;
            Ok(ChunkKind::Data)
        }
    }
}

/// Shared streaming COW writer driven by a [`ChunkReader`]. Byte layout,
/// allocator, and traps are identical to the in-memory [`write`]; see its docs.
fn write_streaming_impl<CR: ChunkReader, W: Write + Seek>(
    reader: &mut CR,
    output: &mut W,
    chunk_size_sectors: u32,
) -> Result<CowStreamMeta> {
    validate_chunk_size(chunk_size_sectors).context("cow::write_streaming: chunk_size_sectors")?;

    let chunk_bytes = (chunk_size_sectors as usize) * (SECTOR_SIZE as usize);
    let exceptions_per_area = chunk_bytes / DISK_EXCEPTION_SIZE;
    let area_stride_chunks = (exceptions_per_area + 1) as u64;

    // Write header at offset 0 (chunk 0). The rest of chunk 0 stays as
    // a sparse hole until we extend the file at the end (reads as zero).
    // Trap 5: little-endian. Trap 6: valid = 1.
    let mut header = [0u8; DISK_HEADER_SIZE];
    header[0..4].copy_from_slice(&SNAP_MAGIC.to_le_bytes());
    header[4..8].copy_from_slice(&1u32.to_le_bytes());
    header[8..12].copy_from_slice(&SNAPSHOT_DISK_VERSION.to_le_bytes());
    header[12..16].copy_from_slice(&chunk_size_sectors.to_le_bytes());
    output
        .seek(SeekFrom::Start(0))
        .context("cow::write_streaming: seek to header")?;
    output
        .write_all(&header)
        .context("cow::write_streaming: write header")?;

    // Read buffer reused across iterations. Always zero-fill before each read
    // so a short last-chunk (or a Hole) is correctly zero-padded when written
    // into its (chunk_bytes-aligned) data slot.
    let mut buf = vec![0u8; chunk_bytes];
    let mut next_free: u64 = NUM_SNAPSHOT_HDR_CHUNKS;
    let mut src_idx: u64 = 0;
    let mut exception_count: u64 = 0;
    let mut max_new_chunk: u64 = 0;

    loop {
        buf.fill(0);
        match reader.next_chunk(&mut buf)? {
            ChunkKind::Eof => break,
            // A sparse gap is all-zero, so it yields no exception (IMPORT-03 /
            // dm-zero origin) — just advance the source chunk index.
            ChunkKind::Hole => src_idx += 1,
            ChunkKind::Data => {
                if buf.iter().all(|&b| b == 0) {
                    // dm-zero origin: skip all-zero chunks (IMPORT-03).
                    src_idx += 1;
                } else {
                    // Allocate a new_chunk slot, bumping past any metadata-chunk
                    // landing position (per dm-snap-persistent.c:275-282).
                    next_free = bump_past_metadata(next_free, area_stride_chunks);
                    let new_chunk = next_free;
                    next_free += 1;
                    max_new_chunk = max_new_chunk.max(new_chunk);

                    // Write the exception entry at its metadata-area slot. Each
                    // metadata chunk is a sparse hole (zeros); slots beyond the
                    // last entry naturally form sentinels (trap 3).
                    let exc_idx = exception_count as usize;
                    let area = (exc_idx / exceptions_per_area) as u64;
                    let slot_in_area = exc_idx % exceptions_per_area;
                    let metadata_chunk = NUM_SNAPSHOT_HDR_CHUNKS + area * area_stride_chunks;
                    let entry_off = (metadata_chunk as usize) * chunk_bytes
                        + slot_in_area * DISK_EXCEPTION_SIZE;
                    let mut entry = [0u8; DISK_EXCEPTION_SIZE];
                    entry[0..8].copy_from_slice(&src_idx.to_le_bytes());
                    entry[8..16].copy_from_slice(&new_chunk.to_le_bytes());
                    output
                        .seek(SeekFrom::Start(entry_off as u64))
                        .context("cow::write_streaming: seek to metadata slot")?;
                    output
                        .write_all(&entry)
                        .context("cow::write_streaming: write metadata entry")?;

                    // Write the chunk's data into its `new_chunk` slot.
                    let dst_off = new_chunk * (chunk_bytes as u64);
                    output
                        .seek(SeekFrom::Start(dst_off))
                        .context("cow::write_streaming: seek to data slot")?;
                    output
                        .write_all(&buf)
                        .context("cow::write_streaming: write data chunk")?;

                    exception_count += 1;
                    src_idx += 1;
                }
            }
        }
    }

    // Compute total_chunks: at least chunk 0 (header) + chunk 1 (metadata
    // area 0). Trap 4: if the last area filled exactly, the next area's
    // metadata chunk must also be present (as a zero sentinel).
    let mut total_chunks = max_new_chunk.max(NUM_SNAPSHOT_HDR_CHUNKS) + 1;
    if exception_count > 0 && exception_count.is_multiple_of(exceptions_per_area as u64) {
        let n_areas_used = exception_count / (exceptions_per_area as u64);
        let next_area_metadata = NUM_SNAPSHOT_HDR_CHUNKS + n_areas_used * area_stride_chunks;
        if next_area_metadata + 1 > total_chunks {
            total_chunks = next_area_metadata + 1;
        }
    }
    if total_chunks < 2 {
        total_chunks = 2;
    }

    let total_bytes = total_chunks * (chunk_bytes as u64);

    // Force the underlying file to extend to total_bytes IF it isn't
    // already that long. Writing a single 0 byte at offset
    // (total_bytes - 1) is the portable way to extend through the
    // `Write + Seek` trait (we can't call `set_len`).
    //
    // CRITICAL: only do this when current EOF < total_bytes. Otherwise
    // we'd overwrite the last byte of the most-recently-written data
    // chunk with 0 (corrupting the COW for any input whose final byte
    // happens to be non-zero). The trap-4 case is the one that needs
    // the extension; the common case naturally ends exactly at
    // total_bytes after the last data write.
    let cur_end = output
        .seek(SeekFrom::End(0))
        .context("cow::write_streaming: seek to end (length probe)")?;
    if cur_end < total_bytes {
        output
            .seek(SeekFrom::Start(total_bytes - 1))
            .context("cow::write_streaming: seek to extend")?;
        output
            .write_all(&[0u8])
            .context("cow::write_streaming: extend file to total_bytes")?;
    }

    output.flush().context("cow::write_streaming: flush")?;

    Ok(CowStreamMeta {
        total_bytes,
        input_chunks: src_idx,
        exception_count,
        chunk_size_sectors,
    })
}

/// Re-emit only the chunks that differ from `origin` as a write-once
/// dm-snapshot persistent COW.
///
/// `origin` is the composed previous layer (the source carapace, or the
/// prior scute); `snapshot` is the merged view after this layer's writes
/// (read through the live dm-snapshot device). The two are walked in
/// lockstep in `chunk_bytes`-sized buffers; a chunk is emitted (with the
/// `snapshot` bytes) only where it differs from `origin`, in ascending
/// chunk order — so the result is deterministic and each changed chunk is
/// written exactly once (BUILD.md §5.2).
///
/// Output layout, allocator, and traps are identical to
/// [`write_streaming`]; the only difference is the emit predicate
/// (differs-from-origin instead of non-zero-vs-dm-zero). The two streams
/// SHOULD have the same length (a dm-snapshot inherits its origin's
/// length); buffers are zero-filled before each read so a ragged tail
/// still compares correctly.
pub fn write_delta<O: Read, S: Read, W: Write + Seek>(
    origin: &mut O,
    snapshot: &mut S,
    output: &mut W,
    chunk_size_sectors: u32,
) -> Result<CowStreamMeta> {
    validate_chunk_size(chunk_size_sectors).context("cow::write_delta: chunk_size_sectors")?;

    let chunk_bytes = (chunk_size_sectors as usize) * (SECTOR_SIZE as usize);
    let exceptions_per_area = chunk_bytes / DISK_EXCEPTION_SIZE;
    let area_stride_chunks = (exceptions_per_area + 1) as u64;

    let mut header = [0u8; DISK_HEADER_SIZE];
    header[0..4].copy_from_slice(&SNAP_MAGIC.to_le_bytes());
    header[4..8].copy_from_slice(&1u32.to_le_bytes());
    header[8..12].copy_from_slice(&SNAPSHOT_DISK_VERSION.to_le_bytes());
    header[12..16].copy_from_slice(&chunk_size_sectors.to_le_bytes());
    output
        .seek(SeekFrom::Start(0))
        .context("cow::write_delta: seek to header")?;
    output
        .write_all(&header)
        .context("cow::write_delta: write header")?;

    let mut obuf = vec![0u8; chunk_bytes];
    let mut sbuf = vec![0u8; chunk_bytes];
    let mut next_free: u64 = NUM_SNAPSHOT_HDR_CHUNKS;
    let mut src_idx: u64 = 0;
    let mut exception_count: u64 = 0;
    let mut max_new_chunk: u64 = 0;

    loop {
        obuf.fill(0);
        sbuf.fill(0);
        let of = read_full(origin, &mut obuf).context("cow::write_delta: read origin")?;
        let sf = read_full(snapshot, &mut sbuf).context("cow::write_delta: read snapshot")?;
        if of == 0 && sf == 0 {
            break;
        }

        if obuf != sbuf {
            next_free = bump_past_metadata(next_free, area_stride_chunks);
            let new_chunk = next_free;
            next_free += 1;
            max_new_chunk = max_new_chunk.max(new_chunk);

            let exc_idx = exception_count as usize;
            let area = (exc_idx / exceptions_per_area) as u64;
            let slot_in_area = exc_idx % exceptions_per_area;
            let metadata_chunk = NUM_SNAPSHOT_HDR_CHUNKS + area * area_stride_chunks;
            let entry_off =
                (metadata_chunk as usize) * chunk_bytes + slot_in_area * DISK_EXCEPTION_SIZE;
            let mut entry = [0u8; DISK_EXCEPTION_SIZE];
            entry[0..8].copy_from_slice(&src_idx.to_le_bytes());
            entry[8..16].copy_from_slice(&new_chunk.to_le_bytes());
            output
                .seek(SeekFrom::Start(entry_off as u64))
                .context("cow::write_delta: seek to metadata slot")?;
            output
                .write_all(&entry)
                .context("cow::write_delta: write metadata entry")?;

            let dst_off = new_chunk * (chunk_bytes as u64);
            output
                .seek(SeekFrom::Start(dst_off))
                .context("cow::write_delta: seek to data slot")?;
            output
                .write_all(&sbuf)
                .context("cow::write_delta: write data chunk")?;

            exception_count += 1;
        }
        src_idx += 1;

        if of < chunk_bytes && sf < chunk_bytes {
            break;
        }
    }

    let mut total_chunks = max_new_chunk.max(NUM_SNAPSHOT_HDR_CHUNKS) + 1;
    if exception_count > 0 && exception_count.is_multiple_of(exceptions_per_area as u64) {
        let n_areas_used = exception_count / (exceptions_per_area as u64);
        let next_area_metadata = NUM_SNAPSHOT_HDR_CHUNKS + n_areas_used * area_stride_chunks;
        if next_area_metadata + 1 > total_chunks {
            total_chunks = next_area_metadata + 1;
        }
    }
    if total_chunks < 2 {
        total_chunks = 2;
    }
    let total_bytes = total_chunks * (chunk_bytes as u64);

    let cur_end = output
        .seek(SeekFrom::End(0))
        .context("cow::write_delta: seek to end (length probe)")?;
    if cur_end < total_bytes {
        output
            .seek(SeekFrom::Start(total_bytes - 1))
            .context("cow::write_delta: seek to extend")?;
        output
            .write_all(&[0u8])
            .context("cow::write_delta: extend file to total_bytes")?;
    }
    output.flush().context("cow::write_delta: flush")?;

    Ok(CowStreamMeta {
        total_bytes,
        input_chunks: src_idx,
        exception_count,
        chunk_size_sectors,
    })
}

/// Read up to `buf.len()` bytes, looping over short reads until the buffer
/// is full or EOF. Returns the number of bytes filled.
fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let n = r.read(&mut buf[filled..]).context("cow: read")?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

/// `dm-snap-persistent.c:275-282` `skip_metadata`. Returns the bumped
/// `next_free` if it would land on a metadata chunk slot, else returns
/// it unchanged.
///
/// Metadata chunks live at chunk indices
/// `NUM_SNAPSHOT_HDR_CHUNKS + N * area_stride_chunks` for any non-negative
/// integer N. The kernel computes
/// `(next_free - NUM_SNAPSHOT_HDR_CHUNKS) % area_stride_chunks == 0`;
/// we mirror.
///
/// # Invariant
///
/// Caller MUST pass `next_free >= NUM_SNAPSHOT_HDR_CHUNKS`. The COW
/// allocator initialises `next_free = NUM_SNAPSHOT_HDR_CHUNKS` and only
/// increments, so the invariant always holds. We hard-assert here (NOT
/// `debug_assert!`) because a violation silently corrupts the COW: the
/// previous "fall back to NUM_SNAPSHOT_HDR_CHUNKS" branch returned chunk
/// index 1 (which IS a metadata chunk slot), so a caller that tripped
/// the precondition would write data into a metadata chunk and the
/// kernel reader would mis-parse the exception map.
fn bump_past_metadata(next_free: u64, area_stride_chunks: u64) -> u64 {
    assert!(
        next_free >= NUM_SNAPSHOT_HDR_CHUNKS,
        "bump_past_metadata invariant violated: next_free ({next_free}) \
         must be >= NUM_SNAPSHOT_HDR_CHUNKS ({NUM_SNAPSHOT_HDR_CHUNKS})"
    );
    let offset = (next_free - NUM_SNAPSHOT_HDR_CHUNKS) % area_stride_chunks;
    if offset == 0 {
        // Would land on a metadata chunk — bump past it.
        next_free + 1
    } else {
        next_free
    }
}

// -- test-only minimal reader for the round-trip self-test (D-05 mitigation) ----

#[cfg(test)]
pub(crate) mod cow_reader {
    //! Minimal `dm-snapshot persistent COW` reader. Test-only per
    //! RESEARCH §Open-Q #2 / VALIDATION §Notes — v0.9 carapace device
    //! will need a `pub` reader; defer that hoist until then.

    use super::*;

    /// Parsed exception map recovered from a COW blob.
    pub(crate) struct CowExceptionMap {
        /// Chunk size in 512-byte sectors, from the COW header.
        pub(crate) chunk_size_sectors: u32,
        /// Map from old_chunk (origin) to new_chunk (cow data slot).
        pub(crate) exceptions: std::collections::BTreeMap<u64, u64>,
    }

    /// Parse a COW blob produced by `write()` and return the exception map.
    pub(crate) fn parse(cow: &[u8]) -> Result<CowExceptionMap> {
        if cow.len() < DISK_HEADER_SIZE {
            bail!("cow too small for header");
        }
        let magic = u32::from_le_bytes(cow[0..4].try_into().unwrap());
        let valid = u32::from_le_bytes(cow[4..8].try_into().unwrap());
        let version = u32::from_le_bytes(cow[8..12].try_into().unwrap());
        let chunk_size_sectors = u32::from_le_bytes(cow[12..16].try_into().unwrap());
        if magic != SNAP_MAGIC {
            bail!("bad magic: 0x{magic:08x} (expected 0x{SNAP_MAGIC:08x})");
        }
        if valid != 1 {
            bail!("valid != 1 (got {valid})");
        }
        if version != SNAPSHOT_DISK_VERSION {
            bail!("bad version: {version} (expected {SNAPSHOT_DISK_VERSION})");
        }
        validate_chunk_size(chunk_size_sectors)?;

        let chunk_bytes = (chunk_size_sectors as usize) * (SECTOR_SIZE as usize);
        let exceptions_per_area = chunk_bytes / DISK_EXCEPTION_SIZE;
        let area_stride_chunks = (exceptions_per_area + 1) as u64;
        let total_chunks = (cow.len() / chunk_bytes) as u64;

        let mut exceptions = std::collections::BTreeMap::new();
        let mut area: u64 = 0;
        loop {
            let metadata_chunk = NUM_SNAPSHOT_HDR_CHUNKS + area * area_stride_chunks;
            if metadata_chunk >= total_chunks {
                break;
            }
            let metadata_off = (metadata_chunk as usize) * chunk_bytes;
            let mut hit_sentinel = false;
            for slot in 0..exceptions_per_area {
                let entry_off = metadata_off + slot * DISK_EXCEPTION_SIZE;
                let old_chunk =
                    u64::from_le_bytes(cow[entry_off..entry_off + 8].try_into().unwrap());
                let new_chunk =
                    u64::from_le_bytes(cow[entry_off + 8..entry_off + 16].try_into().unwrap());
                if new_chunk == 0 {
                    // Sentinel — end of exceptions for this (and all subsequent) areas.
                    hit_sentinel = true;
                    break;
                }
                exceptions.insert(old_chunk, new_chunk);
            }
            if hit_sentinel {
                break;
            }
            area += 1;
        }

        Ok(CowExceptionMap {
            chunk_size_sectors,
            exceptions,
        })
    }

    /// Read the cow data slot for a given `new_chunk` from the cow blob.
    pub(crate) fn read_chunk(cow: &[u8], chunk_size_sectors: u32, new_chunk: u64) -> &[u8] {
        let chunk_bytes = (chunk_size_sectors as usize) * (SECTOR_SIZE as usize);
        let off = (new_chunk as usize) * chunk_bytes;
        &cow[off..off + chunk_bytes]
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    /// `write_delta` emits exactly the chunks that differ from the origin,
    /// and applying that COW over the origin reproduces the snapshot.
    #[test]
    fn write_delta_emits_only_changed_chunks() {
        let css = 8u32; // carapace-spec chunk size (4096 bytes)
        let cb = (css as usize) * SECTOR_SIZE as usize;
        let n = 6usize;

        // Origin: a distinct marker per chunk.
        let mut origin = vec![0u8; n * cb];
        for i in 0..n {
            origin[i * cb] = (i as u8) + 1;
        }
        // Snapshot: change chunks 1 and 4 only.
        let mut snap = origin.clone();
        snap[cb] = 0xAA;
        snap[4 * cb] = 0xBB;
        snap[4 * cb + 5] = 0xCC;

        let mut o = Cursor::new(origin.clone());
        let mut s = Cursor::new(snap.clone());
        let mut out = Cursor::new(Vec::<u8>::new());
        let meta = write_delta(&mut o, &mut s, &mut out, css).unwrap();
        assert_eq!(meta.exception_count, 2, "only 2 chunks changed");
        assert_eq!(meta.input_chunks, n as u64);

        // Reconstruct: origin with the COW's changed chunks overlaid.
        let cow = out.into_inner();
        let map = cow_reader::parse(&cow).unwrap();
        assert_eq!(map.exceptions.len(), 2);
        let mut recon = origin.clone();
        for (old, new) in &map.exceptions {
            let data = cow_reader::read_chunk(&cow, css, *new);
            let off = (*old as usize) * cb;
            recon[off..off + cb].copy_from_slice(data);
        }
        assert_eq!(
            recon, snap,
            "origin + delta COW must reproduce the snapshot"
        );
    }

    /// Identical origin and snapshot ⇒ an empty delta (no exceptions).
    #[test]
    fn write_delta_identical_inputs_emit_nothing() {
        let css = 8u32;
        let cb = (css as usize) * SECTOR_SIZE as usize;
        let img = {
            let mut v = vec![0u8; 4 * cb];
            v[cb] = 9;
            v
        };
        let mut o = Cursor::new(img.clone());
        let mut s = Cursor::new(img.clone());
        let mut out = Cursor::new(Vec::<u8>::new());
        let meta = write_delta(&mut o, &mut s, &mut out, css).unwrap();
        assert_eq!(meta.exception_count, 0);
        assert!(
            cow_reader::parse(&out.into_inner())
                .unwrap()
                .exceptions
                .is_empty()
        );
    }

    /// BL-01 / T-43-01: streaming COW writer produces byte-identical
    /// output to the in-memory `write` for the same input. Covers the
    /// happy path (single non-zero chunk).
    #[test]
    fn streaming_matches_in_memory_single_chunk() {
        let chunk_size_sectors = 32u32;
        let chunk_bytes = (chunk_size_sectors as usize) * (SECTOR_SIZE as usize);
        let mut input = vec![0u8; 4 * chunk_bytes];
        input[2 * chunk_bytes] = 0x42;

        let in_mem = write(&input, chunk_size_sectors).unwrap();

        let mut input_cursor = Cursor::new(input.clone());
        let mut out = Cursor::new(Vec::<u8>::new());
        let meta = write_streaming(&mut input_cursor, &mut out, chunk_size_sectors).unwrap();
        assert_eq!(meta.exception_count, 1);
        assert_eq!(meta.input_chunks, 4);
        assert_eq!(meta.total_bytes as usize, in_mem.len());
        assert_eq!(
            out.into_inner(),
            in_mem,
            "streaming bytes must match in-memory"
        );
    }

    /// BL-01: streaming output matches in-memory for the round-trip
    /// fixture (4 non-zero chunks scattered across 100).
    #[test]
    fn streaming_matches_in_memory_round_trip_fixture() {
        let chunk_size_sectors = 32u32;
        let chunk_bytes = (chunk_size_sectors as usize) * (SECTOR_SIZE as usize);
        let n_chunks = 100usize;
        let mut input = vec![0u8; n_chunks * chunk_bytes];
        for &(idx, fill) in &[(5usize, 0xA1u8), (17, 0xB2), (42, 0xC3), (99, 0xD4)] {
            input[idx * chunk_bytes..(idx + 1) * chunk_bytes].fill(fill);
        }

        let in_mem = write(&input, chunk_size_sectors).unwrap();

        let mut input_cursor = Cursor::new(input.clone());
        let mut out = Cursor::new(Vec::<u8>::new());
        write_streaming(&mut input_cursor, &mut out, chunk_size_sectors).unwrap();
        assert_eq!(
            out.into_inner(),
            in_mem,
            "streaming round-trip bytes must match"
        );
    }

    /// BL-01: streaming output matches in-memory for the trap-4 case
    /// (area fills exactly so a trailing zero metadata chunk is needed).
    #[test]
    fn streaming_matches_in_memory_trap4() {
        let chunk_size_sectors = 8u32;
        let chunk_bytes = (chunk_size_sectors as usize) * (SECTOR_SIZE as usize);
        let exceptions_per_area = chunk_bytes / DISK_EXCEPTION_SIZE;
        let n_nonzero = exceptions_per_area;
        let mut input = vec![0u8; n_nonzero * chunk_bytes];
        for i in 0..n_nonzero {
            input[i * chunk_bytes] = (i % 251) as u8 + 1;
        }

        let in_mem = write(&input, chunk_size_sectors).unwrap();
        let mut input_cursor = Cursor::new(input.clone());
        let mut out = Cursor::new(Vec::<u8>::new());
        write_streaming(&mut input_cursor, &mut out, chunk_size_sectors).unwrap();
        assert_eq!(
            out.into_inner(),
            in_mem,
            "streaming trap-4 case must match in-memory"
        );
    }

    /// BL-01: streaming output matches in-memory for an all-zero input
    /// (degenerate case — no exceptions, just header + first metadata chunk).
    #[test]
    fn streaming_matches_in_memory_all_zero() {
        let chunk_size_sectors = 32u32;
        let chunk_bytes = (chunk_size_sectors as usize) * (SECTOR_SIZE as usize);
        let input = vec![0u8; 8 * chunk_bytes];

        let in_mem = write(&input, chunk_size_sectors).unwrap();
        let mut input_cursor = Cursor::new(input);
        let mut out = Cursor::new(Vec::<u8>::new());
        let meta = write_streaming(&mut input_cursor, &mut out, chunk_size_sectors).unwrap();
        assert_eq!(meta.exception_count, 0);
        assert_eq!(
            out.into_inner(),
            in_mem,
            "all-zero streaming must match in-memory"
        );
    }

    /// BL-01: streaming round-trips with the existing cow_reader. Real
    /// proof that streaming output is a valid dm-snapshot persistent COW.
    #[test]
    fn streaming_round_trip_with_reader() {
        let chunk_size_sectors = 32u32;
        let chunk_bytes = (chunk_size_sectors as usize) * (SECTOR_SIZE as usize);
        let n_chunks = 64usize;
        let mut input = vec![0u8; n_chunks * chunk_bytes];
        for &(idx, fill) in &[(3usize, 0x11u8), (7, 0x22), (50, 0x33)] {
            input[idx * chunk_bytes..(idx + 1) * chunk_bytes].fill(fill);
        }

        let mut input_cursor = Cursor::new(input);
        let mut out = Cursor::new(Vec::<u8>::new());
        write_streaming(&mut input_cursor, &mut out, chunk_size_sectors).unwrap();
        let cow = out.into_inner();

        let parsed = cow_reader::parse(&cow).unwrap();
        assert_eq!(parsed.exceptions.len(), 3);
        for &(src_idx, fill) in &[(3u64, 0x11u8), (7, 0x22), (50, 0x33)] {
            let new_chunk = parsed.exceptions[&src_idx];
            let recovered = cow_reader::read_chunk(&cow, parsed.chunk_size_sectors, new_chunk);
            assert!(
                recovered.iter().all(|&b| b == fill),
                "src chunk {src_idx} round-trip mismatch"
            );
        }
    }

    /// The sparse reader produces byte-identical COW output to the sequential
    /// reader over the same on-disk (sparse) file, and reports the same
    /// exception / input-chunk counts. Data is placed at two chunks with a
    /// large gap between them, so SEEK_DATA/SEEK_HOLE are exercised.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn sparse_matches_sequential() {
        use std::os::unix::fs::FileExt as _;

        let css = 8u32; // carapace-spec chunk size (4096 bytes)
        let cb = (css as usize) * SECTOR_SIZE as usize;
        let n_chunks = 64usize;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sparse.img");
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        // A sparse file: length covers 64 chunks, but only chunks 3 and 50 hold
        // data — everything else is a hole.
        f.set_len((n_chunks * cb) as u64).unwrap();
        f.write_all_at(&vec![0xEEu8; cb], (3 * cb) as u64).unwrap();
        f.write_all_at(&vec![0x77u8; cb], (50 * cb) as u64).unwrap();
        f.sync_all().unwrap();
        let len = f.metadata().unwrap().len();

        // Sparse path.
        let mut sparse_out = Cursor::new(Vec::<u8>::new());
        let sparse_meta = write_streaming_sparse(&f, len, &mut sparse_out, css).unwrap();

        // Sequential path over the identical bytes.
        let mut seq_in = std::fs::File::open(&path).unwrap();
        let mut seq_out = Cursor::new(Vec::<u8>::new());
        let seq_meta = write_streaming(&mut seq_in, &mut seq_out, css).unwrap();

        assert_eq!(
            sparse_out.into_inner(),
            seq_out.into_inner(),
            "sparse COW must be byte-identical to sequential"
        );
        assert_eq!(sparse_meta.exception_count, 2, "two data chunks");
        assert_eq!(sparse_meta.exception_count, seq_meta.exception_count);
        assert_eq!(sparse_meta.input_chunks, seq_meta.input_chunks);
        assert_eq!(sparse_meta.total_bytes, seq_meta.total_bytes);
    }

    /// The sparse reader handles a trailing hole (SEEK_DATA returns ENXIO past
    /// the last data extent) and data in the very last, partial chunk.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn sparse_trailing_hole_and_partial_final_chunk() {
        use std::os::unix::fs::FileExt as _;

        let css = 8u32;
        let cb = (css as usize) * SECTOR_SIZE as usize;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sparse2.img");
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        // Data at chunk 0, then a long hole. Total length is not a chunk
        // multiple (a short final chunk, which is a hole).
        f.set_len((10 * cb + 123) as u64).unwrap();
        f.write_all_at(&vec![0x5Au8; cb], 0).unwrap();
        f.sync_all().unwrap();
        let len = f.metadata().unwrap().len();

        let mut sparse_out = Cursor::new(Vec::<u8>::new());
        let sparse_meta = write_streaming_sparse(&f, len, &mut sparse_out, css).unwrap();

        let mut seq_in = std::fs::File::open(&path).unwrap();
        let mut seq_out = Cursor::new(Vec::<u8>::new());
        let seq_meta = write_streaming(&mut seq_in, &mut seq_out, css).unwrap();

        assert_eq!(sparse_out.into_inner(), seq_out.into_inner());
        assert_eq!(sparse_meta.exception_count, 1);
        assert_eq!(sparse_meta.input_chunks, seq_meta.input_chunks);
    }

    /// BL-01: streaming rejects bad chunk sizes (same validation as `write`).
    #[test]
    fn streaming_rejects_bad_chunk_size() {
        let mut input = Cursor::new(vec![0u8; 1024]);
        let mut out = Cursor::new(Vec::<u8>::new());
        assert!(write_streaming(&mut input, &mut out, 0).is_err());
        assert!(write_streaming(&mut input, &mut out, 33).is_err());
    }

    /// WR-02: `cow::write` itself rejects bad chunk sizes at runtime
    /// (release mode) — not just under `debug_assert!`. Earlier code
    /// would `attempt to divide by zero` if a release-mode caller skipped
    /// `validate_chunk_size` and passed `0`.
    #[test]
    fn write_rejects_zero_chunk_size_in_release_mode() {
        let err = write(b"\x00\x01\x02\x03", 0).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("chunk_size_sectors") && msg.contains("> 0"),
            "expected chunk_size validation error, got: {msg}"
        );
    }

    /// WR-02: `cow::write` rejects non-power-of-two at runtime.
    #[test]
    fn write_rejects_non_power_of_two_in_release_mode() {
        let err = write(b"\x00\x01\x02\x03", 33).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("power of two"), "got: {msg}");
    }

    /// IMPORT-07 / RESEARCH §2 trap #1: validate rejects non-power-of-two,
    /// < 8, and zero; accepts valid values.
    #[test]
    fn validate_chunk_size_rejects_bad_values() {
        assert!(validate_chunk_size(0).is_err(), "zero must fail");
        assert!(
            validate_chunk_size(7).is_err(),
            "7 < 8 and not power-of-two"
        );
        assert!(
            validate_chunk_size(4).is_err(),
            "4 < 8 (even though power-of-two)"
        );
        assert!(validate_chunk_size(9).is_err(), "9 not power of two");
        assert!(validate_chunk_size(8).is_ok(), "8 is minimum valid");
        assert!(validate_chunk_size(16).is_ok(), "16 is valid");
        assert!(validate_chunk_size(32).is_ok(), "32 is valid (default)");
        assert!(validate_chunk_size(2048).is_ok(), "2048 is valid");
    }

    /// IMPORT-02 / RESEARCH §2 trap #5: header bytes 0..16 are the magic
    /// (LE), valid=1, version=1, chunk_size in sectors.
    ///
    /// Hex-dump confirmation: `SNAP_MAGIC = 0x70416e53` stored as LE =
    /// bytes `[0x53, 0x6e, 0x41, 0x70]`.
    #[test]
    fn header_magic_le() {
        // Single non-zero chunk so write produces a non-empty cow.
        let chunk_bytes = 32 * SECTOR_SIZE as usize;
        let mut input = vec![0u8; chunk_bytes];
        input[0] = 0xFF;
        let cow = write(&input, 32).unwrap();
        // RESEARCH §2: hex-dump shows `53 6e 41 70` at offset 0 (LE).
        assert_eq!(&cow[0..4], &[0x53, 0x6e, 0x41, 0x70]);
        assert_eq!(&cow[4..8], &1u32.to_le_bytes(), "valid = 1");
        assert_eq!(&cow[8..12], &1u32.to_le_bytes(), "version = 1");
        assert_eq!(&cow[12..16], &32u32.to_le_bytes(), "chunk_size in sectors");
    }

    /// IMPORT-02 / RESEARCH §2 trap #2: a single non-zero source chunk
    /// produces ONE exception with `new_chunk = 2` (chunk 0 = header,
    /// chunk 1 = first metadata area, chunk 2 = first data slot).
    #[test]
    fn single_nonzero_chunk_uses_new_chunk_two() {
        let chunk_bytes = 32 * SECTOR_SIZE as usize;
        let mut input = vec![0u8; 4 * chunk_bytes];
        // Mark chunk 2 (the third source chunk) as non-zero.
        input[2 * chunk_bytes] = 0x42;
        let cow = write(&input, 32).unwrap();
        let parsed = cow_reader::parse(&cow).unwrap();
        assert_eq!(parsed.exceptions.len(), 1);
        assert_eq!(
            parsed.exceptions[&2], 2,
            "first data chunk = 2 (chunks 0=hdr, 1=md, 2=first data)"
        );
    }

    /// IMPORT-03 / RESEARCH §2 dm-zero origin: all-zero source chunks
    /// produce NO exception entry.
    #[test]
    fn skip_zero_chunks() {
        let chunk_bytes = 32 * SECTOR_SIZE as usize;
        let input = vec![0u8; 8 * chunk_bytes]; // all zeros
        let cow = write(&input, 32).unwrap();
        let parsed = cow_reader::parse(&cow).unwrap();
        assert!(
            parsed.exceptions.is_empty(),
            "no exceptions for all-zero input"
        );
    }

    /// RESEARCH §2 trap #4: if an area fills exactly, the NEXT area's
    /// metadata chunk MUST also be present and zeroed so the reader
    /// terminates correctly.
    ///
    /// For chunk_size = 8 sectors = 4096 bytes, exceptions_per_area =
    /// 4096 / 16 = 256. We feed exactly 256 non-zero chunks and confirm
    /// the cow contains a zeroed next-area metadata chunk.
    #[test]
    fn area_filled_exactly_writes_next_zero_metadata() {
        let chunk_size_sectors = 8u32;
        let chunk_bytes = (chunk_size_sectors as usize) * (SECTOR_SIZE as usize);
        let exceptions_per_area = chunk_bytes / DISK_EXCEPTION_SIZE; // 256
        let n_nonzero = exceptions_per_area;
        let mut input = vec![0u8; n_nonzero * chunk_bytes];
        for i in 0..n_nonzero {
            input[i * chunk_bytes] = (i % 251) as u8 + 1; // ensure non-zero
        }
        let cow = write(&input, chunk_size_sectors).unwrap();
        let parsed = cow_reader::parse(&cow).unwrap();
        assert_eq!(
            parsed.exceptions.len(),
            n_nonzero,
            "all {n_nonzero} exceptions parsed"
        );

        // The next area's metadata chunk should be in the cow and all-zero
        // (trap 4 / dm-snap-persistent.c:744 mirror).
        let area_stride = (exceptions_per_area + 1) as u64;
        let next_area_md_chunk = NUM_SNAPSHOT_HDR_CHUNKS + area_stride; // area 1
        let off = (next_area_md_chunk as usize) * chunk_bytes;
        assert!(
            cow.len() >= off + chunk_bytes,
            "cow must include next-area metadata chunk (len={}, need={})",
            cow.len(),
            off + chunk_bytes
        );
        assert!(
            cow[off..off + chunk_bytes].iter().all(|&b| b == 0),
            "next-area metadata chunk must be all zeros (sentinel)"
        );
    }

    /// RESEARCH §2 traps #1, #2, #5, #6: the round-trip self-test (D-05
    /// mitigation). Writer → cow_reader → byte-equal recovery for every
    /// non-zero source chunk; zero source chunks have no exception.
    ///
    /// This is the ONLY Phase 43 gate that catches a wrong COW byte
    /// layout (kernel acceptance is deferred to v0.9 boot tests per
    /// CONTEXT D-05).
    #[test]
    fn round_trip_byte_exact() {
        let chunk_size_sectors = 32u32;
        let chunk_bytes = (chunk_size_sectors as usize) * (SECTOR_SIZE as usize);
        let n_chunks = 100usize;
        let mut input = vec![0u8; n_chunks * chunk_bytes];

        // Per RESEARCH §"Round-trip self-test": chunks 5, 17, 42, 99
        // marked non-zero with distinct fill patterns so a swap doesn't pass.
        let nonzero_chunks: &[(usize, u8)] = &[(5, 0xA1), (17, 0xB2), (42, 0xC3), (99, 0xD4)];
        for &(idx, fill) in nonzero_chunks {
            input[idx * chunk_bytes..(idx + 1) * chunk_bytes].fill(fill);
        }

        let cow = write(&input, chunk_size_sectors).unwrap();
        let parsed = cow_reader::parse(&cow).unwrap();

        assert_eq!(
            parsed.exceptions.len(),
            nonzero_chunks.len(),
            "exception count mismatch"
        );
        for &(src_idx, fill) in nonzero_chunks {
            let new_chunk = parsed
                .exceptions
                .get(&(src_idx as u64))
                .copied()
                .unwrap_or_else(|| panic!("missing exception for src chunk {src_idx}"));
            // Use the chunk_size_sectors parsed FROM the cow header (not just the local
            // variable) — exercises the CowExceptionMap::chunk_size_sectors field.
            let recovered = cow_reader::read_chunk(&cow, parsed.chunk_size_sectors, new_chunk);
            assert!(
                recovered.iter().all(|&b| b == fill),
                "src chunk {src_idx} (fill 0x{fill:02x}) recovered with wrong bytes"
            );
        }
        // Zero chunks must have NO entry in the exception map.
        for src_idx in 0..n_chunks {
            if !nonzero_chunks.iter().any(|(i, _)| *i == src_idx) {
                assert!(
                    !parsed.exceptions.contains_key(&(src_idx as u64)),
                    "zero src chunk {src_idx} must have no exception"
                );
            }
        }
    }
}
