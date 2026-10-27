// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use super::{Metadata, Plan, Planned, SparseFile, BATCH, BLOCK};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
};

impl Plan for Planned<File> {
    fn plan(mut image: File) -> io::Result<Self> {
        let mut metadata = Metadata::new(&image.metadata()?)?;
        let mut position = 0;
        let mut buffer = vec![0; BATCH];
        while position < metadata.image_size {
            let extent = image.next_extent(position, metadata.image_size)?;
            if extent.start == metadata.image_size {
                break;
            }
            position = extent.start / BLOCK as u64 * BLOCK as u64;
            let end = extent.end.div_ceil(BLOCK as u64) * BLOCK as u64;
            image.seek(SeekFrom::Start(position))?;
            while position < end.min(metadata.image_size) {
                let count = (end.min(metadata.image_size) - position).min(BATCH as u64) as usize;
                image.read_exact(&mut buffer[..count])?;
                metadata.inspect(position, &buffer[..count])?;
                position += count as u64;
            }
        }
        Ok(metadata.finish(image))
    }
}

impl Read for Planned<File> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.failed {
            return Err(io::Error::other("planned reader failed"));
        }
        if self.used == self.available {
            if let Some((offset, want)) = self.prepare_block() {
                let result = self
                    .source
                    .seek(SeekFrom::Start(offset))
                    .and_then(|_| self.source.read_exact(&mut self.block[..want]));
                if let Err(error) = result {
                    self.failed = true;
                    return Err(error);
                }
                self.ready();
            }
        }
        Ok(self.copy(output))
    }
}
