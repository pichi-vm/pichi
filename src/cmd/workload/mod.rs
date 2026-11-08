// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Pichi owns workload identity and attachment; madrillo owns supervision.

#![deny(unsafe_code)]

mod client;
mod console;
mod logs;
pub(crate) mod record;
mod resize;
#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "Approved private native handle-inheritance boundary"
)]
mod windows;

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
    pub(crate) held: Vec<File>,
    pub(crate) cleanup: Option<&'a Path>,
}

#[derive(Debug)]
pub(crate) struct Running {
    client: Client,
    #[cfg(unix)]
    monitor: Box<dyn process_wrap::std::ChildWrapper>,
    #[cfg(windows)]
    monitor: windows::Monitor,
}

impl Launch<'_> {
    fn options(&self) -> Result<Vec<OsString>> {
        let mut options = vec![
            "--state-dir".into(),
            self.state.into(),
            "--listen".into(),
            self.endpoint.into(),
        ];
        if self.input {
            options.push("--stdin".into());
        }
        if let Some(path) = self.cleanup {
            options.extend([
                "--exit-command".into(),
                std::env::current_exe()?.into(),
                "--exit-command-arg".into(),
                "cleanup-workload".into(),
                "--exit-command-arg".into(),
                path.into(),
            ]);
        }
        Ok(options)
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
        command.args(self.options()?);
        let mut control = 3_i32;
        for (role, file) in self
            .inherited
            .iter()
            .map(|file| ("--inherit", file))
            .chain(self.held.iter().map(|file| ("--hold", file)))
        {
            let number = file.as_raw_fd();
            if number < 3 {
                bail!("workload inheritance must be distinct from stdio");
            }
            control = control.max(number.checked_add(1).context("descriptor limit")?);
            command.args([OsString::from(role), number.to_string().into()]);
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
        use std::os::windows::io::{AsHandle, AsRawHandle};
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
        let mut arguments = self.options()?;
        let mut handles = Vec::new();
        for (role, file) in self
            .inherited
            .iter()
            .map(|file| ("--inherit", file))
            .chain(self.held.iter().map(|file| ("--hold", file)))
        {
            arguments.extend([
                role.into(),
                (file.as_raw_handle() as usize).to_string().into(),
            ]);
            handles.push(file.as_handle());
        }
        arguments.extend([
            "--control".into(),
            (initial.as_raw_handle() as usize).to_string().into(),
            "--".into(),
            self.program.into(),
        ]);
        arguments.extend_from_slice(self.arguments);
        handles.push(initial.as_handle());
        let monitor =
            windows::Monitor::spawn(self.monitor, &arguments, &handles, self.diagnostic_file()?)
                .with_context(|| format!("start independent monitor {}", self.monitor.display()))?;
        drop(initial);
        Ok(Running {
            client: Client::new(parent.into()),
            monitor,
        })
    }
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    mod job;

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
            held: Vec::new(),
            cleanup: None,
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
    #[ignore = "requires madrillo on PATH; no workload is executed"]
    async fn startup_failure_reports_monitor_diagnostic() -> Result<()> {
        let root = tempfile::tempdir()?;
        let graphroot = root.path().join("cache");
        std::fs::create_dir(&graphroot)?;
        let records = record::Records::open(&graphroot)?;
        let image = format!("sha256:{}", "0".repeat(64)).parse()?;
        let record = records.reserve(vec![image])?;
        // A preexisting state file fails before the monitor can prepare its
        // endpoint or execute any workload, including the default Dillo path.
        std::fs::write(record.state(), b"reserved")?;
        let error = Running::start(&record, &[], Vec::new(), Vec::new(), false)
            .await
            .expect_err("preexisting monitor state must fail");
        let diagnostic = record.monitor_diagnostic()?;
        assert!(diagnostic.contains("madrillo [Error]"), "{diagnostic:?}");
        assert!(error.to_string().contains(&diagnostic), "{error:#}");
        assert!(format!("{error:#}").contains("monitor disconnected without completion"));
        assert!(!record.state().join("result.json").exists());
        println!(
            "native startup failure: retained monitor diagnostic reported without executing a workload"
        );
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires MADRILLO_BIN and its qualification fixture; run explicitly"]
    async fn monitor_crash_preserves_unknown_state_and_cleanup_inputs() -> Result<()> {
        let monitor = PathBuf::from(std::env::var_os("MADRILLO_BIN").context("set MADRILLO_BIN")?);
        let fixture =
            monitor.with_file_name(format!("madrillo-fixture{}", std::env::consts::EXE_SUFFIX));
        let root = tempfile::tempdir()?;
        let graphroot = root.path().join("cache");
        std::fs::create_dir(&graphroot)?;
        let records = record::Records::open(&graphroot)?;
        let image = format!("sha256:{}", "0".repeat(64)).parse()?;
        let id = uuid::Uuid::new_v4();
        #[cfg(unix)]
        let endpoint = root.path().join("crashed.sock");
        #[cfg(windows)]
        let endpoint = PathBuf::from(format!(r"\\.\pipe\pichi-crashed-{id}"));
        let record = records.create(vec![image], endpoint, id)?;
        std::fs::create_dir(record.storage_directory())?;
        let storage = record.storage_directory().join("storage.raw");
        let output = record.create_build_output()?.join("partial-output");
        std::fs::write(&storage, b"guest storage")?;
        std::fs::write(&output, b"incomplete build")?;
        let mut running = ReapMonitor(
            Launch {
                monitor: &monitor,
                program: &fixture,
                arguments: &["echo".into()],
                state: &record.state(),
                endpoint: record.endpoint(),
                input: true,
                inherited: Vec::new(),
                held: Vec::new(),
                cleanup: None,
            }
            .spawn()
            .await?,
        );
        running.started().await?;
        let payload = b"before crash\0\xff\n";
        running
            .client
            .send(ClientMessage::Input {
                bytes: payload.to_vec(),
            })
            .await?;
        let reply = tokio::time::timeout(Duration::from_secs(5), running.client.next()).await??;
        assert!(matches!(reply, ServerMessage::Output { bytes, .. } if bytes == payload));
        // Kill only our still-owned monitor, never a PID recovered from a record.
        // Its echo workload exits when the monitor's console writer disappears.
        #[cfg(unix)]
        let monitor = running.monitor.inner_mut();
        #[cfg(windows)]
        let monitor = &mut running.monitor;
        monitor.kill()?;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = monitor.try_wait()? {
                break status;
            }
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "monitor crash deadline"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        assert!(!status.success());
        let workload = Workload(records.get(record.id())?);
        for _ in 0..2 {
            for error in [
                workload.wait().await.unwrap_err(),
                workload.stop(Duration::ZERO).await.unwrap_err(),
                workload.attach(false).await.unwrap_err(),
            ] {
                assert!(error.to_string().contains("workload state is unknown"));
            }
            Workload::cleanup(record.path())?;
            record.release_build_output()?;
            assert!(record.completion()?.is_none());
            assert_eq!(records.pins()?, record.images());
            assert_eq!(std::fs::read(&storage)?, b"guest storage");
            assert_eq!(std::fs::read(&output)?, b"incomplete build");
        }
        println!("native monitor crash: unknown state and cleanup inputs retained");
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
                held: Vec::new(),
                cleanup: None,
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
                    held: Vec::new(),
                    cleanup: None,
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
        #[cfg(windows)]
        {
            use std::{
                io::Read,
                os::windows::io::{AsRawHandle, OwnedHandle},
                sync::mpsc,
            };
            let (mut reader, writer) = std::io::pipe()?;
            let writer = File::from(OwnedHandle::from(writer));
            let number = writer.as_raw_handle() as usize;
            let hidden_path = root.path().join("hidden");
            std::fs::write(&hidden_path, b"monitor-only resource")?;
            let hidden = File::open(hidden_path)?;
            let hidden_number = hidden.as_raw_handle() as usize;
            let endpoint =
                PathBuf::from(format!(r"\\.\pipe\pichi-inherit-{}", uuid::Uuid::new_v4()));
            let record = records.create(
                record.images().to_vec(),
                endpoint.clone(),
                uuid::Uuid::new_v4(),
            )?;
            let mut running = ReapMonitor(
                Launch {
                    monitor: &monitor,
                    program: &fixture,
                    arguments: &[
                        "inherited-probe".into(),
                        number.to_string().into(),
                        hidden_number.to_string().into(),
                    ],
                    state: &record.state(),
                    endpoint: &endpoint,
                    input: false,
                    inherited: vec![writer],
                    held: vec![hidden],
                    cleanup: None,
                }
                .spawn()
                .await?,
            );
            running.started().await?;
            let (send, receive) = mpsc::sync_channel(1);
            let reader = std::thread::Builder::new()
                .name("pichi-inherited-reader".into())
                .spawn(move || {
                    let mut bytes = Vec::new();
                    let result = reader.read_to_end(&mut bytes).map(|_| bytes);
                    let _ = send.send(result);
                })?;
            assert_eq!(
                receive.recv_timeout(Duration::from_secs(8))??,
                b"opaque\0\xff"
            );
            reader
                .join()
                .map_err(|_| anyhow::anyhow!("inheritance reader panicked"))?;
            let completion =
                tokio::time::timeout(Duration::from_secs(8), running.wait(None)).await??;
            assert_eq!(completion.leader, LeaderResult::Exit { code: 37 });
            assert_eq!(completion.cleanup, madrillo::CleanupResult::Complete);
            assert!(completion.monitor_errors.is_empty(), "{completion:?}");
            assert!(running.monitor.wait()?.success());
            println!(
                "native Pichi exact inheritance: opaque bytes, excluded handle and EOF passed"
            );
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
        held: Vec<File>,
        input: bool,
    ) -> Result<Self> {
        let mut arguments = arguments.to_vec();
        arguments.extend(["--console-resize".into(), record.resize_endpoint().into()]);
        let mut running = Launch {
            monitor: &Self::executable(),
            program: &crate::cmd::run::find_dillo(),
            arguments: &arguments,
            state: &record.state(),
            endpoint: record.endpoint(),
            input,
            inherited,
            cleanup: Some(record.path()),
            held,
        }
        .spawn()
        .await
        .with_context(|| {
            format!(
                "launch workload {}; its retained state is unknown",
                record.id()
            )
        })?;
        if let Err(error) = running.started().await {
            return Err(running.startup_error(record, error).await);
        }
        Ok(running)
    }

    async fn startup_error(&mut self, record: &Record, error: anyhow::Error) -> anyhow::Error {
        // EOF can precede the monitor's final diagnostic flush. Await only our
        // own process, with a bound; a launch timeout must not kill a workload
        // whose startup state is still unknown.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        loop {
            #[cfg(unix)]
            let status = self.monitor.inner_mut().try_wait();
            #[cfg(windows)]
            let status = self.monitor.try_wait();
            if !matches!(status, Ok(None)) || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let diagnostic = match record.monitor_diagnostic() {
            Ok(text) if !text.is_empty() => text,
            Ok(_) => "no monitor diagnostic was recorded".into(),
            Err(error) => format!("could not read monitor diagnostic: {error:#}"),
        };
        error.context(format!(
            "launch workload {}; monitor diagnostic ({}): {diagnostic}",
            record.id(),
            record.path().join("monitor.err").display()
        ))
    }

    pub(crate) async fn view(&mut self, record: &Record, interactive: bool) -> Result<u32> {
        console::Console::open(interactive)?
            .view(&mut self.client, &record.resize_endpoint())
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
        PathBuf::from(if cfg!(windows) {
            "madrillo.exe"
        } else {
            "madrillo"
        })
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
    /// Idempotent exit callback. Unknown/incomplete cleanup retains runtime files.
    pub fn cleanup(path: &Path) -> Result<()> {
        Record::at(path)?.release_runtime_files()
    }
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
        self.0.release_runtime_files()?;
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
        self.0.release_runtime_files()?;
        if let Some(result) = self.0.completion()? {
            return Ok(Outcome(result));
        }
        let result = async { Client::connect(self.0.endpoint()).await?.stop(grace).await }.await;
        self.retained_or_error(result).map(Outcome)
    }

    pub async fn attach(&self, interactive: bool) -> Result<u32> {
        self.0.release_runtime_files()?;
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
            console::Console::open(interactive)?
                .view(&mut client, &self.0.resize_endpoint())
                .await
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
        self.0.release_runtime_files()?;
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
