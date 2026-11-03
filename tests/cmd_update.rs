// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use conglobate::plan::{Lock, Remote};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::Duration,
};

struct Project(tempfile::TempDir);

impl Project {
    fn new() -> Self {
        let project = Self(tempfile::tempdir().unwrap());
        fs::write(
            project.0.path().join("config.toml"),
            format!(
                "[storage]\ngraphroot = {:?}\n",
                project.cache().to_str().unwrap()
            ),
        )
        .unwrap();
        project
    }

    fn cache(&self) -> PathBuf {
        self.0.path().join("cache")
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pichi"));
        command
            .current_dir(self.0.path())
            .env("PICHI_CONFIG", self.0.path().join("config.toml"))
            .env("XDG_CONFIG_HOME", self.0.path().join("config"))
            // A successful update must not need Git, Conglobate, or dillo.
            .env("PATH", "");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn success(output: Output) -> String {
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    fn import(&self, reference: &str, byte: u8) -> String {
        fs::write(self.0.path().join("input.raw"), vec![byte; 64 * 1024]).unwrap();
        Self::success(self.run(&["import", "raw", "input.raw", "-t", reference]));
        Self::success(self.run(&["inspect", "--format", "{{ digest }}", reference]))
            .trim()
            .into()
    }

    fn plan(&self, text: &str) {
        fs::write(self.0.path().join("pichi.yaml"), text).unwrap();
    }

    fn lock(&self) -> Lock {
        serde_saphyr::from_slice(&fs::read(self.0.path().join("pichi.lock")).unwrap()).unwrap()
    }
}

#[test]
fn working_files_regenerate_the_lock_without_git_or_other_executables() {
    let project = Project::new();
    let a = "registry.test/a:1";
    let b = "registry.test/b:1";
    let first = project.import(a, 1);
    project.import(b, 2);
    project.plan(&format!("version: 1\nstages: {{root: {{extend: {a}, steps: [{{exec: {{command: 'touch executed'}}}}]}}}}\nimage: {{root: root, boot: {b}}}"));
    let summary = Project::success(project.run(&["update", "--pull=never"]));
    assert!(
        summary.contains("2 added, 0 changed, 0 removed"),
        "{summary}"
    );
    assert_eq!(
        project.lock().images[&Remote::new(a.into()).unwrap()].to_string(),
        first
    );
    assert!(!project.0.path().join("executed").exists());

    let second = project.import(a, 3);
    project.plan(&format!("version: 1\nimage: {a}"));
    let summary = Project::success(project.run(&["update", "--pull=never"]));
    assert!(
        summary.contains("0 added, 1 changed, 1 removed"),
        "{summary}"
    );
    assert_eq!(project.lock().images.len(), 1);
    assert_eq!(
        project.lock().images[&Remote::new(a.into()).unwrap()].to_string(),
        second
    );
    let summary = Project::success(project.run(&["update", "--pull=never"]));
    assert!(
        summary.contains("0 added, 0 changed, 0 removed"),
        "{summary}"
    );

    // A malformed prior output must not prevent complete regeneration.
    fs::write(project.0.path().join("pichi.lock"), "old invalid lock").unwrap();
    let summary = Project::success(project.run(&["update", "--pull=never"]));
    assert!(summary.contains("unavailable for comparison"), "{summary}");
    assert_eq!(
        project.lock().images[&Remote::new(a.into()).unwrap()].to_string(),
        second
    );

    let nested = project.0.path().join("nested");
    fs::create_dir(&nested).unwrap();
    fs::write(nested.join("plan.yaml"), format!("version: 1\nimage: {a}")).unwrap();
    Project::success(project.run(&[
        "update",
        "nested",
        "-p",
        "plan.yaml",
        "-l",
        "lock.yaml",
        "--pull=never",
    ]));
    let lock: Lock =
        serde_saphyr::from_slice(&fs::read(nested.join("lock.yaml")).unwrap()).unwrap();
    assert_eq!(lock, project.lock());
}

#[test]
fn errors_preserve_the_lock_and_retired_options_are_rejected() {
    let project = Project::new();
    let before = b"existing lock stays byte-identical\n";
    fs::write(project.0.path().join("pichi.lock"), before).unwrap();
    for plan in [
        "invalid: [",
        "version: 1\nimage: {}",
        "version: 1\nimage: missing",
        "version: 1\nimage: registry.test/missing:1",
        "version: 1\nimage: registry.test/base:1\nimage: registry.test/other:1",
        "version: 1\nimage: x\nstages: {x: {extend: registry.test/base:1, steps: [{exec: {command: ':', network: yes}}]}}",
    ] {
        project.plan(plan);
        let output = project.run(&["update", "--pull=never"]);
        assert!(!output.status.success(), "accepted {plan}");
        assert_eq!(
            fs::read(project.0.path().join("pichi.lock")).unwrap(),
            before
        );
    }
    for flag in ["--ref", "--context", "--conglobate"] {
        let output = project.run(&["update", flag, "unused"]);
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument"));
    }
    let help = Project::success(project.run(&["update", "--help"]));
    for expected in [
        "[DIR]",
        "pichi.yaml",
        "pichi.lock",
        "always",
        "missing",
        "never",
        "newer",
    ] {
        assert!(help.contains(expected), "{help}");
    }
    for retired in ["--ref", "--context", "--conglobate"] {
        assert!(!help.contains(retired), "{help}");
    }
}

#[test]
fn plan_and_lock_paths_stay_in_the_project() {
    let project = Project::new();
    let reference = "registry.test/base:1";
    project.import(reference, 1);
    project.plan(&format!("version: 1\nimage: {reference}"));
    let before = fs::read(project.0.path().join("pichi.yaml")).unwrap();
    for args in [
        vec!["update", "--pull=never", "--plan", "../pichi.yaml"],
        vec!["update", "--pull=never", "--lock", "../pichi.lock"],
        vec!["update", "--pull=never", "--lock", "pichi.yaml"],
        vec!["update", "--pull=never", "--lock", "."],
    ] {
        assert!(!project.run(&args).status.success(), "accepted {args:?}");
    }
    assert_eq!(
        fs::read(project.0.path().join("pichi.yaml")).unwrap(),
        before
    );
    assert!(!project.0.path().join("pichi.lock").exists());

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        use wait_timeout::ChildExt as _;

        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("plan.yaml"), &before).unwrap();
        fs::write(outside.path().join("lock.yaml"), "outside sentinel").unwrap();
        symlink(outside.path(), project.0.path().join("escape")).unwrap();
        symlink(
            outside.path().join("lock.yaml"),
            project.0.path().join("pichi.lock"),
        )
        .unwrap();
        for args in [
            vec!["update", "--pull=never", "--plan", "escape/plan.yaml"],
            vec!["update", "--pull=never", "--lock", "escape/lock.yaml"],
            vec!["update", "--pull=never"],
        ] {
            assert!(!project.run(&args).status.success(), "accepted {args:?}");
        }
        assert_eq!(
            fs::read_to_string(outside.path().join("lock.yaml")).unwrap(),
            "outside sentinel"
        );
        fs::remove_file(project.0.path().join("pichi.lock")).unwrap();
        symlink("pichi.yaml", project.0.path().join("selected.yaml")).unwrap();
        Project::success(project.run(&["update", "--pull=never", "--plan", "selected.yaml"]));
        // Invalid file kinds must fail before any potentially blocking read.
        let fifo = project.0.path().join("fifo");
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        for option in ["--plan", "--lock"] {
            let mut child = project
                .command()
                .args(["update", "--pull=never", option, "fifo"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let status = child.wait_timeout(Duration::from_secs(5)).unwrap();
            if status.is_none() {
                child.kill().unwrap();
                child.wait().unwrap();
            }
            assert!(
                status.is_some_and(|status| !status.success()),
                "blocked reading {option}"
            );
        }
        // No failed publication leaves a temporary lock behind.
        assert!(!fs::read_dir(project.0.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".pichi-lock-")
        }));
    }
}

