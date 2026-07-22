// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Builds the typed `Manifest` for a base scute (one cow, no PMI)
//! per Phase 42's typed schema and Phase 42 D-06 locked chain annotations.
//!
//! Private — only `lib::run` calls this. The chain-annotation values
//! (`sha256` / `4096` / `4096`) MUST match Phase 42's locked defaults
//! (verified against `tests/cmd_tag.rs::chain_annotations` lines 15–24).
//! Caller supplies `created_rfc3339` so this crate doesn't need its own
//! `chrono` dep — the root `pichi` binary already pulls chrono.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result};

use pichi_artifact::{
    CHAIN_ANNOTATION_VERITY_HASH, ConfigDescriptor, Digest, Layer, MEDIA_TYPE_PICHI_ARTIFACT_V1,
    Manifest, ScuteAnnotations, ScuteDescriptor,
};

/// Build a `Manifest` for a base carapace import (one Scute, no PMI).
///
/// `pichi import` produces base carapaces only; the bootable appliance form
/// (Scute + PMI + DTB) is assembled later by `pichi import pmi`, which appends the
/// PMI/DTB layers onto an already-imported carapace. See BUILD.md §15.
///
/// - One Scute layer carrying `cow_digest`, `cow_size`, and the
///   `dev.pichi.scute.verity.salt` annotation (hex-encoded full salt —
///   32-byte zero prefix per CONTEXT D-01 plus optional author suffix).
/// - Top-level chain annotations per Phase 42 D-06:
///   - `dev.pichi.carapace.verity.algo` = `"sha256"`
///   - `dev.pichi.carapace.verity.data-block-size` = `"4096"`
///   - `dev.pichi.carapace.verity.hash-block-size` = `"4096"`
///   - `dev.pichi.carapace.verity.hash` = hex `root_hash` (the single scute's
///     dm-verity root is the carapace top root `rootₙ₋₁`; BUILD.md §7.1).
/// - `org.opencontainers.image.created` = caller-supplied RFC 3339 string.
/// - Config descriptor: the canonical OCI empty config (base carapaces carry
///   no launch contract; `pichi import pmi` sets one if given a `--config`).
///
/// Calls `Manifest::validate()` defensively to catch any drift in
/// our own builder against Phase 42 D-07 validation rules. Returns the
/// validated manifest ready for `to_bytes()` + `BlobStore::put_blob`.
pub(crate) fn build(
    cow_digest: &Digest,
    cow_size: u64,
    full_salt: &[u8],
    root_hash: &[u8; 32],
    created_rfc3339: &str,
) -> Result<Manifest> {
    let mut annotations = BTreeMap::new();
    annotations.insert(
        "dev.pichi.carapace.verity.algo".to_string(),
        "sha256".to_string(),
    );
    annotations.insert(
        "dev.pichi.carapace.verity.data-block-size".to_string(),
        "4096".to_string(),
    );
    annotations.insert(
        "dev.pichi.carapace.verity.hash-block-size".to_string(),
        "4096".to_string(),
    );
    annotations.insert(
        CHAIN_ANNOTATION_VERITY_HASH.to_string(),
        hex::encode(root_hash),
    );
    annotations.insert(
        "org.opencontainers.image.created".to_string(),
        created_rfc3339.to_string(),
    );

    let layers = vec![Layer::Scute(ScuteDescriptor {
        digest: cow_digest.to_string(),
        size: cow_size,
        annotations: ScuteAnnotations {
            salt: hex::encode(full_salt),
        },
    })];

    let manifest = Manifest {
        schema_version: 2,
        media_type: "application/vnd.oci.image.manifest.v1+json".to_string(),
        artifact_type: MEDIA_TYPE_PICHI_ARTIFACT_V1.to_string(),
        config: ConfigDescriptor::canonical(),
        layers,
        annotations,
    };

    manifest
        .validate()
        .context("internally constructed manifest failed self-validation (bug)")?;

    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// IMPORT-05 + manifest_has_locked_verity_params (VALIDATION.md):
    /// chain annotations carry the Phase 42 D-06 locked values.
    #[test]
    fn manifest_has_locked_chain_annotations() {
        let cow_digest = Digest::from_bytes_sha256(b"deadbeef");
        let salt = vec![0u8; 32];
        let m = build(
            &cow_digest,
            16384,
            &salt,
            &[0u8; 32],
            "2026-05-07T12:00:00Z",
        )
        .unwrap();
        assert_eq!(
            m.annotations
                .get("dev.pichi.carapace.verity.algo")
                .map(String::as_str),
            Some("sha256")
        );
        assert_eq!(
            m.annotations
                .get("dev.pichi.carapace.verity.data-block-size")
                .map(String::as_str),
            Some("4096")
        );
        assert_eq!(
            m.annotations
                .get("dev.pichi.carapace.verity.hash-block-size")
                .map(String::as_str),
            Some("4096")
        );
        assert_eq!(
            m.annotations
                .get("org.opencontainers.image.created")
                .map(String::as_str),
            Some("2026-05-07T12:00:00Z")
        );
    }

    /// CONTEXT D-01: bottom-scute salt prefix is exactly 32 zero bytes;
    /// the salt annotation in the manifest reflects this (64 hex chars
    /// of zeros for a default-flag import).
    #[test]
    fn bottom_scute_salt_is_zero_prefix() {
        let cow_digest = Digest::from_bytes_sha256(b"deadbeef");
        let salt = vec![0u8; 32]; // default: just the prefix, no suffix
        let m = build(
            &cow_digest,
            16384,
            &salt,
            &[0u8; 32],
            "2026-05-07T12:00:00Z",
        )
        .unwrap();
        let Layer::Scute(scute) = &m.layers[0] else {
            panic!("expected scute layer");
        };
        assert_eq!(
            scute.annotations.salt,
            "0".repeat(64),
            "salt annotation = 32 zero bytes hex-encoded = 64 hex zeros"
        );
    }

    /// CONTEXT D-01: author-supplied suffix is appended AFTER the
    /// 32-byte zero prefix.
    #[test]
    fn salt_with_author_suffix_appends_after_prefix() {
        let cow_digest = Digest::from_bytes_sha256(b"deadbeef");
        let mut salt = vec![0u8; 32];
        salt.extend_from_slice(&[0xAB, 0xCD, 0xEF]);
        let m = build(
            &cow_digest,
            16384,
            &salt,
            &[0u8; 32],
            "2026-05-07T12:00:00Z",
        )
        .unwrap();
        let Layer::Scute(scute) = &m.layers[0] else {
            panic!("expected scute layer");
        };
        assert_eq!(
            scute.annotations.salt,
            format!("{}{}", "0".repeat(64), "abcdef"),
            "32 zero bytes + 3 author bytes = 64+6 hex chars"
        );
    }

    /// Manifest passes `validate()` (Phase 42 D-07 rules).
    #[test]
    fn build_passes_validate() {
        let cow_digest = Digest::from_bytes_sha256(b"deadbeef");
        let salt = vec![0u8; 32];
        let m = build(
            &cow_digest,
            16384,
            &salt,
            &[0u8; 32],
            "2026-05-07T12:00:00Z",
        )
        .unwrap();
        // build() already calls validate() internally; double-check is
        // cheap and proves the invariant.
        assert!(m.validate().is_ok());
    }

    /// import produces a base carapace: exactly one Scute layer, no PMI, and
    /// the canonical OCI empty config (the appliance form is assembled later
    /// by `pichi import pmi`).
    #[test]
    fn build_is_carapace_only() {
        let cow_digest = Digest::from_bytes_sha256(b"deadbeef");
        let salt = vec![0u8; 32];
        let m = build(
            &cow_digest,
            16384,
            &salt,
            &[0u8; 32],
            "2026-05-07T12:00:00Z",
        )
        .unwrap();
        assert_eq!(m.layers.len(), 1, "expected exactly 1 layer (Scute)");
        assert!(matches!(m.layers[0], Layer::Scute(_)), "layer[0] is Scute");
        assert!(
            !m.layers.iter().any(|l| matches!(l, Layer::Pmi(_))),
            "base carapace carries no PMI layer"
        );
        assert!(m.config.is_empty(), "base carapace uses the empty config");
    }
}
