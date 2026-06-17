// SPDX-License-Identifier: Apache-2.0

//! The user-mode stack thread: the single owner of the smoltcp [`Interface`],
//! the [`SocketSet`], the [`ProxyDevice`] queues, the mio [`Poll`], and every
//! per-flow host socket. Nothing else touches these, so no locking is needed
//! around them — the only cross-thread state is the two frame queues and the
//! stop flag, all behind their own synchronization.
//!
//! Each iteration: ingest guest frames (demuxing new flows into freshly
//! provisioned smoltcp sockets), drive smoltcp + the host sockets to quiescence,
//! reap finished flows, hand outbound frames to the guest, then block in
//! `mio::Poll` until a host socket is ready or the backend nudges the waker.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration as StdDuration;

use mio::net::{TcpListener, TcpStream, UdpSocket};
use mio::{Events, Interest, Poll, Token};
use smoltcp::iface::{Interface, SocketHandle, SocketSet};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{
    EthernetFrame, EthernetProtocol, IpAddress, IpEndpoint, IpListenEndpoint, IpProtocol,
    Ipv4Address, Ipv4Packet, TcpPacket, UdpPacket,
};

use super::device::ProxyDevice;
use super::forward::{Forward, ForwardListener, Proto};
use super::tcp::TcpFlow;
use super::udp::UdpFlow;
use super::{
    FIRST_DYNAMIC_TOKEN, GATEWAY_IP, GUEST_IP, TCP_BUFFER, UDP_IDLE_TIMEOUT, UDP_META_SLOTS,
    UDP_PAYLOAD_BUFFER, build_interface, endpoint_to_socket_addr,
};

/// Host loopback, where gateway-destined (guest→host) connections are sent.
const HOST_LOOPBACK: IpAddress = IpAddress::v4(127, 0, 0, 1);
/// Longest the stack sleeps between wakeups, so it stays responsive to the stop
/// flag and to UDP idle reclamation even with no socket activity.
const MAX_POLL_WAIT: StdDuration = StdDuration::from_millis(100);
/// Bound on the inner drive loop so a busy flow can't starve the wait/stop check.
const MAX_DRIVE_ROUNDS: usize = 8;
/// Ephemeral local ports for stack-originated connections start here.
const EPHEMERAL_BASE: u16 = 49152;

/// All state owned by the user-mode stack thread.
pub(super) struct Stack {
    poll: Poll,
    events: Events,
    device: ProxyDevice,
    iface: Interface,
    sockets: SocketSet<'static>,

    inbound: Arc<Mutex<VecDeque<Vec<u8>>>>,
    outbound: Arc<(Mutex<VecDeque<Vec<u8>>>, Condvar)>,
    stop: Arc<AtomicBool>,

    /// Next mio token to hand out for a dynamically registered source.
    next_token: usize,
    /// Next ephemeral local port for a stack-originated connection.
    next_ephemeral: u16,

    /// Established/connecting outbound + inbound-forward TCP flows.
    tcp_flows: HashMap<SocketHandle, TcpFlow>,
    /// Destinations with a smoltcp socket currently in `Listen` awaiting a SYN.
    tcp_listening: HashMap<(IpAddress, u16), SocketHandle>,
    /// Outbound UDP flows, keyed by guest destination `(ip, port)`.
    udp_flows: HashMap<(IpAddress, u16), (SocketHandle, UdpFlow)>,
    /// Local ports the stack uses for inbound UDP forwards toward the guest, so
    /// the guest's replies aren't mistaken for new outbound flows.
    forward_udp_local_ports: HashSet<u16>,
    /// Sockets to drop after the next poll flushes their final segment (RST/FIN).
    pending_removal: Vec<SocketHandle>,

    /// Bound, mio-registered inbound forward listeners.
    forwards: Vec<ForwardListener>,
}

