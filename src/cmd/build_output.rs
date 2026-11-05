// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Keep appliance output available without making it the build interface.

use anyhow::{Context as _, Result};
use std::{
    fs::File,
    io::{Read as _, Seek as _, SeekFrom},
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub(super) struct BuildLog {
    file: File,
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

    pub(super) fn attach(&self, command: &mut tokio::process::Command) -> Result<()> {
        command
            .stdin(std::process::Stdio::null())
            .stdout(self.file.try_clone()?)
            .stderr(self.file.try_clone()?);
        Ok(())
    }

    pub(super) fn failure(&self, error: anyhow::Error) -> anyhow::Error {
        // Read a bounded suffix, even if a verbose build wrote a very large log.
        match self.tail() {
            Ok(tail) if !tail.is_empty() => eprintln!("\nBuild log (last 20 lines):\n{tail}"),
            Ok(_) => {}
            Err(err) => eprintln!("Could not read build log: {err}"),
        }
        error.context(format!("build failed; full log: {}", self.path.display()))
    }

    fn tail(&self) -> Result<String> {
        let mut file = File::open(&self.path)?;
        let length = file.metadata()?.len();
        let start = length.saturating_sub(16 * 1024);
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.take(16 * 1024).read_to_end(&mut bytes)?;
        let text = String::from_utf8_lossy(&bytes);
        let mut lines: Vec<_> = text.lines().collect();
        if start > 0 && !lines.is_empty() {
            lines.remove(0);
        }
        Ok(lines[lines.len().saturating_sub(20)..].join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn retains_full_log_but_bounds_failure_tail() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = BuildLog::create(dir.path()).unwrap();
        for n in 0..1000 {
            writeln!(log.file, "line {n}: diagnostic data").unwrap();
        }
        let tail = log.tail().unwrap();
        assert_eq!(tail.lines().count(), 20);
        assert!(tail.starts_with("line 980:"));
        assert!(tail.ends_with("line 999: diagnostic data"));
        assert_eq!(
            std::fs::read_to_string(&log.path).unwrap().lines().count(),
            1000
        );
    }
}
