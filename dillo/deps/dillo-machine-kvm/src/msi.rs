// SPDX-License-Identifier: Apache-2.0

//! MSI message → KVM irqfd bridge.

use std::sync::{Arc, Mutex};

use dillo_mmio::{Interrupt, InterruptError, MessageInterrupt, MessageInterruptDomain};
use vmm_sys_util::eventfd::EventFd;

use crate::irq::IrqManager;

/// MSI notifier that bridges message updates to KVM irqfd via [`IrqManager`].
///
/// Each vector maps to a KVM GSI with an irqfd. On first programming, a fresh
/// irqfd is allocated via `IrqManager::allocate_irqfd`. On re-programming, the
/// existing GSI route is updated to the new address/data.
///
/// At device-activate time, devices use [`Self::interrupt_for_vector`].
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

    fn eventfd_for_vector(&self, vector: u16) -> Option<EventFd> {
        let vectors = self.vectors.lock().ok()?;
        let slot = vectors.get(vector as usize)?.as_ref()?;
        slot.1.try_clone().ok()
    }

    /// Portable interrupt handle for in-process virtio devices.
    pub fn interrupt_for_vector(&self, vector: u16) -> Option<Interrupt> {
        let eventfd = self.eventfd_for_vector(vector)?;
        Some(Interrupt::from_fn(move || {
            if let Err(e) = eventfd.write(1) {
                log::warn!("KVM MSI-X irqfd signal for vector {vector} failed: {e}");
            }
        }))
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

    pub fn vector_updated(&self, vector: u16, addr_lo: u32, addr_hi: u32, data: u32, masked: bool) {
        log::info!(
            "IrqfdNotifier::vector_updated: vector={vector} addr={:#x} data={:#x} ctl={:#x}",
            addr_lo,
            data,
            if masked { 1 } else { 0 }
        );
        if masked {
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
            if let Err(e) = mgr.update_route(gsi, addr_lo, addr_hi, data) {
                log::error!("IrqfdNotifier: failed to update route for vector {vector}: {e}");
            } else {
                log::debug!(
                    "IrqfdNotifier: updated vector {vector} gsi={gsi} addr={:#x} data={:#x}",
                    addr_lo,
                    data
                );
            }
        } else {
            match mgr.allocate_irqfd(addr_lo, addr_hi, data) {
                Ok((gsi, fd)) => {
                    log::debug!(
                        "IrqfdNotifier: allocated vector {vector} gsi={gsi} addr={:#x} data={:#x}",
                        addr_lo,
                        data
                    );
                    vectors[idx] = Some((gsi, fd));
                }
                Err(e) => {
                    log::error!("IrqfdNotifier: failed to allocate irqfd for vector {vector}: {e}");
                }
            }
        }
    }

    pub fn set_enabled(&self, enabled: bool) {
        log::info!("IrqfdNotifier: MSI enabled={enabled}");
    }
}

impl MessageInterruptDomain for IrqfdNotifier {
    fn update(&self, vector: u16, msg: MessageInterrupt) -> Result<(), InterruptError> {
        self.vector_updated(
            vector,
            msg.address as u32,
            (msg.address >> 32) as u32,
            msg.data,
            msg.masked,
        );
        Ok(())
    }

    fn enabled(&self, enabled: bool) -> Result<(), InterruptError> {
        self.set_enabled(enabled);
        Ok(())
    }

    fn interrupt(&self, vector: u16) -> Option<Interrupt> {
        self.interrupt_for_vector(vector)
    }
}
