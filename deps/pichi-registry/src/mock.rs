// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! In-memory [`Registry`] implementation for tests.
//!
//! Per D-22, Phase 42 ships this mock alongside the [`crate::Registry`] trait
//! so Phase 43's `pichi import` tests + future cmd tests have a deterministic
//! registry without waiting for Phase 44's HTTP impl.
//!
//! All async fn bodies are synchronous (`Mutex::lock`); the futures resolve on
//! first poll. The clippy lint `unused_async` is allowed for this reason
//! (Pitfall 8 in 42-RESEARCH.md).

#![allow(clippy::unused_async)]

use std::collections::HashMap;
use std::sync::Mutex;

use bytes::Bytes;
use futures_util::stream::{Stream, StreamExt};
use pichi_artifact::{Digest, Reference};
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::{Registry, RegistryError, Result};

/// In-memory `Registry` for tests. Backed by `Mutex<HashMap<...>>` keyed by
/// `(registry, repo, digest)` for blobs and `(registry, repo, tag)` for tags.
///
/// `pub(crate)` fields let test helpers in this module poke directly at the
/// state. External consumers go through the public helper methods.
#[derive(Debug, Default)]
pub struct MockRegistry {
    /// (registry, repo, digest) → blob bytes
    blobs: Mutex<HashMap<(String, String, Digest), Bytes>>,
    /// (registry, repo, tag) → manifest digest
    tags: Mutex<HashMap<(String, String, String), Digest>>,
    /// (registry, repo, digest) → manifest bytes (separate from blobs because
    /// pull_manifest_by_tag returns the bytes alongside the digest)
    manifests: Mutex<HashMap<(String, String, Digest), Bytes>>,
    /// Audit log of every `push_blob_stream` call (test helper). Records the
    /// concatenated bytes drained from the stream so tests can assert on
    /// payload content (D-01: bytes are observable via the log even though
    /// `push_blob_stream` itself is streaming-only).
    pushed_blobs_log: Mutex<Vec<(String, String, Digest, Bytes)>>,
    /// Audit log of every `push_manifest` call (test helper).
    pushed_manifests_log: Mutex<Vec<(Reference, String, Digest)>>,
}

impl MockRegistry {
    /// Create a new empty `MockRegistry`.
    pub fn new() -> Self {
        Self::default()
    }

    // --- Test helpers (pre-populate state) ----------------------------------

    /// Pre-populate a blob in the mock. Tests use this to set up a registry
    /// that already has blobs for `pull_blob` to succeed.
    pub fn insert_blob(&self, registry: &str, repo: &str, digest: Digest, bytes: Bytes) {
        self.blobs
            .lock()
            .expect("blobs mutex poisoned")
            .insert((registry.to_string(), repo.to_string(), digest), bytes);
    }

    /// Pre-populate a manifest in the mock + its tag pointer. The manifest
    /// digest is computed from `bytes` (so callers don't have to compute it
    /// themselves and risk a mismatch).
    pub fn insert_manifest(&self, registry: &str, repo: &str, tag: &str, bytes: Bytes) -> Digest {
        let digest = Digest::from_bytes_sha256(&bytes);
        self.manifests
            .lock()
            .expect("manifests mutex poisoned")
            .insert(
                (registry.to_string(), repo.to_string(), digest.clone()),
                bytes,
            );
        self.tags.lock().expect("tags mutex poisoned").insert(
            (registry.to_string(), repo.to_string(), tag.to_string()),
            digest.clone(),
        );
        digest
    }

    /// Pre-populate a manifest indexed by digest only — no tag is created.
    /// Used by Phase 44 cmd::pull tests to seed the destination of an OCI
    /// image-index walk: the picked manifest is fetched via
    /// `pull_manifest_by_digest`, never via tag. Returns the inserted
    /// digest (computed from `bytes`).
    pub fn insert_manifest_by_digest(&self, registry: &str, repo: &str, bytes: Bytes) -> Digest {
        let digest = Digest::from_bytes_sha256(&bytes);
        self.manifests
            .lock()
            .expect("manifests mutex poisoned")
            .insert(
                (registry.to_string(), repo.to_string(), digest.clone()),
                bytes,
            );
        digest
    }

    /// Pre-populate a tag → manifest-digest mapping (without inserting a
    /// manifest blob). Tests use this when they want to fake a tag pointing
    /// at an absent manifest (to exercise NotFound paths).
    pub fn insert_tag(&self, registry: &str, repo: &str, tag: &str, manifest_digest: Digest) {
        self.tags.lock().expect("tags mutex poisoned").insert(
            (registry.to_string(), repo.to_string(), tag.to_string()),
            manifest_digest,
        );
    }

