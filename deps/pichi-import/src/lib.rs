// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `pichi import` library. Pure-userspace, byte-pure conversion of a raw
//! block image into a base carapace artifact (one scute, no PMI) per
//! Phase 43 / IMPORT-01..07.
//!
//! The dm-snapshot persistent COW writer ([`cow`]) and the dm-verity v1
//! hash tree builder ([`verity`]) are pure carapace scute-format primitives.
//! They are exposed as `pichi_import::{cow, verity}` and used by `pichi
//! import` and `pichi pull` (via `verity::*`).

pub mod cow;
pub mod verity;

/// The dm-snapshot COW chunk size every carapace scute MUST use: 8 sectors
/// (4096 bytes). Fixed by the carapace spec's parameter whitelist; the
/// carapace read side rejects any other value. Not a tunable (the generic
/// [`cow::DEFAULT_CHUNK_SIZE_SECTORS`] reflects dm's own default and is wrong
/// for carapaces).
pub const SCUTE_CHUNK_SIZE_SECTORS: u32 = 8;

mod manifest;

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest as _, Sha256};

use pichi_artifact::{Digest, Reference};
use pichi_storage::{
    BlobSidecarExt, BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb,
    sidecar::write_sidecar_atomic,
};

/// 32-byte zero salt prefix per CONTEXT D-01 / D-02.
const SALT_ZERO_PREFIX: [u8; 32] = [0u8; 32];

/// Plain Rust args struct (no clap derives — clap derives live in
/// `src/cli.rs` per Phase 37 D-04 / Phase 40 pattern).
#[derive(Debug)]
pub struct ImportArgs {
    /// Path to the raw image file to import.
    pub raw_image: PathBuf,
    /// Tag to assign (e.g. `myapp:base`). `None` caches the carapace without
    /// tagging it — the root hash is still printed (an ephemeral import, e.g.
    /// to compute the hash or as a throwaway intermediate).
    pub tag: Option<String>,
    /// Optional author-supplied salt suffix bytes (already hex-decoded).
    /// `None` = use just the 32-byte zero prefix (D-01 default).
    pub salt_suffix: Option<Vec<u8>>,
    /// If true, suppress progress reporting.
    pub quiet: bool,
    /// Caller-supplied RFC 3339 timestamp (avoids a `chrono` dep here --
    /// the root `pichi` binary already has chrono and supplies it via
    /// `src/cmd/import.rs`). Plan 03 manifest.rs decision.
    pub created_rfc3339: String,
    /// Extra OCI/provenance annotations to stamp on the manifest, already
    /// parsed from `KEY=VALUE`. Merged verbatim; the structural
    /// `dev.pichi.carapace.verity.*` keys always take precedence, and a
    /// caller-supplied `org.opencontainers.image.created` wins over the
    /// timestamp above.
    pub annotations: std::collections::BTreeMap<String, String>,
}

/// Entry point for `pichi import`.
///
/// Per CONTEXT D-06 (IMPORT-05 amended): treats the input file as opaque
/// bytes -- no GPT parsing, no CRC, no partition-table inspection.
///
/// Per Pattern Mapper Correction #1: `set_tag` is called DIRECTLY (bare
/// call, no advisory-lock wrapper). The flock lives inside set_tag itself;
/// adding an outer lock from the same process would deadlock -- see
/// `pichi-storage/src/lock.rs:50-58`. The flow is safe because:
///   - `BlobStore::put_blob` is content-addressed (digest collision
///     means byte-equal content; concurrent writes are safe).
///   - `TagDb::set_tag` takes its own internal flock.
///   - There is no read-then-write race because import never reads
///     existing tags.
pub async fn run(args: ImportArgs, graphroot: &Path) -> Result<()> {
    let blob_store = FilesystemBlobStore::new(graphroot);
    let scratch = blob_store
        .scratch_dir()
        .await
        .context("preparing scratch dir for streaming COW")?;
    let quiet = args.quiet;

    // All the heavy work — cow streaming, sha256, dm-verity, PMI/DTB staging,
    // manifest build — is CPU + blocking file I/O, so it runs off the runtime.
    let staged = tokio::task::spawn_blocking(move || stage_import(args, &scratch))
        .await
        .context("import staging task panicked")??;

    // Commit blobs (async): cow -> verity sidecar -> manifest, then the tag.
    // Ordering matches the original single-threaded path so error semantics
    // (no partial tag without blobs) are unchanged.
    blob_store
        .put_blob_from_path(&staged.cow_temp_path, &staged.cow_digest)
        .await
        .with_context(|| format!("put_blob_from_path cow {}", staged.cow_digest))?;
    let cow_blob_path = blob_store.blob_path(&staged.cow_digest);
    let scratch2 = blob_store
        .scratch_dir()
        .await
        .context("scratch_dir for verity sidecar")?;
    write_sidecar_atomic(&scratch2, &cow_blob_path.verity_path(), &staged.verity_blob)
        .await
        .with_context(|| format!("write verity sidecar for cow {}", staged.cow_digest))?;
    blob_store
        .put_blob(&staged.manifest_digest, &staged.manifest_bytes)
        .await
        .with_context(|| format!("put_blob manifest {}", staged.manifest_digest))?;

    // set_tag — DIRECT bare call (the flock is internal to set_tag; an outer
    // advisory-lock from the same process would deadlock). Skipped for an
    // untagged (ephemeral) import — the blobs are cached by digest.
    if let Some(tag_key) = &staged.tag_key {
        let db = FilesystemTagDb::open(graphroot)
            .with_context(|| format!("opening tag db at {}", graphroot.display()))?;
        db.set_tag(tag_key, &staged.manifest_digest)
            .await
            .with_context(|| format!("set_tag {tag_key}"))?;
    }

    if !quiet {
        log::info!(
            "pichi import: cached manifest {} (tag: {})",
            staged.manifest_digest,
            staged.tag_key.as_deref().unwrap_or("<none>"),
        );
    }
    // Print the produced artifact's content ID to stdout. Docker prints the
    // image ID (its config-blob digest) here; pichi's artifact identity is the
    // manifest digest instead — it's what tags, `@sha256:…`, `inspect`/`rmi`,
    // and `--carapace` all reference. It doubles as a `--carapace` reference for
    // an untagged import. The carapace root hash is a manifest annotation, read
    // back via `pichi inspect`.
    println!("{}", staged.manifest_digest);

    Ok(())
}

