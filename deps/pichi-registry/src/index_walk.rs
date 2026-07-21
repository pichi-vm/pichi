// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

// Phase 44 D-02 / D-20 / REGISTRY-05 / REGISTRY-07: OCI Image Index walk.
//
// Tag-resolved manifests may be either bare manifests OR multi-entry indices.
// This module owns the discrimination logic for the index case: walk the
// `manifests[]` array, pick the entry whose `artifactType` matches pichi
// AND whose `platform` matches the runtime. On no-match, error with all
// entries listed (D-20 mandate). On multiple matches, error (ambiguous).
//
// Lives in pichi-registry (not the pichi binary) so unit tests can exercise
// the walk without spinning up an HTTP impl. The function is feature-gated
// `#[cfg(feature = "http-client")]` only because index walks happen in the
// HTTP path; logically it could be unconditional but feature-gating avoids
// a default-build dep on serde_json.

#![cfg(feature = "http-client")]

//! OCI Image Index walker (Phase 44 D-02 / D-20 / REGISTRY-05).
//!
//! When `pichi pull <tag>` resolves to an `application/vnd.oci.image.index.v1+json`
//! the walker picks the single pichi entry matching the runtime platform; the
//! index itself is dropped (D-02). REGISTRY-05 / Pitfall 1: this module exists
//! precisely so pichi never relies on oci-client's auto-resolving manifest
//! API (the one that internally invokes the platform-resolver); the walk
//! semantics are pichi-controlled and brittle-tested. The literal API name
//! is intentionally not spelled out here so the REGISTRY-05 negative-grep
//! gate stays a clean "0 matches" signal.

use anyhow::{Context, Result, anyhow};
use pichi_artifact::MEDIA_TYPE_PICHI_ARTIFACT_V1;
use serde_json::Value;

/// OCI Image Index media type (per OCI Image Spec 1.1). Re-exported so callers
/// have a single canonical constant; the value is also accepted by
/// `pull_manifest_raw`'s `accepted_media_types` array in `http.rs`.
pub const OCI_IMAGE_INDEX_MEDIA_TYPE: &str = "application/vnd.oci.image.index.v1+json";

/// Walk an OCI Image Index `manifests[]` array; return the digest string of
/// the entry matching `(artifactType=MEDIA_TYPE_PICHI_ARTIFACT_V1,
/// platform.os=target_os, platform.architecture=target_arch)`.
///
/// On no-match: Err whose Display lists every entry's
/// `(digest, artifactType, platform)` tuple per D-20.
/// On multiple matches: Err (ambiguous — current pichi only supports
/// single-platform-per-tag).
///
/// # Errors
///
/// Returns an error when:
/// - The bytes are not valid JSON.
/// - The JSON has no `manifests` array (not an index).
/// - No entry matches the pichi artifact type AND target platform.
/// - Multiple entries match (ambiguous index).
pub fn pick_pichi_entry_from_index(
    index_json: &[u8],
    target_os: &str,
    target_arch: &str,
) -> Result<String> {
    let v: Value = serde_json::from_slice(index_json).context("parse OCI Image Index")?;
    let manifests = v
        .get("manifests")
        .and_then(|m| m.as_array())
        .ok_or_else(|| anyhow!("OCI image index missing `manifests` array"))?;

    let mut candidates = Vec::new();
    let mut summary = Vec::with_capacity(manifests.len());
    for m in manifests {
        let artifact_type = m.get("artifactType").and_then(|v| v.as_str());
        let digest = m
            .get("digest")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let platform = m.get("platform");
        let pl_os = platform.and_then(|p| p.get("os")).and_then(|v| v.as_str());
        let pl_arch = platform
            .and_then(|p| p.get("architecture"))
            .and_then(|v| v.as_str());

        summary.push(format!(
            "  - {} (artifactType={:?}, platform={}/{})",
            digest,
            artifact_type.unwrap_or("<none>"),
            pl_os.unwrap_or("<none>"),
            pl_arch.unwrap_or("<none>"),
        ));

        if artifact_type == Some(MEDIA_TYPE_PICHI_ARTIFACT_V1)
            && pl_os == Some(target_os)
            && pl_arch == Some(target_arch)
        {
            candidates.push(digest);
        }
    }

    if candidates.len() == 1 {
        return Ok(candidates.remove(0));
    }
    if candidates.is_empty() {
        return Err(anyhow!(
            "no pichi entry for platform {target_os}/{target_arch} in image index. \
             Index entries:\n{}",
            summary.join("\n")
        ));
    }
    Err(anyhow!(
        "ambiguous: {} pichi entries match platform {target_os}/{target_arch}. \
         Index entries:\n{}",
        candidates.len(),
        summary.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::{OCI_IMAGE_INDEX_MEDIA_TYPE, pick_pichi_entry_from_index};
    use pichi_artifact::MEDIA_TYPE_PICHI_ARTIFACT_V1;

    fn idx(entries: &str) -> Vec<u8> {
        format!(
            r#"{{ "schemaVersion": 2, "mediaType": "{OCI_IMAGE_INDEX_MEDIA_TYPE}", "manifests": [{entries}] }}"#
        )
        .into_bytes()
    }

    #[test]
    fn pick_pichi_entry_from_index_happy_path() {
        let entries = format!(
            r#"
            {{ "digest": "sha256:aaaa", "platform": {{"os":"linux","architecture":"amd64"}} }},
            {{ "digest": "sha256:dddd", "artifactType": "{MEDIA_TYPE_PICHI_ARTIFACT_V1}",
               "platform": {{"os":"linux","architecture":"amd64"}} }}
        "#
        );
        let digest = pick_pichi_entry_from_index(&idx(&entries), "linux", "amd64").unwrap();
        assert_eq!(
            digest, "sha256:dddd",
            "must pick the pichi entry, not the container-image entry"
        );
    }

    #[test]
    fn pick_pichi_entry_from_index_no_match_lists_entries() {
        let entries = r#"
            { "digest": "sha256:cccc1", "platform": {"os":"linux","architecture":"amd64"} },
            { "digest": "sha256:cccc2", "platform": {"os":"linux","architecture":"arm64"} }
        "#;
        let err = pick_pichi_entry_from_index(&idx(entries), "linux", "amd64").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("no pichi entry for platform linux/amd64"),
            "wrong header: {msg}"
        );
        assert!(
            msg.contains("sha256:cccc1"),
            "missing first entry digest in listing: {msg}"
        );
        assert!(
            msg.contains("sha256:cccc2"),
            "missing second entry digest in listing: {msg}"
        );
        assert!(
            msg.contains("artifactType=None") || msg.contains("artifactType=\"<none>\""),
            "missing artifactType placeholder: {msg}"
        );
        assert!(
            msg.contains("linux/arm64"),
            "missing arm64 platform tuple: {msg}"
        );
    }

    #[test]
    fn pick_pichi_entry_from_index_wrong_arch() {
        let entries = format!(
            r#"
            {{ "digest": "sha256:dddd_arm", "artifactType": "{MEDIA_TYPE_PICHI_ARTIFACT_V1}",
               "platform": {{"os":"linux","architecture":"arm64"}} }}
        "#
        );
        let err = pick_pichi_entry_from_index(&idx(&entries), "linux", "amd64").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no pichi entry for platform linux/amd64"));
        assert!(msg.contains("sha256:dddd_arm"));
        assert!(msg.contains("linux/arm64"));
    }
}
