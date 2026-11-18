// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Host console attachment. EOF releases input; the monitor retains its writer.

use super::{Client, Outcome};
use anyhow::{Context, Result, bail};
use madrillo::{ClientMessage, ErrorCode, ServerMessage};
use std::collections::VecDeque;
use std::io::{self, IsTerminal, Read, Write};
use tokio::sync::mpsc;

/// Cap queued stdin batches beyond the bounded reader channel and active write.
const READ_AHEAD: usize = 8;

struct RawTerminal {
    raw: bool,
    // Windows disable_raw_mode sets cooked bits instead of restoring the
    // original mode, which may already have been raw.
    #[cfg(windows)]
    original: Option<(crossterm_winapi::ConsoleMode, u32)>,
}

impl RawTerminal {
    fn open(raw: bool) -> Result<Self> {
        #[cfg(windows)]
        let original = if raw {
            let console =
                crossterm_winapi::ConsoleMode::from(crossterm_winapi::Handle::input_handle()?);
            let mode = console.mode()?;
            Some((console, mode))
        } else {
            None
        };
        if raw {
            crossterm::terminal::enable_raw_mode()?;
        }
        Ok(Self {
            raw,
            #[cfg(windows)]
            original,
        })
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Some((console, mode)) = &self.original {
            let _ = console.set_mode(*mode);
        }
        #[cfg(not(windows))]
        if self.raw {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

/// Restore the output buffer's exact mode when the attachment ends.
#[cfg(windows)]
struct TerminalOutput(crossterm_winapi::ConsoleMode, u32);

#[cfg(windows)]
impl TerminalOutput {
    fn open() -> Result<Option<Self>> {
        // SetConsoleMode requires processed output alongside VT processing.
        const ENABLE_PROCESSED_OUTPUT: u32 = 0x0001;
        const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
        let handle = crossterm_winapi::Handle::output_handle()?;
        // GetConsoleMode also accepts input buffers, whose bits have different
        // meanings. Apply output flags only to a readable screen buffer.
        if crossterm_winapi::ScreenBuffer::new(handle.clone())
            .info()
            .is_err()
        {
            return Ok(None);
        }
        let console = crossterm_winapi::ConsoleMode::from(handle);
        // A writable handle need not allow reading its console mode. Without
        // a snapshot, leave the mode untouched and keep forwarding bytes.
        let Ok(original) = console.mode() else {
            return Ok(None);
        };
        console
            .set_mode(original | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING)?;
        Ok(Some(Self(console, original)))
    }
}

#[cfg(windows)]
impl Drop for TerminalOutput {
    fn drop(&mut self) {
        let _ = self.0.set_mode(self.1);
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
    #[cfg(windows)]
    _output: Option<TerminalOutput>,
    sized: bool,
    input: mpsc::Receiver<io::Result<Vec<u8>>>,
    interactive: bool,
    keys: DetachKeys,
    signals: Signals,
}

impl Console {
    pub(super) fn open(interactive: bool, tty: bool) -> Result<Self> {
        #[cfg(windows)]
        let output = if tty { TerminalOutput::open()? } else { None };
        let sized = tty && io::stdin().is_terminal();
        let terminal = RawTerminal::open(interactive && sized)?;
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
            #[cfg(windows)]
            _output: output,
            sized,
            input,
            interactive,
            keys: DetachKeys::default(),
            signals: Signals::open()?,
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
        let (writer, reader) = client.halves();
        let resize = super::resize::Resize(resize);
        let resize = resize.watch();
        tokio::pin!(resize);
        let mut staged = VecDeque::new();
        let mut idle = Some(writer);
        // Keep a partially sent frame alive across select iterations.
        let mut flight = std::pin::pin!(None);
        let mut forward = self.interactive;
        loop {
            if flight.is_none()
                && let Some(message) = staged.pop_front()
                && let Some(writer) = idle.take()
            {
                flight.set(Some(async move {
                    let sent = writer.send(message).await;
                    (writer, sent)
                }));
            }
            tokio::select! {
                () = &mut resize, if self.sized => {},
                response = reader.next() => match response? {
                    ServerMessage::Output { stream, bytes } => {
                        Self::write(&stream, &bytes)?;
                    }
                    // An attachment can precede authorization of the boot.
                    ServerMessage::Started { .. } => {},
                    ServerMessage::Completed { result } => return Outcome(result).code(),
                    ServerMessage::Error { code: ErrorCode::InputClosed, .. } => {
                        // Keep receiving output and recognizing detach keys.
                        forward = false;
                        staged.clear();
                    }
                    ServerMessage::Error { code, message } => bail!("monitor {code:?}: {message}"),
                    response => bail!("unexpected monitor response: {response:?}"),
                },
                (writer, sent) = async { flight.as_mut().as_pin_mut().expect("guard").await }, if flight.is_some() => {
                    flight.set(None);
                    idle = Some(writer);
                    sent?;
                },
                message = self.input.recv(), if self.interactive && staged.len() < READ_AHEAD => {
                    let Some(bytes) = message else {
                        if forward {
                            if self.keys.prefix {
                                staged.push_back(ClientMessage::Input { bytes: vec![16] });
                            }
                            // Release only this attachment, after all its input.
                            staged.push_back(ClientMessage::InputEnd);
                        }
                        self.interactive = false;
                        self.sized = false;
                        continue;
                    };
                    let bytes = bytes.context("read console input")?;
                    let (bytes, detached) = if self.terminal.raw { self.keys.consume(&bytes) } else { (bytes, false) };
                    if detached {
                        // Closing this connection releases input without waiting
                        // for the guest to consume outstanding writes.
                        return Ok(0);
                    }
                    if forward && !bytes.is_empty() {
                        staged.push_back(ClientMessage::Input { bytes });
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
    quit: tokio::signal::unix::Signal,
    #[cfg(unix)]
    hangup: tokio::signal::unix::Signal,
}

impl Signals {
    fn open() -> Result<Self> {
        Ok(Self {
            #[cfg(unix)]
            interrupt: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
            #[cfg(unix)]
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
            #[cfg(unix)]
            quit: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::quit())?,
            #[cfg(unix)]
            hangup: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?,
        })
    }

    #[cfg(unix)]
    async fn cancelled(&mut self) -> Result<u32> {
        tokio::select! {
            _ = self.interrupt.recv() => Ok(130),
            _ = self.terminate.recv() => Ok(143),
            _ = self.quit.recv() => Ok(131),
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
