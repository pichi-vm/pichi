// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Console capture and failure diagnostics for quiet builds.

use anyhow::{Context as _, Result};
use std::{
    fs::File,
    io::{Read as _, Seek as _, SeekFrom},
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub(super) struct BuildLog {
    pub(super) file: File,
    path: PathBuf,
}

impl BuildLog {
    pub(super) fn create(runroot: &Path) -> Result<Self> {
        let directory = runroot.join("builds");
        std::fs::create_dir_all(&directory).context("creating build log directory")?;
        let (file, path) = tempfile::Builder::new()
            .prefix("build-")
            .suffix(".log")
            .tempfile_in(directory)?
            .keep()?;
        Ok(Self { file, path })
    }

    pub(super) fn failure(&self, error: anyhow::Error) -> anyhow::Error {
        match self.report() {
            Ok(report) if !report.is_empty() => eprintln!("\nBuild log:\n{report}"),
            Ok(_) => {}
            Err(err) => eprintln!("Could not read build log: {err}"),
        }
        error.context(format!("build failed; full log: {}", self.path.display()))
    }

    fn report(&self) -> Result<String> {
        let mut file = File::open(&self.path)?;
        let start = file.metadata()?.len().saturating_sub(64 * 1024);
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.take(64 * 1024).read_to_end(&mut bytes)?;
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<_> = text.lines().skip(usize::from(start > 0)).collect();
        // Appliance errors precede shutdown chatter. This is presentation only;
        // exit status and OCI validation still determine whether a build failed.
        if let Some(message) = lines
            .iter()
            .rev()
            .find_map(|line| line.split_once("conglobate: ").map(|(_, message)| message))
        {
            return Ok(format!("conglobate: {message}"));
        }
        Ok(lines[lines.len().saturating_sub(20)..].join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn keeps_full_log_and_limits_fallback_output() {
        let directory = tempfile::tempdir().unwrap();
        let mut log = BuildLog::create(directory.path()).unwrap();
        for n in 0..5000 {
            writeln!(log.file, "diagnostic line {n}").unwrap();
        }
        let report = log.report().unwrap();
        assert_eq!(report.lines().count(), 20);
        assert!(report.starts_with("diagnostic line 4980"));
        assert_eq!(
            std::fs::read_to_string(&log.path).unwrap().lines().count(),
            5000
        );
    }

    #[test]
    fn reports_appliance_error_before_shutdown_chatter() {
        let directory = tempfile::tempdir().unwrap();
        let mut log = BuildLog::create(directory.path()).unwrap();
        writeln!(
            log.file,
            "conglobate[12]: conglobate: submodule \"absent/input\" is missing"
        )
        .unwrap();
        for n in 0..200 {
            writeln!(log.file, "systemd-shutdown: stopping device {n}").unwrap();
        }
        assert_eq!(
            log.report().unwrap(),
            "conglobate: submodule \"absent/input\" is missing"
        );
    }
}