#[derive(Default)]
struct RegistryState {
    blobs: BTreeMap<String, Vec<u8>>,
    current: String,
    requests: Vec<String>,
    stop: bool,
}

struct Registry {
    address: String,
    state: Arc<Mutex<RegistryState>>,
    thread: Option<JoinHandle<()>>,
}

impl Registry {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let state = Arc::new(Mutex::new(RegistryState::default()));
        let shared = state.clone();
        let thread = thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                if shared.lock().unwrap().stop {
                    break;
                }
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    if stream.read(&mut byte).unwrap_or(0) == 0 {
                        break;
                    }
                    request.push(byte[0]);
                }
                let request = String::from_utf8_lossy(&request);
                let mut parts = request.split_whitespace();
                let method = parts.next().unwrap_or("");
                let path = parts.next().unwrap_or("");
                let mut state = shared.lock().unwrap();
                state.requests.push(path.into());
                let selector = path.rsplit('/').next().unwrap_or("");
                let digest = if selector == "latest" {
                    &state.current
                } else {
                    selector
                };
                let body = if path == "/v2/" {
                    Some(b"{}".as_slice())
                } else {
                    state.blobs.get(digest).map(Vec::as_slice)
                };
                let status = if body.is_some() {
                    "200 OK"
                } else {
                    "404 Not Found"
                };
                let body = body.unwrap_or(b"{}");
                let json: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
                let media = json["mediaType"]
                    .as_str()
                    .unwrap_or("application/octet-stream");
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {media}\r\nContent-Length: {}\r\nDocker-Content-Digest: {digest}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                if stream.write_all(response.as_bytes()).is_ok() && method != "HEAD" {
                    let _ = stream.write_all(body);
                }
            }
        });
        Self {
            address,
            state,
            thread: Some(thread),
        }
    }

    fn publish(&self, cache: &Path, digest: &str) {
        let mut state = self.state.lock().unwrap();
        for entry in fs::read_dir(cache.join("blobs/sha256")).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                state.blobs.insert(
                    format!("sha256:{}", entry.file_name().to_str().unwrap()),
                    fs::read(entry.path()).unwrap(),
                );
            }
        }
        state.current = digest.into();
    }

    fn update(&self, project: &Project, policy: Option<&str>) -> Output {
        let mut command = project.command();
        command
            .env("PICHI_TEST_REGISTRY", &self.address)
            .env("PICHI_TEST_REGISTRY_INSECURE", "1")
            .arg("update");
        if let Some(policy) = policy {
            command.args(["--pull", policy]);
        }
        command.output().unwrap()
    }

    fn requests(&self) -> usize {
        self.state.lock().unwrap().requests.len()
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        self.state.lock().unwrap().stop = true;
        let _ = TcpStream::connect(&self.address);
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
fn pull_policies_refresh_exact_index_pins_and_preserve_locks_on_failure() {
    let registry = Registry::new();
    let project = Project::new();
    let reference = format!("{}/test/base:latest", registry.address);
    let first = project.import(&reference, 1);
    let second = project.import(&format!("{}/test/base:next", registry.address), 2);
    registry.publish(&project.cache(), &second);
    project.plan(&format!("version: 1\nimage: {reference}"));

    Project::success(registry.update(&project, None));
    assert!(registry.requests() > 0, "default must contact registry");
    assert_eq!(
        project.lock().images[&Remote::new(reference.clone()).unwrap()].to_string(),
        second
    );
    registry.publish(&project.cache(), &first);
    let requests = registry.requests();
    for policy in ["missing", "never"] {
        Project::success(registry.update(&project, Some(policy)));
        assert_eq!(registry.requests(), requests);
        assert_eq!(
            project.lock().images[&Remote::new(reference.clone()).unwrap()].to_string(),
            second
        );
    }
    Project::success(registry.update(&project, Some("newer")));
    assert!(registry.requests() > requests);
    assert_eq!(
        project.lock().images[&Remote::new(reference.clone()).unwrap()].to_string(),
        first
    );
    let requests = registry.requests();
    Project::success(registry.update(&project, Some("newer")));
    assert!(registry.requests() > requests);

    Project::success(project.run(&["rmi", &reference]));
    let before = fs::read(project.0.path().join("pichi.lock")).unwrap();
    let requests = registry.requests();
    assert!(!registry.update(&project, Some("never")).status.success());
    assert_eq!(registry.requests(), requests);
    assert_eq!(
        fs::read(project.0.path().join("pichi.lock")).unwrap(),
        before
    );
    Project::success(registry.update(&project, Some("missing")));
    assert!(registry.requests() > requests);
    assert_eq!(
        project.lock().images[&Remote::new(reference.clone()).unwrap()].to_string(),
        first
    );

    // A foreign-platform index must fail before publishing a new lock.
    {
        use sha2::{Digest as _, Sha256};
        let mut state = registry.state.lock().unwrap();
        let mut index: serde_json::Value = serde_json::from_slice(&state.blobs[&first]).unwrap();
        index["manifests"][0]["platform"]["architecture"] = "unsupported".into();
        let bytes = serde_json::to_vec(&index).unwrap();
        let digest = format!("sha256:{:x}", Sha256::digest(&bytes));
        state.blobs.insert(digest.clone(), bytes);
        state.current = digest;
    }
    assert!(!registry.update(&project, None).status.success());
    assert_eq!(
        fs::read(project.0.path().join("pichi.lock")).unwrap(),
        before
    );

    registry.state.lock().unwrap().current = "unavailable".into();
    assert!(!registry.update(&project, None).status.success());
    assert_eq!(
        fs::read(project.0.path().join("pichi.lock")).unwrap(),
        before
    );
}
