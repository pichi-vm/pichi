// SPDX-License-Identifier: Apache-2.0

//! User-mode networking backend: a no-privilege, cross-platform NAT.
//!
//! The guest sits on a private `/24` and the backend acts as a
//! **transport-terminating proxy**: it runs an in-process [`smoltcp`] TCP/IP
//! stack that locally terminates the guest's connections, then re-originates
//! them as ordinary host sockets (driven by a [`mio`] event loop). This is the
//! same model slirp/passt use, but in safe Rust and in-tree, so it needs no
//! `CAP_NET_ADMIN`, no `/dev/net/tun`, and works identically on Linux, macOS
//! and Windows.
//!
//! - **Outbound** (guest → internet) is masqueraded onto host sockets.
//! - **guest → host** is reached via the gateway IP ([`GATEWAY_IP`]).
//! - **Inbound** is supported via explicit [`Forward`] rules (host port →
//!   guest port), the only way packets initiate from outside (the guest has no
//!   routable address).
//!
//! The device's RX/TX workers talk to this backend through the unchanged
//! [`NetBackend`](crate::NetBackend) contract: [`send`](UserNetBackend::send)
//! pushes a guest frame into the stack; [`recv`](UserNetBackend::recv) pops a
//! frame the stack produced for the guest. All stack ownership lives on one
//! dedicated thread (see [`stack`]).

mod device;
mod forward;
mod stack;
mod tcp;
mod udp;

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use mio::{Poll, Token, Waker};
use smoltcp::iface::{Config, Interface};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address};

use crate::backend::{NetBackend, RECV_POLL};

pub use forward::{Forward, Proto};
#[doc(hidden)]
pub use stack::fuzz_inspect_frame;

// --- Addressing (slirp-compatible defaults, hardcoded in v1) ---------------

/// Guest's single assigned address.
pub(crate) const GUEST_IP: IpAddress = IpAddress::v4(10, 0, 2, 15);
/// Gateway / host alias the smoltcp interface owns. guest→host traffic targets
/// this; it is also the guest's default route.
pub(crate) const GATEWAY_IP: IpAddress = IpAddress::v4(10, 0, 2, 2);
/// DNS alias (reserved; DNS forwarding is a later nicety).
pub(crate) const DNS_IP: IpAddress = IpAddress::v4(10, 0, 2, 3);
/// Subnet prefix length for the private `/24`.
pub(crate) const SUBNET_PREFIX: u8 = 24;
/// Guest IP MTU.
pub(crate) const MTU: usize = 1500;
/// Full Ethernet frame MTU (IP MTU + 14-byte Ethernet header), which is what
/// smoltcp's `max_transmission_unit` means for an Ethernet medium.
pub(crate) const ETH_FRAME_MTU: usize = MTU + 14;

/// The MAC the smoltcp interface (the gateway side) presents. Locally
/// administered and distinct from any guest MAC; the guest learns it via ARP.
const GATEWAY_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x35, 0x02];

/// mio token for the [`Waker`] that nudges the stack thread out of `poll` when
/// the guest hands in a frame or the backend is dropped.
const WAKE_TOKEN: Token = Token(0);
/// First token handed out to dynamically registered sources (listeners/flows).
/// Above [`WAKE_TOKEN`] with room to spare.
const FIRST_DYNAMIC_TOKEN: usize = 16;

/// Per-direction TCP socket buffer for each proxied flow.
pub(crate) const TCP_BUFFER: usize = 64 * 1024;
/// UDP payload ring size per bound endpoint.
pub(crate) const UDP_PAYLOAD_BUFFER: usize = 64 * 1024;
/// UDP datagram-count metadata ring per bound endpoint.
pub(crate) const UDP_META_SLOTS: usize = 32;
/// Reclaim a UDP flow after this much idleness (no datagrams either way).
pub(crate) const UDP_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// The host-side socket bridge for one virtio-net device, in user mode.
pub struct UserNetBackend {
    /// Guest → stack frames awaiting ingestion.
    inbound: Arc<Mutex<VecDeque<Vec<u8>>>>,
    /// Stack → guest frames awaiting delivery, with a condvar so `recv` can
    /// block up to [`RECV_POLL`] without spinning.
    outbound: Arc<(Mutex<VecDeque<Vec<u8>>>, Condvar)>,
    /// Wakes the stack thread's `poll` when a frame is queued or on shutdown.
    waker: Arc<Waker>,
    /// Set on drop to unwind the stack thread.
    stop: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for UserNetBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserNetBackend")
            .field("running", &!self.stop.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl UserNetBackend {
    /// Build a user-mode backend with the given inbound port-`forwards` and
    /// start its stack thread. Fails only if a forward's host bind fails (e.g.
    /// the port is already taken) or a mio/poll resource can't be created.
    pub fn new(forwards: Vec<Forward>) -> io::Result<Self> {
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), WAKE_TOKEN)?);

