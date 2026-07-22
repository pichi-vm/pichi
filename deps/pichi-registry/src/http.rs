// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

// Phase 44 D-01 + D-04 + REGISTRY-04/05/06: production HTTP Registry impl
// wrapping oci-client 0.16. The single trait impl block at the bottom of this
// file binds every D-01 trait method to its oci-client equivalent.
//
// Pitfall 1 (REGISTRY-05): use pull_manifest_raw + pichi-controlled index walk.
//                          NEVER the oci-client auto-resolving manifest API
//                          (the one that runs the internal platform-resolver) —
//                          the literal name is intentionally not written here
//                          so the CI negative-grep gate stays clean.
// Pitfall 6: pichi_artifact::Reference vs oci_client::Reference are different
//            types — convert at the boundary via to_oci_ref helper.
// Pitfall 10: Default Client uses HTTPS; insecure registries need explicit
//            ClientProtocol::HttpsExcept(vec!["host:port"]) per-registry.
// Pitfall 14: OciDistributionError::UnauthorizedError MUST be mapped to a
//            redacted RegistryError::Auth — token strings NEVER bubble through.

#![cfg(feature = "http-client")]

//! Production HTTP [`crate::Registry`] impl wrapping `oci-client 0.16`.
//!
//! Plan 04 instantiates [`HttpRegistry::new`] with a list of [`RegistryEntry`]
//! values translated from `pichi`'s `Config::registries` and an [`AuthEnv`]
//! snapshot. All HTTP/oci-client/error-mapping concerns are concentrated here
//! so the orchestrator (Plan 04 `cmd::pull` / `cmd::push`) is pipeline-
//! composition + policy code only.

use std::collections::HashMap;
use std::sync::Mutex;

use bytes::Bytes;
use futures_util::stream::Stream;
use oci_client::{
    Client, Reference as OciRef, RegistryOperation,
    client::{ClientConfig, ClientProtocol},
    errors::OciDistributionError,
    manifest::OciDescriptor,
    secrets::RegistryAuth,
};
use pichi_artifact::{Digest, Reference as PichiRef};
use tokio::io::AsyncWrite;

use crate::auth::{AuthEnv, AuthHint, resolve_for_registry};
use crate::{Registry, RegistryError, Result};

/// Manifest media types the pichi client will accept from `pull_manifest_raw`.
/// Includes both OCI image manifest + index AND legacy Docker v2 manifest +
/// list shapes, since real-world registries (ghcr.io, quay.io, mirror.gcr.io)
/// still serve Docker types for older tags. The discrimination between
/// "single-manifest" and "multi-arch index" happens at the call site
/// (Plan 04 `cmd::pull`) by inspecting the returned bytes' `mediaType` field.
const ACCEPTED_MANIFEST_TYPES: &[&str] = &[
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
];

/// Per-registry credential hint shipped from the pichi binary's config.
///
/// pichi-registry deliberately does NOT depend on the pichi binary — the
/// binary populates this map at [`HttpRegistry::new`] time by translating its
/// own `Config::registries` into a `Vec<RegistryEntry>`.
#[derive(Debug)]
pub struct RegistryEntry {
    /// Registry hostname (e.g. `"ghcr.io"`). MUST match the
    /// `PichiRef::registry` field exactly for the entry to apply.
    pub prefix: String,
    /// `true` → use plain HTTP for THIS registry (Pitfall 10 escape hatch);
    /// other registries continue to use HTTPS.
    pub insecure: bool,
    /// Static credential hint forwarded to [`resolve_for_registry`] as the
    /// first credential source (D-04 resolution order).
    pub auth_hint: Option<AuthHint>,
}

/// Production HTTP `Registry` impl wrapping `oci-client::Client` (REGISTRY-04).
///
/// Per-registry [`Client`] cache keeps oci-client's internal `TokenCache` warm
/// across repeated calls within one `HttpRegistry` instance — re-fetching a
/// bearer token for every blob HEAD would be a >10x slowdown on push paths.
pub struct HttpRegistry {
    /// Cache of per-registry clients keyed on `(registry, insecure)`. The
    /// `Mutex` guards insertion only; once a [`Client`] is in the map, callers
    /// clone it cheaply (oci-client's Client is `#[derive(Clone)]` with
    /// Arc-wrapped internals).
    clients: Mutex<HashMap<String, Client>>,
    /// Per-registry config keyed on hostname.
    entries: HashMap<String, RegistryEntry>,
    /// Auth resolution environment (XDG/HOME paths). Populated by the binary
    /// from `AuthEnv::from_process_env()`; tests can inject explicit paths.
    auth_env: AuthEnv,
    /// Own reqwest client for streaming blob uploads (oci-client's push API
    /// only accepts an in-memory `Bytes`).
    http: reqwest::Client,
}

