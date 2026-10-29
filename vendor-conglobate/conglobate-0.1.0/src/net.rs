// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! In-process network bring-up / teardown for build stages.
//!
//! dillo gives the build VM a single virtio-net NIC on a user-mode
//! (slirp-like) network with a DHCP server. This module brings that
//! interface up, DHCP-configures it (no fixed IP is presumed), writes the
//! leased DNS server(s) into the chroot's `/etc/resolv.conf` so a chrooted
//! `run:` (e.g. `dnf`) can resolve, and tears it back down again.
//!
//! Everything goes through rustix's safe syscall wrappers: an
//! `AF_NETLINK`/`NETLINK_ROUTE` socket drives link/addr/route changes and an
//! `AF_INET`/`SOCK_DGRAM` socket runs a minimal DHCP exchange. No libc, no
//! subprocess, no `unsafe`.
//!
//! The wire encoders / decoders ([`build_dhcp_discover`],
//! [`build_dhcp_request`], [`parse_dhcp_reply`], and the netlink message
//! builders) are pure byte functions kept separate from the socket I/O so
//! they can be unit-tested without a kernel.

use std::fs;
use std::net::Ipv4Addr;
use std::path::Path;
use std::time::Duration;

use rustix::net::netlink::SocketAddrNetlink;
use rustix::net::sockopt::{
    Timeout, set_socket_broadcast, set_socket_reuseaddr, set_socket_timeout,
};
use rustix::net::{
    AddressFamily, RecvFlags, SendFlags, SocketType, bind, recv, recvfrom, send, sendto, socket,
};

// --- netlink message-type constants (rtnetlink) ---------------------------
const RTM_NEWLINK: u16 = 16;
const RTM_NEWADDR: u16 = 20;
const RTM_NEWROUTE: u16 = 24;
const RTM_DELROUTE: u16 = 25;
const NLMSG_ERROR: u16 = 2;

// --- netlink message flags ------------------------------------------------
const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_ACK: u16 = 0x4;
const NLM_F_REPLACE: u16 = 0x100;
const NLM_F_CREATE: u16 = 0x400;

// --- address families / link flags ----------------------------------------
const AF_UNSPEC: u8 = 0;
const AF_INET: u8 = 2;
const IFF_UP: u32 = 0x1;

// --- route / addr attribute + enum constants ------------------------------
const RT_SCOPE_UNIVERSE: u8 = 0;
const RT_SCOPE_LINK: u8 = 253;
const RTN_UNICAST: u8 = 1;
const RTPROT_BOOT: u8 = 3;
const RT_TABLE_MAIN: u8 = 254;

const RTA_DST: u16 = 1;
const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;

const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;

// --- DHCP / BOOTP constants -----------------------------------------------
const DHCP_MAGIC: u32 = 0x6382_5363;
const DHCP_CLIENT_PORT: u16 = 68;
const DHCP_SERVER_PORT: u16 = 67;
const BOOTP_OP_REQUEST: u8 = 1;
const BOOTP_HTYPE_ETHER: u8 = 1;
const BOOTP_HLEN_ETHER: u8 = 6;
const BOOTP_FLAG_BROADCAST: u16 = 0x8000;

const DHCP_DISCOVER: u8 = 1;
const DHCP_OFFER: u8 = 2;
const DHCP_REQUEST: u8 = 3;
const DHCP_ACK: u8 = 5;

const OPT_SUBNET_MASK: u8 = 1;
const OPT_ROUTER: u8 = 3;
const OPT_DNS: u8 = 6;
const OPT_MSG_TYPE: u8 = 53;
const OPT_PARAM_REQ: u8 = 55;
const OPT_REQ_IP: u8 = 50;
const OPT_SERVER_ID: u8 = 54;
const OPT_LEASE_TIME: u8 = 51;
const OPT_END: u8 = 255;

/// Maximum DISCOVER attempts before giving up on a silent network.
const DHCP_MAX_TRIES: u32 = 5;
/// Per-attempt receive timeout.
const DHCP_TIMEOUT: Duration = Duration::from_secs(3);

// =========================================================================
// Public API
// =========================================================================

/// Bring the build VM's network interface up and DHCP-configure it, writing
/// the DNS server(s) into `<chroot_root>/etc/resolv.conf` so a chrooted
/// `run:` (e.g. dnf) can resolve. Idempotent enough to call per stage.
// The build flow does not call this yet (it is wired in by a later phase),
// so `unreachable_pub` would otherwise fire on the intended public API.
#[allow(unreachable_pub)]
pub fn net_up(chroot_root: &Path) -> Result<(), String> {
    let iface = Iface::discover()?;

    let nl = NetlinkSock::open()?;
    nl.link_set_up(&iface)?;
    nl.add_host_route_broadcast(&iface)?;

    let lease = dhcp_acquire(&iface)?;

    // The temporary all-ones host route only existed to let the DHCP
    // broadcast egress; drop it before installing the real config.
    nl.del_host_route_broadcast(&iface)?;
    nl.add_addr(&iface, lease.yiaddr, lease.prefix_len)?;
    if let Some(router) = lease.router {
        nl.add_default_route(&iface, router)?;
    }

    write_resolv_conf(chroot_root, &lease.dns)?;
    Ok(())
}

