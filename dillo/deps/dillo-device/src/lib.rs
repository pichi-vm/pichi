//! Symmetric device-side abstraction per ARCH §10.2 + §10.3.
//!
//! Per-device backends (virtio-console, future virtio-blk, vsock,
//! vgpt) implement [`DeviceBackend`] once. [`ProcessHost`] (Linux,
//! vhost-user) and [`ThreadHost`] (macOS/Windows, in-process
//! channels) wrap any backend so the backend code is host-agnostic.

use std::sync::Arc;

use virtio::{Interrupt as VirtioInterrupt, Kick};
use vm_memory::GuestMemoryMmap;

/// One virtqueue handle as seen by a backend. Wraps the transport's
/// `Queue` together with its platform-specific kick + call handles.
pub struct BackendQueue {
    pub queue: virtio::queue::Queue,
    /// Guest → device notification.
    pub kick: Kick,
    /// Device → guest interrupt; cloned from the transport's MSI-X
    /// interrupt allocation.
    pub call: Option<VirtioInterrupt>,
}

impl std::fmt::Debug for BackendQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackendQueue").finish_non_exhaustive()
    }
}

/// Abstracted interrupt-injection handle handed to backends at
/// activate time. Wraps either an irqfd (process-mode) or an mpsc
/// sender (thread-mode).
#[derive(Clone, Debug)]
pub struct Interrupt {
    inner: Arc<dyn InterruptInner>,
}

impl Interrupt {
    pub fn from_inner(inner: Arc<dyn InterruptInner>) -> Self {
        Self { inner }
    }
    pub fn signal(&self, vector: u16) {
        self.inner.signal(vector);
    }
}

/// Trait object behind [`Interrupt`] so process- and thread-mode
/// hosts can share the same backend-facing API.
pub trait InterruptInner: Send + Sync + std::fmt::Debug {
    fn signal(&self, vector: u16);
}

/// Per-device contract — implemented once per device kind.
pub trait DeviceBackend: Send + std::fmt::Debug {
    /// Static PCI configuration-space bytes the transport exposes to
    /// the guest (typically empty for virtio devices since the
    /// transport synthesizes the config).
    fn pci_config_space(&self) -> &[u8] {
        &[]
    }

    /// Device feature bits (including `VIRTIO_F_VERSION_1`).
    fn features(&self) -> u64;

    /// Acknowledge the features the driver accepted.
    fn set_driver_features(&mut self, features: u64) {
        let _ = features;
    }

    /// Read device-config bytes (virtio device-config region).
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let _ = offset;
        for b in data.iter_mut() {
            *b = 0;
        }
    }

    /// Write device-config bytes.
    fn write_config(&mut self, offset: u64, data: &[u8]) {
        let _ = (offset, data);
    }

    /// Called after the driver writes DRIVER_OK with negotiated
    /// queues. The backend typically spawns I/O worker threads
    /// blocking on `queue.kick`.
    fn activate(&mut self, queues: Vec<BackendQueue>, mem: GuestMemoryMmap, interrupt: Interrupt);

    /// Called when the driver resets the device. Default: no-op.
    fn deactivate(&mut self) {}
}

/// Wraps a [`DeviceBackend`] for running inside a thread of the VM
/// process — the macOS/Windows model per ARCH §4.2, plus the
/// `--no-default-features` (thread-only) Linux build for development.
#[derive(Debug)]
pub struct ThreadHost<B: DeviceBackend> {
    pub backend: B,
}

impl<B: DeviceBackend> ThreadHost<B> {
    pub fn new(backend: B) -> Self {
        Self { backend }
    }
}

/// Wraps a [`DeviceBackend`] for running as a forked Linux child via
/// vhost-user. Stub today — Phase 3 will fill out the
/// `BackendMutAdapter`-based vhost-user loop.
#[derive(Debug)]
pub struct ProcessHost<B: DeviceBackend> {
    pub backend: B,
}

impl<B: DeviceBackend> ProcessHost<B> {
    pub fn new(backend: B) -> Self {
        Self { backend }
    }
}
