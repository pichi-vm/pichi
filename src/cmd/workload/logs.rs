// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Decode complete retained CRI records without interpreting workload bytes.

use anyhow::{Context, Result, bail};
use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Take, Write},
};

// Madrillo captures 8 KiB chunks. Leave room for the CRI header while bounding
// allocations even if retained state is corrupt or modified by its owner.
const MAX_RECORD: u64 = 16 * 1024;

pub(super) struct Snapshot(BufReader<Take<File>>);

impl TryFrom<File> for Snapshot {
    type Error = anyhow::Error;

    fn try_from(file: File) -> Result<Self> {
        let length = file.metadata()?.len();
        Ok(Self(BufReader::new(file.take(length))))
    }
}

impl Snapshot {
    pub(super) fn write_to(
        mut self,
        stdout: &mut impl Write,
        stderr: &mut impl Write,
    ) -> Result<()> {
        let mut line = Vec::new();
        loop {
            line.clear();
            let count = (&mut self.0)
                .take(MAX_RECORD + 1)
                .read_until(b'\n', &mut line)?;
            if u64::try_from(count)? > MAX_RECORD {
                bail!("retained console record exceeds size limit");
            }
            // The snapshot can end between a writer's header and payload. Only
            // complete CRI records are visible; a later logs call can read it.
            if line.last() != Some(&b'\n') {
                break;
            }
            let mut fields = line.splitn(4, |byte| *byte == b' ');
            let timestamp = fields.next().context("console record timestamp")?;
            chrono::DateTime::parse_from_rfc3339(std::str::from_utf8(timestamp)?)
                .context("invalid console record timestamp")?;
            let stream = fields.next().context("console record stream")?;
            let flag = fields.next().context("console record flag")?;
            let payload = fields.next().context("console record payload")?;
            let payload = match flag {
                b"F" => payload,
                b"P" => &payload[..payload.len() - 1],
                _ => bail!("invalid console record flag"),
            };
            match stream {
                b"stdout" => stdout.write_all(payload)?,
                b"stderr" => stderr.write_all(payload)?,
                _ => bail!("invalid console record stream"),
            }
        }
        stdout.flush()?;
        stderr.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_preserves_binary_fragments_and_excludes_later_appends() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("console.log");
        std::fs::write(
            &path,
            b"2026-11-05T12:00:00.123456789+00:00 stdout P a\0\xff\n\
2026-11-05T12:00:00.123456789+00:00 stderr F error\n\
2026-11-05T12:00:00.123456789+00:00 stdout F b\n\
2026-11-05T12:00:00.123456789+00:00 stdout F \n\
2026-11-05T12:00:00.123456789+00:00 stdout P tail\n\
2026-11-05T12:00:00.123456789+00:00 stdout F incomplete",
        )?;
        let snapshot = Snapshot::try_from(File::open(&path)?)?;
        File::options().append(true).open(&path)?.write_all(b"\n")?;
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        snapshot.write_to(&mut stdout, &mut stderr)?;
        assert_eq!(stdout, b"a\0\xffb\n\ntail");
        assert_eq!(stderr, b"error\n");
        Ok(())
    }

    #[test]
    fn malformed_or_oversized_records_fail_without_unbounded_reads() -> Result<()> {
        for input in [
            b"not-a-timestamp stdout F bad\n".to_vec(),
            b"2026-11-05T12:00:00Z other F bad\n".to_vec(),
            b"2026-11-05T12:00:00Z stdout X bad\n".to_vec(),
            b"2026-11-05T12:00:00Z stdout P\n".to_vec(),
            vec![b'x'; 64 * 1024],
        ] {
            let root = tempfile::tempdir()?;
            let path = root.path().join("console.log");
            std::fs::write(&path, input)?;
            let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
            assert!(
                Snapshot::try_from(File::open(path)?)?
                    .write_to(&mut stdout, &mut stderr)
                    .is_err()
            );
            assert!(stdout.is_empty() && stderr.is_empty());
        }
        Ok(())
    }
}
