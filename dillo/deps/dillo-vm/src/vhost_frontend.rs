//! VM-side proxy: makes a forked vhost-user backend look like an
//! in-process `dillo_virtio::VirtioDevice` so the existing virtio-pci
//! transport can talk to it unchanged.
//!
//! Modeled on the PoC at `~/Projects/dillo/dillo-vmm/src/vhost_frontend.rs`.
//! The construction-time handshake (`set_owner` + `get_features` +
//! `set_protocol_features`) runs in `new()`; the full activation
//! (`set_features` + `set_mem_table` + per-queue `set_vring_*` +
//! `set_vring_enable`) runs in `activate()` when the guest writes
//! DRIVER_OK.

use std::os::unix::net::UnixStream;
use std::process::Child;
use std::sync::Arc;

use dillo_virtio::{ActivateError, VirtioActivate, VirtioDevice, VirtioDeviceHandle};
use vhost::vhost_user::message::{VhostUserConfigFlags, VhostUserProtocolFeatures};
use vhost::vhost_user::{Frontend, VhostUserFrontend as _};
use vhost::{VhostBackend, VhostUserMemoryRegionInfo, VringConfigData};
use vm_memory::{Address, GuestMemory};

use crate::pci_irq::IrqfdNotifier;

const VIRTIO_ID_CONSOLE: u32 = 3;
const QUEUE_MAX: u16 = 64;
const QUEUE_SIZES: [u16; 2] = [QUEUE_MAX, QUEUE_MAX];

/// vhost-user "PROTOCOL_FEATURES supported" meta-bit. Must be acked
/// in `set_features` but stripped before advertising to the guest.
const VHOST_USER_PROTOCOL_FEATURES_BIT: u64 = 0x4000_0000;

/// VM-side handle to a forked vhost-user backend. Owns the parent
/// half of the socketpair (wrapped inside the [`Frontend`]) + the
/// Child handle so the backend can be shut down at VM teardown.
pub struct VhostUserFrontend {
    frontend: Frontend,
    _child: Child,
    /// Feature bits probed from the backend at construction.
    backend_features: u64,
    /// Source of MSI-X→irqfd call eventfds at activate() time.
    notifier: Arc<IrqfdNotifier>,
}

impl std::fmt::Debug for VhostUserFrontend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VhostUserFrontend")
            .field("backend_features", &self.backend_features)
            .finish_non_exhaustive()
    }
}

impl VhostUserFrontend {
    /// Wrap a connected vhost-user socket + child process, run the
    /// construction-time handshake (set_owner / get_features /
    /// optional set_protocol_features), and return the proxy.
    ///
    /// Protocol features are negotiated here (not in `activate`) so
    /// the guest's pre-DRIVER_OK config reads can dispatch to the
    /// backend via `get_config` — the frontend library refuses
    /// `get_config` unless PROTOCOL_FEATURES has been acked.
    pub fn new(
        socket: UnixStream,
        child: Child,
        notifier: Arc<IrqfdNotifier>,
    ) -> Result<Self, anyhow::Error> {
        let mut frontend = Frontend::from_stream(socket, QUEUE_SIZES.len() as u64);

        frontend
            .set_owner()
            .map_err(|e| anyhow::anyhow!("vhost-user set_owner failed: {e}"))?;

        let backend_features = frontend
            .get_features()
            .map_err(|e| anyhow::anyhow!("vhost-user get_features failed: {e}"))?;

        log::debug!("VhostUserFrontend: backend_features={backend_features:#x}");

        if backend_features & VHOST_USER_PROTOCOL_FEATURES_BIT != 0 {
            match frontend.get_protocol_features() {
                Ok(proto_feats) => {
                    // Request CONFIG so read_config / write_config work
                    // before activate. Do NOT request REPLY_ACK — our
                    // backend's req-handler doesn't implement the ack
                    // message path, and REPLY_ACK would cause get_config
                    // to hang waiting for an ack the backend never sends.
                    let want = proto_feats
                        & (VhostUserProtocolFeatures::CONFIG | VhostUserProtocolFeatures::MQ);
                    if let Err(e) = frontend.set_protocol_features(want) {
                        log::warn!("vhost-user set_protocol_features: {e}");
                    } else {
                        log::debug!("VhostUserFrontend: protocol_features_acked={want:?}");
                    }
                }
                Err(e) => log::warn!("vhost-user get_protocol_features: {e}"),
            }
        }

        Ok(Self {
            frontend,
            _child: child,
            backend_features,
            notifier,
        })
    }
}

impl VirtioDevice for VhostUserFrontend {
    fn device_type(&self) -> u32 {
        VIRTIO_ID_CONSOLE
    }

    fn num_queues(&self) -> usize {
        QUEUE_SIZES.len()
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &QUEUE_SIZES
    }

    /// Backend features minus the vhost-user PROTOCOL_FEATURES bit
    /// (which is a transport meta-feature, not a guest-visible one).
    fn features(&self) -> u64 {
        self.backend_features & !VHOST_USER_PROTOCOL_FEATURES_BIT
    }

