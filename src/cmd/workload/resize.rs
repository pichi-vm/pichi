// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Terminal dimensions go directly to Dillo; Madrillo only carries console bytes.

use anyhow::{Context, Result, bail};
use dillo_config::ConsoleSize;
use interprocess::local_socket::{
    GenericFilePath,
    tokio::{Stream, prelude::*},
};
use std::{path::Path, time::Duration};
use tokio::io::AsyncWriteExt;

pub(super) struct Resize<'a>(pub(super) &'a Path);

impl Resize<'_> {
    pub(super) async fn watch(&self) {
        let mut last = None;
        let mut failed = false;
        let mut interval = tokio::time::interval(Duration::from_millis(200));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            // Querying dimensions leaves the stdin reader in charge of every
            // byte, including on Windows where key/resize events share a queue.
            let Ok(size) = crossterm::terminal::size().and_then(|dimensions| {
                ConsoleSize::try_from(dimensions).map_err(std::io::Error::other)
            }) else {
                continue;
            };
            if last == Some(size) {
                continue;
            }
            match self.send(size).await {
                Ok(()) => {
                    last = Some(size);
                    failed = false;
                }
                Err(error) => {
                    // Dillo may still be preparing the VM when Madrillo reports
                    // process start. Retry the latest size without a backlog.
                    if !failed {
                        log::warn!("console resize: {error:#}; retrying");
                    }
                    failed = true;
                }
            }
        }
    }

    async fn send(&self, size: ConsoleSize) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(1), async {
            let mut stream =
                Connection(Stream::connect(self.0.to_fs_name::<GenericFilePath>()?).await?);
            #[cfg(unix)]
            if stream.0.peer_creds()?.euid() != Some(nix::unistd::geteuid().as_raw()) {
                bail!("resize endpoint belongs to another account");
            }
            #[cfg(windows)]
            {
                let Stream::NamedPipe(pipe) = &stream.0;
                let descriptor =
                    win_custody::ObjectSecurityDescriptor::query_kernel_object(pipe.inner())?;
                let user =
                    win_custody::OwnedSid::from_string(&win_custody::current_user_sid_string()?)?;
                if !descriptor.owner()?.matches(&user) {
                    bail!("resize endpoint belongs to another account");
                }
            }
            stream.0.write_all(&<[u8; 4]>::from(size)).await?;
            Ok(())
        })
        .await
        .context("console resize deadline")?
    }
}

struct Connection(Stream);

impl Drop for Connection {
    fn drop(&mut self) {
        // Cancellation/timeout must not leave an unbounded Windows pipe-flush
        // worker waiting for Dillo to read this disposable update.
        #[cfg(windows)]
        {
            let Stream::NamedPipe(pipe) = &self.0;
            pipe.inner().assume_flushed();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use interprocess::local_socket::ListenerOptions;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn resize_reconnects_and_delivers_before_client_close() -> Result<()> {
        let root = tempfile::tempdir()?;
        #[cfg(unix)]
        let endpoint = root.path().join("resize.sock");
        #[cfg(windows)]
        let endpoint = std::path::PathBuf::from(format!(
            r"\\.\pipe\pichi-resize-test-{}",
            uuid::Uuid::new_v4()
        ));
        let listener = ListenerOptions::new()
            .name(endpoint.as_path().to_fs_name::<GenericFilePath>()?)
            .create_tokio()?;
        let resize = Resize(&endpoint);
        for dimensions in [(300, 77), (120, 41)] {
            // Let the writer close before the reader consumes anything. This
            // exercises native named-pipe ownership as well as Unix sockets.
            resize
                .send(ConsoleSize::try_from(dimensions).map_err(std::io::Error::other)?)
                .await?;
            let mut stream = Connection(listener.accept().await?);
            let mut bytes = [0; 4];
            tokio::time::timeout(Duration::from_secs(2), stream.0.read_exact(&mut bytes)).await??;
            let size = ConsoleSize::try_from(bytes).map_err(std::io::Error::other)?;
            assert_eq!((size.cols.get(), size.rows.get()), dimensions);
        }
        drop(listener);
        assert!(
            resize
                .send(ConsoleSize::try_from((80, 24)).map_err(std::io::Error::other)?)
                .await
                .is_err()
        );
        drop(root);
        Ok(())
    }
}
