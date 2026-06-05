// SPDX-License-Identifier: Apache-2.0

// Portions derived from Firecracker (https://github.com/firecracker-microvm/firecracker),
// licensed under Apache-2.0 OR BSD-3-Clause.

//! PCI configuration space primitives and MSI-X support.
//!
//! This crate provides VMM-neutral PCI building blocks that have no dependency
//! on KVM, guest memory, or virtio. Higher-level crates (`virtio-pci`,
//! `dillo-vmm`) compose these types into full PCI device implementations.
//!
//! # Key types
//!
//! - [`PciConfiguration`]: 256-byte Type 0 PCI config space with BAR
//!   management, capability list, and register read/write.
//! - [`MsixTable`] / [`MsixCap`]: MSI-X table and capability structures.
//!   The [`MsixNotifier`] trait abstracts interrupt delivery so that tests
//!   can use [`NoopNotifier`] while production code routes through irqfd.
//! - [`PciBdf`]: Bus/device/function address encoding.
//! - [`BarType`]: BAR type (memory 32/64-bit, I/O).
//! - [`address`]: CF8/CFC legacy PIO and ECAM MMIO address decoding.
//! - [`capability`]: Standard capability IDs (MSI-X, PCIe, PM, vendor).

/// CF8/CFC legacy PIO and ECAM MMIO address decoding.
pub mod address;
/// BAR type definitions and decoding.
pub mod bar;
/// PCI Bus/Device/Function address encoding.
pub mod bdf;
/// Standard PCI capability IDs.
pub mod capability;
/// 256-byte Type 0 PCI configuration space.
pub mod configuration;
/// MSI-X table, capability, and notifier trait.
pub mod msix;

pub use address::{parse_cf8, parse_ecam_offset};
pub use bar::BarType;
pub use bdf::PciBdf;
pub use capability::{CAP_ID_MSIX, CAP_ID_PCIE, CAP_ID_PM, CAP_ID_VENDOR};
pub use configuration::PciConfiguration;
pub use msix::{MsixCap, MsixNotifier, MsixTable, MsixTableEntry, NoopNotifier};
