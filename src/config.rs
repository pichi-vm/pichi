// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Pichi configuration loader. STORAGE-07, STORAGE-08.
//!
//! Precedence (later layers override earlier):
//! 1. compiled-in defaults (`Config::default()`)
//! 2. `/etc/pichi/config.toml`
//! 3. `$XDG_CONFIG_HOME/pichi/config.toml` (or `~/.config/pichi/config.toml`)
//! 4. `PICHI_CONFIG` env var (if set, file MUST exist — silent skip is a footgun for explicit overrides)

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use pichi_registry::{AuthEnv, AuthHint, HttpRegistry, RegistryEntry};
use pichi_storage::CacheLayout;
use serde::{Deserialize, Serialize};

/// Top-level pichi configuration.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct Config {
    /// Default pull policy (`"always"` | `"missing"` | `"never"` | `"newer"`).
    /// Phase 44 (`pichi pull`) consumes this; Phase 41 only loads it.
    #[serde(default)]
    pub default_pull_policy: Option<String>,
    /// Storage path overrides.
    #[serde(default)]
    pub storage: StorageConfig,
    /// Registry list (consumed by Phase 44).
    #[serde(default)]
    pub registries: Vec<RegistryConfig>,
    /// `pichi run` resource defaults.
    #[serde(default)]
    pub run: RunConfig,
}

/// Resource defaults for `pichi run`. CLI flags override these; when both
/// are unset, `pichi run` omits the flag and dillo applies its own default.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct RunConfig {
    /// Default vCPU count.
    pub cpus: Option<u32>,
    /// Default guest memory in MiB.
    pub memory_mib: Option<u32>,
}

/// Storage paths override block.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct StorageConfig {
    /// Override the cache root (`<graphroot>/blobs/...`).
    pub graphroot: Option<PathBuf>,
    /// Override the runtime tmp root.
    pub runroot: Option<PathBuf>,
}

/// Per-registry configuration entry.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct RegistryConfig {
    /// Hostname prefix the entry applies to (e.g. `"ghcr.io"`).
    pub prefix: String,
    /// Static auth credentials (Phase 44 may extend with token-file source).
    #[serde(default)]
    pub auth: Option<RegistryAuth>,
    /// Mirror hostname.
    #[serde(default)]
    pub mirror: Option<String>,
    /// Allow non-TLS connections (testing only).
    #[serde(default)]
    pub insecure: bool,
}

/// Static registry credentials.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct RegistryAuth {
    /// Basic-auth username.
    pub username: Option<String>,
    /// Basic-auth password.
    pub password: Option<String>,
    /// OIDC / bearer identity token.
    pub identity_token: Option<String>,
}

impl Config {
    /// Resolve the on-disk cache layout, applying this config's storage
    /// overrides on top of the podman-convention defaults.
    pub fn resolve_layout(&self) -> Result<CacheLayout> {
        let mut layout = CacheLayout::resolve()?;
        if let Some(p) = &self.storage.graphroot {
            layout.graphroot.clone_from(p);
        }
        if let Some(p) = &self.storage.runroot {
            layout.runroot.clone_from(p);
        }
        Ok(layout)
    }

