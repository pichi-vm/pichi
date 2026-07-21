// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `pichi-registry`: OCI registry I/O abstraction (D-22 / D-23).
//!
//! Phase 42 ships:
//! - The [`Registry`] async trait (using Rust 2024 native AFIT — no
//!   `async-trait` proc macro).
//! - The [`RegistryError`] enum (one variant per failure mode the trait
//!   exposes).
//! - The in-memory [`MockRegistry`] implementation (`mock` module).
//!
//! The production HTTP impl wraps `oci-client 0.16`, gated behind a
//! `http-client` Cargo feature so default consumers (the `MockRegistry`
//! used by `pichi import` tests) pay zero `oci-client` cost.
//!
//! **Isolation invariant:** this crate belongs to pichi's image-management
//! layer only. It never reaches the VM launcher: pichi prepares the image
//! then `exec()`s the separate `dillo` binary, so `tokio` / `oci-client`
//! never enter the running VMM's process.
//!
//! `tokio` (with `features = ["io-util"]`) and `futures-util` are
//! unconditional dependencies — the streaming [`Registry`] trait surface
//! (`pull_blob<W: AsyncWrite + Unpin + Send>` and `push_blob_stream<S:
//! Stream<...>>`) requires these types unconditionally.
//!
//! Other isolation rules:
//! - No `oci-client` deps reachable from default features.
//! - Manifest payloads stay plain [`Vec<u8>`] / [`bytes::Bytes`] — never
//!   `Box<dyn ReadSeek>` (which would re-import `pichi-storage`).
//! - Blob payloads flow through `AsyncWrite` sinks (pull) /
//!   `Stream<Item = io::Result<Bytes>>` (push) so multi-GiB carapace scutes
//!   never need to fit in memory.
//! - The mock impl bodies are synchronous `Mutex` accesses; they `async fn`
//!   for trait conformance but no future ever actually awaits (mock pull
//!   sinks are zero-poll `Vec<u8>`-style writers).
//!
//! Per D-23, the production impl creates a throwaway
//! `tokio::runtime::Builder::new_current_thread().build()?` per call inside
//! the `pichi` binary, drops it before return — so `tokio` lives only behind
//! the feature flag and only inside this crate.

use bytes::Bytes;
use futures_util::stream::Stream;
use pichi_artifact::{Digest, Reference};
use thiserror::Error;
use tokio::io::AsyncWrite;

#[cfg(feature = "http-client")]
pub mod auth;
#[cfg(feature = "http-client")]
pub mod http;
#[cfg(feature = "http-client")]
pub mod index_walk;
pub mod mock;

#[cfg(feature = "http-client")]
pub use auth::{AuthEnv, AuthHint, resolve_for_registry};
#[cfg(feature = "http-client")]
pub use http::{HttpRegistry, RegistryEntry};
#[cfg(feature = "http-client")]
pub use index_walk::{OCI_IMAGE_INDEX_MEDIA_TYPE, pick_pichi_entry_from_index};
pub use mock::MockRegistry;

/// Registry I/O failures. Variants are deliberately coarse — Phase 44's
/// production impl maps `oci-client` errors into these. The mock only ever
/// emits [`Self::NotFound`].
#[derive(Debug, Error)]
pub enum RegistryError {
    /// The requested blob, manifest, or tag is not present in the registry.
    #[error("registry: not found: {0}")]
    NotFound(String),
    /// Authentication or authorization failure.
    #[error("registry: authentication failed: {0}")]
    Auth(String),
    /// A blob's content digest did not match what was requested. Phase 44
    /// triple-validation per D-11 (CVM error reporting constraint).
    #[error("registry: blob digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch {
        /// Expected digest (what was requested).
        expected: Digest,
        /// Actual digest (what was computed from the returned bytes).
        actual: Digest,
    },
    /// HTTP / TLS / I/O error from the transport layer.
    #[error("registry: transport error: {0}")]
    Transport(String),
    /// Manifest bytes failed to parse as a `Manifest` (or the underlying
    /// OCI shape).
    #[error("registry: manifest parse error: {0}")]
    Manifest(#[from] pichi_artifact::Error),
}

/// Convenience alias for `Result<T, RegistryError>`.
pub type Result<T> = std::result::Result<T, RegistryError>;

