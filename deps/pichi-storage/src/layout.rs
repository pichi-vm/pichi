// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Cache path resolution per podman convention. STORAGE-03/04.

use std::ffi::OsString;
use std::path::PathBuf;

#[cfg(unix)]
use anyhow::Context as _;
use anyhow::anyhow;

/// Whether pichi is running as a regular user or as root. Picks between
/// XDG-rooted and `/var/lib`-rooted cache paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// EUID != 0; cache lives under `$XDG_DATA_HOME/pichi/storage`.
    Rootless,
    /// EUID == 0; cache lives under `/var/lib/pichi/storage`.
    Rootful,
}

/// Snapshot of the four environment values that influence path resolution.
/// Populated from the process environment by `EnvSnapshot::from_process()`
/// for production; constructed by hand in unit tests.
#[derive(Debug, Clone, Default)]
pub struct EnvSnapshot {
    /// `XDG_DATA_HOME`.
    pub xdg_data_home: Option<OsString>,
    /// `XDG_RUNTIME_DIR`.
    pub xdg_runtime_dir: Option<OsString>,
    /// `HOME`.
    pub home: Option<OsString>,
    /// EUID (numeric) — only consulted when `XDG_RUNTIME_DIR` is unset and
    /// we need to scope `/tmp/pichi-<uid>/tmp` safely. Unix only.
    pub euid: Option<u32>,
    /// `LOCALAPPDATA` — the per-user app-data root on Windows. Unset (and
    /// unused) on Unix.
    pub local_app_data: Option<OsString>,
}

impl EnvSnapshot {
    /// Capture the current process environment + EUID.
    pub fn from_process() -> Self {
        #[cfg(unix)]
        {
            Self {
                xdg_data_home: std::env::var_os("XDG_DATA_HOME"),
                xdg_runtime_dir: std::env::var_os("XDG_RUNTIME_DIR"),
                home: std::env::var_os("HOME"),
                // The sole remaining use of rustix in this crate. std has no
                // safe effective-uid API (as of std 1.89); drop rustix once
                // one is stabilized. See pichi-storage/Cargo.toml.
                euid: Some(rustix::process::geteuid().as_raw()),
                local_app_data: None,
            }
        }
        #[cfg(windows)]
        {
            Self {
                // Honored as explicit overrides on Windows too (parity with the
                // Unix arm; used by the integration tests and power users).
                // LOCALAPPDATA is the default when these are unset.
                xdg_data_home: std::env::var_os("XDG_DATA_HOME"),
                xdg_runtime_dir: std::env::var_os("XDG_RUNTIME_DIR"),
                home: None,
                euid: None,
                local_app_data: std::env::var_os("LOCALAPPDATA"),
            }
        }
    }
}

/// Resolved cache layout. Constructed once at process start by the binary.
#[derive(Debug, Clone)]
pub struct CacheLayout {
    /// Root of the on-disk cache (blobs/, index.json, oci-layout, locks/).
    pub graphroot: PathBuf,
    /// Root of the runtime tmp area (intermediate state, sockets).
    pub runroot: PathBuf,
    /// Mode under which paths were resolved.
    pub mode: Mode,
}

impl CacheLayout {
    /// Run `op` while holding the cache's `index.json` advisory lock
    /// (`<graphroot>/index.json.lock` — the same path
    /// [`crate::FilesystemTagDb::set_tag`]/`delete_tag` lock against, so calling
    /// those from within `op` in the SAME process would deadlock). Intended to
    /// wrap a multi-step refcount computation that uses only lock-free reads
    /// (`list_tags`/`resolve_tag`/`get_blob`); commit deletions after it
    /// returns. The non-blocking acquire never parks a runtime worker.
    pub async fn with_index_lock<F, Fut, R>(&self, op: F) -> anyhow::Result<R>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<R>>,
    {
        let lock_path = self.graphroot.join("index.json.lock");
        if let Some(parent) = lock_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        // The guard holds the flock across `op().await`; drop releases it.
        let guard = crate::lock::lock_exclusive_async(&lock_path).await?;
        let result = op().await;
        drop(guard);
        result
    }

    /// Resolve cache paths from the live process environment.
    pub fn resolve() -> anyhow::Result<Self> {
        // `is_root` drives podman's rootful/rootless split on Unix; Windows
        // has no effective-uid concept and always uses a per-user store.
        #[cfg(unix)]
        let is_root = rustix::process::geteuid().is_root();
        #[cfg(windows)]
        let is_root = false;
        Self::resolve_with_env(is_root, &EnvSnapshot::from_process())
    }

