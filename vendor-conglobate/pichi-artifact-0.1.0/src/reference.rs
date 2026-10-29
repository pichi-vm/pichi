// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! OCI image reference parser (per D-02). Parses dockerhub shorthand into
//! the canonical fully-qualified form (`docker.io/library/<image>:latest`).

use std::fmt;
use std::str::FromStr;

use thiserror::Error;

use crate::Digest;

/// Parsed OCI image reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// Registry hostname (e.g. `docker.io`).
    pub registry: String,
    /// Repository (e.g. `library/alpine`).
    pub repo: String,
    /// Tag-or-digest selector.
    pub kind: ReferenceKind,
}

/// Either a human-readable tag or a content digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReferenceKind {
    /// Tag string (e.g. `latest`, `3`, `v1.2.3`).
    Tag(String),
    /// Content digest.
    Digest(Digest),
}

/// Errors from parsing a reference string.
#[derive(Debug, Error)]
pub enum ReferenceParseError {
    /// Empty input string.
    #[error("empty reference")]
    Empty,
    /// Repository contains uppercase characters (forbidden by OCI spec).
    #[error("repository must be lowercase: {0}")]
    NotLowercase(String),
    /// Embedded digest failed to parse.
    #[error("invalid digest in reference: {0}")]
    InvalidDigest(#[from] crate::digest::DigestParseError),
    /// A path component is `.` or `..` (path traversal). T-42-03.
    #[error("path-traversal segment in reference: {0:?}")]
    PathTraversalSegment(String),
    /// An empty `/`-separated segment (e.g. `a//b`). T-42-03.
    #[error("empty segment in reference (consecutive '/' or leading/trailing '/')")]
    EmptySegment,
    /// A path component contains a forbidden character (NUL, backslash, ...).
    /// T-42-03 defence-in-depth.
    #[error("invalid character in reference component: {0:?}")]
    InvalidComponent(String),
    /// A tag string contains a character outside the OCI tag grammar
    /// `[A-Za-z0-9_][A-Za-z0-9._-]{0,127}`.
    #[error("invalid tag (must match OCI grammar): {0:?}")]
    InvalidTag(String),
}

/// Returns true if the first path component looks like a registry hostname.
///
/// A component is treated as a registry if it contains `.` or `:`, or equals `localhost`.
///
/// Path-traversal segments (`.`, `..`) are NEVER treated as registries even
/// though they technically contain a `.` — T-42-03 requires they be rejected
/// upstream by the parser.
fn looks_like_registry(component: &str) -> bool {
    if component == "." || component == ".." {
        return false;
    }
    component.contains('.') || component.contains(':') || component == "localhost"
}

/// T-42-03: validate a single `/`-separated path component of `repo`.
///
/// Rejects:
/// - empty components (would arise from `a//b` or leading/trailing `/`),
/// - `.` and `..` (path traversal),
/// - components containing NUL or backslash (defence-in-depth against
///   filesystem APIs that might interpret them — Phase 43/44 will compose
///   `<cache>/<repo>/...` paths from this struct).
fn validate_path_component(c: &str) -> Result<(), ReferenceParseError> {
    if c.is_empty() {
        return Err(ReferenceParseError::EmptySegment);
    }
    if c == "." || c == ".." {
        return Err(ReferenceParseError::PathTraversalSegment(c.to_string()));
    }
    if c.contains('\0') || c.contains('\\') {
        return Err(ReferenceParseError::InvalidComponent(c.to_string()));
    }
    Ok(())
}

/// T-42-03 (and WR-07): validate the tag string against the OCI tag grammar:
/// `[A-Za-z0-9_][A-Za-z0-9._-]{0,127}`. In particular, rejects tags
/// containing `:` (which sneaks through `find(':')` for things like
/// `alpine:3:4:5`).
fn validate_tag(tag: &str) -> Result<(), ReferenceParseError> {
    if tag.is_empty() || tag.len() > 128 {
        return Err(ReferenceParseError::InvalidTag(tag.to_string()));
    }
    let mut chars = tag.chars();
    let first = chars.next().expect("non-empty checked above");
    if !(first.is_ascii_alphanumeric() || first == '_') {
        return Err(ReferenceParseError::InvalidTag(tag.to_string()));
    }
    for c in chars {
        if !(c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-') {
            return Err(ReferenceParseError::InvalidTag(tag.to_string()));
        }
    }
    Ok(())
}

impl FromStr for Reference {
    type Err = ReferenceParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(ReferenceParseError::Empty);
        }