    /// Translate `self.registries` into `pichi-registry`'s `RegistryEntry`
    /// list. Honours the `PICHI_TEST_REGISTRY` / `PICHI_TEST_REGISTRY_INSECURE`
    /// env-var pair so CI integration tests can target an HTTP-only zot without
    /// a config file; the env bridge is a no-op outside CI.
    pub fn registry_entries(&self) -> Vec<RegistryEntry> {
        let test_reg = std::env::var("PICHI_TEST_REGISTRY").ok();
        let test_insec = std::env::var("PICHI_TEST_REGISTRY_INSECURE")
            .ok()
            .is_some_and(|v| !v.is_empty());

        let mut entries: Vec<RegistryEntry> = self
            .registries
            .iter()
            .map(|r| {
                let auth_hint = r.auth.as_ref().map(|a| AuthHint {
                    username: a.username.clone(),
                    password: a.password.clone(),
                    identity_token: a.identity_token.clone(),
                });
                let insecure =
                    r.insecure || (test_insec && test_reg.as_deref() == Some(r.prefix.as_str()));
                RegistryEntry {
                    prefix: r.prefix.clone(),
                    insecure,
                    auth_hint,
                }
            })
            .collect();

        // If the test registry isn't declared in config, synthesise an insecure
        // entry so integration tests work without a config file.
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

    /// Construct an [`HttpRegistry`] from this config.
    pub fn http_registry(&self) -> HttpRegistry {
        HttpRegistry::new(self.registry_entries(), AuthEnv::from_process_env())
    }

    /// Load with the production 4-tier precedence chain.
    pub fn load() -> Result<Self> {
        let system = system_config_path();
        let user = user_config_path();
        let env_override = std::env::var_os("PICHI_CONFIG").map(PathBuf::from);
        Self::load_from_paths(system.as_deref(), user.as_deref(), env_override.as_deref())
    }

    /// Load with explicit paths. Used by tests to avoid mutating env vars.
    ///
    /// `system` and `user`: silently skipped if NotFound.
    /// `env_override`: NotFound is an error (explicit override should not silently fail).
    pub fn load_from_paths(
        system: Option<&Path>,
        user: Option<&Path>,
        env_override: Option<&Path>,
    ) -> Result<Self> {
        let mut cfg = Self::default();
        if let Some(p) = system {
            if let Some(file_cfg) = read_optional(p)? {
                cfg.merge(file_cfg);
            }
        }
        if let Some(p) = user {
            if let Some(file_cfg) = read_optional(p)? {
                cfg.merge(file_cfg);
            }
        }
        if let Some(p) = env_override {
            let contents = std::fs::read_to_string(p)
                .with_context(|| format!("PICHI_CONFIG file not readable: {}", p.display()))?;
            let file_cfg: Config = toml::from_str(&contents)
                .with_context(|| format!("invalid TOML in PICHI_CONFIG: {}", p.display()))?;
            cfg.merge(file_cfg);
        }
        Ok(cfg)
    }

    fn merge(&mut self, other: Config) {
        if other.default_pull_policy.is_some() {
            self.default_pull_policy = other.default_pull_policy;
        }
        if other.storage.graphroot.is_some() {
            self.storage.graphroot = other.storage.graphroot;
        }
        if other.storage.runroot.is_some() {
            self.storage.runroot = other.storage.runroot;
        }
        // Registry list: later REPLACES earlier (not additive).
        if !other.registries.is_empty() {
            self.registries = other.registries;
        }
        if other.run.cpus.is_some() {
            self.run.cpus = other.run.cpus;
        }
        if other.run.memory_mib.is_some() {
            self.run.memory_mib = other.run.memory_mib;
        }
    }
}

fn read_optional(path: &Path) -> Result<Option<Config>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let parsed: Config = toml::from_str(&contents)
                .with_context(|| format!("invalid TOML in {}", path.display()))?;
            Ok(Some(parsed))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// System-wide config path: `/etc/pichi/config.toml` on Unix,
/// `%PROGRAMDATA%\pichi\config.toml` on Windows. `pub(crate)` so
/// `system info` reports the same path the loader consults.
#[cfg(unix)]
pub(crate) fn system_config_path() -> Option<PathBuf> {
    Some(PathBuf::from("/etc/pichi/config.toml"))
}

#[cfg(windows)]
pub(crate) fn system_config_path() -> Option<PathBuf> {
    std::env::var_os("PROGRAMDATA").map(|p| PathBuf::from(p).join("pichi").join("config.toml"))
}

/// Per-user config path: `$XDG_CONFIG_HOME`/`~/.config` on Unix,
/// `%APPDATA%\pichi\config.toml` on Windows.
#[cfg(unix)]
pub(crate) fn user_config_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("pichi").join("config.toml"));
    }
    std::env::var_os("HOME").map(|h| {
        PathBuf::from(h)
            .join(".config")
            .join("pichi")
            .join("config.toml")
    })
}

