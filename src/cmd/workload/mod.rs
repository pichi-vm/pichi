// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Pichi owns workload identity and attachment; madrillo owns supervision.

#![forbid(unsafe_code)]

mod client;
mod console;
mod logs;
pub(crate) mod record;

use anyhow::{Context, Result, bail};
use client::Client;
use madrillo::{ClientMessage, Completion, LeaderResult, ServerMessage};
use record::Record;
use std::{
    ffi::OsString,
    fs::File,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug)]
pub(crate) struct Launch<'a> {
    pub(crate) monitor: &'a Path,
    pub(crate) program: &'a Path,
    pub(crate) arguments: &'a [OsString],
    pub(crate) state: &'a Path,
    pub(crate) endpoint: &'a Path,
    pub(crate) input: bool,
    pub(crate) inherited: Vec<File>,
}

#[derive(Debug)]
pub(crate) struct Running {
    client: Client,
    #[cfg(unix)]
    monitor: Box<dyn process_wrap::std::ChildWrapper>,
    #[cfg(windows)]
    monitor: windows_spawn::Child,
}

impl Launch<'_> {
    fn options(&self) -> Vec<OsString> {
        let mut options = vec![
            "--state-dir".into(),
            self.state.into(),
            "--listen".into(),
            self.endpoint.into(),
        ];
        if self.input {
            options.push("--stdin".into());
        }
        options
    }

    fn diagnostic_file(&self) -> Result<File> {
        let path = self
            .state
            .parent()
            .context("workload state needs a parent")?
            .join("monitor.err");
        Ok(File::options().write(true).create_new(true).open(path)?)
    }

    // The Windows implementation awaits native pipe acceptance; callers use
    // one async API on both hosts.
    #[cfg(unix)]
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    pub(crate) async fn spawn(self) -> Result<Running> {
        use command_fds::{CommandFdExt, FdMapping};
        use process_wrap::std::{CommandWrap, ProcessSession};
        use std::{
            os::{
                fd::{AsFd, AsRawFd},
                unix::net::UnixStream,
            },
            process::{Command, Stdio},
        };
        let (parent, initial) = UnixStream::pair()?;
        parent.set_nonblocking(true)?;
        let mut mappings = Vec::new();
        let mut command = Command::new(self.monitor);
        command.args(self.options());
        let mut control = 3_i32;
        for file in &self.inherited {
            let number = file.as_raw_fd();
            if number < 3 {
                bail!("workload inheritance must be distinct from stdio");
            }
            control = control.max(number.checked_add(1).context("descriptor limit")?);
            command.args([OsString::from("--inherit"), number.to_string().into()]);
            mappings.push(FdMapping {
                parent_fd: file.as_fd().try_clone_to_owned()?,
                child_fd: number,
            });
        }
        mappings.push(FdMapping {
            parent_fd: initial.into(),
            child_fd: control,
        });
        command
            .args(["--control", &control.to_string(), "--"])
            .arg(self.program)
            .args(self.arguments)
            .env(
                "RUST_LOG",
                std::env::var_os("RUST_LOG").unwrap_or_else(|| "warn".into()),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(self.diagnostic_file()?);
        command.fd_mappings(mappings)?;
        // A fresh session prevents the caller's terminal hangup/process-group
        // signals from owning monitor lifetime. Drop only detaches the handle.
        let monitor = CommandWrap::from(command)
            .wrap(ProcessSession)
            .spawn()
            .with_context(|| format!("start monitor {}", self.monitor.display()))?;
        let stream = interprocess::os::unix::uds_local_socket::tokio::Stream::try_from(
            std::os::fd::OwnedFd::from(parent),
        )?;
        Ok(Running {
            client: Client::new(stream.into()),
            monitor,
        })
    }

    #[cfg(windows)]
    pub(crate) async fn spawn(self) -> Result<Running> {
        use interprocess::os::windows::named_pipe::{
            DuplexPipeStream, PipeListenerOptions, pipe_mode::Bytes,
        };
        use windows_spawn::{Command, CreationFlags, SpawnOptions, Stdio};
        if !self.inherited.is_empty() {
            bail!("madrillo's exact workload handle inheritance is not yet available on Windows");
        }
        let name = format!(r"\\.\pipe\pichi-initial-{}", uuid::Uuid::new_v4());
        let path = widestring::U16CString::from_str(&name)?;
        let sid = win_custody::current_user_sid_string()?;
        let access = widestring::U16CString::from_str(format!("D:P(A;;GA;;;{sid})"))?;
        let descriptor =
            interprocess::os::windows::security_descriptor::SecurityDescriptor::deserialize(
                &access,
            )?;
        let listener = PipeListenerOptions::new()
            .path(path.as_ucstr())
            .security_descriptor(Some(descriptor))
            .input_buffer_size_hint(64 * 1024)
            .output_buffer_size_hint(64 * 1024)
            .create_tokio::<Bytes, Bytes>()?;
        let initial = DuplexPipeStream::<Bytes>::connect_by_path(name.as_str())?;
        let parent: interprocess::os::windows::named_pipe::local_socket::tokio::Stream =
            listener.accept().await?.into();
        let mut command = Command::new(self.monitor);
        command
            .args(self.options())
            .arg("--control")
            .arg_handle(&initial)?
            .arg("--")
            .arg(self.program)
            .args(self.arguments)
            .env(
                "RUST_LOG",
                std::env::var_os("RUST_LOG").unwrap_or_else(|| "warn".into()),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(self.diagnostic_file()?);
        let options = SpawnOptions::new().creation_flags(
            CreationFlags::DETACHED_PROCESS
                | CreationFlags::NEW_PROCESS_GROUP
                | CreationFlags::BREAKAWAY_FROM_JOB,
        );
        let monitor = command
            .spawn_with(options)
            .with_context(|| format!("start independent monitor {}", self.monitor.display()))?;
        drop(command);
        drop(initial);
        Ok(Running {
            client: Client::new(parent.into()),
            monitor,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use madrillo::LeaderResult;

    struct ReapMonitor(Running);

    impl std::ops::Deref for ReapMonitor {
        type Target = Running;
        fn deref(&self) -> &Running {
            &self.0
        }
    }

    impl std::ops::DerefMut for ReapMonitor {
        fn deref_mut(&mut self) -> &mut Running {
            &mut self.0
        }
    }

    impl Drop for ReapMonitor {
        fn drop(&mut self) {
            // Own the monitor until reaped, including assertion/error paths.
            // Killing it closes the echo fixture's sole console writer.
            #[cfg(unix)]
            let monitor = self.0.monitor.inner_mut();
            #[cfg(windows)]
            let monitor = &mut self.0.monitor;
            if matches!(monitor.try_wait(), Ok(None)) {
                let _ = monitor.kill();
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while matches!(monitor.try_wait(), Ok(None)) && std::time::Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    #[tokio::test]
    async fn stop_without_monitor_or_result_preserves_unknown_state_and_pins() -> Result<()> {
        let root = tempfile::tempdir()?;
        let graphroot = root.path().join("cache");
        std::fs::create_dir(&graphroot)?;
        let records = record::Records::open(&graphroot)?;
        let id = uuid::Uuid::new_v4();
        #[cfg(unix)]
        let endpoint = root.path().join("missing.sock");
        #[cfg(windows)]
        let endpoint = PathBuf::from(format!(r"\\.\pipe\pichi-missing-{id}"));
        let image = format!("sha256:{}", "0".repeat(64)).parse()?;
        let workload = Workload(records.create(vec![image], endpoint, id)?);
        let error = workload.stop(Duration::ZERO).await.unwrap_err();
        assert!(error.to_string().contains("workload state is unknown"));
        assert!(workload.0.completion()?.is_none());
        assert_eq!(records.pins()?, workload.0.images());
        Ok(())
    }

    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "requires MADRILLO_BIN and a containing Job that forbids breakaway"]
    async fn denied_job_breakaway_keeps_workload_unknown() -> Result<()> {
        let monitor = PathBuf::from(std::env::var_os("MADRILLO_BIN").context("set MADRILLO_BIN")?);
        let fixture = monitor.with_file_name("madrillo-fixture.exe");
        let root = tempfile::tempdir()?;
        let graphroot = root.path().join("cache");
        std::fs::create_dir(&graphroot)?;
        let records = record::Records::open(&graphroot)?;
        let id = uuid::Uuid::new_v4();
        let endpoint = PathBuf::from(format!(r"\\.\pipe\pichi-denied-{id}"));
        let image = format!("sha256:{}", "0".repeat(64)).parse()?;
        let record = records.create(vec![image], endpoint.clone(), id)?;
        let attempt = Launch {
            monitor: &monitor,
            program: &fixture,
            arguments: &["echo".into()],
            state: &record.state(),
            endpoint: &endpoint,
            input: true,
            inherited: Vec::new(),
        }
        .spawn()
        .await;
        let error = match attempt {
            Err(error) => error,
            Ok(running) => {
                drop(ReapMonitor(running));
                bail!("test requires a Job that forbids breakaway");
            }
        };
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .and_then(std::io::Error::raw_os_error),
            Some(5),
            "{error:#}"
        );
        assert!(record.completion()?.is_none());
        assert!(
            !record.state().exists(),
            "denied creation must not run a monitor"
        );
        assert_eq!(records.pins()?, record.images());
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires MADRILLO_BIN and its qualification fixture; run explicitly"]
    async fn native_monitor_launch_and_async_control() -> Result<()> {
        let monitor = PathBuf::from(std::env::var_os("MADRILLO_BIN").context("set MADRILLO_BIN")?);
        let fixture = monitor.with_file_name(if cfg!(windows) {
            "madrillo-fixture.exe"
        } else {
            "madrillo-fixture"
        });
        let root = tempfile::tempdir()?;
        #[cfg(unix)]
        let endpoint = root.path().join("control.sock");
        #[cfg(windows)]
        let endpoint = PathBuf::from(format!(r"\\.\pipe\pichi-test-{}", uuid::Uuid::new_v4()));
        let graphroot = root.path().join("cache");
        std::fs::create_dir(&graphroot)?;
        let records = record::Records::open(&graphroot)?;
        let image = format!("sha256:{}", "0".repeat(64)).parse()?;
        let record = records.create(vec![image], endpoint.clone(), uuid::Uuid::new_v4())?;
        let state = record.state();
        assert_eq!(records.pins()?, record.images());
        assert_eq!(records.get(record.id())?.endpoint(), endpoint);
        let mut running = ReapMonitor(
            Launch {
                monitor: &monitor,
                program: &fixture,
                arguments: &["echo".into()],
                state: &state,
                endpoint: &endpoint,
                input: true,
                inherited: Vec::new(),
            }
            .spawn()
            .await?,
        );
        running.started().await?;
        #[cfg(unix)]
        {
            use nix::unistd::{Pid, getsid};
            assert_eq!(
                getsid(Some(Pid::from_raw(i32::try_from(running.monitor.id())?)))?.as_raw(),
                i32::try_from(running.monitor.id())?
            );
        }
        let payload = b"pichi console\0\xff\n";
        running
            .client
            .send(ClientMessage::Input {
                bytes: payload.to_vec(),
            })
            .await?;
        let reply = tokio::time::timeout(Duration::from_secs(5), running.client.next()).await??;
        assert!(matches!(reply, ServerMessage::Output { bytes, .. } if bytes == payload));
        running.client.send(ClientMessage::InputEnd).await?;
        let mut attached = Client::connect(&endpoint).await?;
        attached
            .send(ClientMessage::Attach {
                input: true,
                streams: Vec::new(),
            })
            .await?;
        assert_eq!(
            attached.next().await?,
            ServerMessage::Attached { input: true }
        );
        // Administrative shutdown must work while this attachment still owns input.
        let workload = Workload(records.get(record.id())?);
        let stopped = workload.stop(Duration::from_secs(5)).await?;
        assert_eq!(stopped.0.leader, LeaderResult::Exit { code: 0 });
        assert_eq!(stopped.0.cause, madrillo::CompletionCause::LeaderExit);
        let reply = tokio::time::timeout(Duration::from_secs(5), attached.next()).await??;
        assert!(
            matches!(reply, ServerMessage::Completed { result } if result.leader == (LeaderResult::Exit { code: 0 }))
        );
        // Reap the caller-owned monitor handle after its retained completion.
        #[cfg(unix)]
        let status = running.monitor.inner_mut().wait()?;
        #[cfg(windows)]
        let status = running.monitor.wait()?;
        assert!(status.success());
        let completion = record
            .completion()?
            .context("missing retained completion")?;
        assert_eq!(completion.leader, LeaderResult::Exit { code: 0 });
        // After the monitor has exited, repeated stop reads the durable result.
        assert_eq!(workload.stop(Duration::ZERO).await?.0, completion);
        if completion.cleanup == madrillo::CleanupResult::Complete {
            assert_eq!(records.pins()?, Vec::new());
        } else {
            assert_eq!(records.pins()?, record.images());
        }
        drop(attached);
        drop(running);
        // Exercise forced stop through the same native transport on every host.
        for grace in [Duration::from_millis(50), Duration::ZERO] {
            let workload = Workload(records.create(
                record.images().to_vec(),
                endpoint.clone(),
                uuid::Uuid::new_v4(),
            )?);
            let mut running = ReapMonitor(
                Launch {
                    monitor: &monitor,
                    program: &fixture,
                    arguments: &["sleep".into()],
                    state: &workload.0.state(),
                    endpoint: &endpoint,
                    input: true,
                    inherited: Vec::new(),
                }
                .spawn()
                .await?,
            );
            running.started().await?;
            let started = std::time::Instant::now();
            let result = workload.stop(grace).await?.0;
            assert!(started.elapsed() >= grace);
            assert_eq!(result.cause, madrillo::CompletionCause::Forced);
            #[cfg(unix)]
            assert!(matches!(
                result.leader,
                LeaderResult::Signal { signal: 9, .. }
            ));
            #[cfg(windows)]
            assert!(matches!(result.leader, LeaderResult::Exit { code } if code != 0));
            assert_eq!(workload.0.completion()?, Some(result));
            #[cfg(unix)]
            let status = running.monitor.inner_mut().wait()?;
            #[cfg(windows)]
            let status = running.monitor.wait()?;
            assert!(status.success());
        }
        Ok(())
    }
}

impl Running {
    /// Both run and build pass the same opaque dillo arguments/resources here.
    pub(crate) async fn start(
        record: &Record,
        arguments: &[OsString],
        inherited: Vec<File>,
        input: bool,
    ) -> Result<Self> {
        let mut running = Launch {
            monitor: &Self::executable(),
            program: &crate::cmd::run::find_dillo(),
            arguments,
            state: &record.state(),
            endpoint: record.endpoint(),
            input,
            inherited,
        }
        .spawn()
        .await
        .with_context(|| {
            format!(
                "launch workload {}; its retained state is unknown",
                record.id()
            )
        })?;
        running.started().await?;
        Ok(running)
    }

    pub(crate) async fn view(&mut self, interactive: bool) -> Result<u32> {
        console::Console::open(interactive)?
            .view(&mut self.client)
            .await
    }

    pub(crate) async fn wait(&mut self, capture: Option<&File>) -> Result<Completion> {
        self.client
            .wait_with(|stream, bytes| {
                if let Some(mut file) = capture {
                    use std::io::Write as _;
                    file.write_all(bytes)?;
                    Ok(())
                } else {
                    console::Console::write(stream, bytes)
                }
            })
            .await
    }

    pub(crate) async fn terminate(&mut self, record: &Record) -> Result<Completion> {
        if let Some(result) = record.completion()? {
            return Ok(result);
        }
        match self.client.terminate().await {
            Ok(result) => Ok(result),
            Err(error) => record.completion()?.ok_or(error),
        }
    }

    pub(crate) async fn started(&mut self) -> Result<u32> {
        tokio::time::timeout(Duration::from_secs(10), async {
            match self.client.next().await? {
                ServerMessage::Prepared { monitor_pid } if monitor_pid == self.monitor.id() => {}
                response => bail!("unexpected monitor preparation: {response:?}"),
            }
            match self.client.next().await? {
                ServerMessage::Started { leader_pid } => Ok(leader_pid),
                response => bail!("workload did not start: {response:?}"),
            }
        })
        .await
        .context("monitor launch deadline")?
    }

    fn executable() -> PathBuf {
        if let Some(path) = std::env::var_os("PICHI_MADRILLO") {
            return path.into();
        }
        let name = if cfg!(windows) {
            "madrillo.exe"
        } else {
            "madrillo"
        };
        if let Ok(current) = std::env::current_exe()
            && let Some(parent) = current.parent()
        {
            let path = parent.join(name);
            if path.is_file() {
                return path;
            }
        }
        name.into()
    }
}

impl Client {
    async fn terminate(&mut self) -> Result<Completion> {
        tokio::time::timeout(Duration::from_secs(30), async {
            self.send(ClientMessage::Terminate).await?;
            self.wait(false).await
        })
        .await
        .context("workload termination deadline; cleanup remains unknown")?
    }

    async fn stop(&mut self, grace: Duration) -> Result<Completion> {
        if !grace.is_zero() {
            self.send(ClientMessage::CloseInput).await?;
            if let Ok(result) = tokio::time::timeout(grace, self.wait(false)).await {
                return result;
            }
        }
        self.terminate().await
    }

    async fn wait(&mut self, output: bool) -> Result<Completion> {
        self.wait_with(|stream, bytes| {
            if output {
                console::Console::write(stream, bytes)?;
            }
            Ok(())
        })
        .await
    }

    async fn wait_with(
        &mut self,
        mut output: impl FnMut(&str, &[u8]) -> Result<()>,
    ) -> Result<Completion> {
        loop {
            match self.next().await? {
                ServerMessage::Output { stream, bytes } => output(&stream, &bytes)?,
                ServerMessage::Completed { result } => return Ok(result),
                ServerMessage::Error { code, message } => bail!("monitor {code:?}: {message}"),
                response => bail!("unexpected monitor response: {response:?}"),
            }
        }
    }
}

/// Native leader status, separately from monitoring/cleanup diagnostics.
#[derive(Debug)]
pub struct Outcome(pub Completion);

impl Outcome {
    pub fn code(&self) -> Result<u32> {
        for error in self.0.log_errors.iter().chain(&self.0.monitor_errors) {
            eprintln!("madrillo: {error}");
        }
        for warning in &self.0.monitor_warnings {
            eprintln!("madrillo: {warning}");
        }
        if self.0.cleanup != madrillo::CleanupResult::Complete {
            eprintln!(
                "madrillo: workload cleanup is {:?}; image pins remain retained",
                self.0.cleanup
            );
        }
        if self.0.output_truncated {
            eprintln!("madrillo: console output was truncated");
        }
        match self.0.leader {
            LeaderResult::Exit { code } => Ok(code),
            LeaderResult::Signal { signal, .. } => Ok(128 + signal),
            ref unknown => bail!("workload has no confirmed exit status: {unknown:?}"),
        }
    }
}

/// Access a retained workload without consulting or signalling a recorded PID.
#[derive(Debug)]
pub struct Workload(Record);

impl Workload {
    pub fn open(id: uuid::Uuid, config: &crate::config::Config) -> Result<Self> {
        let layout = config.resolve_layout()?;
        let records =
            record::Records::existing(&layout.graphroot)?.context("no retained workloads")?;
        Ok(Self(records.get(id)?))
    }

    pub async fn logs(&self) -> Result<()> {
        let snapshot = logs::Snapshot::try_from(self.0.console_log()?)?;
        tokio::task::spawn_blocking(move || {
            snapshot.write_to(&mut std::io::stdout().lock(), &mut std::io::stderr().lock())
        })
        .await?
    }

    pub async fn wait(&self) -> Result<Outcome> {
        if let Some(result) = self.0.completion()? {
            return Ok(Outcome(result));
        }
        let result = async {
            let mut client = Client::connect(self.0.endpoint()).await?;
            client.send(ClientMessage::Wait).await?;
            client.wait(false).await
        }
        .await;
        self.retained_or_error(result).map(Outcome)
    }

    pub async fn stop(&self, grace: Duration) -> Result<Outcome> {
        if let Some(result) = self.0.completion()? {
            return Ok(Outcome(result));
        }
        let result = async { Client::connect(self.0.endpoint()).await?.stop(grace).await }.await;
        self.retained_or_error(result).map(Outcome)
    }

    pub async fn attach(&self, interactive: bool) -> Result<u32> {
        if let Some(result) = self.0.completion()? {
            return Outcome(result).code();
        }
        let result = async {
            let mut client = Client::connect(self.0.endpoint()).await?;
            client
                .send(ClientMessage::Attach {
                    input: interactive,
                    streams: Vec::new(),
                })
                .await?;
            match client.next().await? {
                ServerMessage::Attached { input } if input == interactive => {}
                response => bail!("attachment rejected: {response:?}"),
            }
            console::Console::open(interactive)?.view(&mut client).await
        }
        .await;
        match result {
            Ok(code) => Ok(code),
            Err(error) => match self.0.completion()? {
                Some(result) => Outcome(result).code(),
                None => Err(error).context("workload state is unknown; image pins remain retained"),
            },
        }
    }

    fn retained_or_error(&self, result: Result<Completion>) -> Result<Completion> {
        match result {
            Ok(result) => Ok(result),
            Err(error) => match self.0.completion()? {
                Some(result) => Ok(result),
                None => Err(error).context("workload state is unknown; image pins remain retained"),
            },
        }
    }

    pub async fn list(config: &crate::config::Config) -> Result<()> {
        let layout = config.resolve_layout()?;
        let Some(records) = record::Records::existing(&layout.graphroot)? else {
            return Ok(());
        };
        println!("ID                                   STATE    IMAGES");
        for record in records.list()? {
            let status = if record.completion()?.is_some() {
                "exited"
            } else {
                let live = tokio::time::timeout(Duration::from_secs(5), async {
                    let mut client = Client::connect(record.endpoint()).await?;
                    client
                        .send(ClientMessage::Attach {
                            input: false,
                            streams: Vec::new(),
                        })
                        .await?;
                    Ok::<_, anyhow::Error>(matches!(
                        client.next().await?,
                        ServerMessage::Attached { .. }
                    ))
                })
                .await;
                if matches!(live, Ok(Ok(true))) {
                    "running"
                } else {
                    "unknown"
                }
            };
            println!(
                "{} {:8} {}",
                record.id(),
                status,
                record
                    .images()
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            );
        }
        Ok(())
    }
}