    /// Snapshot of every `push_blob_stream` call (in call order). Each entry
    /// is `(registry, repo, digest, concatenated bytes)`.
    pub fn pushed_blobs(&self) -> Vec<(String, String, Digest, Bytes)> {
        self.pushed_blobs_log
            .lock()
            .expect("pushed_blobs_log mutex poisoned")
            .clone()
    }

    /// Snapshot of every `push_manifest` call (in call order).
    pub fn pushed_manifests(&self) -> Vec<(Reference, String, Digest)> {
        self.pushed_manifests_log
            .lock()
            .expect("pushed_manifests_log mutex poisoned")
            .clone()
    }
}

impl Registry for MockRegistry {
    async fn pull_manifest_by_tag(&self, reference: &Reference) -> Result<(Bytes, Digest)> {
        let key_tag = match &reference.kind {
            pichi_artifact::ReferenceKind::Tag(t) => t.clone(),
            pichi_artifact::ReferenceKind::Digest(d) => {
                // Digest-form refs go straight to manifests by digest.
                let mkey = (
                    reference.registry.clone(),
                    reference.repo.clone(),
                    d.clone(),
                );
                let bytes = self
                    .manifests
                    .lock()
                    .expect("manifests mutex poisoned")
                    .get(&mkey)
                    .cloned()
                    .ok_or_else(|| RegistryError::NotFound(format!("manifest {reference}")))?;
                return Ok((bytes, d.clone()));
            }
        };
        let tag_key = (reference.registry.clone(), reference.repo.clone(), key_tag);
        let digest = self
            .tags
            .lock()
            .expect("tags mutex poisoned")
            .get(&tag_key)
            .cloned()
            .ok_or_else(|| RegistryError::NotFound(format!("tag {reference}")))?;
        let mkey = (
            reference.registry.clone(),
            reference.repo.clone(),
            digest.clone(),
        );
        let bytes = self
            .manifests
            .lock()
            .expect("manifests mutex poisoned")
            .get(&mkey)
            .cloned()
            .ok_or_else(|| {
                RegistryError::NotFound(format!(
                    "manifest blob for tag {reference} (digest {digest})"
                ))
            })?;
        Ok((bytes, digest))
    }

    async fn pull_manifest_by_digest(
        &self,
        registry: &str,
        repo: &str,
        digest: &Digest,
    ) -> Result<Bytes> {
        let key = (registry.to_string(), repo.to_string(), digest.clone());
        self.manifests
            .lock()
            .expect("manifests mutex poisoned")
            .get(&key)
            .cloned()
            .ok_or_else(|| RegistryError::NotFound(format!("manifest {registry}/{repo}@{digest}")))
    }

    async fn pull_blob<W: AsyncWrite + Unpin + Send>(
        &self,
        registry: &str,
        repo: &str,
        digest: &Digest,
        _size: u64,
        sink: &mut W,
    ) -> Result<()> {
        let key = (registry.to_string(), repo.to_string(), digest.clone());
        let bytes = self
            .blobs
            .lock()
            .expect("blobs mutex poisoned")
            .get(&key)
            .cloned()
            .ok_or_else(|| RegistryError::NotFound(format!("blob {registry}/{repo}@{digest}")))?;
        // Verify content-addressing invariant per the trait contract BEFORE
        // any sink write — mismatch errors must never leak partial bytes.
        // This catches insert_blob misuse in tests.
        let actual = Digest::from_bytes_sha256(&bytes);
        if actual != *digest {
            return Err(RegistryError::DigestMismatch {
                expected: digest.clone(),
                actual,
            });
        }
        sink.write_all(&bytes)
            .await
            .map_err(|e| RegistryError::Transport(format!("mock sink write failed: {e}")))?;
        Ok(())
    }

    async fn head_blob(&self, registry: &str, repo: &str, digest: &Digest) -> Result<bool> {
        let key = (registry.to_string(), repo.to_string(), digest.clone());
        Ok(self
            .blobs
            .lock()
            .expect("blobs mutex poisoned")
            .contains_key(&key))
    }

    async fn push_manifest(
        &self,
        reference: &Reference,
        media_type: &str,
        bytes: Bytes,
    ) -> Result<Digest> {
        let digest = Digest::from_bytes_sha256(&bytes);
        let mkey = (
            reference.registry.clone(),
            reference.repo.clone(),
            digest.clone(),
        );
        self.manifests
            .lock()
            .expect("manifests mutex poisoned")
            .insert(mkey, bytes);
        if let pichi_artifact::ReferenceKind::Tag(t) = &reference.kind {
            let tag_key = (
                reference.registry.clone(),
                reference.repo.clone(),
                t.clone(),
            );
            self.tags
                .lock()
                .expect("tags mutex poisoned")
                .insert(tag_key, digest.clone());
        }
        self.pushed_manifests_log
            .lock()
            .expect("pushed_manifests_log mutex poisoned")
            .push((reference.clone(), media_type.to_string(), digest.clone()));
        Ok(digest)
    }

