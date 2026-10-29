// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Media-type string constants. Defined in Phase 41; Phase 42 (FORMAT-01..03)
//! locks the final set per CONTEXT.md D-03/D-04/D-08/D-09:
//!
//! - Drop `scute-cow.v1` (per D-03 — there's no scute-verity to disambiguate
//!   against, so the cow IS the entire scute on the wire).
//! - Drop `scute-verity.v1` (per D-04 — verity blobs are not distributed;
//!   they're recomputed deterministically during pull from cow + salt + chain
//!   params).
//! - Add `scute.v1+zstd` (per D-08 — cow blobs MAY be zstd-compressed; default
//!   level 3; readers must handle either form).
//! - PMI blobs are NOT compressed by pichi (per D-09); no `pmi.v1+zstd`.

/// Wrapper artifact type per OCI 1.1 `artifactType`. Carried at the top level
/// of the manifest (per D-03 — NOT inside `config.mediaType`).
pub const MEDIA_TYPE_PICHI_ARTIFACT_V1: &str = "application/vnd.pichi.artifact.v1+json";

/// Scute layer (uncompressed).
///
/// A scute is a dm-snapshot persistent COW file. The base scute's implicit
/// origin is dm-zero (an all-zero device of matching size); only non-zero
/// chunks are recorded as exceptions (per BUILD.md §4 / Phase 42 dm-zero
/// origin lock). Non-base scutes' origins are the preceding scute's
/// verity-validated state per the carapace salt-chain.
///
/// Phase 43 (`pichi import`) writes scutes following this contract; v0.9
/// Phase 46 (carapace device) consumes them.
pub const MEDIA_TYPE_PICHI_SCUTE_V1: &str = "application/vnd.pichi.scute.v1";

/// Scute layer (zstd-compressed). Discriminator: `+zstd` mediaType suffix
/// (per D-08). Decompressor pipeline runs at pull time; local cache stores
/// the decompressed bytes per D-10.
pub const MEDIA_TYPE_PICHI_SCUTE_V1_ZSTD: &str = "application/vnd.pichi.scute.v1+zstd";

/// PMI payload layer. Per D-09, PMI blobs are NOT compressed by pichi
/// (they typically already wrap an internally-compressed kernel + initrd);
/// no `+zstd` variant exists.
pub const MEDIA_TYPE_PICHI_PMI_V1: &str = "application/vnd.pichi.pmi.v1";

/// `requirements.yaml` launch-contract layer (BUILD.md §7) — the host-facing
/// resource contract for a launchable artifact. A small uncompressed YAML
/// blob; at most one per manifest.
///
/// Superseded by the config blob ([`MEDIA_TYPE_PICHI_CONFIG_V1`]): the launch
/// contract now rides in the manifest config, not a layer. Retained during the
/// migration; new writers MUST NOT emit it.
pub const MEDIA_TYPE_PICHI_REQUIREMENTS_V1: &str = "application/vnd.pichi.requirements.v1+yaml";

/// Base DTB layer: the measured base devicetree for a PMI built in the `dt`
/// extension's *detached* channel mode. `pichi run` supplies it to the VMM
/// out-of-band (`dillo --dtb`). At most one per manifest; appears only
/// alongside a (detached-mode) PMI. Uncompressed (small).
pub const MEDIA_TYPE_PICHI_DTB_V1: &str = "application/vnd.pichi.dtb.v1";

/// pichi image config blob — the OCI 1.1 `config` descriptor's target. Carries
/// the launch contract (the `requirements` projection of `config.yaml`,
/// BUILD.md §7.1); replaces the OCI empty-config for pichi artifacts. JSON.
pub const MEDIA_TYPE_PICHI_CONFIG_V1: &str = "application/vnd.pichi.config.v1+json";

/// OCI 1.1 empty descriptor. Formerly the artifact's config blob (inline
/// `data: "e30="`); pichi artifacts now use [`MEDIA_TYPE_PICHI_CONFIG_V1`].
pub const MEDIA_TYPE_OCI_EMPTY_V1: &str = "application/vnd.oci.empty.v1+json";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_constants_have_pichi_prefix_or_oci() {
        assert!(MEDIA_TYPE_PICHI_ARTIFACT_V1.starts_with("application/vnd.pichi."));
        assert!(MEDIA_TYPE_PICHI_SCUTE_V1.starts_with("application/vnd.pichi."));
        assert!(MEDIA_TYPE_PICHI_SCUTE_V1_ZSTD.starts_with("application/vnd.pichi."));
        assert!(MEDIA_TYPE_PICHI_SCUTE_V1_ZSTD.ends_with("+zstd"));
        assert!(MEDIA_TYPE_PICHI_PMI_V1.starts_with("application/vnd.pichi."));
        assert!(MEDIA_TYPE_PICHI_REQUIREMENTS_V1.starts_with("application/vnd.pichi."));
        assert!(MEDIA_TYPE_PICHI_DTB_V1.starts_with("application/vnd.pichi."));
        assert!(MEDIA_TYPE_PICHI_CONFIG_V1.starts_with("application/vnd.pichi."));
        assert!(MEDIA_TYPE_PICHI_CONFIG_V1.ends_with("+json"));
        assert!(MEDIA_TYPE_OCI_EMPTY_V1.starts_with("application/vnd.oci."));
    }

    #[test]
    fn scute_v1_zstd_is_scute_v1_with_zstd_suffix() {
        // Discriminator pattern from D-08: variants share a stem.
        assert_eq!(
            format!("{}+zstd", MEDIA_TYPE_PICHI_SCUTE_V1),
            MEDIA_TYPE_PICHI_SCUTE_V1_ZSTD
        );
    }
}
