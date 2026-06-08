// SPDX-License-Identifier: Apache-2.0

//! VirtioDevice trait defining the device contract for transport layers.

use vm_memory::GuestMemoryMmap;

use crate::kick::Kick;
use crate::queue::Queue;

/// Transport-resolved activation inputs for one virtio device.
#[derive(Debug)]
pub struct VirtioActivate {
    pub mem: GuestMemoryMmap,
    pub queues: Vec<Queue>,
    pub queue_evts: Vec<Kick>,
}

impl VirtioActivate {
    pub fn new(mem: GuestMemoryMmap, queues: Vec<Queue>, queue_evts: Vec<Kick>) -> Self {
        Self {
            mem,
            queues,
            queue_evts,
        }
    }
}

/// Errors returned by [`VirtioDevice::activate`].
#[derive(Debug, thiserror::Error)]
pub enum ActivateError {
    /// The device received an invalid configuration.
    #[error("invalid device configuration: {0}")]
    InvalidConfig(String),

    /// An internal I/O error occurred during activation.
    #[error("activation I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Transport-agnostic virtio device contract.
///
/// Implementors define device-specific behaviour; the transport layer
/// (virtio-pci or virtio-mmio) handles capability layout, feature
/// negotiation, and queue setup before calling [`activate`](Self::activate).
pub trait VirtioDevice: Send {
    /// Virtio device type identifier (e.g. 1 = net, 2 = block, 3 = console).
    fn device_type(&self) -> u32;

    /// Number of virtqueues this device uses.
    fn num_queues(&self) -> usize;

    /// Maximum queue size for each virtqueue.
    fn queue_max_sizes(&self) -> &[u16];

    /// Device feature bits (including `VIRTIO_F_VERSION_1`).
    fn features(&self) -> u64;

    /// Activate the device with negotiated queues and notification eventfds.
    ///
    /// The transport calls this after feature negotiation and queue setup are
    /// complete. The device typically spawns an I/O thread that blocks on
    /// `queue_evts` and processes descriptors via the provided queues.
    ///
    /// `queue_evts` are [`Kick`]s: on Linux they wrap KVM-ioeventfd-driven
    /// eventfds; on macOS/HVF they are in-process condvar notifiers raised by
    /// the transport's MMIO notify path.
    fn activate(&mut self, activation: VirtioActivate) -> Result<(), ActivateError>;

    /// Read device-specific configuration space.
    fn read_config(&self, offset: u64, data: &mut [u8]);

    /// Write device-specific configuration space.
    fn write_config(&mut self, offset: u64, data: &[u8]);
}
