// SPDX-License-Identifier: Apache-2.0

//! One proxied UDP flow: a smoltcp UDP socket bound to the guest's intended
//! destination, bridged to a connected [`mio::net::UdpSocket`] on the host.
//!
//! Flows are keyed by destination `(ip, port)` and track the most recent guest
//! source endpoint, which is where replies are returned. A single guest source
//! per destination is the common case (and exactly what the echo test
//! exercises); concurrent sources to the same host port is a v1 limitation.
//! Idle flows are reclaimed after [`UDP_IDLE_TIMEOUT`](super::UDP_IDLE_TIMEOUT).

use std::time::{Duration, Instant};

use mio::Token;
use mio::net::UdpSocket;
use smoltcp::socket::udp;
use smoltcp::wire::IpEndpoint;

/// State for one bridged UDP destination.
pub(super) struct UdpFlow {
    /// Host-side socket, connected to the destination.
    pub(super) sock: UdpSocket,
    /// mio token (for deregistration on reclaim).
    pub(super) token: Token,
    /// Where to send replies — the guest's source endpoint, refreshed on each
    /// outbound datagram. `None` until the first guest datagram is seen.
    guest_endpoint: Option<IpEndpoint>,
    /// Last time a datagram moved in either direction (for idle reclaim).
    last_active: Instant,
}

impl std::fmt::Debug for UdpFlow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UdpFlow")
            .field("token", &self.token)
            .field("guest_endpoint", &self.guest_endpoint)
            .finish_non_exhaustive()
    }
}

impl UdpFlow {
    pub(super) fn new(sock: UdpSocket, token: Token) -> Self {
        Self {
            sock,
            token,
            guest_endpoint: None,
            last_active: Instant::now(),
        }
    }

    /// Whether this flow has been idle longer than `timeout` as of `now`.
    pub(super) fn is_idle(&self, now: Instant, timeout: Duration) -> bool {
        now.saturating_duration_since(self.last_active) >= timeout
    }

    /// Pump datagrams between the guest socket and the host socket once.
    /// Returns `true` if any datagram moved.
    pub(super) fn pump(&mut self, socket: &mut udp::Socket<'_>) -> bool {
        let mut worked = false;
        worked |= self.guest_to_host(socket);
        worked |= self.host_to_guest(socket);
        worked
    }

    /// Forward every queued guest datagram to the host, remembering the guest
    /// source so replies can be returned to it.
    fn guest_to_host(&mut self, socket: &mut udp::Socket<'_>) -> bool {
        let mut worked = false;
        while socket.can_recv() {
            match socket.recv() {
                Ok((payload, meta)) => {
                    self.guest_endpoint = Some(meta.endpoint);
                    let _ = self.sock.send(payload);
                    self.last_active = Instant::now();
                    worked = true;
                }
                Err(_) => break,
            }
        }
        worked
    }

    /// Relay every queued host reply back to the remembered guest endpoint.
    fn host_to_guest(&mut self, socket: &mut udp::Socket<'_>) -> bool {
        let Some(guest) = self.guest_endpoint else {
            return false;
        };
        let mut worked = false;
        let mut buf = [0u8; 65_535];
        loop {
            match self.sock.recv(&mut buf) {
                Ok(n) => {
                    if socket.can_send() && socket.send_slice(&buf[..n], guest).is_ok() {
                        worked = true;
                    }
                    self.last_active = Instant::now();
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        worked
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_predicate_uses_last_active() {
        // A flow constructed "now" is not idle against a 60s timeout, but is
        // idle once enough simulated time elapses.
        let sock = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let flow = UdpFlow::new(sock, Token(1));
        let now = flow.last_active;
        assert!(!flow.is_idle(now, Duration::from_secs(60)));
        assert!(!flow.is_idle(now + Duration::from_secs(59), Duration::from_secs(60)));
        assert!(flow.is_idle(now + Duration::from_secs(60), Duration::from_secs(60)));
        assert!(flow.is_idle(now + Duration::from_secs(3600), Duration::from_secs(60)));
    }
}