impl Stack {
    /// Build the stack: the smoltcp interface over fresh queues, plus every
    /// inbound forward bound and registered with `poll`.
    pub(super) fn new(
        poll: Poll,
        forwards: Vec<Forward>,
        inbound: Arc<Mutex<VecDeque<Vec<u8>>>>,
        outbound: Arc<(Mutex<VecDeque<Vec<u8>>>, Condvar)>,
        stop: Arc<AtomicBool>,
    ) -> std::io::Result<Self> {
        let mut device = ProxyDevice::new();
        let iface = build_interface(&mut device);
        let sockets = SocketSet::new(Vec::new());

        let mut next_token = FIRST_DYNAMIC_TOKEN;
        let mut bound = Vec::with_capacity(forwards.len());
        for f in forwards {
            let addr = SocketAddr::new(f.host_ip, f.host_port);
            let token = Token(next_token);
            next_token += 1;
            match f.proto {
                Proto::Tcp => {
                    let mut listener = TcpListener::bind(addr)?;
                    poll.registry()
                        .register(&mut listener, token, Interest::READABLE)?;
                    bound.push(ForwardListener::Tcp {
                        listener,
                        guest_port: f.guest_port,
                        token,
                    });
                }
                Proto::Udp => {
                    let mut sock = UdpSocket::bind(addr)?;
                    poll.registry()
                        .register(&mut sock, token, Interest::READABLE)?;
                    bound.push(ForwardListener::Udp {
                        sock,
                        guest_port: f.guest_port,
                        token,
                        outside: None,
                        guest_socket: None,
                    });
                }
            }
        }

        Ok(Self {
            poll,
            events: Events::with_capacity(256),
            device,
            iface,
            sockets,
            inbound,
            outbound,
            stop,
            next_token,
            next_ephemeral: EPHEMERAL_BASE,
            tcp_flows: HashMap::new(),
            tcp_listening: HashMap::new(),
            udp_flows: HashMap::new(),
            forward_udp_local_ports: HashSet::new(),
            pending_removal: Vec::new(),
            forwards: bound,
        })
    }

    /// Run until the stop flag is set (on backend drop).
    pub(super) fn run(mut self) {
        while !self.stop.load(Ordering::Acquire) {
            self.ingest_inbound();

            let mut rounds = 0;
            loop {
                let now = Instant::now();
                self.iface.poll(now, &mut self.device, &mut self.sockets);
                self.promote_listeners();
                self.accept_forwards();
                let worked = self.pump_all();
                // Flush any FIN/RST/ACK that pumping (close/abort) generated
                // before we reap the now-closed sockets.
                self.iface
                    .poll(Instant::now(), &mut self.device, &mut self.sockets);
                self.reap_closed();
                rounds += 1;
                if !worked || rounds >= MAX_DRIVE_ROUNDS {
                    break;
                }
            }

            self.reclaim_idle_udp();
            self.drain_to_guest();

            let now = Instant::now();
            let wait = mio_wait(self.iface.poll_delay(now, &self.sockets));
            let _ = self.poll.poll(&mut self.events, Some(wait));
            // Events are pure wakeups; every source is serviced each iteration,
            // so there is nothing to route by token here.
        }
    }

    // --- ingest + demux ----------------------------------------------------

    /// Move every queued guest frame into the smoltcp device, provisioning a
    /// listening/bound socket for any frame that opens a new flow.
    fn ingest_inbound(&mut self) {
        let frames: Vec<Vec<u8>> = {
            let mut q = self.inbound.lock().expect("user-net inbound poisoned");
            q.drain(..).collect()
        };
        for frame in frames {
            self.demux_and_push(frame);
        }
    }

    fn demux_and_push(&mut self, frame: Vec<u8>) {
        if let Some((proto, dst, dst_port, opens)) = inspect_frame(&frame) {
            match proto {
                IpProtocol::Tcp if opens => self.provision_tcp_listener(dst, dst_port),
                IpProtocol::Udp => self.provision_udp_flow(dst, dst_port),
                _ => {}
            }
        }
        // ARP, ICMP, established-flow segments, etc. are all handled by smoltcp.
        self.device.push_rx(frame);
    }

