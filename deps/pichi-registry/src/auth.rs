// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

// Phase 44 D-04 / REGISTRY-06: lazy per-registry auth resolution.
//
// Search order (per podman convention; CITED: containers-auth.json(5)):
//   1. $REGISTRY_AUTH_FILE env var       (deferred — not in v0.8)
//   2. $XDG_RUNTIME_DIR/containers/auth.json
//   3. $XDG_CONFIG_HOME/containers/auth.json (default $HOME/.config/containers/auth.json)
//   4. $HOME/.docker/config.json
//
// Within the resolved file, lookup is by registry hostname. First-hit-wins
// per request — this is LAZY-PER-REGISTRY: parse failure or unsupported
// credstore for an unrelated registry does NOT block anonymous pulls from
// public registries (D-04).
//
// Pitfall 14: bearer-token strings MUST NOT appear in any anyhow::Error or
// std::fmt::Display chain. The map_oci_unauth_error helper (consumed by
// Plan 03's http.rs) is responsible for redacting OciDistributionError;
// THIS module ensures auth values are never inlined into format! strings
// that bubble up — they only flow into the returned RegistryAuth value.

#![cfg(feature = "http-client")]

//! containers-auth.json + ~/.docker/config.json parser with lazy
//! per-registry resolution semantics (Phase 44 D-04 / REGISTRY-06).
//!
//! Returns [`oci_client::secrets::RegistryAuth`] for use by Plan 03's
//! `HttpRegistry`. The parser is feature-gated `#[cfg(feature = "http-client")]`
//! because `RegistryAuth` is foreign to the http-client feature; downstream
//! `MockRegistry` consumers (Phase 43 cmd_import tests) are unaffected.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use oci_client::secrets::RegistryAuth;
use serde::Deserialize;

/// Wire schema for `containers-auth.json` / `~/.docker/config.json`.
#[derive(Debug, Default, Deserialize)]
struct AuthFile {
    #[serde(default)]
    auths: BTreeMap<String, AuthEntry>,
}

/// Single registry entry inside an auth file's `auths` map.
#[derive(Debug, Default, Clone, Deserialize)]
struct AuthEntry {
    /// Base64-encoded "username:password" (basic auth — standard, NOT URL-safe).
    #[serde(default)]
    auth: Option<String>,
    /// OIDC / refresh token (podman convention).
    #[serde(default)]
    identitytoken: Option<String>,
    /// External credential helper — NOT SUPPORTED in v0.8 (REGISTRY-06 loud error).
    #[serde(default, rename = "credsStore")]
    creds_store: Option<String>,
    /// Per-registry helper map — same NOT SUPPORTED treatment.
    #[serde(default, rename = "credHelpers")]
    cred_helpers: Option<BTreeMap<String, String>>,
}

/// pichi's own per-registry static credentials, shipped from the binary's
/// `config.toml` parsing (`src/config.rs::RegistryAuth`). This crate does NOT
/// depend on the pichi binary — the binary translates its `RegistryAuth` into
/// this struct at the boundary (Plan 03/04 wire it).
#[derive(Debug, Default, Clone)]
pub struct AuthHint {
    /// Static username (paired with [`Self::password`] for basic auth).
    pub username: Option<String>,
    /// Static password (paired with [`Self::username`] for basic auth).
    pub password: Option<String>,
    /// Static identity / OIDC token (bearer auth). Takes precedence only when
    /// both [`Self::username`] and [`Self::password`] are unset.
    pub identity_token: Option<String>,
}

impl AuthHint {
    fn to_registry_auth(&self) -> Option<RegistryAuth> {
        if let (Some(u), Some(p)) = (&self.username, &self.password) {
            return Some(RegistryAuth::Basic(u.clone(), p.clone()));
        }
        if let Some(t) = &self.identity_token {
            return Some(RegistryAuth::Bearer(t.clone()));
        }
        None
    }
}

/// Environment paths injected by the caller. Production callers populate from
/// [`std::env::var_os`]; tests pass explicit paths to drive each search-order
/// branch.
#[derive(Debug, Default, Clone)]
pub struct AuthEnv {
    /// `$REGISTRY_AUTH_FILE` — explicit override (deferred; not honored in v0.8
    /// per Plan 02's deferral note, but the field is reserved for forward
    /// compatibility).
    pub registry_auth_file: Option<PathBuf>,
    /// `$XDG_RUNTIME_DIR` (e.g. `/run/user/1000`).
    pub xdg_runtime_dir: Option<PathBuf>,
    /// `$XDG_CONFIG_HOME` (e.g. `~/.config`); falls back to `$HOME/.config`
    /// when unset.
    pub xdg_config_home: Option<PathBuf>,
    /// `$HOME` — used for both the XDG fallback and for `~/.docker/config.json`.
    pub home: Option<PathBuf>,
}

impl AuthEnv {
    /// Snapshot the current process env. Production entry point.
    #[must_use]
    pub fn from_process_env() -> Self {
        Self {
            registry_auth_file: std::env::var_os("REGISTRY_AUTH_FILE").map(PathBuf::from),
            xdg_runtime_dir: std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
            home: std::env::var_os("HOME").map(PathBuf::from),
        }
    }