    /// Resolve cache paths from injected EUID + environment. Used by tests
    /// to drive every branch without mutating process state.
    ///
    /// Follows podman convention: rootful (`euid == 0`) uses the system
    /// store under `/var/lib`; rootless uses XDG-rooted per-user paths.
    #[cfg(unix)]
    pub fn resolve_with_env(is_root: bool, env: &EnvSnapshot) -> anyhow::Result<Self> {
        if is_root {
            return Ok(Self {
                graphroot: PathBuf::from("/var/lib/pichi/storage"),
                runroot: PathBuf::from("/run/pichi/tmp"),
                mode: Mode::Rootful,
            });
        }

        // Rootless: graphroot
        let graphroot = if let Some(xdg) = env.xdg_data_home.as_ref() {
            PathBuf::from(xdg).join("pichi").join("storage")
        } else if let Some(home) = env.home.as_ref() {
            PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("pichi")
                .join("storage")
        } else {
            return Err(anyhow!(
                "cannot determine rootless graphroot: neither XDG_DATA_HOME nor HOME is set"
            ));
        };

        // Rootless: runroot
        let runroot = if let Some(xrd) = env.xdg_runtime_dir.as_ref() {
            PathBuf::from(xrd).join("pichi").join("tmp")
        } else {
            let euid = env.euid.context(
                "cannot determine rootless runroot fallback: XDG_RUNTIME_DIR unset and EUID unknown",
            )?;
            PathBuf::from(format!("/tmp/pichi-{euid}")).join("tmp")
        };

        Ok(Self {
            graphroot,
            runroot,
            mode: Mode::Rootless,
        })
    }

    /// Resolve cache paths on Windows. There is no rootful/rootless split
    /// (no effective-uid concept), so the store is always per-user. The
    /// default roots live under `%LOCALAPPDATA%\pichi`; `XDG_DATA_HOME` /
    /// `XDG_RUNTIME_DIR`, when set, act as explicit overrides (parity with
    /// the Unix rootless arm). The `is_root` argument is ignored.
    #[cfg(windows)]
    pub fn resolve_with_env(_is_root: bool, env: &EnvSnapshot) -> anyhow::Result<Self> {
        let graphroot = if let Some(xdg) = env.xdg_data_home.as_ref() {
            PathBuf::from(xdg).join("pichi").join("storage")
        } else if let Some(local) = env.local_app_data.as_ref() {
            PathBuf::from(local).join("pichi").join("storage")
        } else {
            return Err(anyhow!(
                "cannot determine Windows storage root: neither XDG_DATA_HOME nor LOCALAPPDATA is set"
            ));
        };

        let runroot = if let Some(xrd) = env.xdg_runtime_dir.as_ref() {
            PathBuf::from(xrd).join("pichi").join("run")
        } else if let Some(local) = env.local_app_data.as_ref() {
            PathBuf::from(local).join("pichi").join("run")
        } else {
            return Err(anyhow!(
                "cannot determine Windows runtime root: neither XDG_RUNTIME_DIR nor LOCALAPPDATA is set"
            ));
        };

        Ok(Self {
            graphroot,
            runroot,
            mode: Mode::Rootless,
        })
    }

    /// Path of the blob directory: `<graphroot>/blobs/sha256`.
    pub fn blob_dir(&self) -> PathBuf {
        self.graphroot.join("blobs").join("sha256")
    }

    /// Path of a single blob file: `<graphroot>/blobs/sha256/<hex>`.
    pub fn blob_path(&self, digest: &pichi_artifact::Digest) -> PathBuf {
        self.blob_dir().join(digest.hex())
    }

    /// Path of the OCI Image Layout index file: `<graphroot>/index.json`.
    /// This is the cache's tag → manifest-digest map; consumed by
    /// `FilesystemTagDb`.
    pub fn index_json_path(&self) -> PathBuf {
        self.graphroot.join("index.json")
    }

    /// Path of the OCI Image Layout marker file: `<graphroot>/oci-layout`.
    /// Contains `{"imageLayoutVersion":"1.0.0"}`; written by
    /// `FilesystemTagDb` on first cache use.
    pub fn oci_layout_marker_path(&self) -> PathBuf {
        self.graphroot.join("oci-layout")
    }