    fn provision_tcp_listener(&mut self, dst: Ipv4Address, port: u16) {
        let key = (IpAddress::Ipv4(dst), port);
        if self.tcp_listening.contains_key(&key) {
            return;
        }
        let mut sock = make_tcp_socket();
        let endpoint = IpListenEndpoint {
            addr: Some(IpAddress::Ipv4(dst)),
            port,
        };
        if sock.listen(endpoint).is_ok() {
            let handle = self.sockets.add(sock);
            self.tcp_listening.insert(key, handle);
        }
    }

    fn provision_udp_flow(&mut self, dst: Ipv4Address, port: u16) {
        let addr = IpAddress::Ipv4(dst);
        let key = (addr, port);
        if self.udp_flows.contains_key(&key) {
            return;
        }
        // A datagram to the gateway on a port we use for an inbound UDP forward
        // is a guest *reply*, not a new outbound flow — leave it for smoltcp.
        if addr == GATEWAY_IP && self.forward_udp_local_ports.contains(&port) {
            return;
        }
        let Some(target) = self.host_target(IpEndpoint { addr, port }) else {
            return;
        };
        let Ok(mut host) = UdpSocket::bind(unspecified_v4()) else {
            return;
        };
        if host.connect(target).is_err() {
            return;
        }
        let token = self.alloc_token();
        if self
            .poll
            .registry()
            .register(&mut host, token, Interest::READABLE)
            .is_err()
        {
            return;
        }
        let mut sock = make_udp_socket();
        let endpoint = IpListenEndpoint {
            addr: Some(addr),
            port,
        };
        if sock.bind(endpoint).is_err() {
            let _ = self.poll.registry().deregister(&mut host);
            return;
        }
        let handle = self.sockets.add(sock);
        self.udp_flows
            .insert(key, (handle, UdpFlow::new(host, token)));
    }

    // --- promotion + accept ------------------------------------------------