        let inbound = Arc::new(Mutex::new(VecDeque::new()));
        let outbound = Arc::new((Mutex::new(VecDeque::new()), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let stack = stack::Stack::new(
            poll,
            forwards,
            Arc::clone(&inbound),
            Arc::clone(&outbound),
            Arc::clone(&stop),
        )?;

        let thread = std::thread::Builder::new()
            .name("virtio-net-user".into())
            .spawn(move || stack.run())?;

        Ok(Self {
            inbound,
            outbound,
            waker,
            stop,
            thread: Mutex::new(Some(thread)),
        })
    }
}

impl NetBackend for UserNetBackend {
    fn send(&self, frame: &[u8]) -> io::Result<()> {
        {
            let mut q = self.inbound.lock().expect("user-net inbound poisoned");
            if q.len() < device::MAX_QUEUE_FRAMES {
                q.push_back(frame.to_vec());
            }
            // Past the cap we drop, exactly as a NIC does on overrun.
        }
        // Nudge the stack thread to ingest it promptly.
        let _ = self.waker.wake();
        Ok(())
    }

    fn recv(&self, buf: &mut [u8]) -> io::Result<Option<usize>> {
        let (lock, cvar) = &*self.outbound;
        let mut q = lock.lock().expect("user-net outbound poisoned");
        if q.is_empty() {
            let (g, _timeout) = cvar
                .wait_timeout(q, RECV_POLL)
                .expect("user-net outbound poisoned");
            q = g;
        }
        match q.pop_front() {
            Some(frame) => {
                let n = frame.len().min(buf.len());
                buf[..n].copy_from_slice(&frame[..n]);
                Ok(Some(n))
            }
            None => Ok(None),
        }
    }
}

impl Drop for UserNetBackend {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.waker.wake();
        if let Some(handle) = self
            .thread
            .lock()
            .expect("user-net thread handle poisoned")
            .take()
        {
            let _ = handle.join();
        }
    }
}

// --- shared helpers --------------------------------------------------------

/// Build the smoltcp [`Interface`] over `device`: it owns the gateway/DNS
/// aliases, accepts packets to any destination (so it can terminate the
/// guest's outbound flows), and routes everything via itself.
fn build_interface(device: &mut device::ProxyDevice) -> Interface {
    let mut config = Config::new(EthernetAddress(GATEWAY_MAC).into());
    config.random_seed = random_seed();
    let mut iface = Interface::new(config, device, Instant::now());
    iface.update_ip_addrs(|addrs| {
        let _ = addrs.push(IpCidr::new(GATEWAY_IP, SUBNET_PREFIX));
        let _ = addrs.push(IpCidr::new(DNS_IP, SUBNET_PREFIX));
    });
    // A default route via the interface's own gateway IP, paired with
    // `set_any_ip`, lets smoltcp locally terminate connections to arbitrary
    // destinations (the crux of the user-mode NAT).
    let _ = iface
        .routes_mut()
        .add_default_ipv4_route(Ipv4Address::new(10, 0, 2, 2));
    iface.set_any_ip(true);
    iface
}

/// A non-deterministic seed for TCP ISN / ephemeral-port selection. This is a
/// local proxy with no ISN-prediction threat model, but a varying seed avoids
/// pathological reuse across rapid device re-creation.
fn random_seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x5254_0012_3502)
}

/// `smoltcp` IPv4 endpoint → std [`std::net::SocketAddr`]. The user-mode stack
/// is IPv4-only (built without smoltcp's `proto-ipv6`), so `IpAddress` has only
/// the `Ipv4` variant.
fn endpoint_to_socket_addr(ep: IpEndpoint) -> Option<std::net::SocketAddr> {
    let IpAddress::Ipv4(v4) = ep.addr;
    let o = v4.octets();
    Some(std::net::SocketAddr::from((
        std::net::Ipv4Addr::new(o[0], o[1], o[2], o[3]),
        ep.port,
    )))
}

#[cfg(test)]
mod tests;