// oci-client's `Client` does not implement `Debug`, so the `clients` cache is
// rendered opaque here.
impl std::fmt::Debug for HttpRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpRegistry")
            .field("entries", &self.entries)
            .field("auth_env", &self.auth_env)
            .finish_non_exhaustive()
    }
}

impl HttpRegistry {
    /// Create a new `HttpRegistry` with per-registry config.
    ///
    /// `auth_env` is typically [`AuthEnv::from_process_env`] in production;
    /// tests can construct an [`AuthEnv`] with explicit paths to drive the
    /// D-04 resolution order without reading the operator's real
    /// `$HOME` / `$XDG_CONFIG_HOME`.
    #[must_use]
    pub fn new(entries: Vec<RegistryEntry>, auth_env: AuthEnv) -> Self {
        let map = entries.into_iter().map(|e| (e.prefix.clone(), e)).collect();
        Self {
            clients: Mutex::new(HashMap::new()),
            entries: map,
            auth_env,
            // Own reqwest client for the streaming blob PUT (see
            // `push_blob_stream`). rustls, http/1.1+2; reused across pushes.
            http: reqwest::Client::builder()
                .build()
                .expect("build reqwest client"),
        }
    }

    fn client_for(&self, registry: &str) -> Client {
        let mut cache = self.clients.lock().expect("HttpRegistry::clients poisoned");
        if let Some(c) = cache.get(registry) {
            return c.clone(); // oci_client::Client is Arc-internally — cheap clone.
        }
        let insecure = self
            .entries
            .get(registry)
            .map(|e| e.insecure)
            .unwrap_or(false);
        let protocol = if insecure {
            // Pitfall 10: explicit per-registry insecure escape hatch.
            ClientProtocol::HttpsExcept(vec![registry.to_string()])
        } else {
            ClientProtocol::Https
        };
        let client = Client::new(ClientConfig {
            protocol,
            ..Default::default()
        });
        cache.insert(registry.to_string(), client.clone());
        client
    }

    /// Resolve credentials for `registry`. The resolution reads
    /// `containers-auth.json` / `~/.docker/config.json` from disk, so it runs
    /// on a blocking thread rather than stalling a runtime worker on every
    /// registry call (the previous sync read was a hidden block on the async
    /// path).
    async fn auth_for(&self, registry: &str) -> Result<RegistryAuth> {
        let hint = self.entries.get(registry).and_then(|e| e.auth_hint.clone());
        let auth_env = self.auth_env.clone();
        let registry = registry.to_string();
        // Pitfall 14: any error here may include the registry name but NOT
        // auth values — auth.rs is designed to never inline values; we trust it.
        tokio::task::spawn_blocking(move || {
            resolve_for_registry(&registry, hint.as_ref(), &auth_env)
                .map_err(|e| RegistryError::Auth(format!("auth resolution for {registry}: {e}")))
        })
        .await
        .map_err(|e| RegistryError::Auth(format!("auth resolution task panicked: {e}")))?
    }
}

/// Convert pichi's [`PichiRef`] to oci-client's [`OciRef`] at the boundary
/// (Pitfall 6). Pichi's `Display` canonicalises to `"host/repo:tag"` or
/// `"host/repo@digest"`; oci-client parses the same form.
fn to_oci_ref(r: &PichiRef) -> Result<OciRef> {
    r.to_string().parse().map_err(|e: oci_client::ParseError| {
        RegistryError::Transport(format!("oci ref parse: {e}"))
    })
}

/// Build an [`OciRef`] from raw `(registry, repo, tag-or-digest)` parts. Used
/// by the trait methods that take `(&str, &str, &Digest)` rather than a full
/// [`PichiRef`].
fn build_oci_ref(registry: &str, repo: &str, tag_or_digest: Option<&str>) -> Result<OciRef> {
    let s = match tag_or_digest {
        Some(td) if td.starts_with("sha256:") => format!("{registry}/{repo}@{td}"),
        Some(td) => format!("{registry}/{repo}:{td}"),
        None => format!("{registry}/{repo}"),
    };
    s.parse().map_err(|e: oci_client::ParseError| {
        RegistryError::Transport(format!("oci ref build: {e}"))
    })
}

