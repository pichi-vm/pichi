// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Native caller Job loss. Run the parent outside restrictive runner/Cargo Jobs.

#![forbid(unsafe_code)]

use super::{Client, ClientMessage, Launch, LeaderResult, ReapMonitor, ServerMessage};
use anyhow::{Context, Result, bail};
use std::{
    io::{Read, Write},
    os::windows::io::AsRawHandle,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use win_custody::{Process, ProcessAccess, WaitOutcome};

struct Caller {
    process: Child,
    job: Option<win32job::Job>,
    monitor: Option<Process>,
}

impl Drop for Caller {
    fn drop(&mut self) {
        if let Some(monitor) = &self.monitor {
            let _ = monitor.terminate(1);
            let _ = monitor.wait(Some(Duration::from_secs(5)));
        }
        drop(self.job.take());
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

#[tokio::test]
#[ignore = "internal fixture: started only by caller_job_loss_preserves_workload"]
async fn caller() -> Result<()> {
    let root =
        PathBuf::from(std::env::var_os("PICHI_JOB_TEST_ROOT").context("fixture root missing")?);
    let endpoint =
        PathBuf::from(std::env::var_os("PICHI_JOB_TEST_PIPE").context("fixture pipe missing")?);
    // The parent assigns this process to its Job before authorizing launch.
    std::io::stdin().read_exact(&mut [0])?;
    let monitor = PathBuf::from(std::env::var_os("MADRILLO_BIN").context("set MADRILLO_BIN")?);
    let fixture = monitor.with_file_name("madrillo-fixture.exe");
    let mut running = ReapMonitor(
        Launch {
            monitor: &monitor,
            program: &fixture,
            arguments: &["echo".into()],
            state: &root.join("state"),
            endpoint: &endpoint,
            input: true,
            inherited: Vec::new(),
        }
        .spawn()
        .await?,
    );
    running.started().await?;
    std::fs::write(root.join("monitor-pid"), running.monitor.id().to_string())?;
    std::fs::write(root.join("ready"), b"ready")?;
    // A failing parent still gets a bounded fixture lifetime and owned cleanup.
    tokio::time::sleep(Duration::from_secs(30)).await;
    bail!("parent did not terminate the caller Job")
}

#[tokio::test]
#[ignore = "requires MADRILLO_BIN and execution outside restrictive runner/Cargo Jobs"]
async fn caller_job_loss_preserves_workload() -> Result<()> {
    let root = tempfile::tempdir()?;
    let endpoint = PathBuf::from(format!(r"\\.\pipe\pichi-job-loss-{}", uuid::Uuid::new_v4()));
    let diagnostics = root.path().join("caller.err");
    let process = Command::new(std::env::current_exe()?)
        .args([
            "cmd::workload::tests::job::caller",
            "--exact",
            "--ignored",
            "--nocapture",
        ])
        .env("PICHI_JOB_TEST_ROOT", root.path())
        .env("PICHI_JOB_TEST_PIPE", &endpoint)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&diagnostics)?)
        .spawn()?;
    let mut caller = Caller {
        process,
        job: None,
        monitor: None,
    };
    let mut limits = win32job::ExtendedLimitInfo::new();
    limits.limit_kill_on_job_close().limit_breakaway_ok();
    let job = win32job::Job::create_with_limit_info(&limits)?;
    job.assign_process(caller.process.as_raw_handle() as isize)?;
    caller.job = Some(job);
    caller
        .process
        .stdin
        .take()
        .context("caller gate missing")?
        .write_all(b"go")?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while !root.path().join("ready").exists() {
        if let Some(status) = caller.process.try_wait()? {
            bail!(
                "caller exited {status}: {}",
                std::fs::read_to_string(&diagnostics)?
            );
        }
        if Instant::now() >= deadline {
            bail!("caller did not launch the monitor");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let pid = std::fs::read_to_string(root.path().join("monitor-pid"))?.parse()?;
    caller.monitor = Some(Process::open(pid, ProcessAccess::Terminate)?);
    let monitor = caller.monitor.as_ref().context("monitor handle missing")?;
    assert_eq!(monitor.wait(Some(Duration::ZERO))?, WaitOutcome::TimedOut);
    assert!(caller.process.try_wait()?.is_none());
    // Closing the only Job handle terminates its members. The monitor must
    // have broken away before its own first instruction, as production requires.
    drop(caller.job.take());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if caller.process.try_wait()?.is_some() {
            // Job close can terminate with code zero. The fixture cannot
            // return normally within this deadline, so waitability is proof.
            break;
        }
        if Instant::now() >= deadline {
            bail!("caller survived Job closure");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(monitor.wait(Some(Duration::ZERO))?, WaitOutcome::TimedOut);
    assert!(!root.path().join("state/result.json").exists());
    let mut client = Client::connect(&endpoint).await?;
    client
        .send(ClientMessage::Attach {
            input: true,
            streams: Vec::new(),
        })
        .await?;
    let attached = tokio::time::timeout(Duration::from_secs(5), client.next()).await??;
    assert_eq!(attached, ServerMessage::Attached { input: true });
    let bytes = b"after caller Job loss\0\xff\n";
    client
        .send(ClientMessage::Input {
            bytes: bytes.to_vec(),
        })
        .await?;
    let response = tokio::time::timeout(Duration::from_secs(5), client.next()).await??;
    assert!(matches!(response, ServerMessage::Output { bytes: reply, .. } if reply == bytes));
    client.send(ClientMessage::CloseInput).await?;
    let result = tokio::time::timeout(Duration::from_secs(5), client.wait(false)).await??;
    assert_eq!(result.leader, LeaderResult::Exit { code: 0 });
    assert_eq!(result.cleanup, madrillo::CleanupResult::Complete);
    assert_eq!(
        monitor.wait(Some(Duration::from_secs(5)))?,
        WaitOutcome::Signaled
    );
    assert_eq!(monitor.exit_code()?, Some(0));
    Ok(())
}
