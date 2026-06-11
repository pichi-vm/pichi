// SPDX-License-Identifier: Apache-2.0

//! `pichi import <raw-image> <tag>` (IMPORT-01..07 / Phase 43). Thin wrapper
//! that converts clap-derived `ImportArgs` to `pichi_import::ImportArgs`
//! (TryFrom -- fallible hex decode for `--salt`), parses the tag through
//! `Reference::from_str` for path-traversal safety (T-43-02), resolves the
//! cache layout, supplies the RFC3339 timestamp (so tools/import doesn't
//! need its own chrono dep -- Plan 03 manifest.rs decision), and dispatches
//! to the library `run`.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use anyhow::{Context, Result};

use pichi_artifact::Reference;
use pichi_storage::CacheLayout;

use crate::cli::ImportArgs;
use crate::config::Config;

/// `pichi import` entry point — import a raw image into the local cache
/// as a base carapace artifact (IMPORT-01..07; Phase 43).
pub fn run(args: ImportArgs, config: &Config) -> Result<()> {
    // T-43-02: parse the tag through the path-traversal-safe parser
    // BEFORE any I/O. Phase 42 BL-02 already covers traversal vectors.
    let _tag_ref: Reference = args
        .tag
        .parse()
        .with_context(|| format!("invalid tag reference: {}", args.tag))?;

    let layout = resolve_layout(config)?;
    let mut lib_args: pichi_import::ImportArgs = args.try_into()?;
    // Supply the RFC3339 timestamp here (chrono is a workspace dep of
    // the root `pichi` crate; tools/import deliberately doesn't pull
    // chrono -- Plan 03 manifest.rs decision).
    lib_args.created_rfc3339 = chrono::Utc::now().to_rfc3339();
    pichi_import::run(lib_args, &layout.graphroot)
}

fn resolve_layout(config: &Config) -> Result<CacheLayout> {
    let mut layout = CacheLayout::resolve()?;
    if let Some(p) = &config.storage.graphroot {
        layout.graphroot.clone_from(p);
    }
    if let Some(p) = &config.storage.runroot {
        layout.runroot.clone_from(p);
    }
    Ok(layout)
}
