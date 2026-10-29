// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::collections::VecDeque;
use std::fmt;
use std::io::{BufRead, BufReader, Seek};
use std::path::Path;

use anyhow::{Context, Result, bail};

/// A fresh output mount, kept alive until the guest exits and import finishes.
pub(super) struct BuildOutput(tempfile::TempDir);

impl BuildOutput {
    pub(super) fn new() -> Result<Self> {
        Ok(Self(
            tempfile::tempdir().context("creating build output dir")?,
        ))
    }

    pub(super) fn check_empty(&self) -> Result<()> {
        if std::fs::read_dir(self.as_ref())?
            .next()
            .transpose()?
            .is_some()
        {
            bail!(
                "build output directory {} is not empty; refusing to launch build VM",
                self.as_ref().display()
            );
        }
        Ok(())
    }

    pub(super) fn verdict(&self) -> Result<OutputVerdict> {
        let names = std::fs::read_dir(self.as_ref())?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<std::io::Result<Vec<_>>>()?;
        if names
            .iter()
            .any(|name| name.to_string_lossy().starts_with(".conglobate-output-"))
        {
            return Ok(OutputVerdict::Partial);
        }
        let marker = names.iter().any(|name| name == "oci-layout");
        let index = names.iter().any(|name| name == "index.json");
        Ok(match (marker, index) {
            (true, true) => OutputVerdict::Complete,
            (false, true) => OutputVerdict::MissingMarker("oci-layout"),
            (true, false) => OutputVerdict::MissingMarker("index.json"),
            (false, false) if names.is_empty() => OutputVerdict::Empty,
            (false, false) if names.len() == 1 && names[0] == "blobs" => OutputVerdict::BlobsOnly,
            (false, false) => OutputVerdict::MissingMarker("oci-layout and index.json"),
        })
    }
}

impl AsRef<Path> for BuildOutput {
    fn as_ref(&self) -> &Path {
        self.0.path()
    }
}

/// Complete means ready for Open Container Initiative (OCI) import; validation
/// still checks the full image layout.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum OutputVerdict {
    Complete,
    MissingMarker(&'static str),
    Partial,
    BlobsOnly,
    Empty,
}

impl fmt::Display for OutputVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Complete => f.write_str("build output has both OCI markers"),
            Self::MissingMarker(name) => write!(f, "build output is missing {name}"),
            Self::Partial => f.write_str(
                "build stopped partway through: unfinished .conglobate-output-* staging directory",
            ),
            Self::BlobsOnly => {
                f.write_str("build stopped partway through: blobs without oci-layout or index.json")
            }
            Self::Empty => f.write_str("build produced no output (output directory is empty)"),
        }
    }
}

/// Both console output and launcher diagnostics share this temporary log.
pub(super) struct ConsoleLog(std::fs::File);

impl ConsoleLog {
    pub(super) fn new() -> Result<Self> {
        Ok(Self(
            tempfile::tempfile().context("creating build console log")?,
        ))
    }

    pub(super) fn capture(&self, cmd: &mut tokio::process::Command) -> Result<()> {
        cmd.stdout(self.0.try_clone()?).stderr(self.0.try_clone()?);
        Ok(())
    }

    pub(super) fn failure(&mut self, error: anyhow::Error) -> anyhow::Error {
        match self.tail() {
            Ok(mut tail) => {
                eprintln!(
                    "build VM console tail (up to 40 lines):\n{}",
                    tail.lines.make_contiguous().join("\n")
                );
                if let Some(reason) = tail.reason {
                    return error.context(reason);
                }
            }
            Err(log_error) => eprintln!("could not read build VM console log: {log_error:#}"),
        }
        error
    }

    fn tail(&mut self) -> Result<ConsoleTail> {
        self.0.rewind()?;
        let mut tail = ConsoleTail::default();
        for bytes in BufReader::new(&self.0).split(b'\n') {
            let bytes = bytes?;
            let line = String::from_utf8_lossy(&bytes);
            tail.push(line.trim_end_matches('\r'));
        }
        Ok(tail)
    }
}

#[derive(Default)]
struct ConsoleTail {
    lines: VecDeque<String>,
    reason: Option<String>,
}

impl ConsoleTail {
    fn push(&mut self, line: &str) {
        // Keep the last reason even when shutdown noise pushes it out of the
        // visible tail. Read the log a line at a time, not all into memory.
        if let Some(start) = line.find("conglobate: ") {
            self.reason = Some(line[start..].to_owned());
        }
        if self.lines.len() == 40 {
            self.lines.pop_front();
        }
        self.lines.push_back(line.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn output_verdicts() {
        for (entries, expected) in [
            (
                vec!["oci-layout", "index.json", "blobs"],
                OutputVerdict::Complete,
            ),
            (
                vec!["index.json", "blobs"],
                OutputVerdict::MissingMarker("oci-layout"),
            ),
            (
                vec!["oci-layout"],
                OutputVerdict::MissingMarker("index.json"),
            ),
            (vec![".conglobate-output-abcd"], OutputVerdict::Partial),
            (
                vec![".conglobate-output-abcd", "blobs", "index.json"],
                OutputVerdict::Partial,
            ),
            (vec!["blobs"], OutputVerdict::BlobsOnly),
            (vec![], OutputVerdict::Empty),
            (
                vec!["unexpected"],
                OutputVerdict::MissingMarker("oci-layout and index.json"),
            ),
        ] {
            let output = BuildOutput::new().unwrap();
            for name in entries {
                let path = output.as_ref().join(name);
                if name == "blobs" || name.starts_with(".conglobate-output-") {
                    std::fs::create_dir(path).unwrap();
                } else {
                    std::fs::write(path, b"{}").unwrap();
                }
            }
            assert_eq!(output.verdict().unwrap(), expected);
            assert!(!expected.to_string().is_empty());
        }
    }

    #[test]
    fn refuses_nonempty_output_before_launch() {
        let output = BuildOutput::new().unwrap();
        output.check_empty().unwrap();
        std::fs::write(output.as_ref().join(".stale"), b"old build").unwrap();
        assert!(
            output
                .check_empty()
                .unwrap_err()
                .to_string()
                .contains("is not empty")
        );
        let fresh = BuildOutput::new().unwrap();
        fresh.check_empty().unwrap();
        assert_ne!(output.as_ref(), fresh.as_ref());
    }

    #[test]
    fn console_tail_picks_last_conglobate_reason() {
        let mut log = ConsoleLog::new().unwrap();
        for _ in 0..100 {
            writeln!(log.0, "old boot noise").unwrap();
        }
        writeln!(log.0, "conglobate: first error").unwrap();
        writeln!(log.0, "[guest] conglobate: invalid plan\r").unwrap();
        for _ in 0..100 {
            writeln!(log.0, "Powering off.").unwrap();
        }
        let tail = log.tail().unwrap();
        assert_eq!(tail.lines.len(), 40);
        assert!(tail.lines.iter().all(|line| line == "Powering off."));
        assert_eq!(tail.reason.as_deref(), Some("conglobate: invalid plan"));
        assert_eq!(ConsoleLog::new().unwrap().tail().unwrap().reason, None);
    }
}
