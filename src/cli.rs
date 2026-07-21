// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
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

    /// Optional base DTB file for a detached-mode PMI. Bundled as a
    /// `vnd.pichi.dtb.v1` layer; `pichi run` supplies it to the VMM
    /// out-of-band. Treated as opaque bytes.
    #[arg(long)]
    pub dtb: Option<std::path::PathBuf>,

    /// Optional launch-contract config (JSON or YAML with a `requirements`
    /// section; memory in bytes). Stored as the manifest config blob
    /// (`vnd.pichi.config.v1+json`) instead of the OCI empty config.
    #[arg(long)]
    pub config: Option<std::path::PathBuf>,
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

/// Sub-subcommands for `pichi manifest <verb>` — assemble and push a
/// multi-arch OCI image index, mirroring `docker manifest`.
#[derive(Debug, Subcommand)]
pub enum ManifestCmd {
    /// Create a local manifest list from one or more pushed per-arch refs.
    Create(ManifestCreateArgs),

    /// Set the platform (`os`/`architecture`) of a list entry.
    Annotate(ManifestAnnotateArgs),

    /// Push the assembled list to a registry as an OCI image index.
    Push(ManifestPushArgs),
}

/// Args for `pichi manifest create <list> <source>...`.
#[derive(Debug, ClapArgs)]
pub struct ManifestCreateArgs {
    /// The manifest-list reference to create (e.g. `ghcr.io/org/img:43`).
    pub list: String,
    /// One or more per-arch source references, already pushed to the
    /// registry (e.g. `ghcr.io/org/img:43-amd64`).
    #[arg(required = true)]
    pub sources: Vec<String>,
}

/// Args for `pichi manifest annotate <list> <source> --os <os> --arch <arch>`.
#[derive(Debug, ClapArgs)]
pub struct ManifestAnnotateArgs {
    /// The manifest-list reference.
    pub list: String,
    /// The source reference (as passed to `create`) whose entry to annotate.
    pub source: String,
    /// Platform OS (pichi artifacts use `pichi`).
    #[arg(long)]
    pub os: String,
    /// Platform architecture (e.g. `amd64`, `arm64`).
    #[arg(long)]
    pub arch: String,
}

/// Args for `pichi manifest push <list> <dest>`.
#[derive(Debug, ClapArgs)]
pub struct ManifestPushArgs {
    /// The local manifest-list reference to push.
    pub list: String,
    /// Destination registry reference (e.g. `ghcr.io/org/img:43`). All
    /// referenced local images and the list are pushed here atomically.
    pub dest: String,
}

/// Args for `pichi save <ref> -o <dir>` — export to an OCI image layout dir.
#[derive(Debug, ClapArgs)]
pub struct SaveArgs {
    /// Cached image reference to export (e.g. `fedora:43`).
    pub reference: String,
    /// Output directory for the OCI image layout.
    #[arg(short = 'o', long = "output")]
    pub output: std::path::PathBuf,
}

/// Args for `pichi load <dir>` — import an OCI image layout dir.
#[derive(Debug, ClapArgs)]
pub struct LoadArgs {
    /// OCI image layout directory to import (as produced by `pichi save`).
    pub input: std::path::PathBuf,
    /// Override the tag to register (default: the tag recorded in the layout).
    #[arg(long)]
    pub tag: Option<String>,
}

/// Args for `pichi build [-t <tag>] [--build-image <ref>] <dir>` (BUILD.md §3).
#[derive(Debug, ClapArgs)]
pub struct BuildArgs {
    /// Project directory containing `pichi.build/`.
    pub dir: std::path::PathBuf,
    /// Tag for the produced artifact (e.g. `myapp:v1`).
    #[arg(short = 't', long)]
    pub tag: Option<String>,
    /// Build-image reference (a PMI-only appliance). Falls back to the
    /// `PICHI_BUILD_IMAGE` environment variable.
    #[arg(long = "build-image")]
    pub build_image: Option<String>,
    /// Guest memory in MiB for the build VM (forwarded to dillo).
    #[arg(long)]
    pub memory: Option<u32>,
    /// vCPUs for the build VM (forwarded to dillo).
    #[arg(long)]
    pub cpus: Option<u32>,
}

/// Args for `pichi update [<dir>]` — pin carapace references in
/// `pichi.build/*.yaml` into `pichi.build/refs.lock` (BUILD.md §2.4 / §14).
#[derive(Debug, ClapArgs)]
pub struct UpdateArgs {
    /// Project directory containing `pichi.build/` (default: current dir).
    pub dir: Option<std::path::PathBuf>,
}

/// Args for `pichi run <ref>` — boot an artifact (auto-pulling if not cached).
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
            salt_suffix,
            quiet: a.quiet,
            print_verity_info: a.print_verity_info,
            created_rfc3339: String::new(), // overwritten by cmd::import::run
            pmi: a.pmi,
            dtb: a.dtb,
            config_json: None, // set by cmd::import::run from --config
        })
    }
}