/// Tear the interface back down (link down + flush addrs/routes). Best-effort.
//
// Returns `Result` (always `Ok`) to match `net_up`'s signature and the
// build flow's per-stage convention, even though teardown never fails.
#[allow(unreachable_pub, clippy::unnecessary_wraps)]
pub fn net_down() -> Result<(), String> {
    // Best-effort: a missing iface or a transient netlink error must not
    // fail teardown. Taking the link down drops its addrs and routes, so an
    // explicit flush is unnecessary.
    if let (Ok(iface), Ok(nl)) = (Iface::discover(), NetlinkSock::open()) {
        let _ = nl.link_set_down(&iface);
    }
    Ok(())
}

// =========================================================================
// Interface discovery
// =========================================================================

/// The single non-loopback network interface dillo attached.
#[derive(Debug, Clone)]
struct Iface {
    name: String,
    index: u32,
    mac: [u8; 6],
}

impl Iface {
    /// Discover the one non-`lo` interface under `/sys/class/net`.
    fn discover() -> Result<Self, String> {
        let mut name = None;
        let entries =
            fs::read_dir("/sys/class/net").map_err(|e| format!("read /sys/class/net: {e}"))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("read /sys/class/net entry: {e}"))?;
            let n = entry.file_name().to_string_lossy().into_owned();
            if n == "lo" {
                continue;
            }
            if name.is_some() {
                return Err(format!(
                    "expected exactly one non-lo interface, found at least two ({n})"
                ));
            }
            name = Some(n);
        }
        let name = name.ok_or_else(|| "no non-lo network interface found".to_owned())?;

        let index = read_ifindex(&name)?;
        let mac = read_mac(&name)?;
        Ok(Self { name, index, mac })
    }
}

/// Read `/sys/class/net/<name>/ifindex` and parse it to a `u32`.
fn read_ifindex(name: &str) -> Result<u32, String> {
    let path = format!("/sys/class/net/{name}/ifindex");
    let raw = fs::read_to_string(&path).map_err(|e| format!("read {path}: {e}"))?;
    raw.trim()
        .parse::<u32>()
        .map_err(|e| format!("parse ifindex {:?}: {e}", raw.trim()))
}

/// Read `/sys/class/net/<name>/address` and parse `aa:bb:..` into 6 bytes.
fn read_mac(name: &str) -> Result<[u8; 6], String> {
    let path = format!("/sys/class/net/{name}/address");
    let raw = fs::read_to_string(&path).map_err(|e| format!("read {path}: {e}"))?;
    parse_mac(raw.trim())
}

/// Parse a colon-separated hex MAC (`aa:bb:cc:dd:ee:ff`) into 6 bytes.
fn parse_mac(s: &str) -> Result<[u8; 6], String> {
    let mut out = [0u8; 6];
    let mut count = 0;
    for (i, part) in s.split(':').enumerate() {
        if i >= 6 {
            return Err(format!("MAC has too many octets: {s:?}"));
        }
        out[i] =
            u8::from_str_radix(part, 16).map_err(|e| format!("parse MAC octet {part:?}: {e}"))?;
        count += 1;
    }
    if count != 6 {
        return Err(format!("MAC has {count} octets, expected 6: {s:?}"));
    }
    Ok(out)
}

// =========================================================================
// Netlink socket I/O
// =========================================================================

/// An open `AF_NETLINK`/`NETLINK_ROUTE` socket, bound for request/ack.
#[derive(Debug)]
struct NetlinkSock {
    fd: rustix::fd::OwnedFd,
    seq: std::cell::Cell<u32>,
}

impl NetlinkSock {
    fn open() -> Result<Self, String> {
        // `NETLINK_ROUTE` is value 0; rustix expects `None` for the default
        // route protocol rather than a named constant.
        let fd = socket(AddressFamily::NETLINK, SocketType::RAW, None)
            .map_err(|e| format!("open netlink socket: {e}"))?;
        bind(&fd, &SocketAddrNetlink::new(0, 0))
            .map_err(|e| format!("bind netlink socket: {e}"))?;
        Ok(Self {
            fd,
            seq: std::cell::Cell::new(1),
        })
    }

    fn next_seq(&self) -> u32 {
        let s = self.seq.get();
        self.seq.set(s.wrapping_add(1));
        s
    }

    /// Send one request and read its `NLMSG_ERROR` ack (errno 0 = ok).
    fn request(&self, msg_type: u16, flags: u16, body: &[u8]) -> Result<(), String> {
        let seq = self.next_seq();
        let buf = build_nlmsg(msg_type, flags | NLM_F_REQUEST | NLM_F_ACK, seq, body);
        send(&self.fd, &buf, SendFlags::empty())
            .map_err(|e| format!("netlink send (type {msg_type}): {e}"))?;

        let mut rbuf = [0u8; 4096];
        let (_, n) = recv(&self.fd, &mut rbuf[..], RecvFlags::empty())
            .map_err(|e| format!("netlink recv (type {msg_type}): {e}"))?;
        parse_nlmsg_ack(&rbuf[..n], msg_type)
    }

    fn link_set_up(&self, iface: &Iface) -> Result<(), String> {
        let body = build_ifinfomsg(iface.index, IFF_UP, 0x1);
        self.request(RTM_NEWLINK, 0, &body)
    }

