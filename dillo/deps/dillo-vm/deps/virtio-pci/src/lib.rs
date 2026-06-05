// SPDX-License-Identifier: Apache-2.0

//! Virtio PCI transport layer.
//!
//! This crate bridges the virtio device model and the PCI bus. It wraps any
//! [`VirtioDevice`](virtio::VirtioDevice) as a PCI device with all 5 virtio
//! PCI capabilities (common cfg, notify, ISR, device cfg, PCI cfg access),
//! MSI-X interrupts, device status FSM, feature negotiation, and
//! ioeventfd-based queue notification.
//!
//! # Composition chain
//!
//! `VirtioPciDevice` wraps any `Arc<Mutex<dyn VirtioDevice>>`. In dillo,
//! the concrete implementation is always `VhostUserFrontendDevice` (from the
//! `vhost_frontend` module in `dillo-vmm`), which delegates all device I/O
//! to an out-of-process backend via the vhost-user protocol. On `activate()`,
//! the frontend performs the vhost-user handshake with the backend process.
//!
//! `dillo-vmm::pci::VirtioPciAdapter` wraps `VirtioPciDevice` as `PciDevice`,
//! and `PciBus` dispatches config-space and BAR accesses to it.
//!
//! This layering keeps PCI transport concerns separate from device I/O.
//!
//! # BAR 0 layout
//!
//! All virtio PCI regions are mapped into a single 4 KiB BAR:
//! - `0x000..0x038`: Common configuration (device status, feature bits, queues)
//! - `0x038..0x03C`: ISR status
//! - `0x040..0x080`: Device-specific configuration
//! - `0x100..`:      Per-queue notify registers (2-byte stride)
//!
//! MSI-X uses BAR 1 for the table and BAR 2 for the PBA.

/// Virtio PCI capability structure helpers for config space registration.
pub(crate) mod capabilities;
/// PCI transport implementation: device status FSM, feature negotiation, BAR I/O.
pub mod transport;

pub use transport::VirtioPciDevice;