    async fn push_blob_stream<S>(
        &self,
        registry: &str,
        repo: &str,
        digest: &Digest,
        _size: u64,
        stream: S,
    ) -> Result<()>
    where
        S: Stream<Item = std::io::Result<Bytes>> + Send + 'static,
    {
        let mut pinned = Box::pin(stream);
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = pinned.next().await {
            let chunk =
                chunk.map_err(|e| RegistryError::Transport(format!("mock stream chunk: {e}")))?;
            buf.extend_from_slice(&chunk);
        }
        let bytes = Bytes::from(buf);
        let key = (registry.to_string(), repo.to_string(), digest.clone());
        self.blobs
            .lock()
            .expect("blobs mutex poisoned")
            .insert(key, bytes.clone());
        self.pushed_blobs_log
            .lock()
            .expect("pushed_blobs_log mutex poisoned")
            .push((
                registry.to_string(),
                repo.to_string(),
                digest.clone(),
                bytes,
            ));
        Ok(())
    }

    async fn try_blob_mount(
        &self,
        registry: &str,
        target_repo: &str,
        source_repo: &str,
        digest: &Digest,
    ) -> Result<bool> {
        let src_key = (
            registry.to_string(),
            source_repo.to_string(),
            digest.clone(),
        );
        let blobs = self.blobs.lock().expect("blobs mutex poisoned");
        let Some(bytes) = blobs.get(&src_key).cloned() else {
            return Ok(false);
        };
        drop(blobs);
        let dst_key = (
            registry.to_string(),
            target_repo.to_string(),
            digest.clone(),
        );
        self.blobs
            .lock()
            .expect("blobs mutex poisoned")
            .insert(dst_key, bytes);
        Ok(true)
    }
}