    fn activate(
        &mut self,
        activation: VirtioActivate,
    ) -> Result<VirtioDeviceHandle, ActivateError> {
        let VirtioActivate {
            mem,
            queues,
            queue_evts,
        } = activation;
        // Re-intersect with backend_features as a safety net (the value
        // passed in is the driver-negotiated subset of what we advertised).
        // PROTOCOL_FEATURES MUST be preserved in the value passed to
        // set_features even though we strip it from features() above —
        // the frontend library records the set_features value in
        // `acked_virtio_features`, and set_vring_enable refuses to
        // proceed unless PROTOCOL_FEATURES is set there.
        let negotiated = self.backend_features;
        self.frontend
            .set_features(negotiated)
            .map_err(|e| ActivateError::InvalidConfig(format!("set_features: {e}")))?;

        // Share guest memory with the backend.
        let regions: Vec<VhostUserMemoryRegionInfo> = mem
            .iter()
            .filter_map(|region| {
                VhostUserMemoryRegionInfo::from_guest_region(region)
                    .map_err(|e| log::warn!("vhost-user: skipping region: {e}"))
                    .ok()
            })
            .collect();
        if regions.is_empty() {
            return Err(ActivateError::InvalidConfig(
                "no guest memory regions available for vhost-user".into(),
            ));
        }
        self.frontend
            .set_mem_table(&regions)
            .map_err(|e| ActivateError::InvalidConfig(format!("set_mem_table: {e}")))?;

        // Per-queue setup.
        for (i, queue) in queues.iter().enumerate() {
            let kick = queue_evts.get(i).ok_or_else(|| {
                ActivateError::InvalidConfig(format!("missing kick eventfd for queue {i}"))
            })?;
            let call_opt = self.notifier.get_irqfd_for_vector(queue.msix_vector);
            // Both ends of vhost's set_vring_kick / set_vring_call want a
            // raw &EventFd; pull it back out of our cross-platform wrappers
            // (Linux only — vhost itself is Linux-only).
            let kick_fd = kick.as_eventfd();

            self.frontend
                .set_vring_num(i, queue.size)
                .map_err(|e| ActivateError::InvalidConfig(format!("set_vring_num[{i}]: {e}")))?;

            let config = VringConfigData {
                queue_max_size: queue.max_size,
                queue_size: queue.size,
                flags: 0,
                desc_table_addr: queue.desc_table.raw_value(),
                avail_ring_addr: queue.avail_ring.raw_value(),
                used_ring_addr: queue.used_ring.raw_value(),
                log_addr: None,
            };
            self.frontend
                .set_vring_addr(i, &config)
                .map_err(|e| ActivateError::InvalidConfig(format!("set_vring_addr[{i}]: {e}")))?;

            self.frontend
                .set_vring_base(i, 0)
                .map_err(|e| ActivateError::InvalidConfig(format!("set_vring_base[{i}]: {e}")))?;

            self.frontend
                .set_vring_kick(i, kick_fd)
                .map_err(|e| ActivateError::InvalidConfig(format!("set_vring_kick[{i}]: {e}")))?;

            if let Some(ref call) = call_opt {
                self.frontend
                    .set_vring_call(i, call.as_eventfd())
                    .map_err(|e| {
                        ActivateError::InvalidConfig(format!("set_vring_call[{i}]: {e}"))
                    })?;
            } else {
                log::warn!(
                    "VhostUserFrontend: queue {i} has no call eventfd \
                     (msix_vector={:#x} not programmed)",
                    queue.msix_vector
                );
            }

            self.frontend
                .set_vring_enable(i, true)
                .map_err(|e| ActivateError::InvalidConfig(format!("set_vring_enable[{i}]: {e}")))?;
        }

        log::info!(
            "VhostUserFrontend: activated ({} queues, features={:#x})",
            queues.len(),
            negotiated
        );
        Ok(VirtioDeviceHandle::noop())
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // get_config takes &mut self on VhostUserFrontend; Frontend has
        // interior mutability (Arc<Mutex<...>>), so clone is cheap and
        // race-free. Falls back to zero-fill if the backend declines
        // (e.g. CONFIG protocol feature not negotiated).
        let mut frontend = self.frontend.clone();
        let size = data.len() as u32;
        match frontend.get_config(
            offset as u32,
            size,
            VhostUserConfigFlags::WRITABLE,
            &vec![0u8; data.len()],
        ) {
            Ok((_hdr, payload)) => {
                let n = payload.len().min(data.len());
                data[..n].copy_from_slice(&payload[..n]);
                for b in &mut data[n..] {
                    *b = 0;
                }
            }
            Err(e) => {
                log::debug!("vhost-user get_config: {e}; zero-filling");
                for b in data.iter_mut() {
                    *b = 0;
                }
            }
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        if let Err(e) =
            self.frontend
                .set_config(offset as u32, VhostUserConfigFlags::WRITABLE, data)
        {
            log::debug!("vhost-user set_config: {e}");
        }
    }
}

/// Fork+exec `/proc/self/exe backend <kind> --fd=N` over a fresh
/// socketpair; return the parent-side stream + Child handle.
pub fn spawn_backend(kind: &str) -> std::io::Result<(UnixStream, Child)> {
    use std::os::unix::io::{AsRawFd, IntoRawFd};
    let (parent, child_sock) = UnixStream::pair()?;
    let child_fd = child_sock.into_raw_fd();
    let exe = std::env::current_exe()?;
    // Clear FD_CLOEXEC on the child half so it survives execve.
    // SAFETY: fcntl is a pure syscall; arg validated by kernel.
    #[allow(unsafe_code)]
    unsafe {
        let flags = libc::fcntl(child_fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(child_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
        }
    }
    let child = std::process::Command::new(&exe)
        .arg("backend")
        .arg(kind)
        .arg("--fd")
        .arg(child_fd.to_string())
        .spawn()?;
    // Close our copy of the child fd; the child owns it post-exec.
    // SAFETY: close on a valid fd we no longer reference.
    #[allow(unsafe_code)]
    unsafe {
        libc::close(child_fd);
    }
    log::info!(
        "spawned `{}` backend as PID {} (parent fd={})",
        kind,
        child.id(),
        parent.as_raw_fd()
    );
    Ok((parent, child))
}
