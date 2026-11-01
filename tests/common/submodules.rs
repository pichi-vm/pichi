// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Local Git fixtures for submodules (repositories recorded inside a parent).

use std::fmt::Write as _;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use conglobate::artifact::{Digest, Layer, Manifest};
use pichi::cmd::manifest_ext::ManifestExt as _;
use pichi_storage::{BlobSidecarExt, BlobStore, FilesystemBlobStore};
use wait_timeout::ChildExt;

use super::{DILLO_BIN, PICHI_BIN, SMOKE_SOURCE_REF};

#[derive(Debug)]
pub struct GitRepo {
    pub path: PathBuf,
}

impl GitRepo {
    pub fn new(path: PathBuf) -> Self {
        std::fs::create_dir_all(&path).unwrap();
        let repo = Self { path };
        repo.git(&["init", "-q", "-b", "main"]);
        repo
    }

    pub fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .args([
                "-c",
                "protocol.allow=never",
                "-c",
                "protocol.file.allow=always",
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(&self.path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    pub fn write(&self, path: &str, bytes: &str) {
        let path = self.path.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    pub fn commit(&self) -> String {
        self.git(&["add", "--all"]);
        self.git(&["commit", "-qm", "Commit fixture inputs"]);
        self.git(&["rev-parse", "HEAD"])
    }

    pub fn add_module(&self, source: &Self, name: &str, path: &str) -> Self {
        self.git(&[
            "submodule",
            "add",
            "-q",
            "--name",
            name,
            source.path.to_str().unwrap(),
            path,
        ]);
        Self {
            path: self.path.join(path),
        }
    }
}

/// Shares one appliance pull and source image across four bounded guest boots.
#[derive(Debug)]
pub struct SubmoduleFixture {
    tmp: tempfile::TempDir,
    graphroot: PathBuf,
    pub repo: GitRepo,
}

impl SubmoduleFixture {
    pub async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let graphroot = tmp.path().join("graph");
        std::fs::create_dir_all(&graphroot).unwrap();
        super::prepare_build_image(&graphroot);
        let index = super::seed_smoke_source_carapace(&graphroot, tmp.path()).await;
        let repo = GitRepo::new(tmp.path().join("repo"));
        repo.write(
            "pichi.lock",
            &format!("version: 1\nimages:\n  {SMOKE_SOURCE_REF}: {index}\n"),
        );
        Self {
            tmp,
            graphroot,
            repo,
        }
    }

    pub fn source(&self, name: &str, payload: &str) -> GitRepo {
        let source = GitRepo::new(self.tmp.path().join(name));
        source.write("payload.txt", payload);
        source.commit();
        source
    }

    /// Stage nested stores in .git/modules, Git's local submodule storage.
    /// Every URL is a local fixture path; network protocols are disabled.
    pub fn nested(&self, depth: usize) -> String {
        let mut child = self.source(
            &format!("level-{depth}"),
            &format!("NESTED_DEPTH_{depth}_PAYLOAD"),
        );
        for level in (1..depth).rev() {
            let parent = self.source(
                &format!("level-{level}"),
                &format!("NESTED_DEPTH_{level}_PAYLOAD"),
            );
            parent.add_module(&child, "child", "child");
            parent.commit();
            child = parent;
        }
        self.repo.add_module(&child, "chain", "chain");
        self.repo
            .git(&["submodule", "update", "--init", "--recursive"]);
        std::iter::once("chain")
            .chain(std::iter::repeat_n("child", depth - 1))
            .collect::<Vec<_>>()
            .join("/")
    }

    /// A gitlink is the parent's recorded submodule commit. Add one without
    /// initializing its store, then use it in both unused and used plans.
    pub fn missing(&self, commit: &str) {
        // A deinitialized submodule keeps its empty checkout directory, so a
        // later git add --all retains its gitlink without creating a store.
        std::fs::create_dir_all(self.repo.path.join("absent/input")).unwrap();
        self.repo.git(&[
            "config",
            "-f",
            ".gitmodules",
            "submodule.missing-store.path",
            "absent/input",
        ]);
        self.repo.git(&[
            "config",
            "-f",
            ".gitmodules",
            "submodule.missing-store.url",
            "/unavailable-local-fixture",
        ]);
        self.repo.git(&[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{commit},absent/input"),
        ]);
        assert!(!self.repo.path.join(".git/modules/missing-store").exists());
    }

    pub fn plan(&self, name: &str, sources: &[&str], reject_before_execution: bool) {
        let mut plan =
            format!("version: 1\nstages:\n  rootfs:\n    extend: {SMOKE_SOURCE_REF}\n    steps:\n");
        if reject_before_execution {
            // If execution starts before all COPY inputs are checked, this
            // first RUN fails instead of the specific input error we assert.
            plan.push_str("    - exec:\n        command: exit 97\n");
        }
        for (index, source) in sources.iter().enumerate() {
            // A committed symlink reaches the submodule file through normal
            // context resolution.
            let alias = format!("{}-{index}", name.trim_end_matches(".yaml"));
            std::os::unix::fs::symlink(source, self.repo.path.join(&alias)).unwrap();
            write!(
                plan,
                "    - copy:\n        path: {alias}\n        into: /payload-{index}\n"
            )
            .unwrap();
        }
        plan.push_str("image: rootfs\n");
        self.repo.write(name, &plan);
    }

    pub fn build(&self, plan: &str, tag: &str) -> Output {
        let stdout = self.tmp.path().join(format!("{tag}.out"));
        let stderr = self.tmp.path().join(format!("{tag}.err"));
        let start = Instant::now();
        let mut child = Command::new(PICHI_BIN)
            .process_group(0)
            .env("XDG_DATA_HOME", &self.graphroot)
            .env("PICHI_DILLO", DILLO_BIN)
            .stdin(Stdio::null())
            .args([
                "build",
                self.repo.path.to_str().unwrap(),
                "--ref",
                "HEAD",
                "-p",
                plan,
                "--build-image",
                super::build_image(),
                "-t",
                tag,
                "--memory",
                "2048",
                "--cpus",
                "1",
            ])
            .stdout(std::fs::File::create(&stdout).unwrap())
            .stderr(std::fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap();
        let status = child
            .wait_timeout(Duration::from_secs(180))
            .unwrap()
            .unwrap_or_else(|| {
                let group = nix::unistd::Pid::from_raw(child.id().try_into().unwrap());
                nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL).unwrap();
                child.wait().unwrap();
                panic!(
                    "{plan} timed out:\n{}\n{}",
                    std::fs::read_to_string(&stdout).unwrap(),
                    std::fs::read_to_string(&stderr).unwrap()
                );
            });
        eprintln!(
            "submodule boot {plan}: {status}, {:.2}s",
            start.elapsed().as_secs_f64()
        );
        Output {
            status,
            stdout: std::fs::read(stdout).unwrap(),
            stderr: std::fs::read(stderr).unwrap(),
        }
    }

    pub fn reject(&self, plan: &str, expected: &[&str]) {
        let tag = format!("{}-rejected", plan.trim_end_matches(".yaml"));
        let output = self.build(plan, &tag);
        assert!(!output.status.success(), "accepted {plan}");
        let console = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        for text in expected {
            assert!(
                console.contains(text),
                "{plan} missing {text:?}:\n{console}"
            );
        }
        eprintln!("{plan}: matched input rejection {expected:?}");
        assert!(
            !super::pichi(&self.graphroot, &[], &["inspect", &tag])
                .status
                .success(),
            "published rejected build {plan}"
        );
    }

    /// Check the imported OCI layout (standard container-image folder format)
    /// through its manifest and decoded disk layers, without mounting a disk.
    pub async fn assert_payloads(&self, tag: &str, expected: &[&str], absent: &[&str]) {
        let digest: Digest = super::tag_digest(&self.graphroot, tag).parse().unwrap();
        let store = FilesystemBlobStore::new(self.graphroot.join("pichi/storage"));
        let index = store.get_blob(&digest).await.unwrap();
        let architecture = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => other,
        };
        let manifest_digest: Digest =
            pichi_registry::pick_pichi_entry_from_index(&index, "pichi", architecture)
                .unwrap()
                .parse()
                .unwrap();
        let manifest =
            Manifest::decode_json(&store.get_blob(&manifest_digest).await.unwrap()[..]).unwrap();
        let mut layers = Vec::new();
        for layer in &manifest.layers {
            let path = store.blob_path(&layer.digest_str().parse().unwrap());
            let path = match layer {
                Layer::Scute(_) => path,
                Layer::ScuteZstd(scute) => path.deflated_path(
                    manifest
                        .verity_header(scute, None)
                        .unwrap()
                        .parameters
                        .data_size()
                        .unwrap(),
                ),
                _ => continue,
            };
            layers.push(std::fs::read(path).unwrap());
        }
        for payload in expected {
            assert!(
                layers.iter().any(|bytes| bytes
                    .windows(payload.len())
                    .any(|w| w == payload.as_bytes())),
                "missing committed payload {payload}"
            );
        }
        for payload in absent {
            assert!(
                !layers.iter().any(|bytes| bytes
                    .windows(payload.len())
                    .any(|w| w == payload.as_bytes())),
                "copied working-tree payload {payload}"
            );
        }
    }
}