    fn auth_search_paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::with_capacity(4);
        if let Some(p) = &self.registry_auth_file {
            paths.push(p.clone());
        }
        if let Some(p) = &self.xdg_runtime_dir {
            paths.push(p.join("containers/auth.json"));
        }
        if let Some(p) = &self.xdg_config_home {
            paths.push(p.join("containers/auth.json"));
        } else if let Some(home) = &self.home {
            paths.push(home.join(".config/containers/auth.json"));
        }
        if let Some(home) = &self.home {
            paths.push(home.join(".docker/config.json"));
        }
        paths
    }
}

/// Resolve credentials for `registry` (hostname, e.g. `"ghcr.io"`) per D-04
/// lazy resolution order. Returns [`RegistryAuth::Anonymous`] if no source
/// supplies creds.
///
/// CONTRACT (D-04):
///   - Order: `pichi_hint` → containers-auth.json → ~/.docker/config.json → Anonymous
///   - First-hit-wins per file (a hit in containers-auth.json halts before checking docker config)
///   - `credsStore` / `credHelpers` entries for the TARGET registry → loud error
///   - `credsStore` / `credHelpers` entries for OTHER registries in the same file → silently skipped
///   - Parse errors on a file → propagated with file-path context (NOT silent)
///   - File-not-found → silent skip (NotFound treated as "user has no auth file there")
///
/// # Errors
///
/// Returns an error if an auth file exists but fails to parse, if the target
/// registry's entry uses an unsupported `credsStore` / `credHelpers` helper,
/// or if a basic-auth `auth` field is malformed (not base64, not utf-8, or
/// missing the `user:pass` colon separator). Per Pitfall 14, error messages
/// reference the registry hostname and field name only — never the decoded
/// credential bytes themselves.
impl AuthEnv {
    /// Resolve credentials for `registry`, checking pichi's per-registry hint
    /// first, then the credential-file search paths in order (first file with an
    /// `auths.<registry>` entry wins). Errors reference the registry hostname
    /// and field name only — never the decoded credential bytes.
    pub async fn resolve(
        &self,
        registry: &str,
        pichi_hint: Option<&AuthHint>,
    ) -> Result<RegistryAuth> {
        // 1. pichi's own per-registry config (already loaded by the binary).
        if let Some(hint) = pichi_hint
            && let Some(auth) = hint.to_registry_auth()
        {
            return Ok(auth);
        }

        // 2-4. Walk the search paths in order. Reads are async so no worker is
        // parked on a config read during a registry call.
        for path in self.auth_search_paths() {
            if let Some(entry) = read_entry_for(&path, registry).await? {
                return entry.into_registry_auth(registry);
            }
        }

        Ok(RegistryAuth::Anonymous)
    }
}

/// Read the auth file at `path`, returning `Some(entry)` only if the file
/// exists, parses, and has an entry for `registry`. NotFound → Ok(None);
/// parse errors propagated with path context.
async fn read_entry_for(path: &Path, registry: &str) -> Result<Option<AuthEntry>> {
    let contents = match tokio::fs::read_to_string(path).await {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read auth file {}", path.display())),
    };
    let file: AuthFile = serde_json::from_str(&contents)
        .with_context(|| format!("parse auth file {}", path.display()))?;
    Ok(file.auths.get(registry).cloned())
}

impl AuthEntry {
    /// Extract a [`RegistryAuth`] from this entry, applying D-04's loud-error
    /// rule for `credsStore` / `credHelpers` (the entry has already been keyed
    /// on `registry` by [`read_entry_for`]).
    fn into_registry_auth(&self, registry: &str) -> Result<RegistryAuth> {
        if let Some(store) = &self.creds_store {
            return Err(anyhow!(
                "credsStore \"{store}\" not supported by pichi; configure static credentials \
                 in pichi's config.toml or remove the credsStore entry"
            ));
        }
        if let Some(helpers) = &self.cred_helpers
            && let Some(helper) = helpers.get(registry)
        {
            return Err(anyhow!(
                "credHelpers helper \"{helper}\" for registry \"{registry}\" not supported \
                 by pichi; configure static credentials in pichi's config.toml"
            ));
        }
        if let Some(b64) = &self.auth {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .with_context(|| format!("decode `auth` field for {registry}"))?;
            let s = String::from_utf8(decoded)
                .with_context(|| format!("`auth` field for {registry} is not valid utf-8"))?;
            let (u, p) = s.split_once(':').ok_or_else(|| {
                anyhow!("malformed `auth` for {registry}: expected user:password")
            })?;
            return Ok(RegistryAuth::Basic(u.to_string(), p.to_string()));
        }
        if let Some(t) = &self.identitytoken {
            return Ok(RegistryAuth::Bearer(t.clone()));
        }
        Ok(RegistryAuth::Anonymous)
    }
}

#[cfg(test)]
mod tests {
    use super::{AuthEnv, RegistryAuth};

    #[tokio::test]
    async fn anonymous_when_no_sources() {
        let env = AuthEnv::default();
        let auth = env.resolve("ghcr.io", None).await.unwrap();
        assert!(matches!(auth, RegistryAuth::Anonymous));
    }
}
