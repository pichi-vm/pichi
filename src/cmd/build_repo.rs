// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Host layout checks only; Conglobate decides which committed paths are used.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

#[derive(Debug)]
pub(super) struct BuildRepository {
    pub path: PathBuf,
}

impl TryFrom<&Path> for BuildRepository {
    type Error = anyhow::Error;

    fn try_from(path: &Path) -> Result<Self> {
        let path = path
            .canonicalize()
            .with_context(|| format!("opening build repository {}", path.display()))?;
        let repo = Self { path };
        let git = repo.path.join(".git");
        if git.is_file() {
            bail!(
                "{} is a file: git worktrees, submodule checkouts, and separate Git directories cannot supply history through the repo mount; build from the main checkout instead",
                git.display()
            );
        }
        if !git.is_dir() {
            bail!(
                "{} is not the top of a git checkout (.git directory required); point <dir> at the main checkout's top and use --context for the context directory",
                repo.path.display()
            );
        }
        Ok(repo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(tempfile::TempDir);

    impl Fixture {
        fn new() -> Self {
            let fixture = Self(tempfile::tempdir().unwrap());
            std::fs::create_dir_all(fixture.0.path().join(".git/objects/info")).unwrap();
            fixture
        }

        fn open(&self) -> Result<BuildRepository> {
            BuildRepository::try_from(self.0.path())
        }

        fn write(&self, path: &str, bytes: &[u8]) {
            std::fs::write(self.0.path().join(path), bytes).unwrap();
        }
    }

    #[test]
    fn preflight_accepts_checkout_without_modules() {
        let fixture = Fixture::new();
        fixture.write(
            ".gitmodules",
            b"[submodule \"unused\"]\npath = missing\nurl = never-fetch.invalid\n",
        );
        let repo = fixture.open().unwrap();
        assert_eq!(repo.path, fixture.0.path().canonicalize().unwrap());
        assert!(!repo.path.join(".git/modules").exists());
        assert!(!repo.path.join("missing").exists());
    }

    #[test]
    fn preflight_rejects_git_file() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::write(fixture.path().join(".git"), "gitdir: /elsewhere").unwrap();
        let error = BuildRepository::try_from(fixture.path())
            .unwrap_err()
            .to_string();
        for expected in [
            "is a file",
            "worktrees",
            "submodule checkouts",
            "main checkout",
        ] {
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn preflight_rejects_non_checkout_and_context_directory() {
        let fixture = Fixture::new();
        let context_dir = fixture.0.path().join("plans");
        std::fs::create_dir(&context_dir).unwrap();
        for path in [context_dir.as_path(), tempfile::tempdir().unwrap().path()] {
            let error = BuildRepository::try_from(path).unwrap_err().to_string();
            assert!(error.contains("not the top of a git checkout"), "{error}");
            assert!(error.contains("--context"), "{error}");
        }
    }
}
