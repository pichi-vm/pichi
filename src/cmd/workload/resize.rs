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
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
                Err(error)
                    if last.is_none()
                        && error
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
                {
                    // Dillo creates its listener after the monitor starts it.
                    log::debug!("console resize endpoint is not ready; retrying");
                }
                Err(error) => {
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
            // A synchronous Windows pipe listener discards connections that
            // close before accept. Retain this writer until the receiver has
            // consumed the update and closed its end; there is no reply frame.
            if stream.0.read(&mut [0]).await? != 0 {
                bail!("unexpected console resize response");
            }
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
    use interprocess::local_socket::{ListenerNonblockingMode, ListenerOptions, prelude::*};
    use std::{io::Read as _, time::Instant};

    #[tokio::test]
    async fn resize_reconnects_after_receiver_consumes_each_update() -> Result<()> {
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
            .nonblocking(ListenerNonblockingMode::Accept)
            .create_sync()?;
        let receiver = std::thread::spawn(move || -> Result<()> {
            for dimensions in [(300, 77), (120, 41)] {
                // Dillo polls a synchronous listener. Delay accept so that
                // prematurely closing a Windows client loses the update.
                std::thread::sleep(Duration::from_millis(100));
                let deadline = Instant::now() + Duration::from_secs(3);
                let mut stream = loop {
                    match listener.accept() {
                        Ok(stream) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            anyhow::ensure!(Instant::now() < deadline, "resize accept deadline");
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => return Err(error.into()),
                    }
                };
                let mut bytes = [0; 4];
                stream.read_exact(&mut bytes)?;
                let size = ConsoleSize::try_from(bytes).map_err(std::io::Error::other)?;
                assert_eq!((size.cols.get(), size.rows.get()), dimensions);
                // Closing after consumption lets the client finish without
                // a native FlushFileBuffers worker or a reply message.
            }
            Ok(())
        });
        let resize = Resize(&endpoint);
        let sent: Result<()> = async {
            for dimensions in [(300, 77), (120, 41)] {
                resize
                    .send(ConsoleSize::try_from(dimensions).map_err(std::io::Error::other)?)
                    .await?;
            }
            Ok(())
        }
        .await;
        let received = receiver.join().expect("resize fixture panicked");
        sent?;
        received?;
        assert!(
            resize
                .send(ConsoleSize::try_from((80, 24)).map_err(std::io::Error::other)?)
                .await
                .is_err()
        );
        let listener = ListenerOptions::new()
            .name(endpoint.as_path().to_fs_name::<GenericFilePath>()?)
            .create_tokio()?;
        let receive_without_closing = async {
            let mut stream = Connection(listener.accept().await?);
            stream.0.read_exact(&mut [0; 4]).await?;
            Ok::<_, std::io::Error>(stream)
        };
        let (sent, retained) = tokio::join!(
            resize.send(ConsoleSize::try_from((80, 24)).map_err(std::io::Error::other)?),
            receive_without_closing,
        );
        assert!(sent.unwrap_err().to_string().contains("resize deadline"));
        drop(retained?);
        drop(listener);
        drop(root);
        Ok(())
    }
}
