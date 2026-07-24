// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! dm-verity v1 hash tree builder. Byte-exact per cryptsetup's
//! `struct verity_sb` and `verity_hash.c` — see RESEARCH.md §3 for the
//! authoritative spec citations.
//!
//! `pub` so Phase 44 (`pichi pull`) can re-use the same hash-tree code
//! when scutes land in the local cache (CONTEXT D-03). Designed to be
//! deterministic: same `(cow_bytes, params)` → same `(blob, root_hash)`.
//!
//! All multi-byte fields in the superblock are little-endian
//! (`cpu_to_le16` / `cpu_to_le32` / `cpu_to_le64` in cryptsetup; we use
//! `to_le_bytes` here).

use sha2::{Digest as _, Sha256};
use thiserror::Error;

/// Errors produced by `verity::compute` (and shared with the streaming
/// `VerityBuilder` introduced for BL-01).
///
/// `pub` because Phase 44's `pichi pull` re-uses `compute` per CONTEXT
/// D-03 — the typed error surface is part of that contract.
#[derive(Debug, Error)]
pub enum VerityError {
    /// `hash_block_size / digest_size < 2`. A hash block must hold at
    /// least two child digests, otherwise the tree never collapses (each
    /// parent level has the same node count as its child) and
    /// `compute()` would spin forever. Phase 42 D-06 locks
    /// `hash_block_size = 4096` so this is unreachable from the in-tree
    /// caller, but `VerityParams` is `pub` and Phase 44 picks block sizes
    /// at consumption time.
    #[error(
        "hash_block_size ({hash_block_size}) must hold at least two digests \
         (digest_size = {digest_size}); got hashes_per_block = {hashes_per_block}"
    )]
    HashBlockTooSmall {
        /// `params.hash_block_size`.
        hash_block_size: u32,
        /// SHA-256 digest size in bytes (32).
        digest_size: u32,
        /// Computed `hashes_per_block` (= 1 means infinite loop).
        hashes_per_block: u32,
    },
    /// `params.salt.len() > 256`. The on-disk `verity_sb.salt` field is
    /// fixed at 256 bytes; longer salts cannot be written.
    #[error("salt too long: got {got} bytes, max is {max}")]
    SaltTooLong {
        /// Actual salt length.
        got: usize,
        /// Maximum allowed (256).
        max: usize,
    },
    /// `data_block_size` or `hash_block_size` is zero. Both must be
    /// positive integers (cryptsetup rejects zero too).
    #[error(
        "block sizes must be > 0; got data_block_size = {data_block_size}, hash_block_size = {hash_block_size}"
    )]
    BlockSizeZero {
        /// `params.data_block_size`.
        data_block_size: u32,
        /// `params.hash_block_size`.
        hash_block_size: u32,
    },
}

/// `verity_sb.signature` — `cryptsetup lib/verity/verity.c:22`.
pub const VERITY_SIGNATURE: &[u8; 8] = b"verity\0\0";
/// Total size of `struct verity_sb` per RESEARCH §3 lines 268–298.
pub const VERITY_SB_SIZE: usize = 512;
/// SHA-256 digest size in bytes.
const SHA256_DIGEST_SIZE: usize = 32;
/// Verity v1 superblock format version. Public so the manifest producer can
/// convey it as a reconstruction input rather than re-hardcoding the literal.
pub const VERITY_FORMAT_VERSION: u32 = 1;
/// "normal" hash type per RESEARCH §3 lines 305–308 (Chrome OS would be 0).
/// Public for the same reason as [`VERITY_FORMAT_VERSION`].
pub const VERITY_HASH_TYPE_NORMAL: u32 = 1;
/// Maximum salt bytes that fit in `verity_sb.salt[256]`.
const VERITY_SB_SALT_MAX: usize = 256;
/// Algorithm string field is 32 bytes, NUL-padded.
const VERITY_SB_ALGO_LEN: usize = 32;

/// Verity construction parameters. Locked Phase 42 D-06 defaults:
/// `data_block_size = 4096`, `hash_block_size = 4096`, sha256 algorithm.
///
/// # Preconditions (enforced by [`VerityParams::validate`])
///
/// - `data_block_size > 0`, `hash_block_size > 0`
/// - `hash_block_size / SHA256_DIGEST_SIZE >= 2` (so the hash-tree
///   actually shrinks at each parent level)
/// - `salt.len() <= 256` (fits in `verity_sb.salt[256]`)
///
/// `compute` and `VerityBuilder::new` both call `validate` and surface
/// any violation as a typed [`VerityError`].
#[derive(Debug, Clone)]
pub struct VerityParams {
    /// `verity_sb.data_block_size`. The cow byte length passed to
    /// `compute` is rounded up to a multiple of this (the trailing data
    /// block is zero-padded for hashing).
    pub data_block_size: u32,
    /// `verity_sb.hash_block_size`.
    pub hash_block_size: u32,
    /// FULL salt as written into `verity_sb.salt[..salt_size]` — 32-byte
    /// zero prefix (CONTEXT D-01) plus optional author-supplied suffix.
    /// Length MUST be ≤ 256.
    pub salt: Vec<u8>,
    /// `verity_sb.uuid`. Deterministic per RESEARCH §Open-Q #3:
    /// `SHA256(salt || cow_digest)[..16]`.
    pub uuid: [u8; 16],
}

impl VerityParams {
    /// Check the [`VerityParams`] preconditions documented above. Both
    /// `compute` and `VerityBuilder::new` (BL-01 streaming) call this
    /// before doing any work.
    ///
    /// Returns `Ok(())` when the params are usable, otherwise a typed
    /// [`VerityError`] describing the violation.
    pub fn validate(&self) -> Result<(), VerityError> {
        if self.data_block_size == 0 || self.hash_block_size == 0 {
            return Err(VerityError::BlockSizeZero {
                data_block_size: self.data_block_size,
                hash_block_size: self.hash_block_size,
            });
        }
        if self.salt.len() > VERITY_SB_SALT_MAX {
            return Err(VerityError::SaltTooLong {
                got: self.salt.len(),
                max: VERITY_SB_SALT_MAX,
            });
        }
        // Guard the WR-01 infinite-loop case: a hash block must hold at
        // least two child digests, otherwise each parent level matches
        // its child level in node count and `compute()` spins forever.
        let hpb = (self.hash_block_size as usize) / SHA256_DIGEST_SIZE;
        if hpb < 2 {
            return Err(VerityError::HashBlockTooSmall {
                hash_block_size: self.hash_block_size,
                digest_size: SHA256_DIGEST_SIZE as u32,
                hashes_per_block: hpb as u32,
            });
        }
        Ok(())
    }
}

