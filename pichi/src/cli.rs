// SPDX-License-Identifier: Apache-2.0

//! Clap-derived argument structs for the `pichi` image-management commands.
//!
//! Clap derives live exclusively in the binary crate; each library crate
//! exposes a plain-Rust args struct, and the `From`/`TryFrom` impls here
//! convert at dispatch time.

use clap::{Args as ClapArgs, Subcommand};

/// Sub-subcommands for `pichi system <verb>`.
#[derive(Debug, Subcommand)]
pub enum SystemCmd {
    /// Print system information (paths, config files, version).
    Info(InfoArgs),
    /// Garbage-collect orphan blobs (PRUNE-01..04).
    Prune(PruneArgs),
}

/// Args for `pichi system info`.
#[derive(Debug, ClapArgs)]
pub struct InfoArgs {
    /// Output as machine-readable JSON instead of formatted text.
    #[arg(long)]
    pub json: bool,
}

/// Args for `pichi system prune` (PRUNE-04 — single `--dry-run` flag, no others).
#[derive(Debug, ClapArgs)]
pub struct PruneArgs {
    /// Compute and print orphans without unlinking; exit 0.
    #[arg(long)]
    pub dry_run: bool,
}

/// Args for `pichi images` (LOCAL-01 / D-12..D-19).
#[derive(Debug, ClapArgs)]
pub struct ImagesArgs {
    /// Print FULL sha256:... digests one per line (D-18 — diverges from podman intentionally).
    #[arg(long, short = 'q')]
    pub quiet: bool,
    /// Always include the full DIGEST column (per D-14).
    #[arg(long)]
    pub digests: bool,
    /// Render each row using a `tinytemplate` template (per D-17).
    /// Available fields: `{Repository}`, `{Tag}`, `{Bootable}`, `{ID}`,
    /// `{Digest}`, `{Created}`, `{Size}`, `{ScuteCount}`. UTF-8 terminal
    /// assumed for default `BOOTABLE` glyph (`✓` / `—`); use
    /// `--format "{Bootable}"` for programmatic `true`/`false`.
    #[arg(long)]
    pub format: Option<String>,
}

/// Args for `pichi inspect <ref>` (LOCAL-02 / D-20 image-index aware).
#[derive(Debug, ClapArgs)]
pub struct InspectArgs {
    /// Image reference: `image:tag`, `image@sha256:...`, full registry path,
    /// or dockerhub shorthand (LOCAL-05).
    pub reference: String,
    /// Output format: `json` (default, pretty-printed) or a tinytemplate string.
    #[arg(long)]
    pub format: Option<String>,
}

/// Args for `pichi rmi <ref>...` (LOCAL-03).
#[derive(Debug, ClapArgs)]
pub struct RmiArgs {
    /// One or more image references to remove.
    #[arg(required = true)]
    pub references: Vec<String>,
    /// Remove the tag even if its manifest is shared with other tags
    /// (per LOCAL-03 — without `--force`, a shared-manifest tag rmi errors).
    #[arg(long, short = 'f')]
    pub force: bool,
}

/// Args for `pichi tag <src> <dst>` (LOCAL-04).
#[derive(Debug, ClapArgs)]
pub struct TagArgs {
    /// Source reference (must already exist in the cache).
    pub src: String,
    /// Destination reference (created or overwritten).
    pub dst: String,
}

