// SPDX-License-Identifier: Apache-2.0

//! Layer-1 datapath harness: the user-mode backend exercised end to end with a
//! **second smoltcp stack standing in for the guest**, and ordinary in-process
//! `std::net` servers standing in for the outside world. No VM, no privilege,
//! deterministic, and identical on every platform — so this is what proves the
//! any-ip + per-flow demux mechanism (the one real unknown) actually works.
//!
//! The guest stack's frames go in via [`NetBackend::send`]; the frames the
//! backend produces come back via [`NetBackend::recv`] (drained by a small pump
//! thread so the driver never blocks). Each test drives the guest stack until a
//! condition holds or a deadline expires.

#![allow(clippy::unwrap_used)]

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant as StdInstant};

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{
    EthernetAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint, Ipv4Address,
};

use crate::backend::NetBackend;

use super::{ETH_FRAME_MTU, Forward, GATEWAY_IP, GUEST_IP, Proto, SUBNET_PREFIX, UserNetBackend};

const GUEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0xAA, 0xBB, 0xCC];
const DEADLINE: Duration = Duration::from_secs(15);

// --- guest-side smoltcp stack ---------------------------------------------

/// A `phy::Device` whose two queues are shuttled to/from the backend by the
/// test driver: `tx` frames are sent into the backend, `rx` frames are the ones
/// the backend produced for the guest.
struct GuestDevice {
    rx: VecDeque<Vec<u8>>,
    tx: VecDeque<Vec<u8>>,
}

impl Device for GuestDevice {
    type RxToken<'a> = GuestRx;
    type TxToken<'a> = GuestTx<'a>;

    fn receive(&mut self, _t: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let frame = self.rx.pop_front()?;
        Some((GuestRx(frame), GuestTx(&mut self.tx)))
    }

    fn transmit(&mut self, _t: Instant) -> Option<Self::TxToken<'_>> {
        Some(GuestTx(&mut self.tx))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = ETH_FRAME_MTU;
        caps
    }
}

struct GuestRx(Vec<u8>);
impl RxToken for GuestRx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

struct GuestTx<'a>(&'a mut VecDeque<Vec<u8>>);
impl TxToken for GuestTx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = vec![0u8; len];
        let r = f(&mut buf);
        self.0.push_back(buf);
        r
    }
}

/// The guest stack plus the backend under test and the rx-pump plumbing.
struct Harness {
    backend: Arc<UserNetBackend>,
    iface: Interface,
    sockets: SocketSet<'static>,
    device: GuestDevice,
    rx_from_backend: Arc<Mutex<VecDeque<Vec<u8>>>>,
    stop: Arc<AtomicBool>,
    pump: Option<JoinHandle<()>>,
}

impl Harness {
    fn new(forwards: Vec<Forward>) -> Self {
        let backend = Arc::new(UserNetBackend::new(forwards).unwrap());

        let mut config = Config::new(EthernetAddress(GUEST_MAC).into());
        config.random_seed = 0x1234_5678;
        let mut device = GuestDevice {
            rx: VecDeque::new(),
            tx: VecDeque::new(),
        };
        let mut iface = Interface::new(config, &mut device, Instant::now());
        iface.update_ip_addrs(|addrs| {
            let _ = addrs.push(IpCidr::new(GUEST_IP, SUBNET_PREFIX));
        });
        let _ = iface
            .routes_mut()
            .add_default_ipv4_route(Ipv4Address::new(10, 0, 2, 2));
        let sockets = SocketSet::new(Vec::new());

        // Drain the backend's outbound frames on a thread so the driver never
        // blocks in `recv` (which waits up to RECV_POLL when idle).
        let rx_from_backend = Arc::new(Mutex::new(VecDeque::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let pump = {
            let backend = Arc::clone(&backend);
            let queue = Arc::clone(&rx_from_backend);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 65_535];
                while !stop.load(Ordering::Acquire) {
                    if let Ok(Some(n)) = backend.recv(&mut buf) {
                        queue.lock().unwrap().push_back(buf[..n].to_vec());
                    }
                }
            })
        };

