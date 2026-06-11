// SPDX-License-Identifier: Apache-2.0

//! Crate-level error type. Implementations use `thiserror`; binary callers
//! convert to `anyhow::Error` at the boundary.

use thiserror::Error;

/// Aggregate errors from `pichi-artifact`.
#[derive(Debug, Error)]
pub enum Error {
    /// Digest parse failed.
    #[error(transparent)]
    Digest(#[from] crate::digest::DigestParseError),
    /// Reference parse failed.
    #[error(transparent)]
    Reference(#[from] crate::reference::ReferenceParseError),
    /// JSON serialise / deserialise failed (manifest helpers).
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// I/O error (manifest read helpers).
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Manifest failed D-07 validation rules.
    #[error("manifest validation failed: {0}")]
    Validation(#[from] crate::manifest::ManifestValidationError),
}
