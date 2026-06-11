// SPDX-License-Identifier: Apache-2.0

//! `pichi-artifact`: pure types for OCI-compatible pichi artifacts.
//!
//! This crate has zero I/O, no async, and no heavyweight dependencies. It is
//! consumed by `pichi-storage` (blob/tag I/O), the root `pichi` binary (CLI
//! wiring), `pichi-registry` (Phase 44 pull/push), and `tools/import` (Phase 43).
//!
//! Locked decisions (per Phase 41 CONTEXT.md):
//! - **D-01**: [`Digest`] is a sum-type enum with one variant per algorithm.
//! - **D-02**: [`Reference`] is a typed parser; canonical [`std::fmt::Display`] form is
//!   what `pichi-storage::TagDb` stores.
//! - **D-03**: Manifests are stored as blobs (no separate cache trait);
//!   helpers live on [`Manifest`].

mod digest;
mod error;
mod manifest;
mod media_type;
mod reference;

pub use digest::{Digest, DigestParseError};
pub use error::Error;
pub use manifest::{
    EmptyConfigDescriptor, Layer, Manifest, ManifestValidationError, PmiDescriptor,
    ScuteAnnotations, ScuteDescriptor,
};
pub use media_type::{
    MEDIA_TYPE_OCI_EMPTY_V1, MEDIA_TYPE_PICHI_ARTIFACT_V1, MEDIA_TYPE_PICHI_PMI_V1,
    MEDIA_TYPE_PICHI_SCUTE_V1, MEDIA_TYPE_PICHI_SCUTE_V1_ZSTD,
};
pub use reference::{Reference, ReferenceKind, ReferenceParseError};
