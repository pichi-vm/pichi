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
        repo.check_metadata_path(&git)?;
        repo.check_metadata_path(&git.join("modules"))?;
        let alternates = git.join("objects/info/alternates");
        match std::fs::read(&alternates) {
            Ok(bytes) => {
                if bytes
                    .split(|&b| b == b'\n')
                    .any(|line| !line.is_empty() && !line.starts_with(b"#"))
                {
                    bail!(
                        "{} lists Git alternates (borrowed object stores), which Conglobate refuses; run git repack -a -d in the main checkout and remove the alternates file, or use a full clone",
                        alternates.display()
                    );
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| format!("reading {}", alternates.display()));
            }
        }
        Ok(repo)
    }
}

impl BuildRepository {
    fn check_metadata_path(&self, path: &Path) -> Result<()> {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err).with_context(|| format!("inspecting {}", path.display())),
        };
        let canonical = path
            .canonicalize()
            .with_context(|| format!("resolving build metadata {}", path.display()))?;
        // Even an absolute link into this checkout names a host path, not the
        // guest's /run/pichi/repo path. Relative links within the mount work.
        if !canonical.starts_with(&self.path)
            || (metadata.is_symlink() && std::fs::read_link(path)?.is_absolute())
        {
            bail!(
                "{} must resolve inside the repo mount using relative links; build from the main checkout with its Git metadata inside <dir>",
                path.display()
            );
        }
        Ok(())
    }

    /// Conglobate identifies the needed path; the host does not inspect modules.
    pub(super) fn failure_hint(&self) -> String {
        format!(
            "If Conglobate reported an unavailable needed submodule store or commit, stage it yourself from {} with `git submodule update --init --recursive -- <path>` (use the path in the guest error), then retry. Pichi does not fetch submodules.",
            self.path.display()
        )
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

    #[test]
    fn preflight_allows_empty_alternates_but_rejects_entries() {
        let fixture = Fixture::new();
        for bytes in [b"".as_slice(), b"\n# no borrowed objects\n"] {
            fixture.write(".git/objects/info/alternates", bytes);
            fixture.open().unwrap();
        }
        for bytes in [b"/outside/objects\n".as_slice(), b"../borrowed\n", b" \n"] {
            fixture.write(".git/objects/info/alternates", bytes);
            let error = fixture.open().unwrap_err().to_string();
            for expected in ["alternates", "git repack -a -d", "remove", "full clone"] {
                assert!(error.contains(expected), "{error}");
            }
        }
    }

    #[test]
    fn preflight_reports_unreadable_alternates() {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.0.path().join(".git/objects/info/alternates")).unwrap();
        assert!(fixture.open().unwrap_err().to_string().contains("reading"));
    }

    #[cfg(unix)]
    #[test]
    fn preflight_confines_metadata_links_to_mount() {
        use std::os::unix::fs::symlink;

        for metadata in [".git", ".git/modules"] {
            let fixture = Fixture::new();
            let original = fixture.0.path().join(metadata);
            let target = fixture.0.path().join("metadata");
            if original.exists() {
                std::fs::rename(&original, &target).unwrap();
            } else {
                std::fs::create_dir(&target).unwrap();
            }
            let relative = if metadata == ".git" {
                "metadata"
            } else {
                "../metadata"
            };
            symlink(relative, &original).unwrap();
            fixture.open().unwrap();
            std::fs::remove_file(&original).unwrap();
            // Absolute host paths also cannot be followed from the guest.
            symlink(&target, &original).unwrap();
            assert!(
                fixture
                    .open()
                    .unwrap_err()
                    .to_string()
                    .contains("repo mount")
            );
            std::fs::remove_file(&original).unwrap();
            let outside = tempfile::tempdir().unwrap();
            let relative_outside = Path::new("..").join(outside.path().file_name().unwrap());
            let relative_outside = if metadata == ".git" {
                relative_outside
            } else {
                Path::new("..").join(relative_outside)
            };
            symlink(relative_outside, &original).unwrap();
            assert!(
                fixture
                    .open()
                    .unwrap_err()
                    .to_string()
                    .contains("repo mount")
            );
        }
    }

    #[test]
    fn failure_guidance_points_to_the_guest_path() {
        let fixture = Fixture::new();
        let repo = fixture.open().unwrap();
        let hint = repo.failure_hint();
        assert!(hint.contains("git submodule update --init --recursive -- <path>"));
        assert!(hint.contains("path in the guest error"));
        assert!(!hint.contains("Possible unstaged"));
        assert!(!repo.path.join(".git/modules").exists());
    }
}