/// Everything the async commit phase needs, produced by [`stage_import`] on a
/// blocking thread. The cow temp file is `keep()`-ed so its path stays valid
/// across the `spawn_blocking` boundary; the async phase renames it into the
/// blob store.
struct StagedImport {
    cow_temp_path: PathBuf,
    cow_digest: Digest,
    verity_blob: Vec<u8>,
    manifest_bytes: Vec<u8>,
    manifest_digest: Digest,
    tag_key: Option<String>,
}

/// Synchronous staging pipeline: stream the input into a cow temp file, hash
/// it, build the dm-verity tree, and build the base-carapace manifest. Performs
/// NO blob-store writes — those happen (async) in [`run`].
fn stage_import(args: ImportArgs, scratch: &Path) -> Result<StagedImport> {
    // Carapaces are fixed at the spec-whitelisted scute chunk size; the
    // carapace read side rejects anything else (see
    // `SCUTE_CHUNK_SIZE_SECTORS`). Not a tunable.
    let chunk_size_sectors = SCUTE_CHUNK_SIZE_SECTORS;

    // Defense-in-depth: re-parse the tag at the lib boundary too
    // (PATTERNS.md "Reference parsing" -- defense in depth). `None` = untagged.
    let tag_key = args
        .tag
        .as_deref()
        .map(|t| {
            t.parse::<Reference>()
                .map(|r| r.to_string())
                .with_context(|| format!("invalid tag reference: {t}"))
        })
        .transpose()?;

    // BL-01 / T-43-01 mitigation: STREAM the input into a temp COW file
    // rather than slurping `args.raw_image` into a `Vec<u8>` (which would
    // OOM on multi-GB inputs). The pipeline is:
    //
    //   input file ──read chunks──> cow::write_streaming ──> cow temp file
    //   cow temp file ──read 4 KiB blocks──> VerityBuilder ──> verity blob
    //
    // Memory profile (any input size):
    //   - input read buffer: 1 chunk (default 16 KiB, max 1 MiB)
    //   - cow exception list: 16 bytes per non-zero input chunk
    //   - verity leaf hashes: 32 bytes per 4 KiB cow block (~0.78%)
    //   - verity blob: ~1/127 of cow size (kept in memory; small)
    //
    // The cow + verity temps live in the caller-provided `scratch` dir (same
    // filesystem as the blob dir) so the final `rename(2)` cannot EXDEV.
    let raw_size = std::fs::metadata(&args.raw_image)
        .with_context(|| format!("stat input image: {}", args.raw_image.display()))?
        .len();
    if !args.quiet {
        log::info!(
            "pichi import: streaming {} bytes from {} (chunk_size = {} sectors = {} bytes)",
            raw_size,
            args.raw_image.display(),
            chunk_size_sectors,
            (chunk_size_sectors as usize) * 512
        );
    }

    let input_file = File::open(&args.raw_image)
        .with_context(|| format!("opening input image: {}", args.raw_image.display()))?;

    // Step 1: stream COW into a NamedTempFile in scratch_dir. On Unix we walk
    // the input sparsely (SEEK_DATA/SEEK_HOLE) so large gaps in a sparse disk
    // image cost a couple of lseeks instead of reading every zero byte;
    // elsewhere we fall back to a plain sequential read. Both produce identical
    // COW bytes.
    let cow_temp = tempfile::NamedTempFile::new_in(scratch)
        .with_context(|| format!("creating cow temp file in {}", scratch.display()))?;
    let cow_meta;
    {
        let mut cow_writer = BufWriter::new(cow_temp.as_file());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            cow_meta = cow::write_streaming_sparse(
                &input_file,
                raw_size,
                &mut cow_writer,
                chunk_size_sectors,
            )
            .context("cow::write_streaming_sparse failed")?;
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let mut input = BufReader::new(&input_file);
            cow_meta = cow::write_streaming(&mut input, &mut cow_writer, chunk_size_sectors)
                .context("cow::write_streaming failed")?;
        }
        cow_writer.flush().context("flushing cow temp writer")?;
    }
    // Re-open the cow temp file for reading. fsync to make sure all
    // bytes are on disk before we hash.
    cow_temp
        .as_file()
        .sync_all()
        .context("fsync cow temp file")?;

    let cow_size = cow_meta.total_bytes;

    // Step 2: build full salt (32-byte zero prefix + optional author suffix
    // per CONTEXT D-01 / D-02).
    let mut full_salt: Vec<u8> = SALT_ZERO_PREFIX.to_vec();
    if let Some(suffix) = &args.salt_suffix {
        if SALT_ZERO_PREFIX.len() + suffix.len() > 256 {
            bail!(
                "--salt suffix too long: prefix(32) + suffix({}) > verity_sb.salt[256]",
                suffix.len()
            );
        }
        full_salt.extend_from_slice(suffix);
    }

    // Step 3: stream-hash the cow file in 4 KiB blocks, simultaneously
    // computing its SHA-256 digest AND feeding the VerityBuilder. One
    // pass over the cow file; no full-cow Vec<u8>.
    const VERITY_DBS: u32 = 4096; // Phase 42 D-06 locked default
    const VERITY_HBS: u32 = 4096; // Phase 42 D-06 locked default

    // We need a uuid before constructing VerityParams, but the uuid
    // depends on the cow digest. Compute the cow digest first (one
    // pass, hash-only), then re-stream for verity. This is two passes
    // over the cow file but one pass over the (much larger) input.
    //
    // Alternative: compute uuid lazily after both passes — but then we
    // can't construct VerityParams up-front. The two-pass cow read is
    // cheap (cow << input for sparse images) and keeps the API typed.
    let cow_digest_arr = stream_sha256(cow_temp.path())
        .with_context(|| format!("hashing cow temp file {}", cow_temp.path().display()))?;
    let cow_digest = Digest::Sha256(cow_digest_arr);

    if !args.quiet {
        log::info!(
            "pichi import: cow blob {} bytes, digest {} ({} input chunks, {} exceptions)",
            cow_size,
            cow_digest,
            cow_meta.input_chunks,
            cow_meta.exception_count,
        );
    }

    // Step 4: derive deterministic uuid (RESEARCH Open-Q #3) and compute
    // the verity tree (D-03: re-callable from Phase 44).
    let cow_digest_bytes = cow_digest.as_sha256_array();
    let uuid = verity::VerityParams::derive_uuid(&full_salt, &cow_digest_bytes);

    let params = verity::VerityParams {
        data_block_size: VERITY_DBS,
        hash_block_size: VERITY_HBS,
        salt: full_salt.clone(),
        uuid,
    };

    // Stream the cow file into the VerityBuilder one data block at a time.
    let mut builder =
        verity::VerityBuilder::new(&params).context("verity::VerityBuilder::new failed")?;
    let mut cow_reader = BufReader::new(
        File::open(cow_temp.path())
            .with_context(|| format!("re-opening cow temp file: {}", cow_temp.path().display()))?,
    );
    let mut block_buf = vec![0u8; VERITY_DBS as usize];
    loop {
        // Fill a full block (or short final read at EOF).
        let mut filled = 0usize;
        while filled < block_buf.len() {
            let n = cow_reader
                .read(&mut block_buf[filled..])
                .context("reading cow temp file for verity")?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            break;
        }
        builder
            .add_data_block(&block_buf[..filled])
            .context("VerityBuilder::add_data_block failed")?;
        if filled < block_buf.len() {
            // Short read = EOF.
            break;
        }
    }
    let verity_out = builder.finalize();
    let verity_digest = Digest::from_bytes_sha256(&verity_out.blob);
    if !args.quiet {
        log::info!(
            "pichi import: verity blob {} bytes, digest {}, root {}",
            verity_out.blob.len(),
            verity_digest,
            hex::encode(verity_out.root_hash)
        );
    }

    // Step 5: build the base-carapace manifest (one Scute, no PMI). The single
    // scute's verity root is the carapace top root (rootₙ₋₁), stamped into the
    // manifest. The bootable appliance form is assembled later by `pichi import pmi`.
    let pichi_manifest = manifest::build(
        &cow_digest,
        cow_size,
        &full_salt,
        &verity_out.root_hash,
        &args.created_rfc3339,
        &args.annotations,
    )
    .context("manifest::build failed")?;
    let manifest_bytes = pichi_manifest
        .to_bytes()
        .context("Manifest::to_bytes failed")?;
    let manifest_digest = Digest::from_bytes_sha256(&manifest_bytes);

    // Keep the cow temp file (its rename into the blob store happens in `run`).
    let cow_temp_path = cow_temp
        .into_temp_path()
        .keep()
        .context("keeping cow temp file path")?;

    Ok(StagedImport {
        cow_temp_path,
        cow_digest,
        verity_blob: verity_out.blob,
        manifest_bytes,
        manifest_digest,
        tag_key,
    })
}