        Self {
            backend,
            iface,
            sockets,
            device,
            rx_from_backend,
            stop,
            pump: Some(pump),
        }
    }

    /// One shuttle round: feed backend-produced frames into the guest stack,
    /// poll it, and hand its outbound frames to the backend.
    fn step(&mut self) {
        {
            let mut q = self.rx_from_backend.lock().unwrap();
            while let Some(frame) = q.pop_front() {
                self.device.rx.push_back(frame);
            }
        }
        self.iface
            .poll(Instant::now(), &mut self.device, &mut self.sockets);
        for frame in self.device.tx.drain(..) {
            let _ = self.backend.send(&frame);
        }
    }

    /// Drive until `cond` holds, returning `true`, or the deadline expires.
    fn run_until<F: FnMut(&mut Self) -> bool>(&mut self, mut cond: F) -> bool {
        let deadline = StdInstant::now() + DEADLINE;
        loop {
            self.step();
            if cond(self) {
                return true;
            }
            if StdInstant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn add_tcp(&mut self) -> SocketHandle {
        let socket = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; 64 * 1024]),
            tcp::SocketBuffer::new(vec![0u8; 64 * 1024]),
        );
        self.sockets.add(socket)
    }

    fn connect(&mut self, dst: IpEndpoint, local_port: u16) -> SocketHandle {
        let handle = self.add_tcp();
        let socket = self.sockets.get_mut::<tcp::Socket<'_>>(handle);
        socket
            .connect(self.iface.context(), dst, local_port)
            .unwrap();
        handle
    }

    fn listen(&mut self, port: u16) -> SocketHandle {
        let handle = self.add_tcp();
        let socket = self.sockets.get_mut::<tcp::Socket<'_>>(handle);
        socket.listen(port).unwrap();
        handle
    }

    fn tcp(&mut self, handle: SocketHandle) -> &mut tcp::Socket<'static> {
        self.sockets.get_mut::<tcp::Socket<'static>>(handle)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(p) = self.pump.take() {
            let _ = p.join();
        }
    }
}

// --- in-process servers ----------------------------------------------------

/// A loopback TCP echo server that echoes every byte until EOF, then closes.
/// Returns the bound port and the accept thread.
fn spawn_tcp_echo() -> (u16, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 16 * 1024];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if stream.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = stream.shutdown(Shutdown::Both);
        }
    });
    (port, handle)
}

/// A loopback UDP echo server: echoes a few datagrams back to their sender,
/// then exits (so `join` is quick). A handful covers retries without lingering.
fn spawn_udp_echo() -> (u16, JoinHandle<()>) {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(100))).ok();
    let port = sock.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        let deadline = StdInstant::now() + DEADLINE;
        let mut echoed = 0;
        while echoed < 1 && StdInstant::now() < deadline {
            match sock.recv_from(&mut buf) {
                Ok((n, peer)) => {
                    let _ = sock.send_to(&buf[..n], peer);
                    echoed += 1;
                }
                Err(_) => continue,
            }
        }
    });
    (port, handle)
}

/// A port that was bound then released — almost certainly free, so connecting
/// to it yields a connection-refused.
fn likely_closed_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

fn ipv4(a: u8, b: u8, c: u8, d: u8) -> IpAddress {
    IpAddress::Ipv4(Ipv4Address::new(a, b, c, d))
}

/// Send `payload` on `handle`, half-close, and collect the echo until we have
/// `payload.len()` bytes back (or the deadline expires). Returns the bytes read.
fn echo_exchange(h: &mut Harness, handle: SocketHandle, payload: &[u8]) -> Vec<u8> {
    let mut sent = 0usize;
    let mut closed = false;
    let mut got: Vec<u8> = Vec::new();
    let want = payload.len();
    let _ = h.run_until(|h| {
        let socket = h.tcp(handle);
        while socket.can_send() && sent < payload.len() {
            match socket.send_slice(&payload[sent..]) {
                Ok(0) => break,
                Ok(n) => sent += n,
                Err(_) => break,
            }
        }
        if sent == payload.len() && !closed {
            socket.close();
            closed = true;
        }
        while socket.can_recv() {
            if let Ok(chunk) = socket.recv(|b| (b.len(), b.to_vec())) {
                got.extend_from_slice(&chunk);
            } else {
                break;
            }
        }
        got.len() >= want
    });
    got
}

