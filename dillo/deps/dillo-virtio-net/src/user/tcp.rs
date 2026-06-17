// SPDX-License-Identifier: Apache-2.0

//! One proxied TCP flow: a smoltcp TCP socket (the guest-facing half) bridged
//! to a [`mio::net::TcpStream`] (the host-facing half).
//!
//! The guest's connection is *terminated* by smoltcp; this module shuttles the
//! byte stream between that socket and a real host connection, preserving
//! half-close semantics and backpressure in both directions.

use std::io::{Read, Write};
use std::net::Shutdown;

use mio::Token;
use mio::net::TcpStream;
use smoltcp::socket::tcp;

/// State for one bridged TCP connection. The smoltcp socket lives in the
/// [`SocketSet`](smoltcp::iface::SocketSet); this owns the host side.
pub(super) struct TcpFlow {
    /// Host-side connection (created with a non-blocking connect).
    pub(super) stream: TcpStream,
    /// mio token this stream is registered under (for deregistration on close).
    pub(super) token: Token,
    /// The non-blocking connect has completed (peer address resolved).
    connected: bool,
    /// The guest connection has reached `Established` at least once. Until then
    /// `may_recv()` is false simply because the handshake isn't done — which
    /// must not be mistaken for the guest half-closing.
    established_seen: bool,
    /// We've shut down the host write half after the guest's FIN drained.
    host_write_closed: bool,
    /// The host peer closed (EOF or error); we've FIN'd the guest socket.
    host_read_closed: bool,
    /// A fatal host-side error means the guest connection must be reset (RST).
    pub(super) reset: bool,
}

impl std::fmt::Debug for TcpFlow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TcpFlow")
            .field("token", &self.token)
            .field("connected", &self.connected)
            .field("host_write_closed", &self.host_write_closed)
            .field("host_read_closed", &self.host_read_closed)
            .field("reset", &self.reset)
            .finish()
    }
}

impl TcpFlow {
    /// For an *outbound* flow: the host connect is still in flight (resolved by
    /// [`pump`](Self::pump) before any bytes move).
    pub(super) fn new(stream: TcpStream, token: Token) -> Self {
        Self {
            stream,
            token,
            connected: false,
            established_seen: false,
            host_write_closed: false,
            host_read_closed: false,
            reset: false,
        }
    }

    /// For an *inbound forward* flow: the host stream came from `accept`, so it
    /// is already connected.
    pub(super) fn new_connected(stream: TcpStream, token: Token) -> Self {
        Self {
            stream,
            token,
            connected: true,
            established_seen: false,
            host_write_closed: false,
            host_read_closed: false,
            reset: false,
        }
    }

    /// Pump bytes between the guest-facing `socket` and the host `stream` once.
    /// Returns `true` if any bytes moved in either direction (so the caller
    /// knows to keep draining before it sleeps).
    pub(super) fn pump(&mut self, socket: &mut tcp::Socket<'_>) -> bool {
        if !self.ensure_connected() {
            // Either still connecting, or a connect error already set `reset`.
            if self.reset {
                socket.abort();
            }
            return false;
        }

        // `may_send()` is true only from `Established` onward, so it marks the
        // point after which a dropped `may_recv()` genuinely means a guest FIN.
        if socket.may_send() {
            self.established_seen = true;
        }

        let mut worked = false;
        worked |= self.guest_to_host(socket);
        if self.reset {
            // A host-side failure: RST the guest. The next `iface.poll` flushes
            // the reset; the caller reaps the (now Closed) socket afterward.
            socket.abort();
            return worked;
        }
        self.maybe_shutdown_host_write(socket);
        worked |= self.host_to_guest(socket);
        worked
    }

    /// Resolve the non-blocking connect. Sets `reset` on a connect failure.
    /// Returns `true` once the host side is usable.
    fn ensure_connected(&mut self) -> bool {
        if self.connected {
            return true;
        }
        // A surfaced async error (e.g. ECONNREFUSED) means the connect failed.
        if let Ok(Some(_err)) = self.stream.take_error() {
            self.reset = true;
            return false;
        }
        match self.stream.peer_addr() {
            Ok(_) => {
                self.connected = true;
                true
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::NotConnected
                    || e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                // Still connecting; try again on the next wake.
                false
            }
            Err(_) => {
                self.reset = true;
                false
            }
        }
    }

    /// Drain the guest socket's receive buffer into the host stream, consuming
    /// exactly what the host accepted (the rest stays buffered → backpressure).
    fn guest_to_host(&mut self, socket: &mut tcp::Socket<'_>) -> bool {
        if !socket.can_recv() || self.host_write_closed {
            return false;
        }
        let result = socket.recv(|buf| match self.stream.write(buf) {
            Ok(n) => (n, Ok(n)),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => (0, Ok(0)),
            Err(e) => (0, Err(e)),
        });
        match result {
            Ok(Ok(n)) => n > 0,
            // recv error (socket not in a receivable state) or host write error.
            Ok(Err(_)) | Err(_) => {
                self.reset = true;
                false
            }
        }
    }

    /// Once the guest has sent its FIN (no more data will arrive and the buffer
    /// is drained), propagate it as a host write-shutdown.
    fn maybe_shutdown_host_write(&mut self, socket: &mut tcp::Socket<'_>) {
        if self.host_write_closed
            || !self.established_seen
            || socket.may_recv()
            || socket.can_recv()
        {
            return;
        }
        let _ = self.stream.shutdown(Shutdown::Write);
        self.host_write_closed = true;
    }

    /// Read from the host stream directly into the guest socket's send buffer,
    /// so we never read more than smoltcp can hold. Host EOF/error → FIN.
    fn host_to_guest(&mut self, socket: &mut tcp::Socket<'_>) -> bool {
        if !socket.can_send() || self.host_read_closed {
            return false;
        }
        let result = socket.send(|buf| match self.stream.read(buf) {
            Ok(0) => (0, ReadOutcome::Eof),
            Ok(n) => (n, ReadOutcome::Bytes(n)),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => (0, ReadOutcome::WouldBlock),
            Err(_) => (0, ReadOutcome::Eof),
        });
        match result {
            Ok(ReadOutcome::Bytes(n)) => n > 0,
            Ok(ReadOutcome::Eof) => {
                socket.close();
                self.host_read_closed = true;
                false
            }
            Ok(ReadOutcome::WouldBlock) | Err(_) => false,
        }
    }
}

enum ReadOutcome {
    Bytes(usize),
    Eof,
    WouldBlock,
}
