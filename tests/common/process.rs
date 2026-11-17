// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Stop supervised workloads before a timed-out test releases its private cache.

#![forbid(unsafe_code)]

use std::io;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use wait_timeout::ChildExt as _;

pub(crate) trait ChildExt {
    /// `data_home` must be the test's private XDG_DATA_HOME, never a user cache.
    fn kill_vm(&mut self, data_home: &Path) -> io::Result<()>;
}

impl ChildExt for Child {
    fn kill_vm(&mut self, data_home: &Path) -> io::Result<()> {
        // Stop the client first so it cannot reserve or launch more workloads.
        // A detached client's successful exit does not imply workload exit.
        let client = match self.try_wait() {
            Ok(Some(_)) => Ok(()),
            Ok(None) => self.kill().and_then(|()| self.wait().map(|_| ())),
            Err(error) => Err(error),
        };
        let records = match std::fs::read_dir(data_home.join("pichi/storage.workloads")) {
            Ok(records) => records,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return client,
            Err(error) => return Err(error),
        };
        let mut cleanup = Ok(());
        for record in records {
            let stopped = (|| {
                let record = record?;
                let identity: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(record.path().join("identity.json"))?)
                        .map_err(io::Error::other)?;
                let run = match identity["version"].as_u64() {
                    Some(1) => Some(record.path()),
                    Some(2) => match identity.get("attempt") {
                        Some(serde_json::Value::Null) => None,
                        Some(attempt) => {
                            let id = attempt["id"]
                                .as_str()
                                .ok_or_else(|| io::Error::other("missing boot ID"))?
                                .parse::<uuid::Uuid>()
                                .map_err(io::Error::other)?;
                            Some(record.path().join("runs").join(id.to_string()))
                        }
                        None => return Err(io::Error::other("missing instance attempt field")),
                    },
                    _ => return Err(io::Error::other("unknown workload record version")),
                };
                let mut control = Command::new(env!("CARGO_BIN_EXE_pichi"))
                    .env("XDG_DATA_HOME", data_home)
                    .args(["stop", "--time", "0"])
                    .arg(record.file_name())
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .spawn()?;
                // The product's forced-stop deadline is 30 seconds.
                let Some(status) = control.wait_timeout(Duration::from_secs(35))? else {
                    control.kill()?;
                    control.wait()?;
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "workload cleanup deadline",
                    ));
                };
                let Some(run) = run else {
                    if !status.success() {
                        return Err(io::Error::other("unstarted instance cleanup failed"));
                    }
                    // Exclude a launcher still preparing publication. An
                    // unselected monitor cannot execute the guest without Start.
                    let mut options = std::fs::File::options();
                    options.read(true).write(true);
                    let lease = options.open(record.path().join("operation.lock"))?;
                    lease.try_lock().map_err(io::Error::other)?;
                    drop(lease);
                    return Ok(());
                };
                let result: madrillo::Completion =
                    serde_json::from_slice(&std::fs::read(run.join("monitor/result.json"))?)
                        .map_err(io::Error::other)?;
                if result.cleanup != madrillo::CleanupResult::Complete {
                    return Err(io::Error::other(format!(
                        "workload cleanup is {:?}: {}",
                        result.cleanup,
                        record.path().display()
                    )));
                }
                Ok(())
            })();
            if let Err(error) = stopped {
                // One broken record must not leave other test VMs running.
                cleanup = Err(error);
            }
        }
        client.and(cleanup)
    }
}
