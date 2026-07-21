// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Consumption of the launch contract (BUILD.md §7) that a bootable artifact
//! carries in its manifest **config blob** (`vnd.pichi.config.v1+json`).
//! `pichi build` and `pichi run` read it to size the VM: an explicit operator
//! value (CLI > config) wins but MUST meet the `required` floor; absent, the
//! `recommended` (else `required`) tier is used.

use anyhow::{Context, Result, bail};
use pichi_artifact::{Config, Digest, Manifest, Requirements};
use pichi_storage::BlobStore;

/// Load the artifact's launch contract from its config blob. Returns `Ok(None)`
/// for artifacts with the OCI empty config (no launch contract, e.g. plain
/// carapaces).
pub fn load_requirements(
    manifest: &Manifest,
    blob_store: &impl BlobStore,
) -> Result<Option<Requirements>> {
    if manifest.config.is_empty() {
        return Ok(None);
    }
    let digest: Digest = manifest
        .config
        .digest
        .parse()
        .with_context(|| format!("invalid config digest: {}", manifest.config.digest))?;
    let bytes = blob_store
        .get_blob(&digest)
        .with_context(|| format!("reading config blob {digest}"))?;
    let config = Config::from_json(&bytes).context("parsing config blob")?;
    Ok(Some(config.requirements))
}

/// Resolve one VM-sizing value (cpus or memory-MiB) against its requirement
/// band. `explicit` is the operator's choice (CLI merged over config); it wins
/// but MUST be at least `required` (else a launch error, BUILD.md §7). When
/// absent, fall back to `recommended` then `required`. An explicit value below
/// `recommended` (but meeting `required`) starts with a warning.
pub fn resolve_sized(
    explicit: Option<u32>,
    required: Option<u32>,
    recommended: Option<u32>,
    what: &str,
) -> Result<Option<u32>> {
    match explicit {
        Some(v) => {
            if let Some(r) = required
                && v < r
            {
                bail!("requested {what} ({v}) is below the image's required minimum ({r})");
            }
            if let Some(rec) = recommended
                && v < rec
            {
                log::warn!("requested {what} ({v}) is below the recommended {rec}");
            }
            Ok(Some(v))
        }
        None => Ok(recommended.or(required)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_meeting_floor_is_kept() {
        assert_eq!(
            resolve_sized(Some(4), Some(2), Some(8), "cpus").unwrap(),
            Some(4)
        );
    }

    #[test]
    fn explicit_below_required_errors() {
        assert!(resolve_sized(Some(1), Some(2), Some(8), "cpus").is_err());
    }

    #[test]
    fn absent_uses_recommended_then_required() {
        assert_eq!(
            resolve_sized(None, Some(2), Some(8), "cpus").unwrap(),
            Some(8)
        );
        assert_eq!(resolve_sized(None, Some(2), None, "cpus").unwrap(), Some(2));
        assert_eq!(resolve_sized(None, None, None, "cpus").unwrap(), None);
    }
}