/// Apply registry credentials to a reqwest request. A bearer token (from
/// oci-client's token-endpoint handshake) takes precedence; otherwise Basic
/// credentials are used, and anonymous requests get no auth header. Never logs
/// the values (Pitfall 14).
fn apply_auth(
    rb: reqwest::RequestBuilder,
    token: &Option<String>,
    auth: &RegistryAuth,
) -> reqwest::RequestBuilder {
    if let Some(t) = token {
        rb.bearer_auth(t)
    } else if let RegistryAuth::Basic(user, pass) = auth {
        rb.basic_auth(user, Some(pass))
    } else {
        rb
    }
}

/// Map oci-client errors into pichi-registry's [`RegistryError`], REDACTING
/// any auth values from the message (Pitfall 14).
///
/// Notable mappings:
/// - `UnauthorizedError { url }` → `Auth("authentication failed for {url}")` —
///   token strings are NEVER included; only the request URL bubbles up.
/// - `RegistryError { envelope, url }` → inspect first error's code; map
///   ManifestUnknown / BlobUnknown to NotFound, Denied / Unauthorized to Auth,
///   otherwise Transport with the code-only summary (envelope detail dropped
///   to keep header echoes from leaking).
/// - `ImageManifestNotFoundError`, `RegistryNoDigestError`,
///   `RegistryNoLocationError` → NotFound / Transport as appropriate.
/// - All other variants fall through to `Transport` with the Display string;
///   oci-client's Display impl on these variants does NOT include credential
///   bytes (verified by inspection of `errors.rs`).
fn map_oci_error(e: OciDistributionError) -> RegistryError {
    use OciDistributionError::{
        AuthenticationFailure, ImageManifestNotFoundError, RegistryError as RegErr,
        UnauthorizedError,
    };
    match e {
        UnauthorizedError { url } => {
            // Redacted: registry URL is fine; token strings are NOT included.
            RegistryError::Auth(format!("authentication failed for {url}"))
        }
        AuthenticationFailure(msg) => {
            // oci-client's AuthenticationFailure carries a message that may
            // describe the failure mode (challenge parse error, missing realm,
            // etc.) but never the credential bytes themselves.
            RegistryError::Auth(format!("authentication failed: {msg}"))
        }
        ImageManifestNotFoundError(msg) => RegistryError::NotFound(msg),
        RegErr { envelope, url } => {
            // Match on error code — both ManifestUnknown and BlobUnknown map
            // to NotFound. Drop envelope detail to avoid leaking any header
            // echo the registry chose to include.
            let code_str = envelope
                .errors
                .first()
                .map(|e| format!("{:?}", e.code))
                .unwrap_or_default();
            if code_str.contains("ManifestUnknown")
                || code_str.contains("BlobUnknown")
                || code_str.contains("ManifestBlobUnknown")
                || code_str.contains("NameUnknown")
                || code_str.contains("NotFound")
            {
                RegistryError::NotFound(format!("{code_str} at {url}"))
            } else if code_str.contains("Denied") || code_str.contains("Unauthorized") {
                // Redacted denial — drop envelope detail (may contain header echoes).
                RegistryError::Auth(format!("denied: {code_str} at {url}"))
            } else {
                RegistryError::Transport(format!("registry error {code_str} at {url}"))
            }
        }
        other => RegistryError::Transport(format!("oci-client: {other}")),
    }
}

impl Registry for HttpRegistry {
    async fn pull_manifest_by_tag(&self, reference: &PichiRef) -> Result<(Bytes, Digest)> {
        let oci_ref = to_oci_ref(reference)?;
        let auth = self.auth_for(&reference.registry).await?;
        let client = self.client_for(&reference.registry);
        // REGISTRY-05 / Pitfall 1: pull_manifest_raw — NOT the auto-resolving
        // variant that runs oci-client's internal platform-resolver. Index
        // discrimination is pichi's job (see `crate::index_walk`).
        let (raw, digest_str) = client
            .pull_manifest_raw(&oci_ref, &auth, ACCEPTED_MANIFEST_TYPES)
            .await
            .map_err(map_oci_error)?;
        let digest: Digest = digest_str
            .parse()
            .map_err(|e| RegistryError::Transport(format!("bad digest from registry: {e}")))?;
        Ok((raw, digest))
    }

    async fn pull_manifest_by_digest(
        &self,
        registry: &str,
        repo: &str,
        digest: &Digest,
    ) -> Result<Bytes> {
        let oci_ref = build_oci_ref(registry, repo, Some(&digest.to_string()))?;
        let auth = self.auth_for(registry).await?;
        let client = self.client_for(registry);
        let (raw, _digest_str) = client
            .pull_manifest_raw(&oci_ref, &auth, ACCEPTED_MANIFEST_TYPES)
            .await
            .map_err(map_oci_error)?;
        Ok(raw)
    }