/// OCI registry I/O abstraction.
///
/// Method shape per 42-RESEARCH.md §"Recommended trait surface" + Phase 44 D-01:
/// - All methods are `async` (Rust 2024 native AFIT).
/// - Manifest payloads are returned as raw [`Bytes`] (not parsed) so the
///   caller can choose whether to dispatch via D-20 (image-index walk) or
///   parse directly as [`pichi_artifact::Manifest`]. Manifests are
///   KB-scale; streaming them adds zero value.
/// - Blob payloads are STREAMING (D-01): `pull_blob` writes into an
///   [`AsyncWrite`] sink; `push_blob_stream` reads from a
///   [`Stream<Item = std::io::Result<Bytes>>`]. A single carapace scute can
///   be multi-GiB — buffering through the throwaway `current_thread`
///   runtime risks OOM on small CI runners and operator hosts.
/// - The trait is `Send + Sync` so future consumers can stash an
///   `Arc<dyn Registry>` behind a multi-thread runtime without juggling.
///
/// **Note on dyn dispatch:** native AFIT makes `Box<dyn Registry>` non-trivial
/// (returned futures are `impl Future`, not erased). Consumers should
/// parameterize over `R: Registry` (static dispatch). If a future
/// plug-in registry-selector needs `dyn`, expose a separate
/// `trait DynRegistry` that wraps `R` and boxes futures — but defer that
/// until needed.
// `async fn` in trait emits the `async_fn_in_trait` lint because the returned
// future does not carry a `Send` bound by default. Every concrete impl in this
// crate (`MockRegistry`, `HttpRegistry`) is `Send + Sync` and produces `Send`
// futures because their bodies only `.await` `Send` futures (`oci-client` /
// `Mutex` lock guards held only across `await` points are scoped). Static-
// dispatch consumers don't observe the missing `Send` bound. Plan 03 explicitly
// allows the lint at the trait level so the http-client clippy gate
// (`-D warnings`) stays clean; the trait-shape comment in the rustdoc above
// already documents the dyn-dispatch caveat.
#[allow(async_fn_in_trait)]
pub trait Registry: Send + Sync {
    /// Fetch a manifest (image manifest OR image index per D-20) by tag.
    /// Returns `(raw bytes, resolved digest)`.
    async fn pull_manifest_by_tag(&self, reference: &Reference) -> Result<(Bytes, Digest)>;

    /// Fetch a manifest by digest (no tag-resolution roundtrip).
    async fn pull_manifest_by_digest(
        &self,
        registry: &str,
        repo: &str,
        digest: &Digest,
    ) -> Result<Bytes>;

    /// D-01: streaming blob fetch driven by an `AsyncWrite` sink.
    ///
    /// The `size` parameter is the descriptor byte length (used by oci-client
    /// to build a `BlobDescriptor`; mock impls may ignore it).
    ///
    /// Implementations MUST verify the bytes written to `sink` hash to
    /// `digest` before returning `Ok` (`oci-client::Client::pull_blob` does
    /// this internally; [`MockRegistry`] verifies its in-memory bytes
    /// before any sink write so mismatch errors never leak partial bytes).
    async fn pull_blob<W: AsyncWrite + Unpin + Send>(
        &self,
        registry: &str,
        repo: &str,
        digest: &Digest,
        size: u64,
        sink: &mut W,
    ) -> Result<()>;

    /// HEAD check: does the registry have this blob? Used by push to skip
    /// already-present blobs (REGISTRY-02).
    async fn head_blob(&self, registry: &str, repo: &str, digest: &Digest) -> Result<bool>;

    /// Upload a manifest. Returns the registry-assigned digest (which MUST
    /// equal `sha256(bytes)` — implementations may verify, must not silently
    /// accept a mismatch).
    async fn push_manifest(
        &self,
        reference: &Reference,
        media_type: &str,
        bytes: Bytes,
    ) -> Result<Digest>;

    /// D-01: streaming blob push. Wraps `oci-client::push_blob_stream`.
    /// Caller passes the pre-computed digest (registry verifies on receive)
    /// and a stream that yields `io::Result<Bytes>` chunks. The blob bytes
    /// never need to fit in memory at once — Plan 03's `HttpRegistry` will
    /// drive this from a `BlobStore::open_blob(...)` reader wrapped in a
    /// `stream::unfold`.
    async fn push_blob_stream<S>(
        &self,
        registry: &str,
        repo: &str,
        digest: &Digest,
        stream: S,
    ) -> Result<()>
    where
        S: Stream<Item = std::io::Result<Bytes>> + Send + Sync + 'static;

    /// Cross-repository blob mount (push optimization per REGISTRY-02). Used
    /// to avoid re-uploading blobs that already exist in another repo on the
    /// same registry. `Ok(true)` = mount succeeded; `Ok(false)` = registry
    /// does not support mount (caller falls back to [`Self::push_blob`]).
    async fn try_blob_mount(
        &self,
        registry: &str,
        target_repo: &str,
        source_repo: &str,
        digest: &Digest,
    ) -> Result<bool>;
}