/// Streaming hash-tree builder for BL-01 / T-43-01.
///
/// Lets callers feed the COW blob one [`VerityParams::data_block_size`]
/// chunk at a time without ever materialising the full cow as a `Vec<u8>`.
/// Internally accumulates only the leaf hashes (32 bytes per data block,
/// i.e. ~0.78% of the cow size for SHA-256 + 4 KiB blocks) until
/// [`finalize`] builds the parent levels and assembles the on-disk blob.
///
/// `compute` is now a thin convenience wrapper around `VerityBuilder` for
/// in-memory inputs (tests, small fixtures); production callers
/// (`pichi import`, Phase 44 `pichi pull`) drive the builder directly
/// from a streaming read of the cow file.
///
/// # Memory profile (for a 10 GiB cow at default 4 KiB data blocks)
///
/// - leaf hashes:   ~80 MiB (32 bytes × 2 621 440 data blocks)
/// - parent levels: ~640 KiB (each level shrinks by 128×)
/// - in-flight per-block buffer: 4 KiB (caller's read buffer)
///
/// All bounded; no proportional-to-input allocations.
///
/// [`finalize`]: VerityBuilder::finalize
#[derive(Debug)]
pub struct VerityBuilder {
    params: VerityParams,
    leaf_hashes: Vec<[u8; SHA256_DIGEST_SIZE]>,
    /// Total bytes accepted via `add_data_block` so far. Used by
    /// `finalize` to write `verity_sb.data_blocks` (in BLOCKS, not bytes —
    /// trap V5).
    bytes_accepted: u64,
    /// Set to true once a short block has been fed; subsequent
    /// `add_data_block` calls are rejected. (Only the LAST block may be
    /// short.)
    sealed: bool,
}

impl VerityBuilder {
    /// Create a new streaming hash-tree builder. Validates `params`
    /// up-front — see [`VerityParams::validate`] for the precondition
    /// list (BL-01 / WR-01).
    pub fn new(params: &VerityParams) -> Result<Self, VerityError> {
        params.validate()?;
        Ok(Self {
            params: params.clone(),
            leaf_hashes: Vec::new(),
            bytes_accepted: 0,
            sealed: false,
        })
    }

    /// Hash one data block and append its digest to the leaf level.
    ///
    /// `block.len()` MUST be ≤ `params.data_block_size`. A short block
    /// (less than `data_block_size` bytes) is zero-padded for hashing
    /// (matching `compute`'s in-memory behaviour) and seals the builder
    /// — any further `add_data_block` call returns
    /// [`VerityBuilderError::AfterShortBlock`].
    pub fn add_data_block(&mut self, block: &[u8]) -> Result<(), VerityBuilderError> {
        if self.sealed {
            return Err(VerityBuilderError::AfterShortBlock);
        }
        let dbs = self.params.data_block_size as usize;
        if block.len() > dbs {
            return Err(VerityBuilderError::BlockTooLarge {
                got: block.len(),
                max: dbs,
            });
        }
        // Zero-pad short blocks (RESEARCH §3: hashing operates over
        // fixed-size blocks). Reusing a single per-call buffer here is
        // fine — leaf hashing is the dominant CPU cost so the alloc is
        // noise.
        let mut padded = vec![0u8; dbs];
        padded[..block.len()].copy_from_slice(block);
        self.leaf_hashes.push(hash_v1(&self.params.salt, &padded));
        self.bytes_accepted += block.len() as u64;
        if block.len() < dbs {
            self.sealed = true;
        }
        Ok(())
    }

    /// Build the parent levels, assemble the on-disk blob (top-down),
    /// and return the [`VerityOutput`]. Consumes the builder.
    pub fn finalize(self) -> VerityOutput {
        let hbs = self.params.hash_block_size as usize;
        let hash_per_block = hashes_per_block(hbs, SHA256_DIGEST_SIZE);
        let data_blocks = self.leaf_hashes.len() as u64;

        // Build levels bottom-up. levels[0] = leaf level, levels[N-1] = root.
        // The ROOT level is the topmost level whose hashes ALL FIT in a single
        // hash block (i.e. `top.len() <= hash_per_block`). Per dm-verity spec
        // + RESEARCH §3, the root block is `salt || top_level_packed_into_one_block`
        // (zero-padded), regardless of whether `top` has 1 or `hash_per_block`
        // entries. The loop therefore terminates when the current level fits
        // in one block, NOT when it has exactly 1 hash.
        //
        // Trap V6b (regression for D-04 cross-validation gate): the previous
        // condition `> 1` over-built the tree by one level for any input
        // where `1 < leaf_count <= hash_per_block` (e.g. a 100-block cow at
        // the locked 4 KiB / 4 KiB defaults). `veritysetup verify` rejected
        // those blobs with exit 2 ("Verification failed at position 0"). See
        // `finalize_matches_veritysetup_for_single_block_tree` for the
        // regression coverage.
        //
        // Edge case: 0 data blocks (empty cow) — degenerate, the leaf level
        // is empty, the loop does not execute (`0 > hash_per_block` is
        // false), and Step 4 hashes the all-zero root block (matching the
        // previous behaviour for empty inputs).
        let mut levels: Vec<Vec<[u8; SHA256_DIGEST_SIZE]>> = vec![self.leaf_hashes];
        while levels.last().map_or(0, std::vec::Vec::len) > hash_per_block {
            let cur = levels.last().expect("non-empty");
            let n_parent = cur.len().div_ceil(hash_per_block);
            let mut parent: Vec<[u8; SHA256_DIGEST_SIZE]> = Vec::with_capacity(n_parent);
            for chunk_idx in 0..n_parent {
                let mut hb = vec![0u8; hbs];
                let start = chunk_idx * hash_per_block;
                let end = (start + hash_per_block).min(cur.len());
                for (slot, src) in cur[start..end].iter().enumerate() {
                    let off = slot * SHA256_DIGEST_SIZE;
                    hb[off..off + SHA256_DIGEST_SIZE].copy_from_slice(src);
                }
                parent.push(hash_v1(&self.params.salt, &hb));
            }
            levels.push(parent);
        }

        // Build the output blob.
        let mut blob = Vec::new();

        // 1. Superblock at offset 0.
        write_superblock(&mut blob, &self.params, data_blocks);

        // 2. Zero-pad to hash_block_size boundary (trap V6).
        blob.resize(hbs, 0);

        // 3. Levels TOP-DOWN (trap V7).
        let n_levels = levels.len();
        for level_idx in (0..n_levels).rev() {
            let level_hashes = &levels[level_idx];
            let n_blocks = level_hashes.len().div_ceil(hash_per_block);
            for block_idx in 0..n_blocks {
                let mut hb = vec![0u8; hbs];
                let start = block_idx * hash_per_block;
                let end = (start + hash_per_block).min(level_hashes.len());
                for (slot, src) in level_hashes[start..end].iter().enumerate() {
                    let off = slot * SHA256_DIGEST_SIZE;
                    hb[off..off + SHA256_DIGEST_SIZE].copy_from_slice(src);
                }
                blob.extend_from_slice(&hb);
            }
        }

        // 4. Root hash = SHA256(salt || root_hash_block).
        //
        // After the loop change above, `root_level` contains 0..=hash_per_block
        // entries (NOT just 1). Pack ALL of them into the root block —
        // zero-padding any unused slots. The previous code only copied
        // `root_level[0]`, which was correct only when the loop terminated
        // with exactly one hash; for a single-block leaf level (the common
        // small-import case), that produced a root_hash that disagreed with
        // `veritysetup verify`.
        let root_level = levels.last().expect("non-empty levels");
        let mut root_block = vec![0u8; hbs];
        for (slot, src) in root_level.iter().take(hash_per_block).enumerate() {
            let off = slot * SHA256_DIGEST_SIZE;
            root_block[off..off + SHA256_DIGEST_SIZE].copy_from_slice(src);
        }
        let root_hash = hash_v1(&self.params.salt, &root_block);

        VerityOutput { blob, root_hash }
    }
}

