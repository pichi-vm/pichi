// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Builds the typed `Manifest` for a base scute (one cow, no PMI)
//! per Phase 42's typed schema and Phase 42 D-06 locked chain annotations.
//!
use std::collections::BTreeMap;

use anyhow::{Context as _, Result};

use conglobate::artifact::{
    ConfigDescriptor, Digest, Layer, MEDIA_TYPE_PICHI_ARTIFACT_V1, Manifest, ScuteDescriptor,
};

/// Build a `Manifest` for a base carapace import (one Scute, no PMI).
///
/// `pichi import` produces base carapaces only; the bootable appliance form
/// (Scute + PMI + boot configuration) is assembled later by `pichi import pmi`, which appends the
/// PMI/boot configuration layers onto an already-imported carapace. See BUILD.md §15.
///
/// - One Scute layer carrying `cow_digest`, `cow_size`, and the
///   `dev.pichi.scute.verity.salt` annotation (hex-encoded full salt —
///   32-byte zero parent prefix followed by a 32-byte random suffix).
/// - Top-level chain annotations per Phase 42 D-06:
///   - Default geometry/version/hash-type parameters are omitted.
///   - `dev.pichi.carapace.verity.hash` = `sha256:<hex>` commits the scute root.
///     Caller annotations cannot override the commitment.
/// - `org.opencontainers.image.created` = caller-supplied RFC 3339 string.
/// - Config descriptor: the caller's stored launch config.
///
/// Calls `Manifest::validate()` defensively to catch any drift in
/// our own builder against Phase 42 D-07 validation rules. Returns the
/// validated manifest ready for `to_bytes()` + `BlobStore::put_blob`.
pub(super) trait BaseManifest {
    fn base(
        scute: ScuteDescriptor,
        config: ConfigDescriptor,
        root: &[u8; 32],
        created_rfc3339: &str,
        extra: &BTreeMap<String, String>,
    ) -> Result<Manifest>;
}

impl BaseManifest for Manifest {
    fn base(
        scute: ScuteDescriptor,
        config: ConfigDescriptor,
        root: &[u8; 32],
        created_rfc3339: &str,
        extra: &BTreeMap<String, String>,
    ) -> Result<Manifest> {
        // Caller-supplied provenance annotations go in first; the structural
        // `verity.*` keys below then overwrite (they can't be spoofed via `extra`),
        // and `created` is filled only if the caller didn't supply one.
        let mut annotations = extra.clone();
        annotations.remove("dev.pichi.carapace.verity.algo");
        for key in ["data-block-size", "hash-block-size", "version", "hash-type"] {
            annotations.remove(&format!("dev.pichi.carapace.verity.{key}"));
        }
        annotations.insert(
            "dev.pichi.carapace.verity.hash".into(),
            Digest::Sha256(*root).to_string(),
        );
        annotations
            .entry("org.opencontainers.image.created".to_string())
            .or_insert_with(|| created_rfc3339.to_string());

        let manifest = Manifest {
            schema_version: 2,
            media_type: conglobate::artifact::MEDIA_TYPE_OCI_MANIFEST_V1.into(),
            artifact_type: MEDIA_TYPE_PICHI_ARTIFACT_V1.to_string(),
            config,
            layers: vec![Layer::Scute(scute)],
            annotations,
        };

        manifest
            .validate()
            .context("internally constructed manifest failed self-validation (bug)")?;

        Ok(manifest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use conglobate::artifact::ScuteAnnotations;

    fn scute(digest: &Digest, salt: &[u8]) -> ScuteDescriptor {
        ScuteDescriptor {
            digest: digest.to_string(),
            size: 16384,
            annotations: ScuteAnnotations {
                digest: digest.to_string(),
                size: std::num::NonZeroU64::new(16384).unwrap().into(),
                verity_digest: Digest::Sha256([2; 32]).to_string(),
                data_blocks: std::num::NonZeroU64::new(4).unwrap().into(),
                salt: devmap_verity::Salt::new(salt).unwrap().into(),
            },
        }
    }

    /// IMPORT-05 + manifest_has_locked_verity_params (VALIDATION.md):
    /// chain annotations carry the Phase 42 D-06 locked values.
    #[test]
    fn manifest_has_locked_chain_annotations() {
        let cow_digest = Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(b"deadbeef").into());
        let salt = vec![0u8; 32];
        let m = Manifest::base(
            scute(&cow_digest, &salt),
            ConfigDescriptor::canonical(),
            &[0xab; 32],
            "2026-05-07T12:00:00Z",
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(!m.annotations.contains_key("dev.pichi.carapace.verity.algo"));
        for key in ["data-block-size", "hash-block-size", "version", "hash-type"] {
            assert!(
                !m.annotations
                    .contains_key(&format!("dev.pichi.carapace.verity.{key}"))
            );
        }
        assert_eq!(
            m.annotations
                .get("org.opencontainers.image.created")
                .map(String::as_str),
            Some("2026-05-07T12:00:00Z")
        );
        assert_eq!(
            m.annotations.get("dev.pichi.carapace.verity.hash"),
            Some(&format!("sha256:{}", "ab".repeat(32)))
        );
    }

    /// CONTEXT D-01: bottom-scute salt prefix is exactly 32 zero bytes;
    /// the salt annotation in the manifest reflects this (64 hex chars
    /// of zeros for a default-flag import).
    #[test]
    fn bottom_scute_salt_is_zero_prefix() {
        let cow_digest = Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(b"deadbeef").into());
        let salt = vec![0u8; 32]; // Existing layers may have no suffix.
        let m = Manifest::base(
            scute(&cow_digest, &salt),
            ConfigDescriptor::canonical(),
            &[0xab; 32],
            "2026-05-07T12:00:00Z",
            &BTreeMap::new(),
        )
        .unwrap();
        let Layer::Scute(scute) = &m.layers[0] else {
            panic!("expected scute layer");
        };
        assert_eq!(scute.annotations.data_blocks.get(), 4);
        assert_eq!(
            scute.annotations.salt.as_ref(),
            &[0; 32],
            "salt annotation = 32 zero bytes hex-encoded = 64 hex zeros"
        );
    }

    /// Preserve the producer's full salt, including its random suffix.
    #[test]
    fn preserves_full_salt() {
        let cow_digest = Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(b"deadbeef").into());
        let mut salt = vec![0u8; 32];
        salt.extend_from_slice(&[0xAB; 32]);
        let m = Manifest::base(
            scute(&cow_digest, &salt),
            ConfigDescriptor::canonical(),
            &[0xab; 32],
            "2026-05-07T12:00:00Z",
            &BTreeMap::new(),
        )
        .unwrap();
        let Layer::Scute(scute) = &m.layers[0] else {
            panic!("expected scute layer");
        };
        assert_eq!(scute.annotations.salt.as_ref(), salt);
    }