/// Stream-hash a file with SHA-256, returning the raw 32-byte digest.
/// Used by `lib::run` (BL-01) to hash the cow temp file without ever
/// loading it into memory. 64 KiB read buffer balances syscall count vs.
/// resident-set size.
fn stream_sha256(path: &Path) -> Result<[u8; 32]> {
    let mut f = File::open(path)
        .with_context(|| format!("opening file for hashing: {}", path.display()))?;
    f.seek(SeekFrom::Start(0))
        .context("seek to start of file for hashing")?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .with_context(|| format!("reading file for hashing: {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&hasher.finalize());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_args(tmp: &TempDir, raw_image: PathBuf, tag: &str) -> (ImportArgs, PathBuf) {
        let graphroot = tmp.path().join("storage");
        std::fs::create_dir_all(&graphroot).unwrap();
        let args = ImportArgs {
            raw_image,
            tag: Some(tag.to_string()),
            salt_suffix: None,
            quiet: true,
            created_rfc3339: "2026-05-07T12:00:00Z".to_string(),
            annotations: std::collections::BTreeMap::new(),
        };
        (args, graphroot)
    }

    #[tokio::test]
    async fn run_writes_three_blobs_and_a_tag() {
        let tmp = TempDir::new().unwrap();
        let raw = tmp.path().join("input.raw");
        let mut data = vec![0u8; 64 * 1024]; // 4 chunks at chunk_size=32 sectors
        data[16384] = 0xCC; // chunk 1 non-zero
        std::fs::write(&raw, &data).unwrap();
        let (args, graphroot) = make_args(&tmp, raw, "myapp:base");

        run(args, &graphroot).await.unwrap();

        // Three blobs in <graphroot>/blobs/sha256/.
        let blobs_dir = graphroot.join("blobs").join("sha256");
        let entries: Vec<_> = std::fs::read_dir(&blobs_dir).unwrap().collect();
        assert_eq!(
            entries.len(),
            3,
            "expected exactly 3 blobs (cow, verity, manifest)"
        );

        // Tag resolves (stored under canonical form after Reference::from_str normalization).
        let db = FilesystemTagDb::open(&graphroot).unwrap();
        let canonical_tag = "myapp:base".parse::<Reference>().unwrap().to_string();
        let resolved = db.resolve_tag(&canonical_tag).await.unwrap();
        assert!(
            resolved.is_some(),
            "tag must resolve (canonical: {canonical_tag})"
        );
    }

    #[tokio::test]
    async fn imports_at_the_carapace_scute_chunk_size() {
        // Carapaces are fixed at the spec-whitelisted chunk size; import is
        // no longer tunable and must emit exactly that value.
        let tmp = TempDir::new().unwrap();
        let raw = tmp.path().join("input.raw");
        let mut data = vec![0u8; 4096 * 3];
        data[5000] = 0xCC;
        std::fs::write(&raw, &data).unwrap();
        let (args, graphroot) = make_args(&tmp, raw, "x:y");
        run(args, &graphroot).await.unwrap();

        // The emitted cow blob's header records the chunk size; it must be 8.
        let blobs = graphroot.join("blobs").join("sha256");
        let cow = std::fs::read_dir(&blobs)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_none()) // skip .verity sidecar
            .map(|p| std::fs::read(&p).unwrap())
            .find(|b| b.len() >= 16 && &b[0..4] == b"SnAp") // dm-snapshot cow magic
            .expect("a cow blob");
        let chunk = u32::from_le_bytes(cow[12..16].try_into().unwrap());
        assert_eq!(chunk, crate::SCUTE_CHUNK_SIZE_SECTORS);
    }
}