/// Errors produced by [`VerityBuilder::add_data_block`]. Distinct from
/// [`VerityError`] (which is param-validation only) because misuse of
/// the streaming API is a separate failure mode.
#[derive(Debug, Error)]
pub enum VerityBuilderError {
    /// `add_data_block` was called after a short (< `data_block_size`)
    /// block was fed. Only the final block may be short.
    #[error(
        "add_data_block called after a short final block — only the last block may be < data_block_size"
    )]
    AfterShortBlock,
    /// The block exceeds `data_block_size`.
    #[error("block too large: got {got} bytes, max is {max} (data_block_size)")]
    BlockTooLarge {
        /// Block length received.
        got: usize,
        /// `params.data_block_size`.
        max: usize,
    },
}

/// Drive a [`VerityBuilder`] from any [`std::io::Read`]er one fixed-size
/// data block at a time (Phase 46 D-05 — single source of truth for the
/// pull AND import per-block read loop).
///
/// Reads up to `block_size` bytes per iteration via a "fill the block"
/// inner loop, so a slow reader that returns single bytes at a time does
/// NOT produce short blocks until true EOF. Calls
/// [`VerityBuilder::add_data_block`] once per iteration with the filled
/// slice. Stops when the inner loop reads 0 bytes (EOF) or yields a
/// short block (< `block_size`), whichever happens first. After a short
/// block, `add_data_block` seals the builder and any subsequent call
/// would fail with [`VerityBuilderError::AfterShortBlock`] — this helper
/// guarantees no such call is made.
///
/// Partial-final-block correctness (RESEARCH Pitfall 5) is handled by
/// [`VerityBuilder::add_data_block`] itself (verity.rs zero-pads short
/// blocks internally at lines 213-224); this helper does NOT pad. Adding
/// a padding loop here would double-zero-pad and break byte-equivalence
/// with `veritysetup format`.
///
/// `block_size` SHOULD equal `params.data_block_size` used to construct
/// the builder (the helper does not enforce this — `add_data_block` rejects
/// oversized blocks with [`VerityBuilderError::BlockTooLarge`]).
///
/// # Veritysetup cross-validation (Phase 46 W1)
///
/// ROADMAP Phase 46 success criterion #4 calls for cross-validation of
/// the on-disk verity bytes against `veritysetup format`. Plan 01 Task 3
/// asserts `feed_from_reader == compute` via three streaming-equivalence
/// tests. Phase 43 D-04 added a CI job that asserts `compute() ==
/// veritysetup format` byte-for-byte. Composing the two yields the
/// transitive guarantee `feed_from_reader == compute == veritysetup
/// format`. Plan 03 Task 6 will additionally extend the existing CI
/// cross-validate to exercise the `feed_from_reader` path through
/// `pichi import` end-to-end.
///
/// # Errors
///
/// - [`std::io::Error`] from the underlying reader is propagated unchanged.
/// - [`VerityBuilderError`] from `add_data_block` is wrapped as
///   `io::Error::other` with a `verity feed_from_reader: {err}` prefix.
pub fn feed_from_reader<R: std::io::Read>(
    reader: &mut R,
    builder: &mut VerityBuilder,
    block_size: usize,
) -> std::io::Result<()> {
    let mut buf = vec![0u8; block_size];
    loop {
        let mut filled = 0usize;
        while filled < block_size {
            let n = reader.read(&mut buf[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            return Ok(());
        }
        builder
            .add_data_block(&buf[..filled])
            .map_err(|e| std::io::Error::other(format!("verity feed_from_reader: {e}")))?;
        if filled < block_size {
            return Ok(());
        }
    }
}

/// Output of `verity::compute`.
#[derive(Debug)]
pub struct VerityOutput {
    /// The full verity blob bytes — superblock at offset 0, padded to
    /// `hash_block_size`, then hash-tree levels TOP-DOWN.
    pub blob: Vec<u8>,
    /// Root hash = `SHA256(salt || root_hash_block)`. NOT stored in the
    /// blob (per RESEARCH §3 lines 415–423); printed by `pichi import
    /// --print-verity-info` for CI `veritysetup verify` consumption.
    pub root_hash: [u8; SHA256_DIGEST_SIZE],
}

impl VerityParams {
    /// Deterministic uuid for the verity superblock's `uuid` field, keeping the
    /// blob byte-identical for the same `(cow, salt)` pair (CONTEXT D-03).
    /// Cosmetic — not a security boundary. An associated function because it
    /// produces a `VerityParams` field before the struct exists.
    #[must_use]
    pub fn derive_uuid(salt: &[u8], cow_digest_bytes: &[u8; SHA256_DIGEST_SIZE]) -> [u8; 16] {
        let mut h = Sha256::new();
        h.update(salt);
        h.update(cow_digest_bytes);
        let full = h.finalize();
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&full[..16]);
        uuid
    }
}

/// Build the dm-verity v1 hash tree blob over `cow_bytes`.
///
/// # Algorithm (per RESEARCH §3 lines 353–411)
///
/// 1. Slice `cow_bytes` into `data_block_size`-byte data blocks.
/// 2. Leaf level: hash each data block as `SHA256(salt || data_block)`.
///    Pack `hash_per_block` hashes per `hash_block_size` block; trailing
///    slots zero (RESEARCH §3 trap V8).
/// 3. Walk levels bottom-up: each parent level hashes the previous
///    level's blocks the same way. Stop when one block remains (root).
/// 4. Output: `verity_sb` (512 B) + zero-pad to `hash_block_size` +
///    levels written TOP-DOWN (root level first; RESEARCH §3 trap V7).
/// 5. `root_hash = SHA256(salt || root_block)`.
///
/// # Pitfalls handled (RESEARCH §3 traps V1–V8)
///
/// - V1: `hash_per_block = 1 << get_bits_down(hash_block_size /
///   digest_size)` (bit-shift, NOT division).
/// - V2: salt is PREPEND for v1 (NOT append; that's v0).
/// - V3: salt byte length sent to hasher is `salt_size`, NOT 256.
/// - V4: `algorithm` field is lower-case `b"sha256"`, NUL-padded to 32.
/// - V5: `data_blocks` is in units of `data_block_size`, NOT bytes.
/// - V6: superblock is followed by zero-padding to a `hash_block_size`
///   boundary (3584 bytes for 512-byte SB + 4096 HBS).
/// - V7: levels are written TOP-DOWN (root level first).
/// - V8: within each hash block, trailing slots are zero (initialize
///   block to zeros before writing hashes).
impl VerityParams {
    /// Compute the dm-verity tree for an in-memory cow (tests, small fixtures).
    /// Production callers that stream the cow from disk should drive
    /// [`VerityBuilder`] directly to avoid materialising the full cow.
    ///
    /// Hard-validates the params (WR-01): a future caller that picks non-default
    /// params must not spin forever (hash_per_block < 2) or divide by zero
    /// (block size 0).
    pub fn compute(&self, cow_bytes: &[u8]) -> Result<VerityOutput, VerityError> {
        let mut builder = VerityBuilder::new(self)?;
        let dbs = self.data_block_size as usize;
        for chunk in cow_bytes.chunks(dbs) {
            builder
                .add_data_block(chunk)
                .expect("compute: chunks() emits blocks <= dbs and at most one short tail");
        }
        Ok(builder.finalize())
    }
}

// -- helpers ---------------------------------------------------------------

/// `hashes_per_block` per RESEARCH §3 trap V1: rounded-DOWN power of two.
///
/// `hash_per_block = 1 << floor(log2(hash_block_size / digest_size))`.
/// For SHA-256 + 4 KiB hash blocks: `1 << floor(log2(128)) = 128`.
fn hashes_per_block(hash_block_size: usize, digest_size: usize) -> usize {
    let raw = hash_block_size / digest_size;
    debug_assert!(raw > 0, "hash_block_size must be >= digest_size");
    // get_bits_down equivalent: floor(log2(x)).
    let bits_down = (usize::BITS - 1) - raw.leading_zeros();
    1usize << bits_down
}

/// Tree level count per RESEARCH §3 lines 383–393.
///
/// Returns the number of PARENT levels above the leaf (matching cryptsetup's
/// `*levels` counter exactly). The leaf-hashing pass is always present but
/// is NOT counted here. `compute()` stores leaf + parents in `levels[0..n]`
/// so `levels.len() == compute_levels(...) + 1`.
///
/// For a 4 KiB cow (1 data block) → returns 0 (no parent levels needed).
/// For 128 data blocks → returns 1 (one root block holds all 128 leaf hashes).
///
/// Test-only since BL-01: `VerityBuilder::finalize` walks levels
/// directly (`while levels.last().len() > 1`) without consulting this
/// formula. The function is retained so `tree_levels_ceiling` can
/// continue to assert the formula matches cryptsetup's counter exactly.
#[cfg(test)]
fn compute_levels(data_blocks: u64, hash_per_block_bits: u32) -> u32 {
    let mut levels = 0u32;
    while (hash_per_block_bits * levels) < 64
        && data_blocks > 0
        && ((data_blocks - 1) >> (hash_per_block_bits * levels)) > 0
    {
        levels += 1;
    }
    levels
}

/// `HASH(salt || data_block)` for v1 per RESEARCH §3 lines 322–351 +
/// `cryptsetup lib/verity/verity_hash.c:64-87`.
fn hash_v1(salt: &[u8], block: &[u8]) -> [u8; SHA256_DIGEST_SIZE] {
    let mut h = Sha256::new();
    h.update(salt); // PREPEND for v1 (RESEARCH §3 trap V2)
    h.update(block);
    let full = h.finalize();
    let mut out = [0u8; SHA256_DIGEST_SIZE];
    out.copy_from_slice(&full);
    out
}

/// Write the 512-byte verity_sb at the start of `blob` per RESEARCH §3
/// lines 261–315 + cryptsetup `verity.c:187-198`.
fn write_superblock(blob: &mut Vec<u8>, params: &VerityParams, data_blocks: u64) {
    debug_assert!(blob.is_empty(), "superblock must be at offset 0");

    // signature[8] — RESEARCH §3 trap (no number, but verity.c:22 fixed).
    blob.extend_from_slice(VERITY_SIGNATURE);

    // version (u32 LE) = 1.
    blob.extend_from_slice(&VERITY_FORMAT_VERSION.to_le_bytes());

    // hash_type (u32 LE) = 1 (normal).
    blob.extend_from_slice(&VERITY_HASH_TYPE_NORMAL.to_le_bytes());

    // uuid[16] — deterministic per RESEARCH §Open-Q #3.
    blob.extend_from_slice(&params.uuid);

    // algorithm[32] — RESEARCH §3 trap V4: lowercase ASCII, NUL-padded.
    let mut algo = [0u8; VERITY_SB_ALGO_LEN];
    let algo_bytes = b"sha256";
    algo[..algo_bytes.len()].copy_from_slice(algo_bytes);
    blob.extend_from_slice(&algo);

    // data_block_size (u32 LE).
    blob.extend_from_slice(&params.data_block_size.to_le_bytes());

    // hash_block_size (u32 LE).
    blob.extend_from_slice(&params.hash_block_size.to_le_bytes());

    // data_blocks (u64 LE) — RESEARCH §3 trap V5: in BLOCKS, not bytes.
    blob.extend_from_slice(&data_blocks.to_le_bytes());

    // salt_size (u16 LE) — RESEARCH §3 trap V3: actual length of salt
    // bytes used by the hasher; NOT 256.
    let salt_size = params.salt.len() as u16;
    blob.extend_from_slice(&salt_size.to_le_bytes());

    // _pad1[6] — must be zeroed.
    blob.extend_from_slice(&[0u8; 6]);

    // salt[256] — actual salt bytes followed by zero pad up to 256.
    let mut salt_field = [0u8; VERITY_SB_SALT_MAX];
    salt_field[..params.salt.len()].copy_from_slice(&params.salt);
    blob.extend_from_slice(&salt_field);

    // _pad2[168] — must be zeroed.
    blob.extend_from_slice(&[0u8; 168]);

    debug_assert_eq!(blob.len(), VERITY_SB_SIZE, "superblock must be 512 bytes");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RESEARCH §3 trap V1: hashes_per_block is bit-shift form, NOT
    /// division. For SHA-256 (32 B) + 4 KiB hash blocks → 128.
    #[test]
    fn hashes_per_block_power_of_two() {
        assert_eq!(hashes_per_block(4096, 32), 128); // 4096/32 = 128 = 2^7 (exact)
        assert_eq!(hashes_per_block(4096, 28), 128); // 4096/28 = 146 → floor(log2)=7 → 128
        assert_eq!(hashes_per_block(8192, 32), 256); // 8192/32 = 256 = 2^8
        assert_eq!(hashes_per_block(4096, 64), 64); // 4096/64 = 64 = 2^6
    }

    /// RESEARCH §3 trap V2: v1 hash construction is salt-PREPEND.
    /// We assert by computing the expected hash by hand and comparing.
    #[test]
    fn v1_salt_prepend() {
        let salt = vec![0u8; 32];
        let block = vec![0xAAu8; 4096];
        let got = hash_v1(&salt, &block);
        // Compute expected: SHA256(salt || block).
        let mut h = Sha256::new();
        h.update(&salt);
        h.update(&block);
        let expected_full = h.finalize();
        let mut expected = [0u8; 32];
        expected.copy_from_slice(&expected_full);
        assert_eq!(got, expected);

        // Confirm the WRONG order (block || salt) gives a different hash.
        let mut h2 = Sha256::new();
        h2.update(&block);
        h2.update(&salt);
        let wrong_full = h2.finalize();
        assert_ne!(
            &wrong_full[..],
            &got[..],
            "salt-PREPEND must differ from salt-APPEND"
        );
    }

    /// RESEARCH §3 lines 261–298 + traps V4, V5: superblock byte layout.
    #[test]
    fn superblock_byte_layout() {
        let salt = vec![0u8; 32];
        let uuid = [0xABu8; 16];
        let params = VerityParams {
            data_block_size: 4096,
            hash_block_size: 4096,
            salt: salt.clone(),
            uuid,
        };
        // 16 KiB cow → 4 data blocks.
        let cow = vec![0u8; 16 * 1024];
        let out = params.compute(&cow).unwrap();

        // Superblock checks.
        assert_eq!(
            &out.blob[0..8],
            VERITY_SIGNATURE,
            "signature `verity\\0\\0`"
        );
        assert_eq!(
            u32::from_le_bytes(out.blob[8..12].try_into().unwrap()),
            1,
            "version = 1"
        );
        assert_eq!(
            u32::from_le_bytes(out.blob[12..16].try_into().unwrap()),
            1,
            "hash_type = 1 (normal)"
        );
        assert_eq!(&out.blob[16..32], &uuid, "uuid bytes");
        // algorithm[32]: "sha256" + 26 NUL pad. Trap V4.
        assert_eq!(&out.blob[32..38], b"sha256", "algo prefix lowercase");
        assert!(
            out.blob[38..64].iter().all(|&b| b == 0),
            "algo NUL-padded to 32 bytes"
        );
        assert_eq!(
            u32::from_le_bytes(out.blob[64..68].try_into().unwrap()),
            4096,
            "data_block_size"
        );
        assert_eq!(
            u32::from_le_bytes(out.blob[68..72].try_into().unwrap()),
            4096,
            "hash_block_size"
        );
        // Trap V5: data_blocks in BLOCKS not bytes. 16 KiB / 4 KiB = 4.
        assert_eq!(
            u64::from_le_bytes(out.blob[72..80].try_into().unwrap()),
            4u64,
            "data_blocks = cow_bytes / data_block_size (in blocks)"
        );
        // Trap V3: salt_size = actual len, NOT 256.
        assert_eq!(
            u16::from_le_bytes(out.blob[80..82].try_into().unwrap()),
            32u16,
            "salt_size = actual salt length"
        );
        // _pad1[6] zeroed.
        assert!(out.blob[82..88].iter().all(|&b| b == 0), "_pad1 zero");
        // salt[256]: first 32 bytes = salt, rest zero pad.
        assert_eq!(&out.blob[88..120], &salt[..], "salt bytes");
        assert!(
            out.blob[120..344].iter().all(|&b| b == 0),
            "salt zero-padded to 256"
        );
        // _pad2[168] zeroed.
        assert!(out.blob[344..512].iter().all(|&b| b == 0), "_pad2 zero");

        // Trap V6: superblock followed by zero-pad to hash_block_size.
        assert!(
            out.blob[512..4096].iter().all(|&b| b == 0),
            "superblock zero-padded to hash_block boundary"
        );
        // First hash block starts at offset 4096.
        assert!(out.blob.len() >= 4096 + 4096, "first hash block present");
    }

    /// RESEARCH §3 trap V7 + lines 383–393: tree-level math.
    ///
    /// `compute_levels` returns the cryptsetup parent-level count (the
    /// number of levels ABOVE the leaf layer). A single leaf-hashing
    /// pass is always present but is not counted here.
    #[test]
    fn tree_levels_ceiling() {
        // hash_per_block_bits = 7 (128 = 2^7) for SHA-256 + 4 KiB.
        let hpb_bits = 7u32;
        // 1 data block → leaf level only; 0 parent levels. Cryptsetup:
        // (1-1)>>0 = 0 → loop body never executes → levels = 0.
        assert_eq!(compute_levels(1, hpb_bits), 0);
        // 128 data blocks → all 128 leaf hashes fit in one parent block;
        // (128-1)>>7 = 0 → loop exits after 1 iteration → 1 parent level.
        assert_eq!(compute_levels(128, hpb_bits), 1);
        // 129 data blocks → 2 parent levels (leaf→128-entry parent→root).
        assert_eq!(compute_levels(129, hpb_bits), 2);
        // 16384 (=128^2) data blocks → 2 parent levels.
        // (16383>>7=127>0), (16383>>14=0) → 2.
        assert_eq!(compute_levels(16384, hpb_bits), 2);
        // 16385 data blocks → 3 parent levels.
        assert_eq!(compute_levels(16385, hpb_bits), 3);
    }

    /// RESEARCH §3 lines 415–423 + Open-Q #3: root_hash is deterministic
    /// for the same `(cow_bytes, params)`. Two calls produce the same
    /// blob and same root_hash.
    #[test]
    fn root_hash_deterministic_for_known_fixture() {
        let salt = vec![0u8; 32];
        let cow_digest = [0x42u8; 32];
        let uuid = VerityParams::derive_uuid(&salt, &cow_digest);
        let params = VerityParams {
            data_block_size: 4096,
            hash_block_size: 4096,
            salt,
            uuid,
        };
        let cow = vec![0u8; 16 * 1024];
        let a = params.compute(&cow).unwrap();
        let b = params.compute(&cow).unwrap();
        assert_eq!(a.blob, b.blob, "verity blob is deterministic");
        assert_eq!(a.root_hash, b.root_hash, "root_hash is deterministic");

        // derive_uuid is also deterministic.
        assert_eq!(uuid, VerityParams::derive_uuid(&[0u8; 32], &[0x42u8; 32]));
    }

    /// WR-01: hash_block_size = digest_size (32) makes hashes_per_block
    /// = 1, which would spin `compute()` forever (each parent level has
    /// the same node count as its child). Validation MUST reject before
    /// the loop is entered.
    #[test]
    fn validate_rejects_hash_block_equal_to_digest_size() {
        let params = VerityParams {
            data_block_size: 4096,
            hash_block_size: 32, // == SHA256_DIGEST_SIZE → hashes_per_block = 1
            salt: vec![0u8; 32],
            uuid: [0u8; 16],
        };
        let err = params.validate().unwrap_err();
        match err {
            VerityError::HashBlockTooSmall {
                hash_block_size,
                digest_size,
                hashes_per_block,
            } => {
                assert_eq!(hash_block_size, 32);
                assert_eq!(digest_size, 32);
                assert_eq!(hashes_per_block, 1);
            }
            other => panic!("expected HashBlockTooSmall, got {other:?}"),
        }
        // compute() must surface the same error rather than spinning.
        let cow = vec![0u8; 4096];
        let err2 = params.compute(&cow).unwrap_err();
        assert!(matches!(err2, VerityError::HashBlockTooSmall { .. }));
    }

    /// WR-01: hash_block_size < digest_size also makes hashes_per_block
    /// = 0, causing `1 << bits_down` in `hashes_per_block` to evaluate
    /// `(usize::BITS - 1) - 0.leading_zeros()` (= u32::MAX) and overflow
    /// the shift. Validation MUST catch this before the helper is even
    /// called.
    #[test]
    fn validate_rejects_hash_block_smaller_than_digest_size() {
        let params = VerityParams {
            data_block_size: 4096,
            hash_block_size: 16, // < SHA256_DIGEST_SIZE (32)
            salt: vec![0u8; 32],
            uuid: [0u8; 16],
        };
        let err = params.validate().unwrap_err();
        assert!(matches!(err, VerityError::HashBlockTooSmall { .. }));
    }

    /// `validate` rejects a salt that exceeds the 256-byte on-disk
    /// `verity_sb.salt[256]` field. (Touches the same code path as
    /// `lib::run`'s 32 + suffix > 256 check, but at the typed boundary.)
    #[test]
    fn validate_rejects_oversized_salt() {
        let params = VerityParams {
            data_block_size: 4096,
            hash_block_size: 4096,
            salt: vec![0u8; 257],
            uuid: [0u8; 16],
        };
        let err = params.validate().unwrap_err();
        match err {
            VerityError::SaltTooLong { got, max } => {
                assert_eq!(got, 257);
                assert_eq!(max, 256);
            }
            other => panic!("expected SaltTooLong, got {other:?}"),
        }
    }

    /// `validate` rejects zero block sizes (would otherwise divide by zero).
    #[test]
    fn validate_rejects_zero_block_sizes() {
        let p_dbs = VerityParams {
            data_block_size: 0,
            hash_block_size: 4096,
            salt: vec![],
            uuid: [0u8; 16],
        };
        assert!(matches!(
            p_dbs.validate().unwrap_err(),
            VerityError::BlockSizeZero { .. }
        ));
        let p_hbs = VerityParams {
            data_block_size: 4096,
            hash_block_size: 0,
            salt: vec![],
            uuid: [0u8; 16],
        };
        assert!(matches!(
            p_hbs.validate().unwrap_err(),
            VerityError::BlockSizeZero { .. }
        ));
    }

    /// BL-01: `VerityBuilder` produces the same `(blob, root_hash)` as
    /// the in-memory `compute` path for the same input. This is the
    /// streaming-equivalence guarantee.
    #[test]
    fn streaming_builder_matches_compute() {
        let salt = vec![0u8; 32];
        let cow_digest = [0x42u8; 32];
        let uuid = VerityParams::derive_uuid(&salt, &cow_digest);
        let params = VerityParams {
            data_block_size: 4096,
            hash_block_size: 4096,
            salt,
            uuid,
        };
        // 16 KiB cow → 4 data blocks (4096 each) + 1 short block at the end.
        let cow: Vec<u8> = (0u32..)
            .map(|i| (i & 0xFF) as u8)
            .take(16 * 1024 + 257)
            .collect();
        let in_mem = params.compute(&cow).unwrap();

        let mut builder = VerityBuilder::new(&params).unwrap();
        for chunk in cow.chunks(params.data_block_size as usize) {
            builder.add_data_block(chunk).unwrap();
        }
        let streamed = builder.finalize();

        assert_eq!(streamed.blob, in_mem.blob, "blob bytes must match");
        assert_eq!(streamed.root_hash, in_mem.root_hash, "root_hash must match");
    }

    /// BL-01: `VerityBuilder::add_data_block` rejects subsequent calls
    /// after a short block has been fed (only the LAST block may be short).
    #[test]
    fn streaming_builder_rejects_after_short() {
        let params = VerityParams {
            data_block_size: 4096,
            hash_block_size: 4096,
            salt: vec![0u8; 32],
            uuid: [0u8; 16],
        };
        let mut b = VerityBuilder::new(&params).unwrap();
        b.add_data_block(&[0xAAu8; 4096]).unwrap(); // full
        b.add_data_block(&[0xBBu8; 100]).unwrap(); // short -> seals
        let err = b.add_data_block(&[0xCCu8; 4096]).unwrap_err();
        assert!(matches!(err, VerityBuilderError::AfterShortBlock));
    }

    /// BL-01: `VerityBuilder::add_data_block` rejects a block larger
    /// than `data_block_size`.
    #[test]
    fn streaming_builder_rejects_oversized_block() {
        let params = VerityParams {
            data_block_size: 4096,
            hash_block_size: 4096,
            salt: vec![0u8; 32],
            uuid: [0u8; 16],
        };
        let mut b = VerityBuilder::new(&params).unwrap();
        let err = b.add_data_block(&[0u8; 4097]).unwrap_err();
        assert!(matches!(
            err,
            VerityBuilderError::BlockTooLarge {
                got: 4097,
                max: 4096
            }
        ));
    }

    /// BL-01: `VerityBuilder::new` propagates the same `VerityError` as
    /// `compute` for invalid params.
    #[test]
    fn streaming_builder_validates_params() {
        let params = VerityParams {
            data_block_size: 4096,
            hash_block_size: 16, // too small (WR-01)
            salt: vec![0u8; 32],
            uuid: [0u8; 16],
        };
        let err = VerityBuilder::new(&params).unwrap_err();
        assert!(matches!(err, VerityError::HashBlockTooSmall { .. }));
    }

    /// Regression for the D-04 cross-validation gap (trap V6b in the
    /// `finalize` doc comment): when the leaf level fits in a single hash
    /// block, the leaf level IS the root level — `finalize` must NOT
    /// build a spurious parent level above it, and the root_hash must be
    /// computed over the FULL leaf-packed root block (not just the first
    /// leaf hash).
    ///
    /// Phase 43's existing tests (streaming-equivalence, deterministic
    /// fixture, superblock layout, level-count formula) all PASSED while
    /// the bug was live because they only checked internal
    /// self-consistency. The verifier caught it when running
    /// `veritysetup verify` against the produced blob, exit 2
    /// "Verification failed at position 0".
    ///
    /// 100 data blocks × 4 KiB = 400 KiB cow; with hash_per_block = 128
    /// the leaf level fits in one hash block → tree has exactly 1 logical
    /// level. The buggy code over-built one parent level and then hashed
    /// only `root_level[0]` (the single parent hash), producing a
    /// root_hash that disagreed with the on-disk hash tree.
    #[test]
    fn finalize_matches_veritysetup_for_single_block_tree() {
        let params = VerityParams {
            data_block_size: 4096,
            hash_block_size: 4096,
            salt: vec![0u8; 32],
            uuid: [0u8; 16],
        };
        let mut builder = VerityBuilder::new(&params).unwrap();
        for i in 0..100u32 {
            let block = vec![i as u8; 4096];
            builder.add_data_block(&block).unwrap();
        }
        let output = builder.finalize();

        // Sanity: tree has exactly 1 logical level (leaves ARE the root
        // level). On-disk blob layout is superblock (1 hash_block, with
        // the 512-byte SB zero-padded to hash_block_size per trap V6) +
        // root block (1 hash_block) = 2 × 4096 = 8192 bytes.
        assert_eq!(
            output.blob.len(),
            8192,
            "single-block-tree blob must be exactly superblock + 1 hash block"
        );

        // Reconstruct the BUGGY root hash: SHA256(salt || zero_block_with_only_first_leaf_hash).
        // If the fix did not land, output.root_hash would equal this.
        let mut leaf_hasher = Sha256::new();
        leaf_hasher.update(&params.salt);
        leaf_hasher.update(vec![0u8; 4096]); // first block (i=0, all zeros)
        let leaf_zero = leaf_hasher.finalize();
        let mut bad_root_block = vec![0u8; 4096];
        bad_root_block[..32].copy_from_slice(&leaf_zero);
        let mut bad_hasher = Sha256::new();
        bad_hasher.update(&params.salt);
        bad_hasher.update(&bad_root_block);
        let bad_root_hash = bad_hasher.finalize();

        assert_ne!(
            output.root_hash[..],
            bad_root_hash[..],
            "root_hash matches the BUGGY computation (only first leaf hashed); \
             fix did not land — `veritysetup verify` would reject this blob"
        );

        // Stronger check: root_hash MUST equal SHA256(salt || full_packed_leaf_block).
        // Compute all 100 leaf hashes the same way the builder does
        // (SHA256(salt || padded_block)) and pack them into the root block.
        let mut packed_root = vec![0u8; 4096];
        for i in 0..100u32 {
            let mut h = Sha256::new();
            h.update(&params.salt);
            h.update(vec![i as u8; 4096]);
            let leaf = h.finalize();
            let off = (i as usize) * 32;
            packed_root[off..off + 32].copy_from_slice(&leaf);
        }
        let mut expected_hasher = Sha256::new();
        expected_hasher.update(&params.salt);
        expected_hasher.update(&packed_root);
        let expected_root = expected_hasher.finalize();
        assert_eq!(
            output.root_hash[..],
            expected_root[..],
            "root_hash must equal SHA256(salt || all_100_leaf_hashes_packed_into_one_block)"
        );
    }

    /// Phase 46 D-05: `feed_from_reader` produces byte-identical output
    /// to `compute()` for a buffer whose length is exactly a multiple of
    /// `data_block_size`. Composes with the Phase 43 D-04 CI gate (which
    /// asserts `compute() == veritysetup format`) to give the transitive
    /// `feed_from_reader == veritysetup format` guarantee.
    #[test]
    fn feed_from_reader_matches_compute_on_block_boundary() {
        let salt = vec![0u8; 32];
        let cow_digest = [0x42u8; 32];
        let uuid = VerityParams::derive_uuid(&salt, &cow_digest);
        let params = VerityParams {
            data_block_size: 4096,
            hash_block_size: 4096,
            salt,
            uuid,
        };
        let cow: Vec<u8> = (0u32..).map(|i| (i & 0xFF) as u8).take(16 * 1024).collect();
        let in_mem = params.compute(&cow).unwrap();
        let mut builder = VerityBuilder::new(&params).unwrap();
        let mut reader = std::io::Cursor::new(&cow);
        feed_from_reader(&mut reader, &mut builder, params.data_block_size as usize).unwrap();
        let streamed = builder.finalize();
        assert_eq!(streamed.blob, in_mem.blob);
        assert_eq!(streamed.root_hash, in_mem.root_hash);
    }

    /// Phase 46 D-05: `feed_from_reader` produces byte-identical output
    /// to `compute()` for partial-final-block lengths (4097, 8191, 12289
    /// bytes — covers the three "off by one" cases around 1, 2, 3 full
    /// blocks plus a sub-block tail). RESEARCH Pitfall 5 — proves the
    /// helper does NOT pad on top of `add_data_block`'s internal zero-pad.
    #[test]
    fn feed_from_reader_matches_compute_on_partial_final_block() {
        let salt = vec![0u8; 32];
        let cow_digest = [0x42u8; 32];
        let uuid = VerityParams::derive_uuid(&salt, &cow_digest);
        let params = VerityParams {
            data_block_size: 4096,
            hash_block_size: 4096,
            salt,
            uuid,
        };
        for &n in &[4097usize, 8191, 12289] {
            let cow: Vec<u8> = (0u32..).map(|i| (i & 0xFF) as u8).take(n).collect();
            let in_mem = params.compute(&cow).unwrap();
            let mut builder = VerityBuilder::new(&params).unwrap();
            let mut reader = std::io::Cursor::new(&cow);
            feed_from_reader(&mut reader, &mut builder, params.data_block_size as usize)
                .unwrap_or_else(|e| panic!("feed_from_reader failed at n={n}: {e}"));
            let streamed = builder.finalize();
            assert_eq!(streamed.blob, in_mem.blob, "blob mismatch at n={n}");
            assert_eq!(
                streamed.root_hash, in_mem.root_hash,
                "root_hash mismatch at n={n}"
            );
        }
    }

    /// Phase 46 D-05: `feed_from_reader` handles empty input — no
    /// `add_data_block` calls, builder finalizes over an empty leaf level
    /// (existing behaviour at verity.rs:254-256).
    #[test]
    fn feed_from_reader_handles_empty_input() {
        let params = VerityParams {
            data_block_size: 4096,
            hash_block_size: 4096,
            salt: vec![0u8; 32],
            uuid: [0u8; 16],
        };
        let mut builder = VerityBuilder::new(&params).unwrap();
        let mut reader = std::io::Cursor::new(Vec::<u8>::new());
        feed_from_reader(&mut reader, &mut builder, 4096).unwrap();
        // Empty blob is well-defined; we just assert no panic on finalize.
        let _ = builder.finalize();
    }

    /// `validate` accepts the locked Phase 42 D-06 defaults.
    #[test]
    fn validate_accepts_locked_defaults() {
        let params = VerityParams {
            data_block_size: 4096,
            hash_block_size: 4096,
            salt: vec![0u8; 32],
            uuid: [0u8; 16],
        };
        params.validate().expect("locked defaults must be valid");
    }

    /// Format a 16-byte UUID as the canonical 8-4-4-4-12 hex string that
    /// `veritysetup format --uuid` accepts and writes back byte-identically.
    fn format_uuid(u: &[u8; 16]) -> String {
        let h = hex::encode(u);
        format!(
            "{}-{}-{}-{}-{}",
            &h[0..8],
            &h[8..12],
            &h[12..16],
            &h[16..20],
            &h[20..32]
        )
    }

    /// The internal producer MUST match `veritysetup format` byte-for-byte
    /// (root hash AND on-disk hash blob) for a multi-block tree with a
    /// non-zero salt + derived UUID. This is the load-bearing guarantee that
    /// lets `conglobate` (shells out to `veritysetup`) and `pichi import`
    /// (internal) produce interoperable scutes. Skips when veritysetup is
    /// absent.
    #[test]
    fn matches_veritysetup_format_multiblock() {
        if std::process::Command::new("veritysetup")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("veritysetup absent — skipping cross-check");
            return;
        }
        let mut data = vec![0u8; 4096 * 37];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let salt: Vec<u8> = (0..32u8)
            .map(|i| i.wrapping_mul(7).wrapping_add(1))
            .collect();
        let cow_digest: [u8; 32] = Sha256::digest(&data).into();
        let uuid = VerityParams::derive_uuid(&salt, &cow_digest);
        let params = VerityParams {
            data_block_size: 4096,
            hash_block_size: 4096,
            salt: salt.clone(),
            uuid,
        };
        let out = params.compute(&data).unwrap();

        let tmp = tempfile::TempDir::new().unwrap();
        let datap = tmp.path().join("data.img");
        let hashp = tmp.path().join("hash.img");
        std::fs::write(&datap, &data).unwrap();

        let o = std::process::Command::new("veritysetup")
            .args([
                "format",
                "--data-block-size",
                "4096",
                "--hash-block-size",
                "4096",
                "--hash",
                "sha256",
                "--salt",
                &hex::encode(&salt),
                "--uuid",
                &format_uuid(&uuid),
            ])
            .arg(&datap)
            .arg(&hashp)
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "veritysetup format failed: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        let stdout = String::from_utf8_lossy(&o.stdout);
        let vroot = stdout
            .lines()
            .find_map(|l| l.strip_prefix("Root hash:"))
            .map(|s| s.trim().to_string())
            .expect("veritysetup must print a Root hash line");
        assert_eq!(
            vroot,
            hex::encode(out.root_hash),
            "veritysetup root hash must equal the internal producer's"
        );
        let vblob = std::fs::read(&hashp).unwrap();
        assert_eq!(
            vblob, out.blob,
            "veritysetup hash device must be byte-identical to the internal verity blob"
        );
    }
}