    fn link_set_down(&self, iface: &Iface) -> Result<(), String> {
        let body = build_ifinfomsg(iface.index, 0, 0x1);
        self.request(RTM_NEWLINK, 0, &body)
    }

    /// Add a `255.255.255.255/32` host route out the interface so the DHCP
    /// broadcast egresses the single NIC without `SO_BINDTODEVICE`.
    fn add_host_route_broadcast(&self, iface: &Iface) -> Result<(), String> {
        let body = build_host_route(iface.index);
        self.request(RTM_NEWROUTE, NLM_F_CREATE | NLM_F_REPLACE, &body)
    }

    fn del_host_route_broadcast(&self, iface: &Iface) -> Result<(), String> {
        let body = build_host_route(iface.index);
        self.request(RTM_DELROUTE, 0, &body)
    }

    fn add_addr(&self, iface: &Iface, addr: Ipv4Addr, prefix_len: u8) -> Result<(), String> {
        let body = build_ifaddrmsg(iface.index, addr, prefix_len);
        self.request(RTM_NEWADDR, NLM_F_CREATE | NLM_F_REPLACE, &body)
    }

    fn add_default_route(&self, iface: &Iface, gateway: Ipv4Addr) -> Result<(), String> {
        let body = build_default_route(iface.index, gateway);
        self.request(RTM_NEWROUTE, NLM_F_CREATE | NLM_F_REPLACE, &body)
    }
}

// =========================================================================
// Netlink message builders (pure)
// =========================================================================

/// Round `n` up to the next multiple of 4 (NLMSG_ALIGN / RTA_ALIGN).
const fn align4(n: usize) -> usize {
    (n + 3) & !3
}

/// Build a full netlink message: a 16-byte `nlmsghdr` followed by `body`,
/// with the header's length field set to the total (header + body, the body
/// already 4-aligned by its builder). Little-endian.
fn build_nlmsg(msg_type: u16, flags: u16, seq: u32, body: &[u8]) -> Vec<u8> {
    let total = 16 + body.len();
    let mut buf = Vec::with_capacity(total);
    buf.extend_from_slice(&(total as u32).to_le_bytes()); // nlmsg_len
    buf.extend_from_slice(&msg_type.to_le_bytes()); // nlmsg_type
    buf.extend_from_slice(&flags.to_le_bytes()); // nlmsg_flags
    buf.extend_from_slice(&seq.to_le_bytes()); // nlmsg_seq
    buf.extend_from_slice(&0u32.to_le_bytes()); // nlmsg_pid (0 = kernel assigns)
    buf.extend_from_slice(body);
    buf
}

/// Append an `rtattr` (u16 len incl. 4-byte header, u16 type, payload, pad to
/// 4) onto `buf`.
fn push_rtattr(buf: &mut Vec<u8>, attr_type: u16, payload: &[u8]) {
    let len = 4 + payload.len();
    buf.extend_from_slice(&(len as u16).to_le_bytes());
    buf.extend_from_slice(&attr_type.to_le_bytes());
    buf.extend_from_slice(payload);
    let pad = align4(len) - len;
    for _ in 0..pad {
        buf.push(0);
    }
}

/// `ifinfomsg { family, _pad, type, index, flags, change }` (16 bytes).
fn build_ifinfomsg(index: u32, flags: u32, change: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(16);
    b.push(AF_UNSPEC); // ifi_family
    b.push(0); // padding
    b.extend_from_slice(&0u16.to_le_bytes()); // ifi_type
    // ifi_index is a C `int`; the byte pattern of a valid index is identical
    // whether read as u32 or i32, so emit the u32 bytes directly.
    b.extend_from_slice(&index.to_le_bytes()); // ifi_index
    b.extend_from_slice(&flags.to_le_bytes()); // ifi_flags
    b.extend_from_slice(&change.to_le_bytes()); // ifi_change
    b
}

/// `rtmsg` header (12 bytes): the fixed part of an `RTM_*ROUTE` message.
fn build_rtmsg_header(
    family: u8,
    dst_len: u8,
    scope: u8,
    rt_type: u8,
    protocol: u8,
    table: u8,
) -> Vec<u8> {
    let mut b = Vec::with_capacity(12);
    b.push(family); // rtm_family
    b.push(dst_len); // rtm_dst_len
    b.push(0); // rtm_src_len
    b.push(0); // rtm_tos
    b.push(table); // rtm_table
    b.push(protocol); // rtm_protocol
    b.push(scope); // rtm_scope
    b.push(rt_type); // rtm_type
    b.extend_from_slice(&0u32.to_le_bytes()); // rtm_flags
    b
}

/// `RTM_*ROUTE` body for the temporary `255.255.255.255/32` link-scope host
/// route out `index` (`RTA_DST` + `RTA_OIF`).
fn build_host_route(index: u32) -> Vec<u8> {
    let mut b = build_rtmsg_header(
        AF_INET,
        32,
        RT_SCOPE_LINK,
        RTN_UNICAST,
        RTPROT_BOOT,
        RT_TABLE_MAIN,
    );
    push_rtattr(&mut b, RTA_DST, &Ipv4Addr::BROADCAST.octets());
    push_rtattr(&mut b, RTA_OIF, &index.to_le_bytes());
    b
}