// --- Inline tests -----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake};

    /// Minimal block_on for tests. The mock's futures resolve on first poll
    /// (no real await happens), so we never need a tokio runtime. Uses
    /// `Box::pin` to avoid `unsafe` — satisfies workspace `unsafe_code = "deny"`.
    ///
    /// WR-01: this helper does NOT install a real waker. If a future ever
    /// returns `Pending` (e.g. someone wires in a `tokio::sync::oneshot` or
    /// an `async` combinator that yields once), spinning forever would hang
    /// the CI worker with no diagnostic. We cap the poll count and panic
    /// with a clear message instead.
    ///
    /// The cap (1024) is far above any plausible "actually-ready-on-first-poll"
    /// path; if a future re-polls itself with a noop waker, panicking is
    /// the correct response.
    fn block_on<F: Future>(fut: F) -> F::Output {
        struct NoopWaker;
        impl Wake for NoopWaker {
            fn wake(self: Arc<Self>) {}
        }
        let waker = Arc::new(NoopWaker).into();
        let mut cx = Context::from_waker(&waker);
        let mut fut = Box::pin(fut);
        // Cap iterations: if Pending under a noop waker means nobody will
        // ever wake us, so continuing to poll is a no-progress busy loop.
        for _ in 0..1024 {
            if let Poll::Ready(out) = fut.as_mut().poll(&mut cx) {
                return out;
            }
        }
        panic!(
            "mock::block_on: future returned Pending across 1024 polls under a \
             noop waker. Did someone add a real `await` to a Registry method? \
             The mock test executor cannot drive real async — switch the test \
             to a tokio runtime."
        );
    }

    fn r() -> Reference {
        "alpine:3".parse().unwrap()
    }

    #[test]
    fn pull_blob_round_trip() {
        let m = MockRegistry::new();
        let data = Bytes::from_static(b"hello-blob");
        let digest = Digest::from_bytes_sha256(&data);
        m.insert_blob("docker.io", "library/alpine", digest.clone(), data.clone());
        // Vec<u8> implements tokio::io::AsyncWrite (zero-poll: poll_write
        // always returns Ready), so block_on's 1024-poll cap (WR-01) is
        // never reached.
        let mut sink: Vec<u8> = Vec::new();
        block_on(m.pull_blob(
            "docker.io",
            "library/alpine",
            &digest,
            data.len() as u64,
            &mut sink,
        ))
        .unwrap();
        assert_eq!(sink, data.as_ref());
    }

    #[test]
    fn pull_blob_not_found() {
        let m = MockRegistry::new();
        let digest = Digest::from_bytes_sha256(b"absent");
        let mut sink: Vec<u8> = Vec::new();
        let err = block_on(m.pull_blob("docker.io", "library/alpine", &digest, 0, &mut sink))
            .unwrap_err();
        assert!(matches!(err, RegistryError::NotFound(_)), "got {err:?}");
        // Mismatch errors must never leak partial bytes (D-01 contract).
        assert!(sink.is_empty(), "no bytes must be written on NotFound");
    }

    #[test]
    fn pull_manifest_by_tag_returns_bytes_and_digest() {
        let m = MockRegistry::new();
        let manifest = Bytes::from_static(br#"{"schemaVersion":2}"#);
        let expected_digest =
            m.insert_manifest("docker.io", "library/alpine", "3", manifest.clone());
        let (got, got_digest) = block_on(m.pull_manifest_by_tag(&r())).unwrap();
        assert_eq!(got, manifest);
        assert_eq!(got_digest, expected_digest);
    }

    #[test]
    fn head_blob_false_then_true() {
        let m = MockRegistry::new();
        let data = Bytes::from_static(b"head-test");
        let digest = Digest::from_bytes_sha256(&data);
        assert!(!block_on(m.head_blob("r.io", "x", &digest)).unwrap());
        m.insert_blob("r.io", "x", digest.clone(), data);
        assert!(block_on(m.head_blob("r.io", "x", &digest)).unwrap());
    }

    #[test]
    fn push_blob_stream_round_trip() {
        use futures_util::stream;
        let m = MockRegistry::new();
        let chunk1 = Bytes::from_static(b"hello ");
        let chunk2 = Bytes::from_static(b"world");
        let digest = Digest::from_bytes_sha256(b"hello world");
        let s = stream::iter(vec![Ok(chunk1.clone()), Ok(chunk2.clone())]);
        block_on(m.push_blob_stream("r.io", "x", &digest, 11, s)).unwrap();
        let log = m.pushed_blobs();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].0, "r.io");
        assert_eq!(log[0].1, "x");
        assert_eq!(log[0].2, digest);
        assert_eq!(log[0].3.as_ref(), b"hello world");
    }

    #[test]
    fn push_manifest_records_and_makes_pull_succeed() {
        let m = MockRegistry::new();
        let bytes = Bytes::from_static(br#"{"schemaVersion":2,"artifactType":"x"}"#);
        let r = "registry.io/repo:1".parse::<Reference>().unwrap();
        let pushed = block_on(m.push_manifest(
            &r,
            "application/vnd.oci.image.manifest.v1+json",
            bytes.clone(),
        ))
        .unwrap();
        assert_eq!(pushed, Digest::from_bytes_sha256(&bytes));
        // After push, the same reference resolves on pull.
        let (got, got_digest) = block_on(m.pull_manifest_by_tag(&r)).unwrap();
        assert_eq!(got, bytes);
        assert_eq!(got_digest, pushed);
        // Audit log records the push.
        assert_eq!(m.pushed_manifests().len(), 1);
    }

    #[test]
    fn try_blob_mount_succeeds_when_source_present() {
        let m = MockRegistry::new();
        let data = Bytes::from_static(b"mount-me");
        let digest = Digest::from_bytes_sha256(&data);
        m.insert_blob("r.io", "src/repo", digest.clone(), data.clone());
        let mounted = block_on(m.try_blob_mount("r.io", "dst/repo", "src/repo", &digest)).unwrap();
        assert!(mounted);
        // After mount, the destination repo serves the blob too.
        let mut sink: Vec<u8> = Vec::new();
        block_on(m.pull_blob("r.io", "dst/repo", &digest, data.len() as u64, &mut sink)).unwrap();
        assert_eq!(sink, data.as_ref());
    }

    #[test]
    fn try_blob_mount_returns_false_when_source_absent() {
        let m = MockRegistry::new();
        let digest = Digest::from_bytes_sha256(b"never-stored");
        let mounted = block_on(m.try_blob_mount("r.io", "dst", "src", &digest)).unwrap();
        assert!(!mounted);
    }

    #[test]
    fn pull_blob_detects_digest_mismatch() {
        let m = MockRegistry::new();
        // Insert bytes under the WRONG digest (test helper allows this).
        let real = Bytes::from_static(b"actual-bytes");
        let wrong_digest = Digest::from_bytes_sha256(b"different");
        m.insert_blob("r.io", "x", wrong_digest.clone(), real);
        let mut sink: Vec<u8> = Vec::new();
        let err = block_on(m.pull_blob("r.io", "x", &wrong_digest, 0, &mut sink)).unwrap_err();
        assert!(matches!(err, RegistryError::DigestMismatch { .. }));
        // Mismatch errors must never leak partial bytes (D-01 contract).
        assert!(
            sink.is_empty(),
            "no bytes must be written on DigestMismatch"
        );
    }

    #[test]
    fn registry_is_send_sync() {
        // Compile-time assertion via a function that requires Send + Sync.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MockRegistry>();
        // dyn Registry intentionally is NOT required to be dyn-compatible
        // (native AFIT). Static dispatch only.
    }
}