    /// Path of the lock-sentinel directory: `<graphroot>/locks`.
    pub fn lock_dir(&self) -> PathBuf {
        self.graphroot.join("locks")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn snap(
        xdg: Option<&str>,
        xrd: Option<&str>,
        home: Option<&str>,
        euid: Option<u32>,
    ) -> EnvSnapshot {
        EnvSnapshot {
            xdg_data_home: xdg.map(OsString::from),
            xdg_runtime_dir: xrd.map(OsString::from),
            home: home.map(OsString::from),
            euid,
            local_app_data: None,
        }
    }

    #[cfg(unix)]
    #[test]
    fn rootful_uses_var_lib() {
        let l = CacheLayout::resolve_with_env(true, &snap(None, None, None, None)).unwrap();
        assert_eq!(l.graphroot, PathBuf::from("/var/lib/pichi/storage"));
        assert_eq!(l.runroot, PathBuf::from("/run/pichi/tmp"));
        assert_eq!(l.mode, Mode::Rootful);
    }

    #[cfg(unix)]
    #[test]
    fn rootless_xdg_data_home_takes_precedence() {
        let l = CacheLayout::resolve_with_env(
            false,
            &snap(
                Some("/tmp/xdg"),
                Some("/run/user/1000"),
                Some("/home/u"),
                Some(1000),
            ),
        )
        .unwrap();
        assert_eq!(l.graphroot, PathBuf::from("/tmp/xdg/pichi/storage"));
        assert_eq!(l.runroot, PathBuf::from("/run/user/1000/pichi/tmp"));
        assert_eq!(l.mode, Mode::Rootless);
    }

    #[cfg(unix)]
    #[test]
    fn rootless_home_fallback() {
        let l = CacheLayout::resolve_with_env(
            false,
            &snap(None, Some("/run/user/1000"), Some("/home/u"), Some(1000)),
        )
        .unwrap();
        assert_eq!(
            l.graphroot,
            PathBuf::from("/home/u/.local/share/pichi/storage")
        );
    }

    #[cfg(unix)]
    #[test]
    fn rootless_runtime_dir_fallback_to_uid_scoped_tmp() {
        let l =
            CacheLayout::resolve_with_env(false, &snap(None, None, Some("/home/u"), Some(1234)))
                .unwrap();
        assert_eq!(l.runroot, PathBuf::from("/tmp/pichi-1234/tmp"));
    }

    #[cfg(unix)]
    #[test]
    fn rootless_no_runtime_dir_no_euid_errors() {
        let err = CacheLayout::resolve_with_env(false, &snap(None, None, Some("/home/u"), None))
            .unwrap_err();
        assert!(
            err.to_string().contains("EUID unknown"),
            "expected 'EUID unknown' in error, got: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rootless_no_xdg_no_home_errors() {
        let err = CacheLayout::resolve_with_env(
            false,
            &snap(None, Some("/run/user/1000"), None, Some(1000)),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("XDG_DATA_HOME"),
            "expected 'XDG_DATA_HOME' in error, got: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn helpers_match_canonical_paths() {
        let l = CacheLayout::resolve_with_env(true, &snap(None, None, None, None)).unwrap();
        assert_eq!(
            l.blob_dir(),
            PathBuf::from("/var/lib/pichi/storage/blobs/sha256")
        );
        assert_eq!(
            l.index_json_path(),
            PathBuf::from("/var/lib/pichi/storage/index.json")
        );
        assert_eq!(
            l.oci_layout_marker_path(),
            PathBuf::from("/var/lib/pichi/storage/oci-layout")
        );
        assert_eq!(l.lock_dir(), PathBuf::from("/var/lib/pichi/storage/locks"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_uses_local_app_data() {
        let env = EnvSnapshot {
            xdg_data_home: None,
            xdg_runtime_dir: None,
            home: None,
            euid: None,
            local_app_data: Some(OsString::from(r"C:\Users\u\AppData\Local")),
        };
        let l = CacheLayout::resolve_with_env(false, &env).unwrap();
        assert_eq!(
            l.graphroot,
            PathBuf::from(r"C:\Users\u\AppData\Local\pichi\storage")
        );
        assert_eq!(
            l.runroot,
            PathBuf::from(r"C:\Users\u\AppData\Local\pichi\run")
        );
        assert_eq!(l.mode, Mode::Rootless);
    }

    #[cfg(windows)]
    #[test]
    fn windows_xdg_data_home_overrides_local_app_data() {
        let env = EnvSnapshot {
            xdg_data_home: Some(OsString::from(r"D:\cache")),
            xdg_runtime_dir: None,
            home: None,
            euid: None,
            local_app_data: Some(OsString::from(r"C:\Users\u\AppData\Local")),
        };
        let l = CacheLayout::resolve_with_env(false, &env).unwrap();
        assert_eq!(l.graphroot, PathBuf::from(r"D:\cache\pichi\storage"));
        // runroot falls back to LOCALAPPDATA when XDG_RUNTIME_DIR is unset.
        assert_eq!(
            l.runroot,
            PathBuf::from(r"C:\Users\u\AppData\Local\pichi\run")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_missing_local_app_data_errors() {
        let env = EnvSnapshot {
            xdg_data_home: None,
            xdg_runtime_dir: None,
            home: None,
            euid: None,
            local_app_data: None,
        };
        let err = CacheLayout::resolve_with_env(false, &env).unwrap_err();
        assert!(
            err.to_string().contains("LOCALAPPDATA"),
            "expected 'LOCALAPPDATA' in error, got: {err}"
        );
    }

    #[test]
    fn resolve_smoke() {
        // Live env — must succeed on any machine where tests run.
        let l = CacheLayout::resolve().unwrap();
        #[cfg(unix)]
        {
            let expected_mode = if rustix::process::geteuid().is_root() {
                Mode::Rootful
            } else {
                Mode::Rootless
            };
            assert_eq!(l.mode, expected_mode);
        }
        #[cfg(windows)]
        {
            assert_eq!(l.mode, Mode::Rootless);
        }
    }
}