/// `RTM_NEWROUTE` body for the default route (`dst_len` 0) via `gateway` out
/// `index`, universe scope.
fn build_default_route(index: u32, gateway: Ipv4Addr) -> Vec<u8> {
    let mut b = build_rtmsg_header(
        AF_INET,
        0,
        RT_SCOPE_UNIVERSE,
        RTN_UNICAST,
        RTPROT_BOOT,
        RT_TABLE_MAIN,
    );
    push_rtattr(&mut b, RTA_GATEWAY, &gateway.octets());
    push_rtattr(&mut b, RTA_OIF, &index.to_le_bytes());
    b
}

/// `ifaddrmsg { family, prefixlen, flags, scope, index }` (8 bytes) plus
/// `IFA_LOCAL` + `IFA_ADDRESS` = `addr`.
fn build_ifaddrmsg(index: u32, addr: Ipv4Addr, prefix_len: u8) -> Vec<u8> {
    let mut b = Vec::with_capacity(8);
    b.push(AF_INET); // ifa_family
    b.push(prefix_len); // ifa_prefixlen
    b.push(0); // ifa_flags
    b.push(RT_SCOPE_UNIVERSE); // ifa_scope
    b.extend_from_slice(&index.to_le_bytes()); // ifa_index
    push_rtattr(&mut b, IFA_LOCAL, &addr.octets());
    push_rtattr(&mut b, IFA_ADDRESS, &addr.octets());
    b
}

/// Parse a netlink `NLMSG_ERROR` ack. errno 0 means success; a negative
/// errno is reported with context. `req_type` is only used for messages.
fn parse_nlmsg_ack(buf: &[u8], req_type: u16) -> Result<(), String> {
    if buf.len() < 16 {
        return Err(format!(
            "netlink ack (req type {req_type}) too short: {} bytes",
            buf.len()
        ));
    }
    let msg_type = u16::from_le_bytes([buf[4], buf[5]]);
    if msg_type != NLMSG_ERROR {
        // Some kernels reply with the original message echoed on success when
        // no ack was requested; we always request an ack, so anything that is
        // not NLMSG_ERROR is unexpected.
        return Err(format!(
            "netlink ack (req type {req_type}): unexpected reply type {msg_type}"
        ));
    }
    // The error code is the first i32 of the NLMSG_ERROR payload (after the
    // 16-byte nlmsghdr).
    if buf.len() < 20 {
        return Err("netlink NLMSG_ERROR payload too short".to_owned());
    }
    let errno = i32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]);
    if errno == 0 {
        Ok(())
    } else {
        Err(format!(
            "netlink request (type {req_type}) failed: errno {}",
            -errno
        ))
    }
}

// =========================================================================
// DHCP socket I/O
// =========================================================================

/// What we extracted from a DHCP ACK.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Lease {
    yiaddr: Ipv4Addr,
    prefix_len: u8,
    router: Option<Ipv4Addr>,
    dns: Vec<Ipv4Addr>,
    server_id: Option<Ipv4Addr>,
    lease_secs: Option<u32>,
}

