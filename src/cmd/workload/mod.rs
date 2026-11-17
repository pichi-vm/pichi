// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Pichi owns workload identity and attachment; madrillo owns supervision.

#![deny(unsafe_code)]

mod client;
mod console;
pub(crate) mod instance;
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
use instance::{Instance, State};
use madrillo::{ClientMessage, Completion, LeaderResult, ServerMessage};
use record::{Entry, Record, Run};
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
            "--start-gated".into(),
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
        let workload = Workload::new(Entry::Legacy(records.create(vec![image], endpoint, id)?))?;
        let error = workload.stop(Duration::ZERO).await.unwrap_err();
        assert!(error.to_string().contains("workload state is unknown"));
        assert!(
            Workload::new(records.get(id)?)?
                .run()?
                .completion()?
                .is_none()
        );
        let Entry::Legacy(record) = records.get(id)? else {
            bail!("expected legacy record");
        };
        assert_eq!(records.pins()?, record.images());
        Ok(())
    }

    #[tokio::test]
    async fn attach_reports_exit_only_after_monitor_acceptance() -> Result<()> {
        use futures_util::{SinkExt, StreamExt};
        use interprocess::local_socket::{GenericFilePath, ListenerOptions, tokio::prelude::*};
        use tokio_util::codec::{Framed, LengthDelimitedCodec};

        let root = tempfile::tempdir()?;
        let graphroot = root.path().join("cache");
        std::fs::create_dir(&graphroot)?;
        let records = record::Records::open(&graphroot)?;
        for accepted in [false, true] {
            let id = uuid::Uuid::new_v4();
            #[cfg(unix)]
            let endpoint = root.path().join(format!("{accepted}.sock"));
            #[cfg(windows)]
            let endpoint = PathBuf::from(format!(r"\\.\pipe\pichi-attach-{id}"));
            let listener = ListenerOptions::new()
                .name(endpoint.as_path().to_fs_name::<GenericFilePath>()?)
                .create_tokio()?;
            let image = format!("sha256:{}", "0".repeat(64)).parse()?;
            let workload =
                Workload::new(Entry::Legacy(records.create(vec![image], endpoint, id)?))?;
            let state = workload.run()?.state();
            std::fs::create_dir(&state)?;
            let completion = Completion {
                version: madrillo::PROTOCOL_VERSION,
                leader: LeaderResult::Exit { code: 23 },
                cause: madrillo::CompletionCause::LeaderExit,
                exited_at_unix_ms: 0,
                cleanup: madrillo::CleanupResult::Complete,
                oom: madrillo::OomEvidence::NoneObserved,
                log_errors: Vec::new(),
                monitor_errors: Vec::new(),
                monitor_warnings: Vec::new(),
                output_truncated: false,
            };
            let serving = async {
                let mut server = Framed::new(listener.accept().await?, LengthDelimitedCodec::new());
                let request: madrillo::Request = serde_json::from_slice(
                    &server.next().await.context("missing attach request")??,
                )?;
                assert!(matches!(
                    request.message,
                    ClientMessage::Attach { input: false, .. }
                ));
                // The client already checked for a retained result. Complete
                // now, and either accept attachment or report completion first.
                std::fs::write(state.join("result.json"), serde_json::to_vec(&completion)?)?;
                let message = if accepted {
                    ServerMessage::Attached { input: false }
                } else {
                    ServerMessage::Completed {
                        result: completion.clone(),
                    }
                };
                server
                    .send(
                        serde_json::to_vec(&madrillo::Response {
                            version: madrillo::PROTOCOL_VERSION,
                            message,
                        })?
                        .into(),
                    )
                    .await?;
                #[cfg(windows)]
                {
                    let interprocess::local_socket::tokio::Stream::NamedPipe(pipe) =
                        server.get_ref();
                    pipe.inner().assume_flushed();
                }
                Ok::<_, anyhow::Error>(())
            };
            let (served, result) = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(serving, workload.attach(false))
            })
            .await?;
            served?;
            if accepted {
                assert_eq!(result?, 23);
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("can only attach to running workloads")
                );
            }
        }
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
            state: &record.run.state(),
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
        assert!(record.run.completion()?.is_none());
        assert!(
            !record.run.state().exists(),
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
        std::fs::write(record.run.state(), b"reserved")?;
        let error = Running::start(&record.run, &[], Vec::new(), Vec::new(), false)
            .await
            .expect_err("preexisting monitor state must fail");
        let diagnostic = record.run.monitor_diagnostic()?;
        assert!(diagnostic.contains("madrillo [Error]"), "{diagnostic:?}");
        assert!(error.to_string().contains(&diagnostic), "{error:#}");
        assert!(format!("{error:#}").contains("monitor disconnected without completion"));
        assert!(!record.run.state().join("result.json").exists());
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
                state: &record.run.state(),
                endpoint: record.run.endpoint(),
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
        for _ in 0..2 {
            for error in [
                Workload::new(records.get(record.id())?)?
                    .wait()
                    .await
                    .unwrap_err(),
                Workload::new(records.get(record.id())?)?
                    .stop(Duration::ZERO)
                    .await
                    .unwrap_err(),
                Workload::new(records.get(record.id())?)?
                    .attach(false)
                    .await
                    .unwrap_err(),
            ] {
                assert!(error.to_string().contains("workload state is unknown"));
            }
            Workload::cleanup(record.run.path())?;
            record.release_build_output()?;
            assert!(record.run.completion()?.is_none());
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
        let state = record.run.state();
        assert_eq!(records.pins()?, record.images());
        assert_eq!(
            Workload::new(records.get(record.id())?)?.run()?.endpoint(),
            endpoint
        );
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
        // Both attachments may write; releasing one must leave the other usable.
        for release_initial in [false, true] {
            if release_initial {
                running.client.send(ClientMessage::InputEnd).await?;
                // An ordered reply confirms release before the other client writes.
                running
                    .client
                    .send(ClientMessage::Attach {
                        input: false,
                        streams: Vec::new(),
                    })
                    .await?;
                assert_eq!(
                    running.client.next().await?,
                    ServerMessage::Attached { input: false }
                );
            }
            attached
                .send(ClientMessage::Input {
                    bytes: payload.to_vec(),
                })
                .await?;
            for client in [&mut running.client, &mut attached] {
                let reply = tokio::time::timeout(Duration::from_secs(5), client.next()).await??;
                assert!(matches!(reply, ServerMessage::Output { bytes, .. } if bytes == payload));
            }
        }
        // Both native directions must progress together above the monitor's
        // input/output buffering limits, including on Windows named pipes.
        {
            const SIZE: usize = 8 * 1024 * 1024;
            let (writer, reader) = attached.halves();
            let (_, observer) = running.client.halves();
            let send = async {
                for _ in 0..SIZE / 8192 {
                    writer
                        .send(ClientMessage::Input {
                            bytes: vec![0xa5; 8192],
                        })
                        .await?;
                }
                Ok::<_, anyhow::Error>(())
            };
            let receive = async |reader: &mut client::Receiver| -> Result<()> {
                let mut total = 0;
                while total < SIZE {
                    match reader.next().await? {
                        ServerMessage::Output { bytes, .. } => {
                            assert!(bytes.iter().all(|&byte| byte == 0xa5));
                            total += bytes.len();
                        }
                        response => bail!("unexpected echo response: {response:?}"),
                    }
                }
                assert_eq!(total, SIZE);
                Ok(())
            };
            tokio::time::timeout(Duration::from_secs(60), async {
                tokio::try_join!(send, receive(reader), receive(observer))
            })
            .await??;
        }
        // Administrative shutdown must work while an attachment still forwards input.
        let workload = Workload::new(records.get(record.id())?)?;
        let stopped = workload
            .stop(Duration::from_secs(5))
            .await?
            .context("stop result")?;
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
            .run
            .completion()?
            .context("missing retained completion")?;
        assert_eq!(completion.leader, LeaderResult::Exit { code: 0 });
        // After the monitor has exited, repeated stop reads the durable result.
        assert_eq!(
            Workload::new(records.get(record.id())?)?
                .stop(Duration::ZERO)
                .await?
                .context("stop result")?
                .0,
            completion
        );
        if completion.cleanup == madrillo::CleanupResult::Complete {
            assert_eq!(records.pins()?, Vec::new());
        } else {
            assert_eq!(records.pins()?, record.images());
        }
        // Consume the normal final response, then lose the connection before
        // viewing again. Only the retained result can satisfy the second view.
        assert!(matches!(
            running.client.next().await?,
            ServerMessage::Completed { result } if result == completion
        ));
        assert_eq!(running.view(&record.run, false, false).await?, 0);
        let result_path = record.run.state().join("result.json");
        let saved_result = std::fs::read(&result_path)?;
        std::fs::remove_file(&result_path)?;
        assert!(
            running
                .view(&record.run, false, false)
                .await
                .unwrap_err()
                .is::<client::ConnectionError>()
        );
        std::fs::write(&result_path, b"invalid result")?;
        assert!(
            running
                .view(&record.run, false, false)
                .await
                .unwrap_err()
                .is::<client::ConnectionError>()
        );
        std::fs::write(&result_path, saved_result)?;
        // A retained success must not hide malformed protocol or console
        // output errors. Use the native transport without the live monitor.
        {
            use futures_util::SinkExt;
            use interprocess::local_socket::{GenericFilePath, ListenerOptions, tokio::prelude::*};
            use tokio_util::codec::{Framed, LengthDelimitedCodec};

            #[cfg(unix)]
            let endpoint = root.path().join("view.sock");
            #[cfg(windows)]
            let endpoint = PathBuf::from(format!(r"\\.\pipe\pichi-view-{}", record.id()));
            let listener = ListenerOptions::new()
                .name(endpoint.as_path().to_fs_name::<GenericFilePath>()?)
                .create_tokio()?;
            for frame in [
                b"invalid response".to_vec(),
                serde_json::to_vec(&madrillo::Response {
                    version: madrillo::PROTOCOL_VERSION,
                    message: ServerMessage::Output {
                        stream: "invalid stream".into(),
                        bytes: vec![1],
                    },
                })?,
            ] {
                let serving = async {
                    let mut server =
                        Framed::new(listener.accept().await?, LengthDelimitedCodec::new());
                    server.send(frame.into()).await?;
                    Ok::<_, anyhow::Error>(server)
                };
                let viewing = async {
                    running.client = Client::connect(&endpoint).await?;
                    running.view(&record.run, false, false).await
                };
                let (server, result) = tokio::time::timeout(Duration::from_secs(5), async {
                    tokio::join!(serving, viewing)
                })
                .await?;
                let _server = server?;
                assert!(!result.unwrap_err().is::<client::ConnectionError>());
            }
        }
        drop(attached);
        drop(running);
        // A crashed monitor must not unlock a disk still owned by its VMM.
        // gated-echo ignores console EOF until we release its file gate.
        let mut instance = Instance::create(
            &records,
            instance::Spec {
                root: record.images()[0].clone(),
                manifest: record.images()[0].clone(),
                cpus: 1,
                memory_mib: 128,
                storage_bytes: 2 * 1024 * 1024,
            },
        )?;
        let operation = instance.gate()?;
        let gate = root.path().join("release-orphaned-vmm");
        // Observe child exit independently of the lease being tested, so a
        // missing lease cannot make failure cleanup remove the gate too soon.
        let (mut exited, alive) = std::io::pipe()?;
        #[cfg(unix)]
        let alive = File::from(std::os::fd::OwnedFd::from(alive));
        #[cfg(windows)]
        let alive = File::from(std::os::windows::io::OwnedHandle::from(alive));
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            let _ = send.send(std::io::copy(&mut exited, &mut std::io::sink()));
        });
        let ownership: Result<()> = async {
            let (run, attempt, lease) = instance.prepare_attempt(&operation)?;
            let (held, inherited) = lease.clone_boot()?;
            let mut running = ReapMonitor(
                Launch {
                    monitor: &monitor,
                    program: &fixture,
                    arguments: &["gated-echo".into(), gate.clone().into()],
                    state: &run.state(),
                    endpoint: run.endpoint(),
                    input: false,
                    inherited: vec![inherited, alive],
                    held: vec![held],
                    cleanup: None,
                }
                .spawn()
                .await?,
            );
            running.prepared().await?;
            instance.select_attempt(&operation, attempt)?;
            running.authorize().await?;
            instance.confirm_started(&operation)?;
            drop(lease);
            anyhow::ensure!(instance.live()?, "boot did not inherit the instance lease");
            #[cfg(unix)]
            let monitor = running.monitor.inner_mut();
            #[cfg(windows)]
            let monitor = &mut running.monitor;
            monitor.kill()?;
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while monitor.try_wait()?.is_none() {
                anyhow::ensure!(
                    std::time::Instant::now() < deadline,
                    "monitor crash deadline"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            anyhow::ensure!(instance.live()?, "monitor death released the VMM lease");
            anyhow::ensure!(
                instance.prepare_attempt(&operation).is_err(),
                "a second boot acquired the disk"
            );
            anyhow::ensure!(
                run.completion()?.is_none(),
                "crashed monitor invented a result"
            );
            Ok(())
        }
        .await;
        // Release the fixture on error paths as well, before propagating any
        // failed assertion. No recovered PID is signalled to clean it up.
        std::fs::write(&gate, b"exit")?;
        receive.recv_timeout(Duration::from_secs(5))??;
        reader
            .join()
            .map_err(|_| anyhow::anyhow!("VMM exit reader panicked"))?;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while instance.live()? {
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "VMM did not release its instance lease"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        ownership?;
        assert!(matches!(instance.state()?, State::Unknown));
        instance.check_disk(&operation)?;
        assert!(records.pins()?.contains(instance.root()));
        println!(
            "native instance lease: monitor crash, final-owner release and disk retention passed"
        );
        // Exercise forced stop through the same native transport on every host.
        for grace in [Duration::from_millis(50), Duration::ZERO] {
            let workload = Workload::new(Entry::Legacy(records.create(
                record.images().to_vec(),
                endpoint.clone(),
                uuid::Uuid::new_v4(),
            )?))?;
            let mut running = ReapMonitor(
                Launch {
                    monitor: &monitor,
                    program: &fixture,
                    arguments: &["sleep".into()],
                    state: &workload.run()?.state(),
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
            let id = workload.id();
            let result = workload.stop(grace).await?.context("stop result")?.0;
            assert!(started.elapsed() >= grace);
            assert_eq!(result.cause, madrillo::CompletionCause::Forced);
            #[cfg(unix)]
            assert!(matches!(
                result.leader,
                LeaderResult::Signal { signal: 9, .. }
            ));
            #[cfg(windows)]
            assert!(matches!(result.leader, LeaderResult::Exit { code } if code != 0));
            assert_eq!(
                Workload::new(records.get(id)?)?.run()?.completion()?,
                Some(result)
            );
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
                    state: &record.run.state(),
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

        for authorize in [false, true] {
            // A prepared monitor must wait for authorization. Observers can
            // attach before then and receive Started before completion.
            let gated = tempfile::tempdir()?;
            #[cfg(unix)]
            let endpoint = gated.path().join("control");
            #[cfg(windows)]
            let endpoint = PathBuf::from(format!(r"\\.\pipe\pichi-gated-{}", uuid::Uuid::new_v4()));
            let marker = gated.path().join("guest-started");
            let mut running = ReapMonitor(
                Launch {
                    monitor: &monitor,
                    program: &fixture,
                    arguments: &["touch".into(), marker.clone().into()],
                    state: &gated.path().join("monitor"),
                    endpoint: &endpoint,
                    input: false,
                    inherited: Vec::new(),
                    held: Vec::new(),
                    cleanup: None,
                }
                .spawn()
                .await?,
            );
            assert!(matches!(
                running.client.next().await?,
                ServerMessage::Prepared { monitor_pid } if monitor_pid == running.monitor.id()
            ));
            let mut observers = Vec::new();
            for _ in 0..2 {
                let mut client = Client::connect(&endpoint).await?;
                client
                    .send(ClientMessage::Attach {
                        input: false,
                        streams: Vec::new(),
                    })
                    .await?;
                assert!(matches!(
                    client.next().await?,
                    ServerMessage::Attached { input: false }
                ));
                observers.push(client);
            }
            let mut attached = observers.pop().context("attachment")?;
            let mut waiting = observers.pop().context("waiter")?;
            waiting.send(ClientMessage::Wait).await?;
            assert!(!marker.exists(), "guest ran before authorization");
            if !authorize {
                // Losing the initial caller must cancel preparation even when
                // observers remain connected. No observer may authorize this boot.
                drop(std::mem::replace(&mut running.client, attached));
                let result =
                    tokio::time::timeout(Duration::from_secs(5), waiting.wait(false)).await??;
                assert_eq!(result.cause, madrillo::CompletionCause::LaunchCancelled);
                assert!(matches!(result.leader, LeaderResult::Unknown));
                #[cfg(unix)]
                let process = running.monitor.inner_mut();
                #[cfg(windows)]
                let process = &mut running.monitor;
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                loop {
                    if let Some(status) = process.try_wait()? {
                        assert!(status.success());
                        break;
                    }
                    anyhow::ensure!(
                        std::time::Instant::now() < deadline,
                        "cancelled monitor exit deadline"
                    );
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                assert!(!marker.exists(), "cancelled preparation executed the guest");
                continue;
            }
            running.client.send(ClientMessage::Start).await?;
            let resize = endpoint.with_extension("resize");
            let console = console::Console::open(false, false)?;
            let (waited, viewed) = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(waiting.wait(false), console.view(&mut attached, &resize))
            })
            .await?;
            assert_eq!(waited?.leader, LeaderResult::Exit { code: 0 });
            assert_eq!(viewed?, 0);
            assert_eq!(std::fs::read(marker)?, b"invoked");
        }
        Ok(())
    }
}

impl Running {
    /// Both run and build pass the same opaque dillo arguments/resources here.
    pub(crate) async fn prepare(
        run: &Run,
        arguments: &[OsString],
        inherited: Vec<File>,
        held: Vec<File>,
        input: bool,
    ) -> Result<Self> {
        let mut arguments = arguments.to_vec();
        arguments.extend(["--console-resize".into(), run.resize_endpoint().into()]);
        let mut running = Launch {
            monitor: &Self::executable(),
            program: &crate::cmd::run::find_dillo(),
            arguments: &arguments,
            state: &run.state(),
            endpoint: run.endpoint(),
            input,
            inherited,
            cleanup: Some(run.path()),
            held,
        }
        .spawn()
        .await
        .with_context(|| {
            format!(
                "launch boot {}; its retained state is unknown",
                run.path().display()
            )
        })?;
        if let Err(error) = running.prepared().await {
            return Err(running.startup_error(run, error).await);
        }
        Ok(running)
    }

    pub(crate) async fn start(
        run: &Run,
        arguments: &[OsString],
        inherited: Vec<File>,
        held: Vec<File>,
        input: bool,
    ) -> Result<Self> {
        let mut running = Self::prepare(run, arguments, inherited, held, input).await?;
        if let Err(error) = running.authorize().await {
            return Err(running.startup_error(run, error).await);
        }
        Ok(running)
    }

    pub(crate) async fn startup_error(&mut self, run: &Run, error: anyhow::Error) -> anyhow::Error {
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
        let diagnostic = match run.monitor_diagnostic() {
            Ok(text) if !text.is_empty() => text,
            Ok(_) => "no monitor diagnostic was recorded".into(),
            Err(error) => format!("could not read monitor diagnostic: {error:#}"),
        };
        error.context(format!(
            "launch boot {}; monitor diagnostic ({}): {diagnostic}",
            run.path().display(),
            run.path().join("monitor.err").display()
        ))
    }

    pub(crate) async fn view(&mut self, run: &Run, interactive: bool, tty: bool) -> Result<u32> {
        let result = console::Console::open(interactive, tty)?
            .view(&mut self.client, &run.resize_endpoint())
            .await;
        match result {
            Err(error) if error.is::<client::ConnectionError>() => match run.completion() {
                Ok(Some(result)) => Outcome(result).code(),
                _ => Err(error),
            },
            result => result,
        }
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

    pub(crate) async fn terminate(&mut self, run: &Run) -> Result<Completion> {
        if let Some(result) = run.completion()? {
            return Ok(result);
        }
        match self.client.terminate().await {
            Ok(result) => Ok(result),
            Err(error) => run.completion()?.ok_or(error),
        }
    }

    async fn prepared(&mut self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(10), async {
            match self.client.next().await? {
                ServerMessage::Prepared { monitor_pid } if monitor_pid == self.monitor.id() => {
                    Ok(())
                }
                response => bail!("unexpected monitor preparation: {response:?}"),
            }
        })
        .await
        .context("monitor preparation deadline")?
    }

    pub(crate) async fn authorize(&mut self) -> Result<u32> {
        tokio::time::timeout(Duration::from_secs(10), async {
            self.client.send(ClientMessage::Start).await?;
            match self.client.next().await? {
                ServerMessage::Started { leader_pid } => Ok(leader_pid),
                response => bail!("workload did not start: {response:?}"),
            }
        })
        .await
        .context("monitor launch deadline")?
    }

    #[cfg(test)]
    pub(crate) async fn started(&mut self) -> Result<u32> {
        self.prepared().await?;
        self.authorize().await
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
                ServerMessage::Started { .. } => {}
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

/// A selected boot without open handles, retained across batch dispatch.
#[derive(Debug)]
pub struct Selection {
    root: PathBuf,
    id: uuid::Uuid,
    attempt: Option<(uuid::Uuid, PathBuf)>,
}

impl Selection {
    pub fn open(id: &str, config: &crate::config::Config) -> Result<Self> {
        let layout = config.resolve_layout()?;
        let records =
            record::Records::existing(&layout.graphroot)?.context("no retained workloads")?;
        let workload = Workload::new(records.resolve(id)?)?;
        let attempt = match &workload.entry {
            Entry::Instance(instance) => instance
                .attempt()
                .zip(workload.run.as_ref().map(|run| run.endpoint().to_owned())),
            Entry::Legacy(_) => None,
        };
        Ok(Self {
            root: records.path().to_owned(),
            id: workload.id(),
            attempt,
        })
    }

    pub fn reopen(self) -> Result<Workload> {
        let entry = record::Records::at(&self.root)?.get(self.id)?;
        let run = match &entry {
            Entry::Instance(instance) => self
                .attempt
                .map(|(id, endpoint)| instance.run_of(id, endpoint))
                .transpose()?,
            Entry::Legacy(_) => None,
        };
        Ok(Workload { entry, run })
    }
}

/// Access a retained workload without consulting or signalling a recorded PID.
#[derive(Debug)]
pub struct Workload {
    entry: Entry,
    run: Option<Run>,
}

impl Workload {
    /// Idempotent exit callback. Unknown/incomplete cleanup retains runtime files.
    pub fn cleanup(path: &Path) -> Result<()> {
        if path.parent().and_then(Path::file_name) == Some("runs".as_ref()) {
            Instance::release(path)
        } else {
            Record::at(path)?.release_runtime_files()
        }
    }
    pub fn open(id: &str, config: &crate::config::Config) -> Result<Self> {
        let layout = config.resolve_layout()?;
        let records =
            record::Records::existing(&layout.graphroot)?.context("no retained workloads")?;
        Self::new(records.resolve(id)?)
    }

    fn new(entry: Entry) -> Result<Self> {
        let run = match &entry {
            Entry::Instance(instance) => instance.run()?,
            Entry::Legacy(_) => None,
        };
        Ok(Self { entry, run })
    }

    pub(crate) fn instance(self) -> Result<Instance> {
        match self.entry {
            Entry::Instance(instance) => Ok(instance),
            Entry::Legacy(record) => bail!(
                "{} is a legacy workload without an instance-owned disk",
                record.id()
            ),
        }
    }

    pub fn id(&self) -> uuid::Uuid {
        self.entry.id()
    }

    fn started(&self) -> Option<&Run> {
        match &self.entry {
            Entry::Legacy(record) => Some(&record.run),
            Entry::Instance(_) => self.run.as_ref(),
        }
    }

    fn run(&self) -> Result<&Run> {
        self.started()
            .with_context(|| format!("instance {} has not been started", self.entry.id()))
    }

    fn release(&self) -> Result<()> {
        match &self.entry {
            Entry::Legacy(record) => record.release_runtime_files(),
            Entry::Instance(_) => {
                if let Some(run) = &self.run {
                    run.tidy();
                }
                Ok(())
            }
        }
    }

    pub async fn logs(self, labeled: bool) -> Result<()> {
        let snapshot = match self.started() {
            Some(run) => Some(logs::Snapshot::try_from(run.console_log()?)?),
            None => None,
        };
        let prefix = if labeled {
            format!("{} ", &self.id().simple().to_string()[..12])
        } else {
            String::new()
        };
        drop(self);
        tokio::task::spawn_blocking(move || match snapshot {
            Some(snapshot) => snapshot.write_to(
                &mut logs::Prefixed::new(std::io::stdout().lock(), prefix.as_bytes()),
                &mut logs::Prefixed::new(std::io::stderr().lock(), prefix.as_bytes()),
            ),
            None => Ok(()),
        })
        .await?
    }

    pub async fn wait(self) -> Result<Outcome> {
        let run = self.run()?;
        self.release()?;
        if let Some(result) = run.completion()? {
            return Ok(Outcome(result));
        }
        let (path, endpoint) = (run.path().to_owned(), run.endpoint().to_owned());
        drop(self);
        let result = async {
            let mut client = Client::connect(&endpoint).await?;
            client.send(ClientMessage::Wait).await?;
            client.wait(false).await
        }
        .await;
        Self::retained_or_error(&path, &endpoint, result).map(Outcome)
    }

    /// A never-started instance has nothing to stop and no exit status.
    pub async fn stop(self, grace: Duration) -> Result<Option<Outcome>> {
        let Some(run) = self.started() else {
            return Ok(None);
        };
        self.release()?;
        if let Some(result) = run.completion()? {
            return Ok(Some(Outcome(result)));
        }
        let (path, endpoint) = (run.path().to_owned(), run.endpoint().to_owned());
        drop(self);
        let result = async { Client::connect(&endpoint).await?.stop(grace).await }.await;
        Self::retained_or_error(&path, &endpoint, result).map(|result| Some(Outcome(result)))
    }

    pub async fn attach(self, interactive: bool) -> Result<u32> {
        self.release()?;
        let run = self.run()?;
        anyhow::ensure!(
            run.completion()?.is_none(),
            "can only attach to running workloads"
        );
        let (path, endpoint, resize) = (
            run.path().to_owned(),
            run.endpoint().to_owned(),
            run.resize_endpoint(),
        );
        drop(self);
        let mut attached = false;
        let result = async {
            let mut client = Client::connect(&endpoint).await?;
            client
                .send(ClientMessage::Attach {
                    input: interactive,
                    streams: Vec::new(),
                })
                .await?;
            loop {
                match client.next().await? {
                    ServerMessage::Attached { input } if input == interactive => {
                        attached = true;
                        break;
                    }
                    ServerMessage::Started { .. } => {}
                    response => bail!("attachment rejected: {response:?}"),
                }
            }
            console::Console::open(interactive, interactive)?
                .view(&mut client, &resize)
                .await
        }
        .await;
        match result {
            Ok(code) => Ok(code),
            Err(error) => match Run::reopen(&path, endpoint)?.completion()? {
                Some(result) if attached => Outcome(result).code(),
                Some(_) => bail!("can only attach to running workloads"),
                None => Err(error).context("workload state is unknown; image pins remain retained"),
            },
        }
    }

    fn retained_or_error(
        path: &Path,
        endpoint: &Path,
        result: Result<Completion>,
    ) -> Result<Completion> {
        let run = match Run::reopen(path, endpoint.to_owned()) {
            Ok(run) => run,
            Err(error)
                if result.is_ok()
                    && error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                return result;
            }
            Err(error) => return Err(error),
        };
        Self::cleanup(path)?;
        match result {
            Ok(result) => Ok(result),
            Err(error) => match run.completion()? {
                Some(result) => Ok(result),
                None => Err(error).context("workload state is unknown; image pins remain retained"),
            },
        }
    }

    pub async fn list(config: &crate::config::Config, all: bool, quiet: bool) -> Result<()> {
        let layout = config.resolve_layout()?;
        let Some(records) = record::Records::existing(&layout.graphroot)? else {
            return Ok(());
        };
        if !quiet {
            println!("ID                                   STATE    IMAGES");
        }
        for entry in records.list()? {
            let instance_state = match &entry {
                Entry::Instance(instance) => Some(instance.state()?),
                Entry::Legacy(_) => None,
            };
            let inactive = match &entry {
                Entry::Instance(_) => {
                    matches!(instance_state, Some(State::Created | State::Exited))
                }
                Entry::Legacy(record) => record.run.completion()?.is_some(),
            };
            if inactive && !all {
                continue;
            }
            if quiet {
                println!("{}", entry.id());
                continue;
            }
            if let Entry::Instance(instance) = &entry {
                let status = match instance_state.context("missing instance state")? {
                    State::Created => "created",
                    State::Running => "running",
                    State::Exited => "exited",
                    State::Unknown => "unknown",
                };
                println!("{} {:8} {}", instance.id(), status, instance.root());
                continue;
            }
            let Entry::Legacy(record) = entry else {
                continue;
            };
            let status = if record.run.completion()?.is_some() {
                "exited"
            } else {
                let live = tokio::time::timeout(Duration::from_secs(5), async {
                    let mut client = Client::connect(record.run.endpoint()).await?;
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
