// SPDX-License-Identifier: Apache-2.0

//! Linux TAP backend: a layer-2 `/dev/net/tun` endpoint in `IFF_TAP` mode.
//!
//! This is the "traditional TAP/bridging" path. dillo owns one `tapN` interface
//! (named explicitly, or kernel-assigned when no name is given) and exchanges
//! raw Ethernet frames with it. Bridging is an operator concern: add the tap to
//! a Linux bridge (`ip link set tapN master br0`) to join the guest to a LAN.
//!
//! Creating a tap requires `CAP_NET_ADMIN`. [`TapBackend::open`] surfaces the
//! `EPERM`/`ENOENT` cleanly so the caller can report it.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

use crate::backend::NetBackend;
use crate::linux_fd::FrameFd;

/// `TUNSETIFF` ioctl: `_IOW('T', 202, int)`. Stable kernel ABI.
const TUNSETIFF: libc::c_ulong = 0x4004_54ca;
/// `IFF_TAP`: layer-2 (Ethernet frame) tap device.
const IFF_TAP: u16 = 0x0002;
/// `IFF_NO_PI`: no 4-byte packet-info prefix — deliver bare Ethernet frames.
const IFF_NO_PI: u16 = 0x1000;
/// `IFNAMSIZ`: max interface-name length including the NUL terminator.
const IFNAMSIZ: usize = 16;
/// Size of `struct ifreq` on Linux (name[16] + the largest union member).
const IFREQ_LEN: usize = 40;

/// A TAP-backed virtio-net L2 transport.
#[derive(Debug)]
pub struct TapBackend {
    fd: FrameFd,
    name: String,
}

impl TapBackend {
    /// Open (or attach to) a `IFF_TAP | IFF_NO_PI` device.
    ///
    /// `name` pins the interface name (e.g. `"tap0"`); an empty string lets the
    /// kernel assign one. The resolved name is available via [`TapBackend::name`].
    pub fn open(name: &str) -> io::Result<Self> {
        if name.len() >= IFNAMSIZ {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("tap name {name:?} exceeds {} bytes", IFNAMSIZ - 1),
            ));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/net/tun")?;

        // struct ifreq { char ifr_name[IFNAMSIZ]; union { short ifru_flags; ... } }
        let mut ifr = [0u8; IFREQ_LEN];
        let nb = name.as_bytes();
        ifr[..nb.len()].copy_from_slice(nb);
        let flags = IFF_TAP | IFF_NO_PI;
        ifr[IFNAMSIZ..IFNAMSIZ + 2].copy_from_slice(&flags.to_le_bytes());

        // SAFETY: `ifr` is a correctly sized, initialized `ifreq` buffer; the
        // kernel reads `ifr_name` + `ifr_flags` and writes back the resolved
        // name within the same buffer.
        #[allow(unsafe_code)]
        let rc = unsafe { libc::ioctl(file.as_raw_fd(), TUNSETIFF, ifr.as_mut_ptr()) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }

        // The kernel wrote the (possibly auto-assigned) name back into ifr_name.
        let end = ifr[..IFNAMSIZ]
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(IFNAMSIZ);
        let resolved = String::from_utf8_lossy(&ifr[..end]).into_owned();

        log::info!("virtio-net: opened TAP device {resolved:?}");
        Ok(Self {
            fd: FrameFd::new(file),
            name: resolved,
        })
    }

    /// The resolved interface name (e.g. `"tap0"`).
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl NetBackend for TapBackend {
    fn send(&self, frame: &[u8]) -> io::Result<()> {
        self.fd.send(frame)
    }

    fn recv(&self, buf: &mut [u8]) -> io::Result<Option<usize>> {
        self.fd.recv(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_overlong_name() {
        let err = TapBackend::open("this-name-is-way-too-long").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    /// Opening a real tap needs CAP_NET_ADMIN; in an unprivileged sandbox this
    /// fails cleanly rather than panicking. We only assert it does not succeed
    /// silently with a bogus state — a successful open (privileged CI/dev box)
    /// yields a usable name.
    #[test]
    fn open_is_clean_without_privilege() {
        match TapBackend::open("") {
            Ok(tap) => {
                assert!(!tap.name().is_empty(), "kernel must assign a name");
            }
            Err(e) => {
                // EPERM (no CAP_NET_ADMIN), ENOENT (no /dev/net/tun), or EACCES.
                eprintln!("tap open unavailable in this environment: {e}");
            }
        }
    }
}