/// Run the full DISCOVER → OFFER → REQUEST → ACK exchange and return the lease.
fn dhcp_acquire(iface: &Iface) -> Result<Lease, String> {
    let fd = socket(AddressFamily::INET, SocketType::DGRAM, None)
        .map_err(|e| format!("open DHCP UDP socket: {e}"))?;
    set_socket_reuseaddr(&fd, true).map_err(|e| format!("SO_REUSEADDR: {e}"))?;
    set_socket_broadcast(&fd, true).map_err(|e| format!("SO_BROADCAST: {e}"))?;
    set_socket_timeout(&fd, Timeout::Recv, Some(DHCP_TIMEOUT))
        .map_err(|e| format!("SO_RCVTIMEO: {e}"))?;

    let bind_addr = std::net::SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, DHCP_CLIENT_PORT);
    bind(&fd, &bind_addr).map_err(|e| format!("bind 0.0.0.0:{DHCP_CLIENT_PORT}: {e}"))?;

    let dst = std::net::SocketAddrV4::new(Ipv4Addr::BROADCAST, DHCP_SERVER_PORT);
    let xid = xid_from_mac(iface.mac);

    // DISCOVER → OFFER, retrying on timeout.
    let offer = {
        let mut last_err = String::from("no DHCP OFFER received");
        let mut found = None;
        for _ in 0..DHCP_MAX_TRIES {
            let discover = build_dhcp_discover(iface.mac, xid);
            sendto(&fd, &discover, SendFlags::empty(), &dst)
                .map_err(|e| format!("send DHCP DISCOVER: {e}"))?;
            match dhcp_recv_of_type(&fd, xid, DHCP_OFFER) {
                Ok(o) => {
                    found = Some(o);
                    break;
                }
                Err(e) => last_err = e,
            }
        }
        found.ok_or(last_err)?
    };

    // REQUEST → ACK, retrying on timeout.
    let request = build_dhcp_request(iface.mac, xid, offer.yiaddr, offer.server_id);
    let mut last_err = String::from("no DHCP ACK received");
    for _ in 0..DHCP_MAX_TRIES {
        sendto(&fd, &request, SendFlags::empty(), &dst)
            .map_err(|e| format!("send DHCP REQUEST: {e}"))?;
        match dhcp_recv_of_type(&fd, xid, DHCP_ACK) {
            Ok(ack) => return Ok(ack),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

/// Receive packets until one parses as the expected message type with the
/// matching xid, or the receive times out.
fn dhcp_recv_of_type(fd: &rustix::fd::OwnedFd, xid: u32, want: u8) -> Result<Lease, String> {
    let mut buf = [0u8; 1500];
    // A single timeout slot may deliver a stray packet first; loop until we
    // match or the kernel returns the timeout error.
    loop {
        let (_, n, _) = recvfrom(fd, &mut buf[..], RecvFlags::empty())
            .map_err(|e| format!("recv DHCP reply: {e}"))?;
        match parse_dhcp_reply(&buf[..n], xid) {
            Ok(lease) if reply_is(&buf[..n], want) => return Ok(lease),
            _ => {} // wrong xid / wrong type / malformed: keep waiting
        }
    }
}

/// Cheap predicate: does the parsed reply carry DHCP message type `want`?
/// (Re-reads option 53; cheaper than threading it through [`Lease`].)
fn reply_is(buf: &[u8], want: u8) -> bool {
    parse_message_type(buf) == Some(want)
}

/// Derive a stable-ish transaction id from the MAC (last 4 octets, with the
/// first two folded in so distinct NICs differ).
fn xid_from_mac(mac: [u8; 6]) -> u32 {
    u32::from_be_bytes([mac[2] ^ mac[0], mac[3] ^ mac[1], mac[4], mac[5]])
}

/// Write `nameserver <ip>` lines into `<chroot_root>/etc/resolv.conf`,
/// creating the parent directory.
fn write_resolv_conf(chroot_root: &Path, dns: &[Ipv4Addr]) -> Result<(), String> {
    let etc = chroot_root.join("etc");
    fs::create_dir_all(&etc).map_err(|e| format!("create {}: {e}", etc.display()))?;
    let path = etc.join("resolv.conf");
    let mut contents = String::new();
    for ip in dns {
        contents.push_str("nameserver ");
        contents.push_str(&ip.to_string());
        contents.push('\n');
    }
    fs::write(&path, contents).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}

// =========================================================================
// DHCP wire encode / decode (pure)
// =========================================================================

/// Build the fixed 236-byte BOOTP header for a client request.
fn build_bootp_header(mac: [u8; 6], xid: u32) -> Vec<u8> {
    let mut b = vec![0u8; 236];
    b[0] = BOOTP_OP_REQUEST; // op = BOOTREQUEST
    b[1] = BOOTP_HTYPE_ETHER; // htype = Ethernet
    b[2] = BOOTP_HLEN_ETHER; // hlen = 6
    b[3] = 0; // hops
    b[4..8].copy_from_slice(&xid.to_be_bytes()); // xid (network order)
    // secs (8..10) = 0
    b[10..12].copy_from_slice(&BOOTP_FLAG_BROADCAST.to_be_bytes()); // broadcast flag
    // ciaddr/yiaddr/siaddr/giaddr (12..28) = 0
    b[28..34].copy_from_slice(&mac); // chaddr (first 6 bytes)
    // remainder of chaddr + sname + file = 0
    b
}

/// Build a DHCP DISCOVER packet (BOOTP header + magic cookie + options).
fn build_dhcp_discover(mac: [u8; 6], xid: u32) -> Vec<u8> {
    let mut b = build_bootp_header(mac, xid);
    b.extend_from_slice(&DHCP_MAGIC.to_be_bytes());
    // option 53: message type = DISCOVER
    b.extend_from_slice(&[OPT_MSG_TYPE, 1, DHCP_DISCOVER]);
    // option 55: parameter request list
    b.extend_from_slice(&[
        OPT_PARAM_REQ,
        5,
        OPT_SUBNET_MASK,
        OPT_ROUTER,
        OPT_DNS,
        28,
        15,
    ]);
    b.push(OPT_END);
    b
}

/// Build a DHCP REQUEST packet selecting `yiaddr` from `server_id`.
fn build_dhcp_request(
    mac: [u8; 6],
    xid: u32,
    yiaddr: Ipv4Addr,
    server_id: Option<Ipv4Addr>,
) -> Vec<u8> {
    let mut b = build_bootp_header(mac, xid);
    b.extend_from_slice(&DHCP_MAGIC.to_be_bytes());
    // option 53: message type = REQUEST
    b.extend_from_slice(&[OPT_MSG_TYPE, 1, DHCP_REQUEST]);
    // option 50: requested IP address
    b.push(OPT_REQ_IP);
    b.push(4);
    b.extend_from_slice(&yiaddr.octets());
    // option 54: server identifier (when known from the OFFER)
    if let Some(sid) = server_id {
        b.push(OPT_SERVER_ID);
        b.push(4);
        b.extend_from_slice(&sid.octets());
    }
    // option 55: parameter request list
    b.extend_from_slice(&[
        OPT_PARAM_REQ,
        5,
        OPT_SUBNET_MASK,
        OPT_ROUTER,
        OPT_DNS,
        28,
        15,
    ]);
    b.push(OPT_END);
    b
}

/// Read DHCP option 53 (message type) out of a reply, if present.
fn parse_message_type(buf: &[u8]) -> Option<u8> {
    let opts = dhcp_options_slice(buf)?;
    let mut i = 0;
    while i < opts.len() {
        let code = opts[i];
        if code == OPT_END {
            break;
        }
        if code == 0 {
            i += 1; // pad
            continue;
        }
        let len = *opts.get(i + 1)? as usize;
        let val = opts.get(i + 2..i + 2 + len)?;
        if code == OPT_MSG_TYPE && len == 1 {
            return Some(val[0]);
        }
        i += 2 + len;
    }
    None
}

/// Return the option bytes (everything after the 4-byte magic cookie) of a
/// BOOTP reply, validating op/magic and xid-independent structure.
fn dhcp_options_slice(buf: &[u8]) -> Option<&[u8]> {
    // 236-byte BOOTP header + 4-byte magic cookie = 240.
    if buf.len() < 240 {
        return None;
    }
    let magic = u32::from_be_bytes([buf[236], buf[237], buf[238], buf[239]]);
    if magic != DHCP_MAGIC {
        return None;
    }
    Some(&buf[240..])
}

/// Parse a DHCP OFFER/ACK reply into a [`Lease`]. Validates the magic cookie
/// and xid; extracts yiaddr (BOOTP offset 16), opt 1 (mask→prefix), opt 3
/// (router), opt 6 (DNS), opt 54 (server id), opt 51 (lease).
fn parse_dhcp_reply(buf: &[u8], expect_xid: u32) -> Result<Lease, String> {
    let opts = dhcp_options_slice(buf)
        .ok_or_else(|| "DHCP reply too short or bad magic cookie".to_owned())?;

    let xid = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    if xid != expect_xid {
        return Err(format!(
            "DHCP reply xid {xid:#010x} != expected {expect_xid:#010x}"
        ));
    }

    let yiaddr = Ipv4Addr::new(buf[16], buf[17], buf[18], buf[19]);

    let mut prefix_len = 24; // sane fallback if the server omits opt 1
    let mut router = None;
    let mut dns = Vec::new();
    let mut server_id = None;
    let mut lease_secs = None;

    let mut i = 0;
    while i < opts.len() {
        let code = opts[i];
        if code == OPT_END {
            break;
        }
        if code == 0 {
            i += 1; // pad
            continue;
        }
        let len = *opts
            .get(i + 1)
            .ok_or_else(|| "DHCP option truncated (no length)".to_owned())?
            as usize;
        let val = opts
            .get(i + 2..i + 2 + len)
            .ok_or_else(|| format!("DHCP option {code} truncated"))?;
        match code {
            OPT_SUBNET_MASK if len == 4 => {
                prefix_len = mask_to_prefix([val[0], val[1], val[2], val[3]]);
            }
            OPT_ROUTER if len >= 4 => {
                router = Some(Ipv4Addr::new(val[0], val[1], val[2], val[3]));
            }
            OPT_DNS => {
                for chunk in val.chunks_exact(4) {
                    dns.push(Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]));
                }
            }
            OPT_SERVER_ID if len == 4 => {
                server_id = Some(Ipv4Addr::new(val[0], val[1], val[2], val[3]));
            }
            OPT_LEASE_TIME if len == 4 => {
                lease_secs = Some(u32::from_be_bytes([val[0], val[1], val[2], val[3]]));
            }
            _ => {}
        }
        i += 2 + len;
    }

    Ok(Lease {
        yiaddr,
        prefix_len,
        router,
        dns,
        server_id,
        lease_secs,
    })
}

