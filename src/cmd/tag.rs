// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `pichi tag <src> <dst>` (LOCAL-04). Pure metadata operation — creates a
//! new tag pointing at the same manifest digest as `src`. NO blob copy.
//!
//! Per D-01 / LOCAL-05: both refs are canonicalised via `Reference::from_str`
//! before TagDb lookup, so `pichi tag alpine my-alias` works (alpine →
//! docker.io/library/alpine:latest).

#![cfg_attr(test, allow(clippy::unwrap_used))]

use anyhow::{Context, Result, anyhow};

use pichi_artifact::Reference;
use pichi_storage::{FilesystemTagDb, TagDb};

use crate::cli::TagArgs;
use crate::config::Config;

/// `pichi tag <src> <dst>` entry point — create a new tag pointing at
/// the same manifest digest as `src` (LOCAL-04).
pub async fn run(args: TagArgs, config: &Config) -> Result<()> {
    let layout = config.resolve_layout()?;
    let db = FilesystemTagDb::open(&layout.graphroot)
        .with_context(|| format!("opening tag db at {}", layout.graphroot.display()))?;

    let src_ref: Reference = args
        .src
        .parse()
        .with_context(|| format!("invalid source reference: {}", args.src))?;
    let dst_ref: Reference = args
        .dst
        .parse()
        .with_context(|| format!("invalid destination reference: {}", args.dst))?;

    let src_key = src_ref.to_string();
    let dst_key = dst_ref.to_string();

    let digest = db
        .resolve_tag(&src_key)
        .await?
        .ok_or_else(|| anyhow!("source ref not found in cache: {src_key}"))?;

    db.set_tag(&dst_key, &digest)
        .await
        .with_context(|| format!("creating tag {dst_key}"))?;

    log::info!("tagged {src_key} as {dst_key} (manifest {digest})");
    Ok(())
}