// --- tests -----------------------------------------------------------------

/// Outbound TCP to an arbitrary destination (masquerade): the guest dials
/// `127.0.0.1:port` off-subnet (routed via the gateway); the proxy terminates
/// it and re-originates to that literal destination — the in-process echo.
#[test]
fn outbound_tcp_masquerade() {
    let (port, server) = spawn_tcp_echo();
    let mut h = Harness::new(vec![]);
    let handle = h.connect(IpEndpoint::new(ipv4(127, 0, 0, 1), port), 49001);
    let got = echo_exchange(&mut h, handle, b"hello-masquerade");
    assert_eq!(
        got, b"hello-masquerade",
        "masquerade echo round-trip failed"
    );
    drop(h);
    let _ = server.join();
}

/// guest → host redirect: dialing the gateway IP folds to host loopback.
#[test]
fn gateway_to_host_redirect() {
    let (port, server) = spawn_tcp_echo();
    let mut h = Harness::new(vec![]);
    // Dial the gateway IP on the host server's port; the proxy maps it to
    // 127.0.0.1:port.
    let handle = h.connect(IpEndpoint::new(GATEWAY_IP, port), 49002);
    let got = echo_exchange(&mut h, handle, b"hello-gateway");
    assert_eq!(got, b"hello-gateway", "gateway→host redirect failed");
    drop(h);
    let _ = server.join();
}

/// Inbound port forward: an outside host connection lands on a guest listener.
#[test]
fn inbound_forward_tcp() {
    const GUEST_PORT: u16 = 4242;
    let host_port = likely_closed_port();
    let forwards = vec![Forward {
        proto: Proto::Tcp,
        host_ip: "127.0.0.1".parse().unwrap(),
        host_port,
        guest_port: GUEST_PORT,
    }];
    let mut h = Harness::new(forwards);
    let listen = h.listen(GUEST_PORT);

    // From the outside, connect to the forwarded host port and exchange data.
    let payload = b"hello-into-guest";
    let outside: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let echoed = Arc::clone(&outside);
    let client = std::thread::spawn(move || {
        // Retry briefly: the listener is bound, but give the stack a moment.
        let deadline = StdInstant::now() + DEADLINE;
        let mut stream = loop {
            match TcpStream::connect(("127.0.0.1", host_port)) {
                Ok(s) => break s,
                Err(_) if StdInstant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("forward connect failed: {e}"),
            }
        };
        stream.set_read_timeout(Some(DEADLINE)).ok();
        stream.write_all(payload).unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        let mut buf = Vec::new();
        let _ = stream.read_to_end(&mut buf);
        *echoed.lock().unwrap() = buf;
    });

    // The guest side: accept the forwarded connection, echo it back, then keep
    // driving until the *outside* peer has actually received the echo (the
    // guest's frames only leave its stack while we keep stepping).
    let mut accepted = false;
    let mut relayed = 0usize;
    let mut inbuf: Vec<u8> = Vec::new();
    let mut closed = false;
    let done = h.run_until(|h| {
        let socket = h.tcp(listen);
        if socket.is_active() {
            accepted = true;
        }
        while socket.can_recv() {
            if let Ok(chunk) = socket.recv(|b| (b.len(), b.to_vec())) {
                inbuf.extend_from_slice(&chunk);
            } else {
                break;
            }
        }
        while socket.can_send() && relayed < inbuf.len() {
            match socket.send_slice(&inbuf[relayed..]) {
                Ok(0) => break,
                Ok(n) => relayed += n,
                Err(_) => break,
            }
        }
        if !closed && relayed == payload.len() && inbuf.len() == payload.len() {
            socket.close();
            closed = true;
        }
        outside.lock().unwrap().len() >= payload.len()
    });
    assert!(accepted, "guest listener never became active");
    assert!(done, "outside peer never received the guest's echo");

    let _ = client.join();
    assert_eq!(
        &*outside.lock().unwrap(),
        payload,
        "outside peer did not get the guest's echo back"
    );
}

