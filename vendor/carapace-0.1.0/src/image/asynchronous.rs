// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use super::{AsyncPlan, Loading, Metadata, Planned, BATCH, BLOCK};
use std::{
    io,
    pin::Pin,
    task::{ready, Context, Poll},
};
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
use {super::SparseFile, std::sync::Arc};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt, ReadBuf, SeekFrom};

impl AsyncPlan for Planned<tokio::fs::File> {
    async fn plan(mut image: tokio::fs::File) -> io::Result<Self> {
        let mut metadata = Metadata::new(&image.metadata().await?)?;
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        let query = Arc::new(image.try_clone().await?.into_std().await);
        let mut position = 0;
        let mut buffer = vec![0; BATCH];
        while position < metadata.image_size {
            #[cfg(any(target_os = "linux", target_os = "macos", windows))]
            let extent = {
                let file = query.clone();
                let size = metadata.image_size;
                tokio::task::spawn_blocking(move || file.next_extent(position, size))
                    .await
                    .map_err(io::Error::other)??
            };
            #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
            let extent = position..metadata.image_size;
            if extent.start == metadata.image_size {
                break;
            }
            position = extent.start / BLOCK as u64 * BLOCK as u64;
            let end = extent.end.div_ceil(BLOCK as u64) * BLOCK as u64;
            image.seek(SeekFrom::Start(position)).await?;
            while position < end.min(metadata.image_size) {
                let count = (end.min(metadata.image_size) - position).min(BATCH as u64) as usize;
                image.read_exact(&mut buffer[..count]).await?;
                metadata.inspect(position, &buffer[..count])?;
                position += count as u64;
            }
        }
        Ok(metadata.finish(image))
    }
}

impl AsyncRead for Planned<tokio::fs::File> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if output.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if this.failed {
            return Poll::Ready(Err(io::Error::other("planned reader failed")));
        }
        let result = this.poll_fill(cx);
        match ready!(result) {
            Ok(()) => {
                let n = output.remaining().min(this.available - this.used);
                output.put_slice(&this.block[this.used..this.used + n]);
                this.used += n;
                Poll::Ready(Ok(()))
            }
            Err(error) => {
                this.failed = true;
                Poll::Ready(Err(error))
            }
        }
    }
}

impl Planned<tokio::fs::File> {
    fn poll_fill(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.used < self.available {
            return Poll::Ready(Ok(()));
        }
        loop {
            match self.loading {
                Loading::Idle => {
                    let Some((offset, want)) = self.prepare_block() else {
                        return Poll::Ready(Ok(()));
                    };
                    Pin::new(&mut self.source).start_seek(SeekFrom::Start(offset))?;
                    self.loading = Loading::Seeking { want };
                }
                Loading::Seeking { want } => {
                    ready!(Pin::new(&mut self.source).poll_complete(cx))?;
                    self.loading = Loading::Reading { want, filled: 0 };
                }
                Loading::Reading { want, filled } => {
                    let mut buffer = ReadBuf::new(&mut self.block[filled..want]);
                    ready!(Pin::new(&mut self.source).poll_read(cx, &mut buffer))?;
                    let n = buffer.filled().len();
                    if n == 0 {
                        return Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()));
                    }
                    if filled + n == want {
                        self.ready();
                        self.loading = Loading::Idle;
                        return Poll::Ready(Ok(()));
                    }
                    self.loading = Loading::Reading {
                        want,
                        filled: filled + n,
                    };
                }
            }
        }
    }
}
