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
                let mut stop = Command::new(env!("CARGO_BIN_EXE_pichi"))
                    .env("XDG_DATA_HOME", data_home)
                    .args(["stop", "--time", "0"])
                    .arg(record.file_name())
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .spawn()?;
                // The product's forced-stop deadline is 30 seconds.
                if stop.wait_timeout(Duration::from_secs(35))?.is_none() {
                    stop.kill()?;
                    stop.wait()?;
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "workload cleanup deadline",
                    ));
                }
                let result: madrillo::Completion = serde_json::from_slice(&std::fs::read(
                    record.path().join("monitor/result.json"),
                )?)
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
