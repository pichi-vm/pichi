// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use super::process::ChildExt as _;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use wait_timeout::ChildExt as _;

/// Real CLI builds in an isolated temporary cache.
pub struct Builds {
    _temporary: tempfile::TempDir,
    pub directory: PathBuf,
    pub cache: PathBuf,
}

impl Builds {
    /// Guest executables omit host debug information, as shipped binaries do.
    /// Git's reduced-trust object reader bounds each allocation to 16 MiB.
    pub fn guest_binary(&self, name: &str, source: &Path) -> PathBuf {
        let path = self.directory.join(name);
        fs::copy(source, &path).unwrap();
        let status = Command::new(
            Path::new("tests/fixtures/native-handoff/strip")
                .with_extension(std::env::consts::EXE_EXTENSION),
        )
        .arg("--strip-debug")
        .arg(&path)
        .status()
        .unwrap();
        assert!(status.success(), "strip guest executable");
        assert!(fs::metadata(&path).unwrap().len() < 16 * 1024 * 1024);
        path
    }

    pub fn new(name: &str) -> Self {
        let temporary = tempfile::Builder::new().prefix(name).tempdir().unwrap();
        #[cfg(windows)]
        {
            // Retained evidence may live beneath a shared runner directory.
            // Protect this new, empty fixture before creating workload state.
            let user = win_custody::current_user_sid_string().unwrap();
            win_custody::SecurityDescriptor::from_sddl(&format!("D:P(A;OICI;FA;;;{user})"))
                .unwrap()
                .apply_to_tree(temporary.path(), true)
                .unwrap();
        }
        let directory = temporary.path().to_owned();
        let cache = directory.join("cache");
        fs::create_dir(&cache).unwrap();
        eprintln!("staged-image evidence: {}", directory.display());
        let fixture = Self {
            _temporary: temporary,
            directory,
            cache,
        };
        super::prepare_build_image(&fixture.cache);
        fixture.cli(&["tag", super::build_image(), "qualification.local/tools:1"]);
        fixture
    }

    pub fn cli(&self, args: &[&str]) -> std::process::Output {
        let output = super::pichi(&self.cache, &[], args);
        super::assert_pichi_ok(args[0], &output);
        output
    }

    pub fn image(&self, tag: &str) -> Image {
        let output = self.cli(&["inspect", tag]);
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        Image {
            manifest: value["manifest"].clone(),
            cache: self.cache.clone(),
        }
    }

    pub fn capture(&self, name: &str, yaml: &str, files: &[(&str, &Path)]) -> PathBuf {
        let repo = self.directory.join(name);
        fs::create_dir(&repo).unwrap();
        fs::write(repo.join("pichi.yaml"), yaml).unwrap();
        for (name, path) in files {
            let destination = repo.join(name);
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            fs::copy(path, destination).unwrap();
        }
        Self::git(&repo, &["init", "-q", "-b", "main"]);
        Self::commit(&repo);
        repo
    }

    pub fn plan(&self, name: &str, yaml: &str, files: &[(&str, &Path)]) -> PathBuf {
        let repo = self.capture(name, yaml, files);
        self.cli(&["update", repo.to_str().unwrap(), "--pull=never"]);
        Self::commit(&repo);
        repo
    }

    fn git(repo: &Path, args: &[&str]) -> std::process::Output {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(output.status.success(), "git failed: {output:?}");
        output
    }

    fn commit(repo: &Path) {
        Self::git(repo, &["add", "."]);
        Self::git(
            repo,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "Capture build inputs",
            ],
        );
    }

    pub fn build(&self, name: &str, yaml: &str, files: &[(&str, &Path)]) -> Image {
        let repo = self.plan(name, yaml, files);
        let tag = format!("qualification.local/{name}:1");
        assert!(
            self.execute(&repo, &tag),
            "build failed: {}",
            repo.join("build.log").display()
        );
        let image = self.image(&tag);
        fs::write(
            repo.join("manifest.json"),
            serde_json::to_vec_pretty(&image.manifest).unwrap(),
        )
        .unwrap();
        if let Some(config) = image.boot_config() {
            fs::write(repo.join("bootconfig"), config.as_bytes()).unwrap();
        }
        image
    }

    pub fn execute(&self, repo: &Path, tag: &str) -> bool {
        let revision = Self::git(repo, &["rev-parse", "HEAD"]);
        let revision = String::from_utf8(revision.stdout).unwrap();
        let log = fs::File::create(repo.join("build.log")).unwrap();
        let mut child = Command::new(super::PICHI_BIN)
            .env("XDG_DATA_HOME", &self.cache)
            .env("PATH", super::dillo_path())
            .args([
                "build",
                repo.to_str().unwrap(),
                "--ref",
                revision.trim(),
                "--build-image",
                super::build_image(),
                "--memory",
                "3584",
                "-t",
                tag,
            ])
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let status = child.wait_timeout(Duration::from_secs(600)).unwrap();
        if status.is_none() {
            child.kill_vm(&self.cache).unwrap();
            panic!("build timed out: {}", repo.join("build.log").display());
        }
        status.unwrap().success()
    }

    pub fn boot(&self, tag: &str, name: &str) -> String {
        let path = self.directory.join(format!("{name}.boot.log"));
        let log = fs::File::create(&path).unwrap();
        let mut child = Command::new(super::PICHI_BIN)
            .env("XDG_DATA_HOME", &self.cache)
            .env("PATH", super::dillo_path())
            .args(["run", tag, "--memory", "2048", "--cpus", "1"])
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let status = child.wait_timeout(Duration::from_secs(180)).unwrap();
        if status.is_none() {
            child.kill_vm(&self.cache).unwrap();
            panic!("boot timed out: {}", path.display());
        }
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            status.unwrap().success(),
            "boot failed: {}\n{text}",
            path.display()
        );
        assert!(!text.contains("Kernel panic"), "{text}");
        assert!(!text.contains("boot failed"), "{text}");
        assert!(
            text.contains("PICHI_ROOT_OK"),
            "root init did not finish: {text}"
        );
        text
    }
}

pub struct Image {
    pub manifest: Value,
    cache: PathBuf,
}

impl Image {
    pub fn layer_path(&self, layer: &Value) -> PathBuf {
        let digest = layer["digest"]
            .as_str()
            .unwrap()
            .strip_prefix("sha256:")
            .unwrap();
        self.cache.join("pichi/storage/blobs/sha256").join(digest)
    }

    pub fn layers(&self, kind: &str) -> Vec<Value> {
        self.manifest["layers"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|layer| layer["mediaType"].as_str().unwrap().contains(kind))
            .cloned()
            .collect()
    }

    pub fn bytes(&self, kind: &str) -> Option<Vec<u8>> {
        let layers = self.layers(kind);
        assert!(layers.len() <= 1);
        layers
            .first()
            .map(|layer| fs::read(self.layer_path(layer)).unwrap())
    }

    pub fn root(&self) -> String {
        let manifest: conglobate::artifact::Manifest =
            serde_json::from_value(self.manifest.clone()).unwrap();
        hex::encode(manifest.carapace_root().unwrap().expect("image has a root"))
    }

    pub fn boot_config(&self) -> Option<String> {
        self.bytes("bootconfig").map(|bytes| {
            use std::io::Read as _;
            let mut decoder = ruzstd::decoding::StreamingDecoder::new(bytes.as_slice()).unwrap();
            let mut text = String::new();
            decoder.read_to_string(&mut text).unwrap();
            text.parse().unwrap()
        })
    }
}
