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
//! index is retained by the caller. REGISTRY-05 / Pitfall 1: this module exists
//! precisely so pichi never relies on oci-client's auto-resolving manifest
//! API (the one that internally invokes the platform-resolver); the walk
//! semantics are pichi-controlled and brittle-tested. The literal API name
//! is intentionally not spelled out here so the REGISTRY-05 negative-grep
//! gate stays a clean "0 matches" signal.

use anyhow::{Context, Result, anyhow};
use conglobate::artifact::MEDIA_TYPE_PICHI_ARTIFACT_V1;
use oci_spec::image::ImageIndex;

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
    let index: ImageIndex = serde_json::from_slice(index_json).context("parse OCI Image Index")?;
    let mut candidates = Vec::new();
    let mut summary = Vec::with_capacity(index.manifests().len());
    for descriptor in index.manifests() {
        let artifact_type = descriptor.artifact_type().as_ref().map(AsRef::as_ref);
        let digest = descriptor.digest().to_string();
        let platform = descriptor.platform().as_ref();
        let pl_os = platform.map(|p| p.os().to_string());
        let pl_arch = platform.map(|p| p.architecture().to_string());

        summary.push(format!(
            "  - {} (artifactType={:?}, platform={}/{})",
            digest,
            artifact_type.unwrap_or("<none>"),
            pl_os.as_deref().unwrap_or("<none>"),
            pl_arch.as_deref().unwrap_or("<none>"),
        ));

        if artifact_type == Some(MEDIA_TYPE_PICHI_ARTIFACT_V1)
            && pl_os.as_deref() == Some(target_os)
            && pl_arch.as_deref() == Some(target_arch)
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
    use super::pick_pichi_entry_from_index;
    use conglobate::artifact::MEDIA_TYPE_PICHI_ARTIFACT_V1;

    fn idx(entries: &str) -> Vec<u8> {
        format!(
            r#"{{ "schemaVersion": 2, "mediaType": "{}", "manifests": [{entries}] }}"#,
            oci_spec::image::MediaType::ImageIndex
        )
        .into_bytes()
    }

    #[test]
    fn pick_pichi_entry_from_index_happy_path() {
        let entries = format!(
            r#"
            {{ "mediaType":"application/vnd.oci.image.manifest.v1+json", "size":1, "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "platform": {{"os":"linux","architecture":"amd64"}} }},
            {{ "mediaType":"application/vnd.oci.image.manifest.v1+json", "size":1, "digest": "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd", "artifactType": "{MEDIA_TYPE_PICHI_ARTIFACT_V1}",
               "platform": {{"os":"linux","architecture":"amd64"}} }}
        "#
        );
        let digest = pick_pichi_entry_from_index(&idx(&entries), "linux", "amd64").unwrap();
        assert_eq!(
            digest, "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
            "must pick the pichi entry, not the container-image entry"
        );
    }

    #[test]
    fn pick_pichi_entry_from_index_no_match_lists_entries() {
        let entries = r#"
            { "mediaType":"application/vnd.oci.image.manifest.v1+json", "size":1, "digest": "sha256:ccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc1", "platform": {"os":"linux","architecture":"amd64"} },
            { "mediaType":"application/vnd.oci.image.manifest.v1+json", "size":1, "digest": "sha256:ccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc2", "platform": {"os":"linux","architecture":"arm64"} }
        "#;
        let err = pick_pichi_entry_from_index(&idx(entries), "linux", "amd64").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("no pichi entry for platform linux/amd64"),
            "wrong header: {msg}"
        );
        assert!(
            msg.contains("sha256:ccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc1"),
            "missing first entry digest in listing: {msg}"
        );
        assert!(
            msg.contains("sha256:ccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc2"),
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
            {{ "mediaType":"application/vnd.oci.image.manifest.v1+json", "size":1, "digest": "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd", "artifactType": "{MEDIA_TYPE_PICHI_ARTIFACT_V1}",
               "platform": {{"os":"linux","architecture":"arm64"}} }}
        "#
        );
        let err = pick_pichi_entry_from_index(&idx(&entries), "linux", "amd64").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no pichi entry for platform linux/amd64"));
        assert!(
            msg.contains("sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd")
        );
        assert!(msg.contains("linux/arm64"));
    }
}
