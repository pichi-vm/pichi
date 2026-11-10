// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Native all-command acceptance. Explicitly invoked by CI, never silently
//! skipped for a missing registry or hypervisor. Exercises the current commit.

#[path = "common/process.rs"]
mod process;

use process::ChildExt as _;
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;
use wait_timeout::ChildExt as _;

struct Smoke {
    directory: PathBuf,
    binary: PathBuf,
    cache: PathBuf,
}

impl Smoke {
    fn new() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("pichi command smoke ")
            .tempdir_in(
                std::env::var_os("RUNNER_TEMP").unwrap_or_else(|| std::env::temp_dir().into()),
            )
            .unwrap();
        #[cfg(windows)]
        {
            let user = win_custody::current_user_sid_string().unwrap();
            win_custody::SecurityDescriptor::from_sddl(&format!("D:P(A;OICI;FA;;;{user})"))
                .unwrap()
                .apply_to_tree(directory.path(), true)
                .unwrap();
        }
        let directory = directory.keep();
        eprintln!("Smoke evidence: {}", directory.display());
        Self {
            binary: env!("CARGO_BIN_EXE_pichi").into(),
            cache: directory.join("cache"),
            directory,
        }
    }

    fn run(&self, label: &str, args: &[&str]) -> String {
        let path = self.directory.join(format!("{label}.log"));
        let errors = self.directory.join(format!("{label}.stderr.log"));
        let mut command = Command::new(&self.binary);
        command
            .env("XDG_DATA_HOME", &self.cache)
            .args(args)
            .stdin(Stdio::null())
            .stdout(fs::File::create(&path).unwrap())
            .stderr(fs::File::create(&errors).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        let mut child = command.spawn().unwrap();
        let status = child.wait_timeout(Duration::from_secs(300)).unwrap();
        if status.is_none() {
            if self.cache.join("pichi/storage.workloads").exists() {
                child.kill_vm(&self.cache).unwrap();
            } else {
                // Commands can time out before creating a monitor record.
                #[cfg(unix)]
                nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(child.id().try_into().unwrap()),
                    nix::sys::signal::Signal::SIGKILL,
                )
                .unwrap();
                #[cfg(windows)]
                assert!(
                    Command::new("taskkill")
                        .args(["/F", "/T", "/PID", &child.id().to_string()])
                        .status()
                        .unwrap()
                        .success()
                );
                child.wait().unwrap();
            }
            panic!("{label} timed out; see {}", path.display());
        }
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            status.unwrap().success(),
            "{label} failed:\n{text}\n{}",
            fs::read_to_string(&errors).unwrap()
        );
        eprintln!("PASS {label}");
        text
    }

    fn inspect(&self, tag: &str) -> Value {
        serde_json::from_str(&self.run("inspect", &["inspect", tag])).unwrap()
    }

    fn git(&self, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(&self.directory)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
    }

    fn commands(&mut self) {
        let arch = if cfg!(target_arch = "aarch64") {
            "arm64"
        } else {
            "amd64"
        };
        let registry = std::env::var("PICHI_TEST_REGISTRY").expect("local registry is required");
        let tag = format!("{registry}/smoke/image:one");
        let list = format!("{registry}/smoke/list:one");
        eprintln!(
            "Executable: {}\n{}",
            self.binary.display(),
            self.run("version", &["--version"])
        );
        let raw = self.directory.join("raw.img");
        fs::write(&raw, vec![0x5a; 16384]).unwrap();
        self.run(
            "import-raw",
            &[
                "import",
                "raw",
                "--arch",
                arch,
                raw.to_str().unwrap(),
                "-t",
                &tag,
            ],
        );
        let original = self.inspect(&tag);
        self.run("tag", &["tag", &tag, "smoke:alias"]);
        assert_eq!(self.inspect("smoke:alias")["digest"], original["digest"]);
        assert!(
            self.run("images", &["images", "--digests"])
                .contains(original["digest"].as_str().unwrap())
        );
        let pmi = self.directory.join("opaque.pmi");
        fs::write(&pmi, b"opaque import fixture").unwrap();
        self.run(
            "import-pmi",
            &[
                "import",
                "pmi",
                "--arch",
                arch,
                pmi.to_str().unwrap(),
                "-t",
                "smoke:pmi",
            ],
        );
        assert!(
            self.inspect("smoke:pmi")["manifest"]["layers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|layer| layer["mediaType"].as_str().unwrap().contains("pmi"))
        );
        let layout = self.directory.join("layout");
        self.run("save", &["save", &tag, "-o", layout.to_str().unwrap()]);
        self.run(
            "load",
            &["load", layout.to_str().unwrap(), "--tag", "smoke:loaded"],
        );
        assert_eq!(self.inspect("smoke:loaded")["digest"], original["digest"]);
        self.run("push", &["push", &tag]);
        self.run("manifest-create", &["manifest", "create", &list, &tag]);
        self.run(
            "manifest-annotate",
            &[
                "manifest", "annotate", &list, &tag, "--os", "pichi", "--arch", arch,
            ],
        );
        let pushed = self.run("manifest-push", &["manifest", "push", &list, &list]);
        let pushed_digest = pushed
            .lines()
            .find_map(|line| line.strip_prefix("digest: "))
            .unwrap();
        let source_cache = self.cache.clone();
        self.cache = self.directory.join("fresh-cache");
        self.run("pull", &["pull", &tag]);
        assert_eq!(self.inspect(&tag)["digest"], original["digest"]);
        self.run("pull-index", &["pull", &list]);
        let index = self.inspect(&list);
        assert_eq!(index["digest"], pushed_digest);
        assert_eq!(index["entries"][0]["platform"]["architecture"], arch);
        assert_eq!(index["manifest_digest"], original["manifest_digest"]);
        self.run("system-info", &["system", "info", "--json"]);
        self.run("rmi", &["rmi", "--force", &tag, &list]);
        self.run("prune", &["system", "prune"]);
        assert!(
            self.run("images-after-prune", &["images", "-q"])
                .trim()
                .is_empty()
        );
        self.cache = source_cache;
    }

    fn build_and_boot(&self) {
        eprintln!(
            "Appliance: {}",
            self.inspect("qualification.local/builder:smoke")["digest"]
        );
        fs::write(
            self.directory.join("pichi.yaml"),
            include_str!("command_smoke/pichi.yaml"),
        )
        .unwrap();
        fs::write(
            self.directory.join("pichi-smoke.service"),
            include_str!("command_smoke/pichi-smoke.service"),
        )
        .unwrap();
        fs::write(self.directory.join("message.txt"), "PICHI_SMOKE_CONTENT\n").unwrap();
        let repo = self.directory.to_str().unwrap();
        self.run("update", &["update", repo, "--pull=never"]);
        assert!(
            fs::read_to_string(self.directory.join("pichi.lock"))
                .unwrap()
                .contains("sha256:")
        );
        self.git(&["init", "-q", "-b", "main"]);
        self.git(&[
            "add",
            "pichi.yaml",
            "pichi.lock",
            "pichi-smoke.service",
            "message.txt",
        ]);
        self.git(&[
            "-c",
            "user.name=CI",
            "-c",
            "user.email=ci@example.invalid",
            "commit",
            "-qm",
            "Capture smoke inputs",
        ]);
        self.run(
            "build",
            &[
                "build",
                repo,
                "--ref",
                "HEAD",
                "--build-image",
                "qualification.local/builder:smoke",
                "--memory",
                "3584",
                "--cpus",
                "1",
                "-t",
                "smoke:output",
            ],
        );
        let image = self.inspect("smoke:output");
        assert_ne!(
            image["digest"],
            self.inspect("qualification.local/builder:smoke")["digest"]
        );
        let boot = self.run(
            "run",
            &["run", "smoke:output", "--memory", "2048", "--cpus", "1"],
        );
        assert!(
            boot.contains("PICHI_SMOKE_OK"),
            "guest did not verify built content:\n{boot}"
        );
        assert!(!boot.contains("Kernel panic"), "{boot}");
        let workloads = self.run("ps", &["ps"]);
        let id = workloads
            .lines()
            .filter(|line| line.contains(image["digest"].as_str().unwrap()))
            .find_map(|line| uuid::Uuid::parse_str(line.split_whitespace().next()?).ok())
            .expect("ps lists the foreground workload")
            .to_string();
        assert!(!boot.contains(&id), "{boot}");
        let stderr = fs::read_to_string(self.directory.join("run.stderr.log")).unwrap();
        assert!(!stderr.contains(&id), "{stderr}");
        assert!(self.run("logs", &["logs", &id]).contains("PICHI_SMOKE_OK"));
        assert_eq!(self.run("wait", &["wait", &id]).trim(), "0");
        assert_eq!(self.run("stop", &["stop", &id]).trim(), id);
        self.run("attach-exited", &["attach", &id]);
    }
}