    /// A listening socket that has left `Listen` has accepted the guest's SYN.
    /// Open the matching host connection and turn it into a bridged flow.
    fn promote_listeners(&mut self) {
        let promoted: Vec<((IpAddress, u16), SocketHandle)> = self
            .tcp_listening
            .iter()
            .filter(|(_, h)| self.sockets.get::<tcp::Socket<'_>>(**h).state() != tcp::State::Listen)
            .map(|(k, h)| (*k, *h))
            .collect();

        for (key, handle) in promoted {
            self.tcp_listening.remove(&key);
            let local = self.sockets.get::<tcp::Socket<'_>>(handle).local_endpoint();
            let connected = local
                .and_then(|l| self.host_target(l))
                .and_then(|t| self.connect_host(t));
            match connected {
                Some((stream, token)) => {
                    self.tcp_flows.insert(handle, TcpFlow::new(stream, token));
                }
                None => {
                    // No host side → reset the guest (e.g. invalid destination).
                    self.sockets.get_mut::<tcp::Socket<'_>>(handle).abort();
                    self.pending_removal.push(handle);
                }
            }
        }
    }

    /// Service inbound forwards: accept TCP connections / receive UDP datagrams
    /// from outside and originate the matching guest-side flow.
    fn accept_forwards(&mut self) {
        // Move the listeners out so we can freely call `&mut self` helpers while
        // iterating; they are owned values, so this just sidesteps the borrow.
        let mut forwards = std::mem::take(&mut self.forwards);
        for fl in &mut forwards {
            match fl {
                ForwardListener::Tcp {
                    listener,
                    guest_port,
                    ..
                } => self.accept_tcp_forward(listener, *guest_port),
                ForwardListener::Udp {
                    sock,
                    guest_port,
                    outside,
                    guest_socket,
                    ..
                } => self.relay_udp_forward(sock, *guest_port, outside, guest_socket),
            }
        }
        self.forwards = forwards;
    }

    fn accept_tcp_forward(&mut self, listener: &TcpListener, guest_port: u16) {
        loop {
            match listener.accept() {
                Ok((mut stream, _peer)) => {
                    let Some(handle) = self.originate_guest_tcp(guest_port) else {
                        continue;
                    };
                    let token = self.alloc_token();
                    if self
                        .poll
                        .registry()
                        .register(&mut stream, token, Interest::READABLE | Interest::WRITABLE)
                        .is_err()
                    {
                        self.sockets.get_mut::<tcp::Socket<'_>>(handle).abort();
                        self.pending_removal.push(handle);
                        continue;
                    }
                    self.tcp_flows
                        .insert(handle, TcpFlow::new_connected(stream, token));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
    }

    fn relay_udp_forward(
        &mut self,
        sock: &UdpSocket,
        guest_port: u16,
        outside: &mut Option<SocketAddr>,
        guest_socket: &mut Option<SocketHandle>,
    ) {
        let mut buf = [0u8; 65_535];
        // Outside → guest.
        loop {
            match sock.recv_from(&mut buf) {
                Ok((n, peer)) => {
                    *outside = Some(peer);
                    if guest_socket.is_none() {
                        if let Some((handle, local_port)) = self.originate_guest_udp() {
                            *guest_socket = Some(handle);
                            self.forward_udp_local_ports.insert(local_port);
                        }
                    }
                    if let Some(handle) = guest_socket {
                        let s = self.sockets.get_mut::<udp::Socket<'_>>(*handle);
                        let _ = s.send_slice(&buf[..n], IpEndpoint::new(GUEST_IP, guest_port));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        // Guest → outside (replies).
        if let (Some(handle), Some(peer)) = (*guest_socket, *outside) {
            let s = self.sockets.get_mut::<udp::Socket<'_>>(handle);
            while s.can_recv() {
                match s.recv() {
                    Ok((payload, _meta)) => {
                        let _ = sock.send_to(payload, peer);
                    }
                    Err(_) => break,
                }
            }
        }
    }

    // --- pumping + reaping -------------------------------------------------

    fn pump_all(&mut self) -> bool {
        let mut worked = false;
        let sockets = &mut self.sockets;
        for (handle, flow) in self.tcp_flows.iter_mut() {
            let socket = sockets.get_mut::<tcp::Socket<'_>>(*handle);
            worked |= flow.pump(socket);
        }
        for (handle, flow) in self.udp_flows.values_mut() {
            let socket = sockets.get_mut::<udp::Socket<'_>>(*handle);
            worked |= flow.pump(socket);
        }
        worked
    }

    fn reap_closed(&mut self) {
        let dead: Vec<SocketHandle> = self
            .tcp_flows
            .iter()
            .filter(|(h, _)| self.sockets.get::<tcp::Socket<'_>>(**h).state() == tcp::State::Closed)
            .map(|(h, _)| *h)
            .collect();
        for handle in dead {
            if let Some(mut flow) = self.tcp_flows.remove(&handle) {
                let _ = self.poll.registry().deregister(&mut flow.stream);
                self.sockets.remove(handle);
            }
        }
        for handle in std::mem::take(&mut self.pending_removal) {
            self.sockets.remove(handle);
        }
    }

    fn reclaim_idle_udp(&mut self) {
        let now = std::time::Instant::now();
        let expired: Vec<(IpAddress, u16)> = self
            .udp_flows
            .iter()
            .filter(|(_, (_, flow))| flow.is_idle(now, UDP_IDLE_TIMEOUT))
            .map(|(k, _)| *k)
            .collect();
        for key in expired {
            if let Some((handle, mut flow)) = self.udp_flows.remove(&key) {
                let _ = self.poll.registry().deregister(&mut flow.sock);
                self.sockets.remove(handle);
            }
        }
    }

    // --- outbound to guest -------------------------------------------------

    fn drain_to_guest(&mut self) {
        if self.device.tx.is_empty() {
            return;
        }
        let (lock, cvar) = &*self.outbound;
        let mut q = lock.lock().expect("user-net outbound poisoned");
        q.extend(self.device.tx.drain(..));
        drop(q);
        cvar.notify_all();
    }

    // --- helpers -----------------------------------------------------------

    fn alloc_token(&mut self) -> Token {
        let token = Token(self.next_token);
        self.next_token += 1;
        token
    }

    fn alloc_ephemeral(&mut self) -> u16 {
        let port = self.next_ephemeral;
        self.next_ephemeral = self.next_ephemeral.checked_add(1).unwrap_or(EPHEMERAL_BASE);
        if self.next_ephemeral == 0 {
            self.next_ephemeral = EPHEMERAL_BASE;
        }
        port
    }

    /// Map a guest destination to its host-side target: the gateway IP folds to
    /// loopback (guest→host); everything else is the literal destination
    /// (masquerade). v6 destinations are unsupported (the stack is v4).
    fn host_target(&self, dst: IpEndpoint) -> Option<SocketAddr> {
        let addr = if dst.addr == GATEWAY_IP {
            HOST_LOOPBACK
        } else {
            dst.addr
        };
        endpoint_to_socket_addr(IpEndpoint {
            addr,
            port: dst.port,
        })
    }

    fn connect_host(&mut self, target: SocketAddr) -> Option<(TcpStream, Token)> {
        let mut stream = TcpStream::connect(target).ok()?;
        let token = self.alloc_token();
        self.poll
            .registry()
            .register(&mut stream, token, Interest::READABLE | Interest::WRITABLE)
            .ok()?;
        Some((stream, token))
    }

    /// Originate a guest-side TCP connection (for an inbound forward): a smoltcp
    /// socket connecting from the gateway to `10.0.2.15:<guest_port>`.
    fn originate_guest_tcp(&mut self, guest_port: u16) -> Option<SocketHandle> {
        let mut sock = make_tcp_socket();
        let remote = IpEndpoint::new(GUEST_IP, guest_port);
        let local = IpListenEndpoint {
            addr: Some(GATEWAY_IP),
            port: self.alloc_ephemeral(),
        };
        sock.connect(self.iface.context(), remote, local).ok()?;
        Some(self.sockets.add(sock))
    }

    /// Originate a guest-side UDP socket (for an inbound forward), bound to a
    /// gateway-local ephemeral port. Returns the handle and that local port.
    fn originate_guest_udp(&mut self) -> Option<(SocketHandle, u16)> {
        let local_port = self.alloc_ephemeral();
        let mut sock = make_udp_socket();
        let endpoint = IpListenEndpoint {
            addr: Some(GATEWAY_IP),
            port: local_port,
        };
        sock.bind(endpoint).ok()?;
        Some((self.sockets.add(sock), local_port))
    }
}

/// Inspect a guest frame for flow-opening intent without consuming it.
///
/// Returns `(protocol, dst_ip, dst_port, opens_new_flow)` for IPv4 TCP/UDP.
/// `opens_new_flow` is true only for a pure TCP SYN (so a guest SYN-ACK on an
/// inbound forward isn't mistaken for a new outbound listen). Malformed or
/// non-IPv4/TCP/UDP frames return `None` and are simply forwarded to smoltcp.
fn inspect_frame(frame: &[u8]) -> Option<(IpProtocol, Ipv4Address, u16, bool)> {
    let eth = EthernetFrame::new_checked(frame).ok()?;
    if eth.ethertype() != EthernetProtocol::Ipv4 {
        return None;
    }
    let ip = Ipv4Packet::new_checked(eth.payload()).ok()?;
    let dst = ip.dst_addr();
    match ip.next_header() {
        IpProtocol::Tcp => {
            let tcp = TcpPacket::new_checked(ip.payload()).ok()?;
            let opens = tcp.syn() && !tcp.ack();
            Some((IpProtocol::Tcp, dst, tcp.dst_port(), opens))
        }
        IpProtocol::Udp => {
            let udp = UdpPacket::new_checked(ip.payload()).ok()?;
            Some((IpProtocol::Udp, dst, udp.dst_port(), false))
        }
        _ => None,
    }
}

/// Fuzz entry point for the untrusted guest-frame demux/provisioning *decision*
/// path: run [`inspect_frame`] over arbitrary bytes. It must never panic and
/// must never mis-classify a malformed frame as flow-opening. Hidden from docs;
/// used by `dillo/fuzz` and the in-crate corpus test.
#[doc(hidden)]
pub fn fuzz_inspect_frame(frame: &[u8]) {
    let _ = inspect_frame(frame);
}

fn make_tcp_socket() -> tcp::Socket<'static> {
    tcp::Socket::new(
        tcp::SocketBuffer::new(vec![0u8; TCP_BUFFER]),
        tcp::SocketBuffer::new(vec![0u8; TCP_BUFFER]),
    )
}

fn make_udp_socket() -> udp::Socket<'static> {
    udp::Socket::new(
        udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; UDP_META_SLOTS],
            vec![0u8; UDP_PAYLOAD_BUFFER],
        ),
        udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; UDP_META_SLOTS],
            vec![0u8; UDP_PAYLOAD_BUFFER],
        ),
    )
}

fn unspecified_v4() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 0))
}