#[cfg(windows)]
pub(crate) fn user_config_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|p| PathBuf::from(p).join("pichi").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn all_paths_none_returns_defaults() {
        let cfg = Config::load_from_paths(None, None, None).unwrap();
        assert!(cfg.default_pull_policy.is_none());
        assert!(cfg.storage.graphroot.is_none());
        assert!(cfg.registries.is_empty());
    }

    #[test]
    fn system_only_loaded() {
        let dir = tempfile::TempDir::new().unwrap();
        let sys = write(dir.path(), "sys.toml", r#"default_pull_policy = "always""#);
        let cfg = Config::load_from_paths(Some(&sys), None, None).unwrap();
        assert_eq!(cfg.default_pull_policy.as_deref(), Some("always"));
    }

    #[test]
    fn user_overrides_system() {
        let dir = tempfile::TempDir::new().unwrap();
        let sys = write(dir.path(), "sys.toml", r#"default_pull_policy = "always""#);
        let usr = write(dir.path(), "usr.toml", r#"default_pull_policy = "missing""#);
        let cfg = Config::load_from_paths(Some(&sys), Some(&usr), None).unwrap();
        assert_eq!(cfg.default_pull_policy.as_deref(), Some("missing"));
    }

    #[test]
    fn env_overrides_user() {
        let dir = tempfile::TempDir::new().unwrap();
        let sys = write(dir.path(), "sys.toml", r#"default_pull_policy = "always""#);
        let usr = write(dir.path(), "usr.toml", r#"default_pull_policy = "missing""#);
        let env = write(dir.path(), "env.toml", r#"default_pull_policy = "never""#);
        let cfg = Config::load_from_paths(Some(&sys), Some(&usr), Some(&env)).unwrap();
        assert_eq!(cfg.default_pull_policy.as_deref(), Some("never"));
    }

    #[test]
    fn registries_replaced_not_merged() {
        let dir = tempfile::TempDir::new().unwrap();
        let sys = write(
            dir.path(),
            "sys.toml",
            r#"
                [[registries]]
                prefix = "a.io"
                [[registries]]
                prefix = "b.io"
            "#,
        );
        let usr = write(
            dir.path(),
            "usr.toml",
            r#"
                [[registries]]
                prefix = "c.io"
            "#,
        );
        let cfg = Config::load_from_paths(Some(&sys), Some(&usr), None).unwrap();
        assert_eq!(cfg.registries.len(), 1);
        assert_eq!(cfg.registries[0].prefix, "c.io");
    }

    #[test]
    fn storage_section_round_trip() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = write(
            dir.path(),
            "s.toml",
            r#"
                [storage]
                graphroot = "/data/pichi"
                runroot = "/run/x"
            "#,
        );
        let cfg = Config::load_from_paths(Some(&p), None, None).unwrap();
        assert_eq!(cfg.storage.graphroot, Some(PathBuf::from("/data/pichi")));
        assert_eq!(cfg.storage.runroot, Some(PathBuf::from("/run/x")));
    }

    #[test]
    fn registry_section_round_trip() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = write(
            dir.path(),
            "r.toml",
            r#"
                [[registries]]
                prefix = "ghcr.io"
                insecure = true
                [registries.auth]
                username = "u"
                password = "p"
            "#,
        );
        let cfg = Config::load_from_paths(Some(&p), None, None).unwrap();
        assert_eq!(cfg.registries.len(), 1);
        let r = &cfg.registries[0];
        assert_eq!(r.prefix, "ghcr.io");
        assert!(r.insecure);
        let auth = r.auth.as_ref().unwrap();
        assert_eq!(auth.username.as_deref(), Some("u"));
        assert_eq!(auth.password.as_deref(), Some("p"));
    }

    #[test]
    fn explicit_env_override_missing_file_errors() {
        let dir = tempfile::TempDir::new().unwrap();
        let bogus = dir.path().join("does-not-exist.toml");
        let err = Config::load_from_paths(None, None, Some(&bogus)).unwrap_err();
        assert!(err.to_string().contains("PICHI_CONFIG"));
    }

    #[test]
    fn system_or_user_missing_file_silently_skipped() {
        let dir = tempfile::TempDir::new().unwrap();
        let nope1 = dir.path().join("none1.toml");
        let nope2 = dir.path().join("none2.toml");
        let cfg = Config::load_from_paths(Some(&nope1), Some(&nope2), None).unwrap();
        assert!(cfg.default_pull_policy.is_none());
    }

    #[test]
    fn invalid_toml_errors() {
        let dir = tempfile::TempDir::new().unwrap();
        let bad = write(dir.path(), "bad.toml", "garbage}{not toml{");
        let err = Config::load_from_paths(Some(&bad), None, None).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("invalid toml"));
    }
}