/// Args for `pichi import <raw-image> <tag>` (IMPORT-01..07 / Phase 43).
#[derive(Debug, ClapArgs)]
pub struct ImportArgs {
    /// Path to the raw image file to import (treated as opaque bytes per
    /// CONTEXT D-06 — no GPT parse, no CRC, no partition-table inspection).
    pub raw_image: std::path::PathBuf,
    /// Tag to assign (e.g. `myapp:base`). Validated via
    /// `pichi_artifact::Reference::from_str` before any I/O.
    pub tag: String,
    /// COW chunk size in 512-byte sectors. Power-of-two, ≥ 8 (IMPORT-07).
    /// Default: 32 (16 KiB; matches `DM_CHUNK_SIZE_DEFAULT_SECTORS`).
    #[arg(long)]
    pub chunk_size: Option<u32>,
    /// Hex-encoded author suffix appended after the 32-byte zero salt
    /// prefix (D-01). Default (no flag): salt = 32 zero bytes only.
    #[arg(long)]
    pub salt: Option<String>,
    /// Suppress progress reporting.
    #[arg(long, default_value_t = false)]
    pub quiet: bool,
    /// Emit `{"cow_digest","verity_digest","root_hash"}` JSON on stdout
    /// for CI `veritysetup verify` consumption (D-04 / RESEARCH Open-Q #1).
    #[arg(long, default_value_t = false)]
    pub print_verity_info: bool,
    /// Optional path to a pre-built PMI file to bundle as a sibling layer.
    /// When present, produces an appliance artifact (one Scute + one PMI layer).
    /// The PMI file is treated as opaque bytes — pichi import does NOT
    /// validate PMI format or measurement (producer owns PMI validity).
    #[arg(long)]
    pub pmi: Option<std::path::PathBuf>,
}

/// Pull policy for `pichi pull --pull=...` (REGISTRY-03 / D-05 default `always`).
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum PullPolicy {
    /// Always re-fetch the manifest (default).
    Always,
    /// Skip network if the tag is in the cache.
    Missing,
    /// Fail with a clear error if the tag is not in the cache.
    Never,
    /// Fetch upstream manifest digest and pull only if it differs from cached.
    Newer,
}

/// Args for `pichi pull <ref>` (REGISTRY-01..07).
#[derive(Debug, ClapArgs)]
pub struct PullArgs {
    /// Image reference (REGISTRY-07: dockerhub shorthand, full path, mirror, etc).
    pub reference: String,
    /// Pull policy (default: `always`).
    #[arg(long, value_enum)]
    pub pull: Option<PullPolicy>,
    /// Suppress progress reporting.
    #[arg(long, short = 'q', default_value_t = false)]
    pub quiet: bool,
}

/// Args for `pichi push <ref>` (REGISTRY-02).
#[derive(Debug, ClapArgs)]
pub struct PushArgs {
    /// Image reference.
    pub reference: String,
    /// Suppress progress reporting.
    #[arg(long, short = 'q', default_value_t = false)]
    pub quiet: bool,
}

/// Args for `pichi run <ref>` — boot a cached artifact by exec'ing dillo.
#[derive(Debug, ClapArgs)]
pub struct RunArgs {
    /// Image reference to boot (`image:tag` or `image@sha256:...`).
    pub reference: String,
    /// Number of vCPUs. Overrides config; falls back to dillo's default.
    #[arg(long)]
    pub cpus: Option<u32>,
    /// Guest memory in MiB. Overrides config; falls back to dillo's default.
    #[arg(long)]
    pub memory: Option<u32>,
}

/// `--salt` hex decode is fallible — use TryFrom so bad hex is rejected
/// at the dispatch boundary with `with_context`, not silently swallowed.
impl TryFrom<ImportArgs> for pichi_import::ImportArgs {
    type Error = anyhow::Error;
    fn try_from(a: ImportArgs) -> anyhow::Result<Self> {
        use anyhow::Context as _;
        let salt_suffix = a
            .salt
            .as_deref()
            .map(hex::decode)
            .transpose()
            .with_context(|| format!("invalid --salt hex: {:?}", a.salt))?;
        Ok(Self {
            raw_image: a.raw_image,
            tag: a.tag,
            chunk_size_sectors: a.chunk_size,
            salt_suffix,
            quiet: a.quiet,
            print_verity_info: a.print_verity_info,
            created_rfc3339: String::new(), // overwritten by cmd::import::run
            pmi: a.pmi,
        })
    }
}
