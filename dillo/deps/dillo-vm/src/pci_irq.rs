// SPDX-License-Identifier: Apache-2.0

//! MSI-X → KVM irqfd bridge.
//!
//! Cherry-picked from the PoC's `dillo-vmm/src/pci.rs` (the
//! `IrqfdNotifier` block at lines ~738–871). The PoC keeps it in
//! the same module as the rest of the PCI bus / device-trait code,
//! but in the rewrite the PCI dispatcher lives elsewhere (and is
//! the topic of Phase 2 work) — keeping IrqfdNotifier in its own
//! file isolates the irq.rs↔vm-pci dependency to one place.

use std::sync::{Arc, Mutex};

use dillo_pci::{MsixNotifier, MsixTableEntry};
use dillo_virtio::Interrupt;
use vmm_sys_util::eventfd::EventFd;

use crate::irq::IrqManager;

/// MSI-X notifier that bridges vector updates to KVM irqfd via [`IrqManager`].
///
/// Each MSI-X vector maps to a KVM GSI with an irqfd. When the guest
/// programs a vector's address/data via BAR2 (MSI-X table) writes,
/// [`MsixNotifier::vector_updated`] is called. On first programming, a
/// fresh irqfd is allocated via `IrqManager::allocate_irqfd`. On
/// re-programming, the existing GSI route is updated to the new
/// address/data.
///
/// At device-activate time the device's `set_vring_call(idx, fd)` is
/// driven from [`Self::get_irqfd_for_vector`] so the backend's writes
/// trigger KVM to inject MSI-X directly — no VMM relay.
pub struct IrqfdNotifier {
    irq_manager: Arc<Mutex<IrqManager>>,
    /// Per-vector: (gsi, eventfd clone for signaling).
    vectors: Mutex<Vec<Option<(u32, EventFd)>>>,
}

impl std::fmt::Debug for IrqfdNotifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IrqfdNotifier")
            .field(
                "num_vectors",
                &self.vectors.lock().map(|v| v.len()).unwrap_or(0),
            )
            .finish_non_exhaustive()
    }
}

impl IrqfdNotifier {
    /// Create with `num_vectors` empty slots. Slots fill in lazily as
    /// the guest programs each MSI-X vector.
    pub fn new(irq_manager: Arc<Mutex<IrqManager>>, num_vectors: u16) -> Self {
        let mut vectors = Vec::with_capacity(num_vectors as usize);
        for _ in 0..num_vectors {
            vectors.push(None);
        }
        Self {
            irq_manager,
            vectors: Mutex::new(vectors),
        }
    }

    /// Clone the eventfd for `vector` (wrapped as an [`Interrupt`]) if it
    /// has been programmed by the guest. Used at device-activate time to
    /// pick `set_vring_call`'s fd per virtqueue.
    pub fn get_irqfd_for_vector(&self, vector: u16) -> Option<Interrupt> {
        let vectors = self.vectors.lock().ok()?;
        let slot = vectors.get(vector as usize)?.as_ref()?;
        slot.1.try_clone().ok().map(Interrupt::from_eventfd)
    }

    /// All GSIs allocated for this notifier. Used by device-removal
    /// paths to free GSIs cleanly via `IrqManager::teardown_device_irqfds`
    /// (post-MVP, but the accessor is harmless to keep).
    pub fn collect_gsis(&self) -> Vec<u32> {
        let vectors = match self.vectors.lock() {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        vectors
            .iter()
            .filter_map(|slot| slot.as_ref().map(|(gsi, _)| *gsi))
            .collect()
    }
}

impl MsixNotifier for IrqfdNotifier {
    fn vector_updated(&self, vector: u16, entry: &MsixTableEntry) {
        log::info!(
            "IrqfdNotifier::vector_updated: vector={vector} addr={:#x} data={:#x} ctl={:#x}",
            entry.msg_addr_lo,
            entry.msg_data,
            entry.vector_ctl
        );
        // Bit 0 of vector_ctl is the per-vector mask. The PCIe spec
        // says masked vectors should not deliver interrupts; we skip
        // route allocation entirely until the guest unmasks.
        if entry.vector_ctl & 1 != 0 {
            return;
        }

        let mut vectors = match self.vectors.lock() {
            Ok(v) => v,
            Err(_) => return,
        };
        let idx = vector as usize;
        if idx >= vectors.len() {
            return;
        }

        let mut mgr = match self.irq_manager.lock() {
            Ok(m) => m,
            Err(_) => return,
        };

        if let Some((gsi, _)) = &vectors[idx] {
            let gsi = *gsi;
            if let Err(e) =
                mgr.update_route(gsi, entry.msg_addr_lo, entry.msg_addr_hi, entry.msg_data)
            {
                log::error!("IrqfdNotifier: failed to update route for vector {vector}: {e}");
            } else {
                log::debug!(
                    "IrqfdNotifier: updated vector {vector} gsi={gsi} addr={:#x} data={:#x}",
                    entry.msg_addr_lo,
                    entry.msg_data
                );
            }
        } else {
            match mgr.allocate_irqfd(entry.msg_addr_lo, entry.msg_addr_hi, entry.msg_data) {
                Ok((gsi, fd)) => {
                    log::debug!(
                        "IrqfdNotifier: allocated vector {vector} gsi={gsi} addr={:#x} data={:#x}",
                        entry.msg_addr_lo,
                        entry.msg_data
                    );
                    vectors[idx] = Some((gsi, fd));
                }
                Err(e) => {
                    log::error!("IrqfdNotifier: failed to allocate irqfd for vector {vector}: {e}");
                }
            }
        }
    }

    fn msix_enabled(&self, enabled: bool) {
        log::info!("IrqfdNotifier: MSI-X enabled={enabled}");
    }
}
