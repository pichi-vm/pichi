// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Keep timed-out test launches from leaving a VM on a shared runner.

use std::io;
use std::process::{Child, Command};

pub trait CommandExt {
    fn spawn_vm(&mut self) -> io::Result<Child>;
}

impl CommandExt for Command {
    fn spawn_vm(&mut self) -> io::Result<Child> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            self.process_group(0);
        }
        self.spawn()
    }
}

pub trait ChildExt {
    fn kill_vm(&mut self) -> io::Result<()>;
}

impl ChildExt for Child {
    fn kill_vm(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        {
            let group = nix::unistd::Pid::from_raw(self.id().try_into().unwrap());
            nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL)?;
        }
        #[cfg(windows)]
        {
            let output = Command::new("taskkill")
                .args(["/F", "/T", "/PID", &self.id().to_string()])
                .output()?;
            if !output.status.success() {
                return Err(io::Error::other(format!(
                    "kill VM process tree: {output:?}"
                )));
            }
        }
        self.wait()?;
        Ok(())
    }
}
