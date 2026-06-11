// SPDX-License-Identifier: Apache-2.0

//! Phase 44 Plan 04 Task 2: bridge from the pichi binary's `Config` shape
//! to `pichi-registry`'s [`RegistryEntry`].
//!
//! Lives in the pichi binary (NOT in pichi-registry) to preserve the layering
//! invariant Plan 02 established: `pichi-registry` has no dep on the pichi
//! binary, so the binary owns the boundary translation from its
//! `src/config.rs::RegistryConfig` into `RegistryEntry`.
//!
//! Per B2 (revision): the registry-pull-push CI job runs zot at HTTP-only
//! `localhost:5000` and sets `PICHI_TEST_REGISTRY` + `PICHI_TEST_REGISTRY_INSECURE=1`
//! to bypass the default HTTPS requirement. Production callers do NOT set these
//! env vars; the env-var bridge is a no-op outside CI.

use pichi_registry::{AuthEnv, AuthHint, HttpRegistry, RegistryEntry};

use crate::config::Config;

/// Translate `Config::registries` into `Vec<RegistryEntry>` for
/// [`HttpRegistry::new`]. Honours the `PICHI_TEST_REGISTRY` /
/// `PICHI_TEST_REGISTRY_INSECURE` env-var pair so the CI integration tests
/// can target zot at HTTP-only localhost without requiring a config file in
/// every TempDir fixture.
pub fn build_registry_entries(config: &Config) -> Vec<RegistryEntry> {
    let test_reg = std::env::var("PICHI_TEST_REGISTRY").ok();
    let test_insec = std::env::var("PICHI_TEST_REGISTRY_INSECURE")
        .ok()
        .is_some_and(|v| !v.is_empty());

    let mut entries: Vec<RegistryEntry> = config
        .registries
        .iter()
        .map(|r| {
            let auth_hint = r.auth.as_ref().map(|a| AuthHint {
                username: a.username.clone(),
                password: a.password.clone(),
                identity_token: a.identity_token.clone(),
            });
            // Per-registry insecure flag from config.toml; OR-augmented for the
            // matched prefix when PICHI_TEST_REGISTRY_INSECURE is set.
            let insecure =
                r.insecure || (test_insec && test_reg.as_deref() == Some(r.prefix.as_str()));
            RegistryEntry {
                prefix: r.prefix.clone(),
                insecure,
                auth_hint,
            }
        })
        .collect();

    // B2 (revision): if the user has NOT declared the test registry in
    // config.toml, synthesise an insecure entry so integration tests work
    // without a config file. The HttpRegistry per-registry client cache
    // keys on (prefix, insecure), so injecting an entry here flows directly
    // through Plan 03's ClientProtocol::HttpsExcept.
    if let Some(reg) = test_reg {
        if test_insec && !entries.iter().any(|e| e.prefix == reg) {
            entries.push(RegistryEntry {
                prefix: reg,
                insecure: true,
                auth_hint: None,
            });
        }
    }
    entries
}

/// Construct an [`HttpRegistry`] from `config`. Convenience wrapper used by
/// `cmd::pull::run` so the orchestrator does not need to know about
/// [`AuthEnv::from_process_env`] or the entries translation directly.
pub fn build_http_registry(config: &Config) -> HttpRegistry {
    HttpRegistry::new(build_registry_entries(config), AuthEnv::from_process_env())
}