/// smoltcp's optional `poll_delay`, clamped to [`MAX_POLL_WAIT`] so the loop
/// always wakes often enough to honor the stop flag and reclaim idle flows.
fn mio_wait(delay: Option<smoltcp::time::Duration>) -> StdDuration {
    match delay {
        Some(d) => StdDuration::from_micros(d.total_micros()).min(MAX_POLL_WAIT),
        None => MAX_POLL_WAIT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A well-formed Ethernet/IPv4/TCP **SYN** frame (the flow-opening case):
    /// 14 + 20 + 20 bytes, dst 127.0.0.1:80, SYN set / ACK clear.
    const TCP_SYN: &[u8] = &[
        // Ethernet: dst, src, ethertype=IPv4
        0x52, 0x54, 0x00, 0x12, 0x35, 0x02, 0x52, 0x54, 0x00, 0xaa, 0xbb, 0xcc, 0x08, 0x00,
        // IPv4: ver/ihl, tos, total_len=40, id, flags/frag, ttl, proto=6, csum=0, src, dst
        0x45, 0x00, 0x00, 0x28, 0x00, 0x00, 0x40, 0x00, 0x40, 0x06, 0x00, 0x00, 0x0a, 0x00, 0x02,
        0x0f, 0x7f, 0x00, 0x00, 0x01,
        // TCP: sport=12345, dport=80, seq, ack, off=5/flags=SYN, win, csum, urg
        0x30, 0x39, 0x00, 0x50, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x50, 0x02, 0xff,
        0xff, 0x00, 0x00, 0x00, 0x00,
    ];

    #[test]
    fn inspect_recognizes_a_syn() {
        let (proto, dst, port, opens) = inspect_frame(TCP_SYN).expect("valid SYN frame");
        assert_eq!(proto, IpProtocol::Tcp);
        assert_eq!(dst, Ipv4Address::new(127, 0, 0, 1));
        assert_eq!(port, 80);
        assert!(opens, "a pure SYN must be flow-opening");
    }

    #[test]
    fn inspect_never_panics_on_truncation() {
        // Every prefix of a valid frame must be handled without panicking; short
        // frames simply fail `new_checked` and return `None`.
        for len in 0..=TCP_SYN.len() {
            fuzz_inspect_frame(&TCP_SYN[..len]);
        }
    }

    #[test]
    fn inspect_never_panics_on_garbage() {
        // All-zeros, all-ones, and a sweep of single-byte values at many lengths.
        for len in 0..128usize {
            fuzz_inspect_frame(&vec![0u8; len]);
            fuzz_inspect_frame(&vec![0xffu8; len]);
            let ramp: Vec<u8> = (0..len).map(|i| (i.wrapping_mul(31) ^ len) as u8).collect();
            fuzz_inspect_frame(&ramp);
        }
    }

    #[test]
    fn syn_ack_is_not_flow_opening() {
        // Set the ACK bit (offset into TCP flags byte) so it is a SYN-ACK; an
        // inbound-forward guest reply must not be mistaken for a new listen.
        let mut frame = TCP_SYN.to_vec();
        let tcp_flags_off = 14 + 20 + 13; // eth + ip + tcp flags byte
        frame[tcp_flags_off] = 0x12; // SYN | ACK
        let (_, _, _, opens) = inspect_frame(&frame).expect("valid SYN-ACK");
        assert!(!opens, "a SYN-ACK must not be flow-opening");
    }
}
