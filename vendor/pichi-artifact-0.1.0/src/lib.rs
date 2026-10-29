// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Shared, application-independent types for OCI-compatible pichi artifacts.
//!
//! Defines image manifests, component descriptors, media types, digests,
//! references, and the artifact config-blob schema. This crate performs no
//! filesystem, registry, cache, VM-launch, or build orchestration. Reader-based
//! decoding accepts a caller-provided stream.
//!
//! Both pichi and conglobate consume this format contract independently.

#![forbid(unsafe_code)]

mod config;
mod digest;
mod error;
mod manifest;
mod media_type;
mod reference;

pub use config::{
    Band, CONFIG_SCHEMA_VERSION, Config, ConfigError, Ingress, Interface, Memory, PortSpec,
    Requirements,
};
pub use digest::{Digest, DigestParseError};
pub use error::Error;
pub use manifest::{
    CHAIN_ANNOTATION_VERITY_HASH, ConfigDescriptor, DtbDescriptor, Layer, Manifest,
    ManifestValidationError, PmiDescriptor, ScuteAnnotations, ScuteDescriptor,
};
pub use media_type::{
    MEDIA_TYPE_OCI_EMPTY_V1, MEDIA_TYPE_PICHI_ARTIFACT_V1, MEDIA_TYPE_PICHI_CONFIG_V1,
    MEDIA_TYPE_PICHI_DTB_V1, MEDIA_TYPE_PICHI_PMI_V1, MEDIA_TYPE_PICHI_REQUIREMENTS_V1,
    MEDIA_TYPE_PICHI_SCUTE_V1, MEDIA_TYPE_PICHI_SCUTE_V1_ZSTD,
};
pub use reference::{Reference, ReferenceKind, ReferenceParseError};