        // T-42-03: reject leading `/` or `\` on the raw input (would otherwise
        // produce an empty first segment after `find('/')`).
        if s.starts_with('/') || s.starts_with('\\') {
            return Err(ReferenceParseError::EmptySegment);
        }

        // Split off digest suffix (@sha256:...) first, before any tag processing.
        let (before_digest, digest_kind) = if let Some(at_pos) = s.find('@') {
            let digest_str = &s[at_pos + 1..];
            let digest = digest_str.parse::<Digest>()?;
            (&s[..at_pos], Some(ReferenceKind::Digest(digest)))
        } else {
            (s, None)
        };

        // Split off tag suffix (:tag) — only after removing digest, and only from
        // the last component (not the host:port part).
        // Strategy: find the last '/' then look for ':' in the part after it.
        let (before_tag, tag_kind) = if digest_kind.is_none() {
            // Find the tag: look for ':' after the last '/'
            let last_slash = before_digest.rfind('/');
            let search_from = last_slash.map_or(0, |p| p + 1);
            if let Some(colon_pos) = before_digest[search_from..].find(':') {
                let absolute_colon = search_from + colon_pos;
                let tag = before_digest[absolute_colon + 1..].to_string();
                (
                    &before_digest[..absolute_colon],
                    Some(ReferenceKind::Tag(tag)),
                )
            } else {
                (before_digest, None)
            }
        } else {
            (before_digest, None)
        };

        // The kind is whichever was found (digest wins over tag; both can't coexist).
        let kind = digest_kind
            .or(tag_kind)
            .unwrap_or_else(|| ReferenceKind::Tag("latest".to_string()));

        // Now parse registry and repo from `before_tag`.
        let (registry, repo) = if let Some(slash_pos) = before_tag.find('/') {
            let first_component = &before_tag[..slash_pos];
            let rest = &before_tag[slash_pos + 1..];
            if looks_like_registry(first_component) {
                // Explicit registry
                (first_component.to_string(), rest.to_string())
            } else {
                // No explicit registry — dockerhub with user namespace
                ("docker.io".to_string(), before_tag.to_string())
            }
        } else {
            // No slash at all — official library image on dockerhub
            ("docker.io".to_string(), format!("library/{before_tag}"))
        };

        // Validate: repo must be all-lowercase.
        if repo.chars().any(|c| c.is_ascii_uppercase()) {
            return Err(ReferenceParseError::NotLowercase(repo));
        }

        // T-42-03: validate every `/`-separated component of `repo`. Rejects
        // path traversal (`..`, `.`), empty segments (`a//b`), and forbidden
        // chars (NUL, backslash). Phase 43/44 will compose `<cache>/<repo>`
        // paths from this struct, so the type must be unforgeable at parse time.
        for component in repo.split('/') {
            validate_path_component(component)?;
        }

        // T-42-03 / WR-07: tag must match OCI tag grammar (rejects `alpine:3:4:5`,
        // tags with NUL, etc.).
        if let ReferenceKind::Tag(ref t) = kind {
            validate_tag(t)?;
        }

