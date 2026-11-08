// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Host console attachment. EOF releases input; the monitor retains its writer.

use super::{Client, Outcome};
use anyhow::{Context, Result, bail};
use madrillo::{ClientMessage, ServerMessage};
use std::io::{self, IsTerminal, Read, Write};
use tokio::sync::mpsc;

struct RawTerminal(bool);

impl RawTerminal {
    fn open(interactive: bool) -> Result<Self> {
        let raw = interactive && io::stdin().is_terminal();
        if raw {
            crossterm::terminal::enable_raw_mode()?;
        }
        Ok(Self(raw))
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        if self.0 {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

#[derive(Default)]
struct DetachKeys {
    prefix: bool,
}

impl DetachKeys {
    fn consume(&mut self, bytes: &[u8]) -> (Vec<u8>, bool) {
        let mut output = Vec::with_capacity(bytes.len() + 1);
        for &byte in bytes {
            if self.prefix {
                self.prefix = false;
                if byte == 17 {
                    return (output, true);
                }
                output.push(16);
            }
            if byte == 16 {
                self.prefix = true;
            } else {
                output.push(byte);
            }
        }
        (output, false)
    }
}

pub(super) struct Console {
    terminal: RawTerminal,
    input: mpsc::Receiver<io::Result<Vec<u8>>>,
    interactive: bool,
    keys: DetachKeys,
    signals: Signals,
}

impl Console {
    pub(super) fn open(interactive: bool) -> Result<Self> {
        let terminal = RawTerminal::open(interactive)?;
        let (sender, input) = mpsc::channel(8);
        if interactive {
            // A dedicated OS thread may block on stdin without preventing the
            // async runtime from shutting down after exit/detach. It owns no VM
            // or monitor resources. The bounded channel limits pending input.
            std::thread::Builder::new()
                .name("pichi-stdin".into())
                .spawn(move || {
                    let mut stdin = io::stdin().lock();
                    loop {
                        let mut bytes = vec![0; 8192];
                        let message = match stdin.read(&mut bytes) {
                            Ok(0) => break,
                            Ok(count) => {
                                bytes.truncate(count);
                                Ok(bytes)
                            }
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                            Err(error) => Err(error),
                        };
                        let failed = message.is_err();
                        if sender.blocking_send(message).is_err() || failed {
                            break;
                        }
                    }
                })?;
        }
        Ok(Self {
            terminal,
            input,
            interactive,
            keys: DetachKeys::default(),
            #[cfg(unix)]
            signals: Signals::open()?,
            #[cfg(windows)]
            signals: Signals {},
        })
    }

    pub(super) fn write(stream: &str, bytes: &[u8]) -> Result<()> {
        match stream {
            "stdout" => {
                let mut output = io::stdout().lock();
                output.write_all(bytes)?;
                output.flush()?;
            }
            "stderr" => {
                let mut output = io::stderr().lock();
                output.write_all(bytes)?;
                output.flush()?;
            }
            _ => bail!("unexpected console stream {stream}"),
        }
        Ok(())
    }

    pub(super) async fn view(
        mut self,
        client: &mut Client,
        resize: &std::path::Path,
    ) -> Result<u32> {
        let resize = super::resize::Resize(resize);
        let resize = resize.watch();
        tokio::pin!(resize);
        loop {
            tokio::select! {
                () = &mut resize, if self.terminal.0 && self.interactive => {},
                response = client.next() => match response? {
                    ServerMessage::Output { stream, bytes } => {
                        Self::write(&stream, &bytes)?;
                    }
                    ServerMessage::Completed { result } => return Outcome(result).code(),
                    ServerMessage::Error { code, message } => bail!("monitor {code:?}: {message}"),
                    response => bail!("unexpected monitor response: {response:?}"),
                },
                message = self.input.recv(), if self.interactive => {
                    let Some(bytes) = message else {
                        if self.keys.prefix {
                            client.send(ClientMessage::Input { bytes: vec![16] }).await?;
                        }
                        // Host EOF must not close dillo's liveness pipe.
                        client.send(ClientMessage::InputEnd).await?;
                        self.interactive = false;
                        continue;
                    };
                    let bytes = bytes.context("read console input")?;
                    let (bytes, detached) = if self.terminal.0 { self.keys.consume(&bytes) } else { (bytes, false) };
                    if !bytes.is_empty() { client.send(ClientMessage::Input { bytes }).await?; }
                    if detached {
                        client.send(ClientMessage::InputEnd).await?;
                        return Ok(0);
                    }
                },
                code = self.signals.cancelled() => return code,
            }
        }
    }
}

struct Signals {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(unix)]
    hangup: tokio::signal::unix::Signal,
}

impl Signals {
    #[cfg(unix)]
    fn open() -> Result<Self> {
        Ok(Self {
            #[cfg(unix)]
            interrupt: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
            #[cfg(unix)]
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
            #[cfg(unix)]
            hangup: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?,
        })
    }

    #[cfg(unix)]
    async fn cancelled(&mut self) -> Result<u32> {
        tokio::select! {
            _ = self.interrupt.recv() => Ok(130),
            _ = self.terminate.recv() => Ok(143),
            _ = self.hangup.recv() => Ok(129),
        }
    }

    #[cfg(windows)]
    async fn cancelled(&mut self) -> Result<u32> {
        tokio::signal::ctrl_c().await?;
        Ok(130)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_detach_keys_preserve_other_prefix_bytes() {
        let mut keys = DetachKeys::default();
        assert_eq!(keys.consume(&[0, 255, 16]), (vec![0, 255], false));
        assert_eq!(keys.consume(&[16, b'x', 16]), (vec![16, 16, b'x'], false));
        assert_eq!(keys.consume(&[17, b'z']), (Vec::new(), true));
    }
}