#[test]
#[ignore = "requires a native hypervisor, Dillo on PATH, and a local registry"]
fn every_command_builds_and_boots() {
    let mut smoke = Smoke::new();
    smoke.commands();
    smoke.run(
        "pull-appliance",
        &["pull", pichi::cmd::build::DEFAULT_BUILD_IMAGE],
    );
    smoke.run(
        "tag-appliance",
        &[
            "tag",
            pichi::cmd::build::DEFAULT_BUILD_IMAGE,
            "qualification.local/builder:smoke",
        ],
    );
    smoke.build_and_boot();
    fs::remove_dir_all(&smoke.directory).unwrap();
}

#[test]
#[ignore = "requires a native hypervisor, Dillo on PATH, a local registry, and tests/fixtures/appliance-layout"]
fn candidate_appliance_builds_and_boots() {
    let mut smoke = Smoke::new();
    smoke.commands();
    let layout = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/appliance-layout");
    smoke.run(
        "load-appliance",
        &[
            "load",
            layout.to_str().unwrap(),
            "--tag",
            "qualification.local/builder:smoke",
        ],
    );
    smoke.build_and_boot();
    fs::remove_dir_all(&smoke.directory).unwrap();
}

#[test]
fn command_stdout_excludes_diagnostics() {
    let directory = tempfile::tempdir().unwrap();
    let smoke = Smoke {
        directory: directory.path().to_owned(),
        binary: if cfg!(windows) { "cmd.exe" } else { "/bin/sh" }.into(),
        cache: directory.path().join("cache"),
    };
    let arguments: &[&str] = if cfg!(windows) {
        &["/D", "/C", "echo 0&echo monitor warning 1>&2"]
    } else {
        &["-c", "printf '0\\n'; printf 'monitor warning\\n' >&2"]
    };
    assert_eq!(smoke.run("wait", arguments).trim(), "0");
    assert_eq!(
        fs::read_to_string(directory.path().join("wait.stderr.log"))
            .unwrap()
            .trim(),
        "monitor warning",
    );
}
