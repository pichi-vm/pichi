// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Conservative host network selection from a pinned committed plan.

use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use conglobate::plan::{Directive, Plan, Stage};
use gix::{ObjectId, object::tree::EntryKind};

use crate::cli::BuildArgs;

impl BuildArgs {
    pub(super) fn committed_network(&self, repository: &Path) -> Result<(String, bool)> {
        let repo = gix::open::Options::isolated()
            .open(repository)?
            .to_thread_local();
        let commit = match ObjectId::from_hex(self.git_ref.as_bytes()) {
            Ok(id) => id,
            Err(_) => repo.find_reference(&self.git_ref)?.peel_to_commit()?.id,
        };
        let root = repo
            .verified(commit)?
            .try_into_commit()?
            .tree_id()?
            .detach();
        let pinned = commit.to_string();
        // The guest owns link and parent-component resolution. Keep networking
        // for those cases rather than duplicating its context filesystem here.
        let path = format!("{}/{}", self.context, self.plan);
        if path.split('/').any(|part| part == "..") {
            return Ok((pinned, true));
        }
        let mut parts = path
            .split('/')
            .filter(|part| !matches!(*part, "" | "."))
            .peekable();
        let mut id = root;
        while let Some(part) = parts.next() {
            let tree = repo.verified(id)?.try_into_tree()?;
            let entry = tree.find_entry(part.as_bytes()).with_context(|| {
                format!("committed plan path component {part:?} does not exist")
            })?;
            id = entry.object_id();
            match entry.mode().kind() {
                EntryKind::Link | EntryKind::Commit => return Ok((pinned, true)),
                EntryKind::Tree if parts.peek().is_some() => continue,
                EntryKind::Blob | EntryKind::BlobExecutable if parts.peek().is_none() => {
                    let blob = repo.verified(id)?.try_into_blob()?;
                    let plan = Plan::from_yaml(std::str::from_utf8(&blob.data)?)?;
                    let network = plan.stages.values().any(|stage| {
                        let (Stage::Create { steps, .. } | Stage::Extend { steps, .. }) = stage;
                        steps
                            .iter()
                            .any(|step| matches!(step, Directive::Exec { network: true, .. }))
                    });
                    return Ok((pinned, network));
                }
                _ => bail!("committed plan path does not name a regular file"),
            }
        }
        bail!("committed plan path is empty")
    }
}

trait VerifiedObject {
    fn verified(&self, id: ObjectId) -> Result<gix::Object<'_>>;
}

impl VerifiedObject for gix::Repository {
    fn verified(&self, id: ObjectId) -> Result<gix::Object<'_>> {
        let object = self.find_object(id)?;
        let actual = gix::objs::compute_hash(id.kind(), object.kind, &object.data)?;
        ensure!(
            actual == id,
            "Git object content does not match its identity"
        );
        Ok(object)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{Args, FromArgMatches};
    use std::process::Command;

    const OFFLINE: &str = "version: 1\nimage: registry.test/base:1\n";
    const ONLINE: &str = "version: 1\nimage: registry.test/base:1\nstages:\n  unused:\n    extend: registry.test/base:1\n    steps:\n      - exec:\n          command: 'true'\n          network: true\n";

    struct Repository(tempfile::TempDir);
    impl Repository {
        fn new(format: &str) -> Self {
            let repo = Self(tempfile::tempdir().unwrap());
            repo.git(&[
                "init",
                "-q",
                "-b",
                "main",
                &format!("--object-format={format}"),
            ]);
            repo.write("pichi.yaml", OFFLINE);
            repo.commit();
            repo
        }

        fn git(&self, args: &[&str]) -> String {
            let output = Command::new("git")
                .current_dir(self.0.path())
                .args([
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        }

        fn write(&self, name: &str, text: &str) {
            let path = self.0.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }

        fn commit(&self) -> String {
            self.git(&["add", "."]);
            self.git(&["commit", "-qm", "fixture"]);
            self.git(&["rev-parse", "HEAD"])
        }

        fn inspect(&self, reference: &str, context: &str, plan: &str) -> Result<(String, bool)> {
            let matches = BuildArgs::augment_args(clap::Command::new("build"))
                .try_get_matches_from([
                    "build",
                    ".",
                    "--ref",
                    reference,
                    "--context",
                    context,
                    "--plan",
                    plan,
                ])
                .unwrap();
            BuildArgs::from_arg_matches(&matches)
                .unwrap()
                .committed_network(self.0.path())
        }
    }

    #[test]
    fn pins_commit_and_scans_all_declared_stages() {
        for format in ["sha1", "sha256"] {
            let repo = Repository::new(format);
            let offline = repo.git(&["rev-parse", "HEAD"]);
            repo.write("context/plan.yml", ONLINE);
            let online = repo.commit();
            repo.write("context/plan.yml", "invalid dirty working file");
            assert_eq!(
                repo.inspect("HEAD", "context", "plan.yml").unwrap(),
                (online, true)
            );
            assert_eq!(
                repo.inspect(&offline, ".", "pichi.yaml").unwrap(),
                (offline, false)
            );
            repo.write(
                "pichi.yaml",
                &ONLINE.replace("network: true", "network: false"),
            );
            repo.commit();
            assert!(!repo.inspect("HEAD", ".", "pichi.yaml").unwrap().1);
            repo.write(
                "pichi.yaml",
                &ONLINE.replace("          network: true\n", ""),
            );
            repo.commit();
            assert!(!repo.inspect("HEAD", ".", "pichi.yaml").unwrap().1);
        }
    }

    #[test]
    fn links_modules_and_parent_paths_keep_networking() {
        let repo = Repository::new("sha1");
        let commit = repo.git(&["rev-parse", "HEAD"]);
        // Set committed modes directly; this also tests links on Windows hosts.
        let blob = repo.git(&["rev-parse", "HEAD:pichi.yaml"]);
        repo.git(&[
            "update-index",
            "--add",
            "--cacheinfo",
            "120000",
            &blob,
            "link",
        ]);
        repo.git(&[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000",
            &commit,
            "module",
        ]);
        repo.git(&["commit", "-qm", "links"]);
        for (context, plan) in [
            (".", "link"),
            ("link", "plan.yml"),
            ("module", "plan.yml"),
            ("dir/..", "pichi.yaml"),
        ] {
            assert!(repo.inspect("HEAD", context, plan).unwrap().1);
        }
    }

    #[test]
    fn invalid_committed_input_fails() {
        let repo = Repository::new("sha1");
        assert!(repo.inspect("missing-ref", ".", "pichi.yaml").is_err());
        assert!(repo.inspect("HEAD", ".", "missing.yaml").is_err());
        assert!(repo.inspect("HEAD", "pichi.yaml", "child").is_err());
        repo.write("bad.yaml", "not a plan");
        repo.commit();
        assert!(repo.inspect("HEAD", ".", "bad.yaml").is_err());
    }

    #[test]
    fn rejects_corrupt_commit_tree_and_blob() {
        for spec in ["HEAD", "HEAD^{tree}", "HEAD:pichi.yaml"] {
            let repo = Repository::new("sha1");
            let id = repo.git(&["rev-parse", spec]);
            repo.write("different", "different bytes");
            let other = repo.git(&["hash-object", "-w", "different"]);
            let objects = repo.0.path().join(".git/objects");
            let target = objects.join(&id[..2]).join(&id[2..]);
            std::fs::rename(&target, target.with_extension("original")).unwrap();
            std::fs::copy(objects.join(&other[..2]).join(&other[2..]), target).unwrap();
            assert!(repo.inspect("HEAD", ".", "pichi.yaml").is_err());
        }
    }
}