    /// Manifest passes `validate()` (Phase 42 D-07 rules).
    #[test]
    fn build_passes_validate() {
        let cow_digest = Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(b"deadbeef").into());
        let salt = vec![0u8; 32];
        let m = Manifest::base(
            scute(&cow_digest, &salt),
            ConfigDescriptor::canonical(),
            &[0xab; 32],
            "2026-05-07T12:00:00Z",
            &BTreeMap::new(),
        )
        .unwrap();
        // base() already calls validate() internally; double-check is
        // cheap and proves the invariant.
        assert!(m.validate().is_ok());
    }

    /// import produces a base carapace: exactly one Scute layer, no PMI, and
    /// the supplied config (the appliance form is assembled later by
    /// `pichi import pmi`).
    #[test]
    fn build_is_carapace_only() {
        let cow_digest = Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(b"deadbeef").into());
        let salt = vec![0u8; 32];
        let m = Manifest::base(
            scute(&cow_digest, &salt),
            ConfigDescriptor::canonical(),
            &[0xab; 32],
            "2026-05-07T12:00:00Z",
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(m.layers.len(), 1, "expected exactly 1 layer (Scute)");
        assert!(matches!(m.layers[0], Layer::Scute(_)), "layer[0] is Scute");
        assert!(
            !m.layers.iter().any(|l| matches!(l, Layer::Pmi(_))),
            "base carapace carries no PMI layer"
        );
        assert_eq!(m.config, ConfigDescriptor::canonical());
    }

    /// Caller annotations merge in, but structural verity keys can't be spoofed
    /// and a caller-supplied `created` wins over the stamped one.
    #[test]
    fn extra_annotations_merge_with_structural_precedence() {
        let mut extra = BTreeMap::new();
        extra.insert(
            "org.opencontainers.image.source".to_string(),
            "https://example/repo".to_string(),
        );
        extra.insert(
            "org.opencontainers.image.created".to_string(),
            "2020-01-01T00:00:00Z".to_string(),
        );
        extra.insert(
            "dev.pichi.carapace.verity.algo".to_string(),
            "EVIL".to_string(),
        );
        extra.insert(
            "dev.pichi.carapace.verity.hash".to_string(),
            "cd".repeat(32),
        );
        for key in ["data-block-size", "hash-block-size", "version", "hash-type"] {
            extra.insert(format!("dev.pichi.carapace.verity.{key}"), "999".into());
        }
        let m = Manifest::base(
            scute(
                &Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(b"deadbeef").into()),
                &[0; 32],
            ),
            ConfigDescriptor::canonical(),
            &[0xab; 32],
            "2026-05-07T12:00:00Z",
            &extra,
        )
        .unwrap();
        // provenance carried verbatim
        assert_eq!(
            m.annotations
                .get("org.opencontainers.image.source")
                .unwrap(),
            "https://example/repo"
        );
        // caller-supplied created wins over the stamped timestamp
        assert_eq!(
            m.annotations
                .get("org.opencontainers.image.created")
                .unwrap(),
            "2020-01-01T00:00:00Z"
        );
        for key in ["data-block-size", "hash-block-size", "version", "hash-type"] {
            assert!(
                !m.annotations
                    .contains_key(&format!("dev.pichi.carapace.verity.{key}"))
            );
        }
        // structural verity key cannot be overridden by a caller annotation
        assert!(!m.annotations.contains_key("dev.pichi.carapace.verity.algo"));
        assert_eq!(
            m.annotations.get("dev.pichi.carapace.verity.hash"),
            Some(&format!("sha256:{}", "ab".repeat(32)))
        );
    }
}
