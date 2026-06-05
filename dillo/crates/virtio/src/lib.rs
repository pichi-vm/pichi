// SPDX-License-Identifier: Apache-2.0

//! Virtio device primitives: split virtqueue, device trait, feature constants.
//!
//! This crate provides transport-agnostic virtio building blocks that form
//! the foundation of the dillo virtio stack. Device implementations
//! (virtio-blk, virtio-console, virtio-vsock) depend on this crate for the
//! [`VirtioDevice`] trait and [`Queue`]/[`DescriptorChain`] types. Transport
//! layers (virtio-pci) consume [`VirtioDevice`] to present devices on a bus.
//!
//! # Key types
//!
//! - [`VirtioDevice`]: Trait that all virtio devices implement. Covers feature
//!   negotiation, config space access, queue sizing, and activation.
//! - [`Queue`]: Split virtqueue with descriptor table, available ring, and used
//!   ring. Provides `pop()` and `add_used()` for device-side processing.
//! - [`DescriptorChain`]: A single descriptor from the virtqueue, carrying a
//!   guest address, length, flags (readable/writable/next), and chain link.
//! - [`features`]: Common feature bit constants (`VIRTIO_F_VERSION_1`,
//!   `VIRTIO_F_RING_EVENT_IDX`) and device type codes.
//!
//! Device crates are transport-agnostic by design: they never import
//! `virtio-pci` or `dillo-vmm`.

pub mod device;
pub mod features;
pub mod interrupt;
pub mod kick;
pub mod queue;

pub use device::{ActivateError, VirtioDevice};
pub use features::{VIRTIO_F_RING_EVENT_IDX, VIRTIO_F_VERSION_1};
pub use interrupt::Interrupt;
pub use kick::Kick;
pub use queue::{DescriptorChain, Queue, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE};