        Ok(Reference {
            registry,
            repo,
            kind,
        })
    }
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ReferenceKind::Tag(tag) => write!(f, "{}/{}:{}", self.registry, self.repo, tag),
            ReferenceKind::Digest(digest) => {
                write!(f, "{}/{}@{}", self.registry, self.repo, digest)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(s: &str) -> String {
        s.parse::<Reference>()
            .unwrap_or_else(|e| panic!("parse failed for {s:?}: {e}"))
            .to_string()
    }

    #[test]
    fn alpine_shorthand() {
        assert_eq!(parse_ok("alpine"), "docker.io/library/alpine:latest");
    }

    #[test]
    fn alpine_with_tag() {
        assert_eq!(parse_ok("alpine:3"), "docker.io/library/alpine:3");
    }

    #[test]
    fn ubuntu_with_tag() {
        assert_eq!(parse_ok("ubuntu:22.04"), "docker.io/library/ubuntu:22.04");
    }

    #[test]
    fn user_image_no_tag() {
        assert_eq!(
            parse_ok("myuser/myimage"),
            "docker.io/myuser/myimage:latest"
        );
    }

    #[test]
    fn user_image_with_tag() {
        assert_eq!(parse_ok("myuser/myimage:v1"), "docker.io/myuser/myimage:v1");
    }

    #[test]
    fn fully_qualified() {
        assert_eq!(
            parse_ok("registry.io/foo/bar:tag"),
            "registry.io/foo/bar:tag"
        );
    }

    #[test]
    fn host_with_port() {
        assert_eq!(parse_ok("ghcr.io:5000/x/y:1"), "ghcr.io:5000/x/y:1");
    }

    #[test]
    fn digest_ref() {
        let hex = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        let input = format!("alpine@sha256:{hex}");
        let expected = format!("docker.io/library/alpine@sha256:{hex}");
        assert_eq!(parse_ok(&input), expected);
    }

    #[test]
    fn empty_is_err() {
        let err = "".parse::<Reference>().unwrap_err();
        assert!(matches!(err, ReferenceParseError::Empty));
    }

    #[test]
    fn uppercase_repo_is_err() {
        let err = "MyRepo/image".parse::<Reference>().unwrap_err();
        assert!(matches!(err, ReferenceParseError::NotLowercase(_)));
    }

    /// D-02: `localhost/foo:bar` (no port) parses as a localhost-hosted registry,
    /// NOT as dockerhub user `localhost`. Verifies that `looks_like_registry`
    /// treats the literal `localhost` as a registry hostname.
    #[test]
    fn localhost_no_port_is_registry() {
        assert_eq!(parse_ok("localhost/foo:bar"), "localhost/foo:bar");
    }

    /// D-02: `localhost:5000/foo:bar` (with port) parses as a localhost-hosted
    /// registry. The `:` in the first component triggers `looks_like_registry`.
    #[test]
    fn localhost_with_port_is_registry() {
        assert_eq!(parse_ok("localhost:5000/foo:bar"), "localhost:5000/foo:bar");
    }

    /// D-02 corner: bare `localhost` (no slash) is treated as a dockerhub
    /// official image name (matches podman). To address localhost-as-registry
    /// without a path, the user must include a `/` (see `localhost_no_port_is_registry`).
    #[test]
    fn localhost_official_image_falls_back_to_dockerhub() {
        assert_eq!(parse_ok("localhost"), "docker.io/library/localhost:latest");
    }

    // --- T-42-03: path-traversal rejection (BL-02) -------------------------

    #[test]
    fn rejects_dot_dot_traversal_at_root() {
        let err = "../../etc/passwd:tag".parse::<Reference>().unwrap_err();
        assert!(
            matches!(err, ReferenceParseError::PathTraversalSegment(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_single_dot_segment() {
        let err = "./foo:bar".parse::<Reference>().unwrap_err();
        assert!(
            matches!(err, ReferenceParseError::PathTraversalSegment(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_dot_dot_in_middle() {
        let err = "foo/../bar:tag".parse::<Reference>().unwrap_err();
        assert!(
            matches!(err, ReferenceParseError::PathTraversalSegment(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_empty_segment_double_slash() {
        let err = "a//b:tag".parse::<Reference>().unwrap_err();
        assert!(
            matches!(err, ReferenceParseError::EmptySegment),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_leading_slash() {
        let err = "/foo:tag".parse::<Reference>().unwrap_err();
        assert!(
            matches!(err, ReferenceParseError::EmptySegment),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_backslash_in_repo() {
        let err = "foo\\bar:tag".parse::<Reference>().unwrap_err();
        assert!(
            matches!(err, ReferenceParseError::InvalidComponent(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_bare_dot_dot() {
        // Bare `..` has no slash → falls through to the "official image"
        // branch, producing repo="library/.." which the component validator
        // must reject.
        let err = "..".parse::<Reference>().unwrap_err();
        assert!(
            matches!(err, ReferenceParseError::PathTraversalSegment(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn looks_like_registry_rejects_dot_and_dotdot() {
        assert!(!looks_like_registry("."));
        assert!(!looks_like_registry(".."));
    }

    // --- WR-07: tag containing colon -------------------------------------

    #[test]
    fn rejects_tag_with_colon() {
        let err = "alpine:3:4:5".parse::<Reference>().unwrap_err();
        assert!(
            matches!(err, ReferenceParseError::InvalidTag(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_empty_tag() {
        let err = "alpine:".parse::<Reference>().unwrap_err();
        assert!(
            matches!(err, ReferenceParseError::InvalidTag(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn accepts_normal_tags_unchanged() {
        // Smoke check: every existing valid tag still parses after WR-07.
        for r in ["alpine", "alpine:3", "ubuntu:22.04", "myuser/myimage:v1"] {
            assert!(r.parse::<Reference>().is_ok(), "{r} should parse");
        }
    }
}