    async fn pull_blob<W: AsyncWrite + Unpin + Send>(
        &self,
        registry: &str,
        repo: &str,
        digest: &Digest,
        size: u64,
        sink: &mut W,
    ) -> Result<()> {
        let oci_ref = build_oci_ref(registry, repo, None)?;
        let auth = self.auth_for(registry).await?;
        let client = self.client_for(registry);
        // CI-found bug (CI #5 against GHCR): blob endpoints don't auto-resolve
        // creds — must call client.auth() explicitly so the bearer token is
        // cached before the actual blob fetch. Without this, GHCR (and any
        // auth-required registry) returns 401 even when ~/.docker/config.json
        // has valid credentials.
        client
            .auth(&oci_ref, &auth, RegistryOperation::Pull)
            .await
            .map_err(map_oci_error)?;
        let descriptor = OciDescriptor {
            // pull_blob ignores media_type for the descriptor — only digest
            // and (optionally) size are consumed.
            media_type: "application/octet-stream".to_string(),
            digest: digest.to_string(),
            size: i64::try_from(size).unwrap_or(i64::MAX),
            ..Default::default()
        };
        // oci-client verifies sha256(stream) == descriptor.digest internally
        // (RESEARCH §"oci-client API Reference"); Plan 04's pipeline composes
        // a defence-in-depth sha256 in cmd/pull's sink — that's not duplication.
        client
            .pull_blob(&oci_ref, &descriptor, sink)
            .await
            .map_err(map_oci_error)
    }

    async fn head_blob(&self, registry: &str, repo: &str, digest: &Digest) -> Result<bool> {
        let oci_ref = build_oci_ref(registry, repo, None)?;
        let auth = self.auth_for(registry).await?;
        let client = self.client_for(registry);
        // Pre-auth: blob_exists hits a non-manifest endpoint that doesn't
        // auto-resolve creds (CI-found bug, CI #5 GHCR). Push-side HEAD uses
        // RegistryOperation::Push so the cached token has write scope.
        client
            .auth(&oci_ref, &auth, RegistryOperation::Push)
            .await
            .map_err(map_oci_error)?;
        client
            .blob_exists(&oci_ref, &digest.to_string())
            .await
            .map_err(map_oci_error)
    }

    async fn push_manifest(
        &self,
        reference: &PichiRef,
        media_type: &str,
        bytes: Bytes,
    ) -> Result<Digest> {
        let oci_ref = to_oci_ref(reference)?;
        let auth = self.auth_for(&reference.registry).await?;
        let client = self.client_for(&reference.registry);
        // Pre-auth: manifest push needs write-scope bearer token before the PUT.
        client
            .auth(&oci_ref, &auth, RegistryOperation::Push)
            .await
            .map_err(map_oci_error)?;
        // The Content-Type MUST match the payload: an image manifest and an
        // image index have different schemas, and the registry validates the
        // bytes against the schema the Content-Type selects. `pichi manifest
        // push` sends an index; `pichi push` sends a manifest.
        let ct: http::HeaderValue = media_type
            .parse()
            .map_err(|e| RegistryError::Transport(format!("manifest content-type parse: {e}")))?;
        // Compute the locally-expected digest BEFORE moving bytes into push
        // (push consumes the bytes). The registry's location-header URL also
        // contains the assigned digest, but we trust local sha256 for the
        // return value (D-11: CVM-friendly local validation).
        let computed = Digest::from_bytes_sha256(&bytes);
        let _location_url = client
            .push_manifest_raw(&oci_ref, bytes, ct)
            .await
            .map_err(map_oci_error)?;
        Ok(computed)
    }

