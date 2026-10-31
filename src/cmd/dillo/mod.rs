// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Shared VMM lookup for `pichi run` and `pichi build`.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

pub(super) struct Dillo {
    path: PathBuf,
    search: String,
}

impl Dillo {
    // Pichi is the VMM's only user; avoid Fedora's unrelated dillo browser.
    const LIBEXEC_PATH: &str = match option_env!("PICHI_DILLO_DEFAULT") {
        Some(path) => path,
        None => "/usr/libexec/pichi/dillo",
    };
    const EXE_NAME: &str = if cfg!(windows) { "dillo.exe" } else { "dillo" };

    pub(super) fn locate() -> Self {
        Self::resolve(
            std::env::var_os("PICHI_DILLO"),
            Path::new(Self::LIBEXEC_PATH),
            std::env::current_exe().ok().as_deref(),
            std::env::var_os("PATH"),
        )
    }

    fn resolve(
        override_path: Option<OsString>,
        libexec: &Path,
        current_exe: Option<&Path>,
        path: Option<OsString>,
    ) -> Self {
        if let Some(path) = override_path {
            return Self {
                path: path.into(),
                search: "selected by PICHI_DILLO; no fallback is searched".into(),
            };
        }

        let mut search = format!("PICHI_DILLO is unset; searched {}", libexec.display());
        if libexec.is_file() {
            return Self {
                path: libexec.into(),
                search,
            };
        }
        if let Some(dir) = current_exe.and_then(Path::parent) {
            let sibling = dir.join(Self::EXE_NAME);
            write!(search, ", then {} beside pichi", sibling.display()).ok();
            if sibling.is_file() {
                return Self {
                    path: sibling,
                    search,
                };
            }
        } else {
            search.push_str("; pichi's executable directory is unavailable");
        }
        write!(
            search,
            ", then {} on PATH ({})",
            Self::EXE_NAME,
            path.as_deref()
                .map_or_else(|| "unset".into(), |p| p.to_string_lossy())
        )
        .ok();
        Self {
            path: Self::EXE_NAME.into(),
            search,
        }
    }

    pub(super) fn command(&self) -> Command {
        Command::new(&self.path)
    }

    pub(super) fn failure_context(&self) -> String {
        format!(
            "could not launch dillo at {}; {}",
            self.path.display(),
            self.search
        )
    }

    /// Replace pichi on Unix; spawn, wait, and propagate the exit code elsewhere.
    pub(super) fn exec(&self, args: &[String]) -> Result<()> {
        log::info!("exec dillo: {} {}", self.path.display(), args.join(" "));
        let mut cmd = self.command();
        cmd.args(args);

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            Err(cmd.exec()).with_context(|| self.failure_context())
        }
        #[cfg(not(unix))]
        {
            let status = cmd.status().with_context(|| self.failure_context())?;
            std::process::exit(status.code().unwrap_or(1));
        }
    }
}

impl AsRef<Path> for Dillo {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // Free functions are required by Rust's test harness.
    #[test]
    fn override_wins_over_libexec_and_sibling() -> Result<()> {
        let tmp = TempDir::new()?;
        let libexec = tmp.path().join("packaged-dillo");
        let sibling = tmp.path().join(Dillo::EXE_NAME);
        std::fs::write(&libexec, [])?;
        std::fs::write(&sibling, [])?;
        let exe = tmp.path().join("pichi");
        for override_path in ["/custom/dillo", "relative/dillo", "dillo-dev", ""] {
            let dillo = Dillo::resolve(Some(override_path.into()), &libexec, Some(&exe), None);
            assert_eq!(dillo.as_ref(), Path::new(override_path));
            assert!(dillo.failure_context().contains("PICHI_DILLO"));
            assert!(!dillo.failure_context().contains("PATH"));
        }
        // A broken override must not silently select the packaged VMM.
        let missing = tmp.path().join("missing");
        let dillo = Dillo::resolve(Some(missing.as_os_str().into()), &libexec, Some(&exe), None);
        assert_eq!(dillo.as_ref(), missing);
        // User-mode emulation may report a failed exec as a child exit instead.
        assert!(
            !dillo
                .command()
                .status()
                .is_ok_and(|status| status.success())
        );
        Ok(())
    }

    #[test]
    fn libexec_wins_over_sibling_and_path() -> Result<()> {
        let tmp = TempDir::new()?;
        let libexec = tmp.path().join("packaged-dillo");
        let sibling = tmp.path().join(Dillo::EXE_NAME);
        std::fs::write(&libexec, [])?;
        std::fs::write(&sibling, [])?;
        let dillo = Dillo::resolve(
            None,
            &libexec,
            Some(&tmp.path().join("pichi")),
            Some(tmp.path().into()),
        );
        assert_eq!(dillo.as_ref(), libexec);
        Ok(())
    }

    #[test]
    fn sibling_wins_over_path_when_libexec_is_absent() -> Result<()> {
        let tmp = TempDir::new()?;
        let sibling = tmp.path().join(Dillo::EXE_NAME);
        std::fs::write(&sibling, [])?;
        let dillo = Dillo::resolve(
            None,
            &tmp.path().join("missing"),
            Some(&tmp.path().join("pichi")),
            Some(tmp.path().into()),
        );
        assert_eq!(dillo.as_ref(), sibling);
        Ok(())
    }

    #[test]
    fn directories_are_not_candidates_and_missing_error_names_search() -> Result<()> {
        let tmp = TempDir::new()?;
        let sibling = tmp.path().join(Dillo::EXE_NAME);
        std::fs::create_dir(&sibling)?;
        let dillo = Dillo::resolve(
            None,
            tmp.path(),
            Some(&tmp.path().join("pichi")),
            Some(tmp.path().into()),
        );
        assert_eq!(dillo.as_ref(), Path::new(Dillo::EXE_NAME));
        let error = dillo
            .command()
            .env("PATH", tmp.path())
            .status()
            .with_context(|| dillo.failure_context())
            .expect_err("no dillo executable exists");
        let message = error.to_string();
        for place in [
            "PICHI_DILLO is unset",
            &tmp.path().display().to_string(),
            &sibling.display().to_string(),
            "beside pichi",
            "on PATH",
        ] {
            assert!(message.contains(place), "{message}");
        }
        Ok(())
    }

    #[test]
    fn unavailable_executable_directory_and_path_are_reported() -> Result<()> {
        let tmp = TempDir::new()?;
        let dillo = Dillo::resolve(None, &tmp.path().join("missing"), None, None);
        assert_eq!(dillo.as_ref(), Path::new(Dillo::EXE_NAME));
        let message = dillo.failure_context();
        assert!(message.contains("executable directory is unavailable"));
        assert!(message.contains("PATH (unset)"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn path_fallback_launches_developer_binary() -> Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = TempDir::new()?;
        let binary = tmp.path().join(Dillo::EXE_NAME);
        std::fs::write(&binary, b"#!/bin/sh\nprintf developer-dillo\n")?;
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))?;
        let dillo = Dillo::resolve(
            None,
            &tmp.path().join("missing"),
            None,
            Some(tmp.path().into()),
        );
        let output = dillo.command().env("PATH", tmp.path()).output()?;
        assert!(output.status.success());
        assert_eq!(output.stdout, b"developer-dillo");
        Ok(())
    }
}