/// Count the leading set bits of a dotted subnet mask to get the prefix len.
fn mask_to_prefix(mask: [u8; 4]) -> u8 {
    u32::from_be_bytes(mask).count_ones() as u8
}

// =========================================================================
// Unit tests (pure encode/parse only — no kernel / no socket I/O)
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

    #[test]
    fn parse_mac_roundtrip() {
        assert_eq!(parse_mac("52:54:00:12:34:56").unwrap(), MAC);
        assert!(parse_mac("52:54:00:12:34").is_err()); // too few
        assert!(parse_mac("52:54:00:12:34:56:78").is_err()); // too many
        assert!(parse_mac("gg:54:00:12:34:56").is_err()); // bad hex
    }

    #[test]
    fn mask_to_prefix_lengths() {
        assert_eq!(mask_to_prefix([255, 255, 255, 0]), 24);
        assert_eq!(mask_to_prefix([255, 255, 0, 0]), 16);
        assert_eq!(mask_to_prefix([255, 255, 255, 252]), 30);
        assert_eq!(mask_to_prefix([0, 0, 0, 0]), 0);
    }

    #[test]
    fn align4_rounds_up() {
        assert_eq!(align4(0), 0);
        assert_eq!(align4(1), 4);
        assert_eq!(align4(4), 4);
        assert_eq!(align4(5), 8);
    }

    #[test]
    fn discover_is_well_formed() {
        let xid = 0xDEAD_BEEF;
        let pkt = build_dhcp_discover(MAC, xid);

        // BOOTP fixed header.
        assert_eq!(pkt[0], BOOTP_OP_REQUEST);
        assert_eq!(pkt[1], BOOTP_HTYPE_ETHER);
        assert_eq!(pkt[2], BOOTP_HLEN_ETHER);
        assert_eq!(&pkt[4..8], &xid.to_be_bytes());
        // broadcast flag set.
        assert_eq!(&pkt[10..12], &BOOTP_FLAG_BROADCAST.to_be_bytes());
        // chaddr carries the MAC.
        assert_eq!(&pkt[28..34], &MAC);
        // magic cookie at offset 236.
        assert_eq!(
            u32::from_be_bytes([pkt[236], pkt[237], pkt[238], pkt[239]]),
            DHCP_MAGIC
        );
        // option 53 = DISCOVER right after the cookie.
        assert_eq!(&pkt[240..243], &[OPT_MSG_TYPE, 1, DHCP_DISCOVER]);
        // ends with the END option.
        assert_eq!(*pkt.last().unwrap(), OPT_END);
        // the message type we encode parses back out.
        assert_eq!(parse_message_type(&pkt), Some(DHCP_DISCOVER));
    }

    #[test]
    fn request_carries_requested_ip_and_server_id() {
        let xid = 0x0102_0304;
        let yi = Ipv4Addr::new(10, 0, 2, 15);
        let sid = Ipv4Addr::new(10, 0, 2, 2);
        let pkt = build_dhcp_request(MAC, xid, yi, Some(sid));

        assert_eq!(parse_message_type(&pkt), Some(DHCP_REQUEST));

        let opts = dhcp_options_slice(&pkt).unwrap();
        // option 50 (requested IP) present with yiaddr.
        assert_eq!(find_option(opts, OPT_REQ_IP), Some(yi.octets().to_vec()));
        // option 54 (server id) present.
        assert_eq!(
            find_option(opts, OPT_SERVER_ID),
            Some(sid.octets().to_vec())
        );
    }

    #[test]
    fn request_omits_server_id_when_unknown() {
        let pkt = build_dhcp_request(MAC, 1, Ipv4Addr::new(1, 2, 3, 4), None);
        let opts = dhcp_options_slice(&pkt).unwrap();
        assert_eq!(find_option(opts, OPT_SERVER_ID), None);
    }

    #[test]
    fn parse_ack_extracts_lease_fields() {
        let xid = 0xABCD_1234;
        let yi = Ipv4Addr::new(10, 0, 2, 15);
        let ack = build_test_ack(
            xid,
            yi,
            [255, 255, 255, 0],
            Some(Ipv4Addr::new(10, 0, 2, 2)),
            &[Ipv4Addr::new(10, 0, 2, 3), Ipv4Addr::new(8, 8, 8, 8)],
            Some(Ipv4Addr::new(10, 0, 2, 2)),
            Some(86400),
        );
        let lease = parse_dhcp_reply(&ack, xid).unwrap();
        assert_eq!(lease.yiaddr, yi);
        assert_eq!(lease.prefix_len, 24);
        assert_eq!(lease.router, Some(Ipv4Addr::new(10, 0, 2, 2)));
        assert_eq!(
            lease.dns,
            vec![Ipv4Addr::new(10, 0, 2, 3), Ipv4Addr::new(8, 8, 8, 8)]
        );
        assert_eq!(lease.server_id, Some(Ipv4Addr::new(10, 0, 2, 2)));
        assert_eq!(lease.lease_secs, Some(86400));
    }

    #[test]
    fn parse_rejects_wrong_xid_and_bad_magic() {
        let xid = 0x1111_1111;
        let ack = build_test_ack(
            xid,
            Ipv4Addr::new(1, 2, 3, 4),
            [255, 255, 255, 0],
            None,
            &[],
            None,
            None,
        );
        // wrong xid rejected.
        assert!(parse_dhcp_reply(&ack, 0x2222_2222).is_err());
        // corrupt the magic cookie -> rejected.
        let mut bad = ack.clone();
        bad[236] = 0;
        assert!(parse_dhcp_reply(&bad, xid).is_err());
        // truncated -> rejected.
        assert!(parse_dhcp_reply(&ack[..100], xid).is_err());
    }

    #[test]
    fn nlmsg_header_length_and_alignment() {
        let body = build_ifinfomsg(3, IFF_UP, 0x1);
        assert_eq!(body.len(), 16, "ifinfomsg is 16 bytes");
        let msg = build_nlmsg(RTM_NEWLINK, NLM_F_REQUEST | NLM_F_ACK, 7, &body);
        // header length field = total length.
        let len = u32::from_le_bytes([msg[0], msg[1], msg[2], msg[3]]) as usize;
        assert_eq!(len, msg.len());
        assert_eq!(len, 16 + 16);
        // type + flags + seq decode.
        assert_eq!(u16::from_le_bytes([msg[4], msg[5]]), RTM_NEWLINK);
        assert_eq!(
            u16::from_le_bytes([msg[6], msg[7]]),
            NLM_F_REQUEST | NLM_F_ACK
        );
        assert_eq!(u32::from_le_bytes([msg[8], msg[9], msg[10], msg[11]]), 7);
        // whole message 4-aligned.
        assert_eq!(msg.len() % 4, 0);
    }

    #[test]
    fn host_route_attrs_are_aligned() {
        let body = build_host_route(5);
        // rtmsg header is 12 bytes, then two 4-aligned attrs of 4+4 each.
        assert_eq!(body.len() % 4, 0);
        assert_eq!(body.len(), 12 + 8 + 8);
        // rtm_family / dst_len / scope / type.
        assert_eq!(body[0], AF_INET);
        assert_eq!(body[1], 32); // dst_len
        assert_eq!(body[6], RT_SCOPE_LINK);
        assert_eq!(body[7], RTN_UNICAST);
        // first attr is RTA_DST = 255.255.255.255.
        let a0_len = u16::from_le_bytes([body[12], body[13]]);
        let a0_type = u16::from_le_bytes([body[14], body[15]]);
        assert_eq!(a0_len, 8);
        assert_eq!(a0_type, RTA_DST);
        assert_eq!(&body[16..20], &[255, 255, 255, 255]);
    }

    #[test]
    fn ifaddrmsg_carries_addr_and_prefix() {
        let body = build_ifaddrmsg(9, Ipv4Addr::new(192, 168, 1, 50), 24);
        assert_eq!(body.len() % 4, 0);
        assert_eq!(body[0], AF_INET);
        assert_eq!(body[1], 24); // prefixlen
        assert_eq!(body[3], RT_SCOPE_UNIVERSE);
        assert_eq!(u32::from_le_bytes([body[4], body[5], body[6], body[7]]), 9);
        // IFA_LOCAL attr payload.
        assert_eq!(u16::from_le_bytes([body[10], body[11]]), IFA_LOCAL);
        assert_eq!(&body[12..16], &[192, 168, 1, 50]);
    }

    #[test]
    fn ack_errno_zero_is_ok_nonzero_is_err() {
        let ok = build_nlmsg_error(0, 7);
        assert!(parse_nlmsg_ack(&ok, RTM_NEWLINK).is_ok());
        let enodev = build_nlmsg_error(-19, 7); // -ENODEV
        assert!(parse_nlmsg_ack(&enodev, RTM_NEWLINK).is_err());
        // a non-error reply type is rejected.
        let not_error = build_nlmsg(RTM_NEWLINK, 0, 7, &[0u8; 16]);
        assert!(parse_nlmsg_ack(&not_error, RTM_NEWLINK).is_err());
    }

    // --- test helpers ---------------------------------------------------

    /// Find a DHCP option's value bytes by code (test-only linear scan).
    fn find_option(opts: &[u8], code: u8) -> Option<Vec<u8>> {
        let mut i = 0;
        while i < opts.len() {
            let c = opts[i];
            if c == OPT_END {
                break;
            }
            if c == 0 {
                i += 1;
                continue;
            }
            let len = opts[i + 1] as usize;
            let val = &opts[i + 2..i + 2 + len];
            if c == code {
                return Some(val.to_vec());
            }
            i += 2 + len;
        }
        None
    }

    /// Hand-build a DHCP ACK byte buffer for the parser tests.
    fn build_test_ack(
        xid: u32,
        yiaddr: Ipv4Addr,
        mask: [u8; 4],
        router: Option<Ipv4Addr>,
        dns: &[Ipv4Addr],
        server_id: Option<Ipv4Addr>,
        lease_secs: Option<u32>,
    ) -> Vec<u8> {
        let mut b = vec![0u8; 236];
        b[0] = 2; // op = BOOTREPLY
        b[1] = BOOTP_HTYPE_ETHER;
        b[2] = BOOTP_HLEN_ETHER;
        b[4..8].copy_from_slice(&xid.to_be_bytes());
        b[16..20].copy_from_slice(&yiaddr.octets());
        b.extend_from_slice(&DHCP_MAGIC.to_be_bytes());
        b.extend_from_slice(&[OPT_MSG_TYPE, 1, DHCP_ACK]);
        b.push(OPT_SUBNET_MASK);
        b.push(4);
        b.extend_from_slice(&mask);
        if let Some(r) = router {
            b.push(OPT_ROUTER);
            b.push(4);
            b.extend_from_slice(&r.octets());
        }
        if !dns.is_empty() {
            b.push(OPT_DNS);
            b.push((dns.len() * 4) as u8);
            for d in dns {
                b.extend_from_slice(&d.octets());
            }
        }
        if let Some(s) = server_id {
            b.push(OPT_SERVER_ID);
            b.push(4);
            b.extend_from_slice(&s.octets());
        }
        if let Some(l) = lease_secs {
            b.push(OPT_LEASE_TIME);
            b.push(4);
            b.extend_from_slice(&l.to_be_bytes());
        }
        b.push(OPT_END);
        b
    }

    /// Build an `NLMSG_ERROR` reply carrying `errno` (a negative kernel errno
    /// or 0) for the ack parser tests.
    fn build_nlmsg_error(errno: i32, seq: u32) -> Vec<u8> {
        // NLMSG_ERROR payload: i32 error code, then the echoed original
        // nlmsghdr (we use a zeroed 16-byte stand-in).
        let mut payload = Vec::new();
        payload.extend_from_slice(&errno.to_le_bytes());
        payload.extend_from_slice(&[0u8; 16]);
        build_nlmsg(NLMSG_ERROR, 0, seq, &payload)
    }
}