    async fn push_blob_stream<S>(
        &self,
        registry: &str,
        repo: &str,
        digest: &Digest,
        size: u64,
        stream: S,
    ) -> Result<()>
    where
        S: Stream<Item = std::io::Result<Bytes>> + Send + 'static,
    {
        // oci-client's push API buffers the whole blob in memory. Instead we
        // perform the OCI monolithic upload ourselves — begin a session, then
        // one streaming PUT — so a large layer flows straight from the source
        // stream to the socket in a single request, never residing in memory.
        // oci-client still runs the auth handshake (WWW-Authenticate → token).
        let oci_ref = build_oci_ref(registry, repo, None)?;
        let auth = self.auth_for(registry).await?;
        let token = self
            .client_for(registry)
            .auth(&oci_ref, &auth, RegistryOperation::Push)
            .await
            .map_err(map_oci_error)?;

        let insecure = self
            .entries
            .get(registry)
            .map(|e| e.insecure)
            .unwrap_or(false);
        let scheme = if insecure { "http" } else { "https" };

        // 1. Begin an upload session.
        let uploads = format!("{scheme}://{registry}/v2/{repo}/blobs/uploads/");
        let resp = apply_auth(
            self.http
                .post(&uploads)
                .header(reqwest::header::CONTENT_LENGTH, "0"),
            &token,
            &auth,
        )
        .send()
        .await
        .map_err(|e| RegistryError::Transport(format!("begin upload in {repo}: {e}")))?;
        if resp.status() != reqwest::StatusCode::ACCEPTED {
            let code = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(RegistryError::Transport(format!(
                "begin upload in {repo}: unexpected status {code}: {body}"
            )));
        }
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| RegistryError::Transport("upload session missing Location".into()))?
            .to_string();

        // 2. Resolve the (possibly relative) location and append ?digest=.
        let loc = if location.starts_with("http") {
            location
        } else {
            format!("{scheme}://{registry}{location}")
        };
        let sep = if loc.contains('?') { '&' } else { '?' };
        let put_url = format!(
            "{loc}{sep}digest={}",
            digest.to_string().replace(':', "%3A")
        );

        // 3. Single streaming PUT.
        let body = reqwest::Body::wrap_stream(stream);
        let resp = apply_auth(
            self.http
                .put(&put_url)
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .header(reqwest::header::CONTENT_LENGTH, size)
                .body(body),
            &token,
            &auth,
        )
        .send()
        .await
        .map_err(|e| RegistryError::Transport(format!("upload blob {digest}: {e}")))?;
        if !resp.status().is_success() {
            let code = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(RegistryError::Transport(format!(
                "upload blob {digest}: unexpected status {code}: {body}"
            )));
        }
        Ok(())
    }

    async fn try_blob_mount(
        &self,
        registry: &str,
        target_repo: &str,
        source_repo: &str,
        digest: &Digest,
    ) -> Result<bool> {
        let target = build_oci_ref(registry, target_repo, None)?;
        let source = build_oci_ref(registry, source_repo, None)?;
        let auth = self.auth_for(registry).await?;
        let client = self.client_for(registry);
        // Pre-auth: cross-repo mount is a push operation against the target
        // repo. Auth must be cached before the mount POST.
        client
            .auth(&target, &auth, RegistryOperation::Push)
            .await
            .map_err(map_oci_error)?;
        match client
            .mount_blob(&target, &source, &digest.to_string())
            .await
        {
            Ok(()) => Ok(true),
            // Mount unsupported (405), source blob absent, or registry simply
            // refuses — treat as fall-back signal per the trait contract.
            // Caller (Plan 04 cmd::push) falls back to push_blob_stream.
            Err(_e) => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{build_oci_ref, map_oci_error};
    use crate::RegistryError;
    use oci_client::errors::OciDistributionError;

    #[test]
    fn map_oci_error_redacts_unauthorized() {
        let e = OciDistributionError::UnauthorizedError {
            url: "https://ghcr.io/v2/owner/repo/manifests/v1".into(),
        };
        let mapped = map_oci_error(e);
        match mapped {
            RegistryError::Auth(msg) => {
                assert!(
                    msg.contains("authentication failed for"),
                    "missing redaction phrasing: {msg}"
                );
                assert!(!msg.contains("token"), "unexpected token leak: {msg}");
                assert!(!msg.contains("Bearer"), "unexpected bearer leak: {msg}");
                assert!(
                    msg.contains("https://ghcr.io/v2/owner/repo/manifests/v1"),
                    "missing url in error: {msg}"
                );
            }
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[test]
    fn build_oci_ref_handles_tag_and_digest() {
        let by_tag = build_oci_ref("ghcr.io", "owner/repo", Some("v1.2")).unwrap();
        assert_eq!(by_tag.to_string(), "ghcr.io/owner/repo:v1.2");
        // sha256 digests must be 64 hex per OCI image spec; oci-client's
        // Reference parser rejects short digests with "invalid reference
        // format" (verified by Plan 03 GREEN-debug iteration).
        let full = "sha256:aaaabbbbccccddddeeeeffff00001111aaaabbbbccccddddeeeeffff00001111";
        let by_digest = build_oci_ref("ghcr.io", "owner/repo", Some(full)).unwrap();
        assert!(
            by_digest.to_string().contains(&format!("@{full}")),
            "expected '@{full}' in ref display, got: {by_digest}"
        );
    }
}