/// UDP outbound + reply through the gateway.
#[test]
fn udp_outbound_and_reply() {
    let (port, server) = spawn_udp_echo();
    let mut h = Harness::new(vec![]);
    let handle = h.sockets.add(udp::Socket::new(
        udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0u8; 64 * 1024]),
        udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0u8; 64 * 1024]),
    ));
    {
        let socket = h.sockets.get_mut::<udp::Socket<'_>>(handle);
        socket
            .bind(IpListenEndpoint {
                addr: None,
                port: 49003,
            })
            .unwrap();
    }

    let mut sent = false;
    let mut got: Vec<u8> = Vec::new();
    let ok = h.run_until(|h| {
        let socket = h.sockets.get_mut::<udp::Socket<'_>>(handle);
        if !sent && socket.can_send() {
            socket
                .send_slice(b"udp-ping", IpEndpoint::new(GATEWAY_IP, port))
                .unwrap();
            sent = true;
        }
        if socket.can_recv() {
            if let Ok((payload, _meta)) = socket.recv() {
                got = payload.to_vec();
            }
        }
        got == b"udp-ping"
    });
    assert!(ok, "UDP echo via gateway did not round-trip: got {got:?}");
    drop(h);
    let _ = server.join();
}

/// A connect to a refused host port resets the guest connection (RST).
///
/// The target port is one that only a *UDP* socket holds: a TCP connect to it
/// is refused on every platform (separate port spaces, no TCP listener), and
/// holding the socket keeps the port from being reused. This is deterministic
/// where the bind-then-drop trick is not — on Windows a just-closed listener
/// port briefly still accepts connects.
#[test]
fn connection_refused_resets_guest() {
    // Hold a UDP socket so the TCP port stays free of any TCP listener.
    let guard = UdpSocket::bind("127.0.0.1:0").unwrap();
    let dead_port = guard.local_addr().unwrap().port();

    let mut h = Harness::new(vec![]);
    let handle = h.connect(IpEndpoint::new(ipv4(127, 0, 0, 1), dead_port), 49004);
    let reset = h.run_until(|h| {
        let socket = h.tcp(handle);
        // The host connect is refused; the proxy RSTs the guest (immediately on
        // Unix, via the connect-timeout backstop on Windows) → Closed.
        matches!(socket.state(), tcp::State::Closed) && !socket.is_active()
    });
    drop(guard);
    assert!(reset, "guest connection to a refused port was not reset");
}

/// Half-close: the guest sends, closes its write half, and still receives the
/// echo of everything it sent.
#[test]
fn half_close_preserves_inbound() {
    let (port, server) = spawn_tcp_echo();
    let mut h = Harness::new(vec![]);
    let handle = h.connect(IpEndpoint::new(GATEWAY_IP, port), 49005);
    // `echo_exchange` closes after sending, then reads — i.e. a half-close.
    let got = echo_exchange(&mut h, handle, b"data-before-fin");
    assert_eq!(
        got, b"data-before-fin",
        "half-closed guest lost its inbound echo"
    );
    drop(h);
    let _ = server.join();
}

/// A multi-segment transfer (well beyond one MSS) round-trips intact, proving
/// windowing/backpressure in both directions.
#[test]
fn multi_segment_transfer() {
    let (port, server) = spawn_tcp_echo();
    let mut h = Harness::new(vec![]);
    let handle = h.connect(IpEndpoint::new(GATEWAY_IP, port), 49006);

    // 256 KiB of a recognizable pattern.
    let payload: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
    let got = echo_exchange(&mut h, handle, &payload);
    assert_eq!(got.len(), payload.len(), "multi-segment length mismatch");
    assert_eq!(got, payload, "multi-segment payload corrupted");
    drop(h);
    let _ = server.join();
}
