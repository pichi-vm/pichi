//! Windows/WHP device glue for virtio-console over virtio-pci.
//!
//! WHP exposes an interrupt-injection primitive, but Dillo still owns the
//! guest-visible PCI/MSI-X policy. This module records guest-programmed MSI-X
//! table entries and turns device completion signals into fixed local-APIC
//! vector injections.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use dillo_machine_backend::InterruptController;
use dillo_pci::{MsixNotifier, MsixTableEntry};
use dillo_virtio::Interrupt;

#[derive(Clone, Copy, Debug, Default)]
struct Vector {
    addr: u64,
    data: u32,
    masked: bool,
}

#[derive(Debug)]
pub(crate) struct WhpMsixNotifier {
    interrupt_controller: InterruptController,
    vectors: Mutex<Vec<Vector>>,
    enabled: AtomicBool,
}

impl WhpMsixNotifier {
    pub(crate) fn new(interrupt_controller: InterruptController, count: u16) -> Self {
        Self {
            interrupt_controller,
            vectors: Mutex::new(vec![Vector::default(); count as usize]),
            enabled: AtomicBool::new(false),
        }
    }

    pub(crate) fn interrupt_for(self: &Arc<Self>, vector: u16) -> Option<Interrupt> {
        let me = Arc::clone(self);
        Some(Interrupt::from_fn(move || {
            let Some(msi) = me.msi_for(vector) else {
                return;
            };
            if let Err(e) = me
                .interrupt_controller
                .request_fixed_interrupt(msi.destination, msi.vector)
            {
                log::warn!(
                    "WHP MSI-X inject failed for table vector {vector}, APIC destination {}, vector {:#x}: {e}",
                    msi.destination,
                    msi.vector,
                );
            }
        }))
    }

    fn msi_for(&self, vector: u16) -> Option<FixedMsi> {
        if !self.enabled.load(Ordering::SeqCst) {
            return None;
        }
        let vectors = self.vectors.lock().expect("msix table poisoned");
        let entry = *vectors.get(vector as usize)?;
        if entry.masked {
            return None;
        }
        decode_fixed_msi(entry)
    }
}

impl MsixNotifier for WhpMsixNotifier {
    fn vector_updated(&self, vector: u16, entry: &MsixTableEntry) {
        let mut vectors = self.vectors.lock().expect("msix table poisoned");
        if let Some(slot) = vectors.get_mut(vector as usize) {
            slot.addr = (u64::from(entry.msg_addr_hi) << 32) | u64::from(entry.msg_addr_lo);
            slot.data = entry.msg_data;
            slot.masked = entry.is_masked();
        }
    }

    fn msix_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FixedMsi {
    destination: u32,
    vector: u8,
}

fn decode_fixed_msi(entry: Vector) -> Option<FixedMsi> {
    const MSI_ADDR_BASE_MASK: u64 = 0xFFF0_0000;
    const MSI_ADDR_BASE: u64 = 0xFEE0_0000;
    const MSI_ADDR_DEST_SHIFT: u64 = 12;
    const MSI_ADDR_DEST_MASK: u64 = 0xFF;
    const MSI_DATA_VECTOR_MASK: u32 = 0xFF;
    const MSI_DATA_DELIVERY_MODE_MASK: u32 = 0x700;
    const MSI_DATA_LEVEL_ASSERT: u32 = 1 << 14;
    const MSI_DATA_TRIGGER_LEVEL: u32 = 1 << 15;

    if (entry.addr & MSI_ADDR_BASE_MASK) != MSI_ADDR_BASE {
        log::warn!(
            "WHP MSI-X entry has non-local-APIC address {:#x}",
            entry.addr
        );
        return None;
    }
    if entry.data & MSI_DATA_DELIVERY_MODE_MASK != 0 {
        log::warn!(
            "WHP MSI-X entry uses unsupported delivery mode data={:#x}",
            entry.data
        );
        return None;
    }
    if entry.data & (MSI_DATA_LEVEL_ASSERT | MSI_DATA_TRIGGER_LEVEL) != 0 {
        log::warn!(
            "WHP MSI-X entry uses unsupported level/trigger data={:#x}",
            entry.data
        );
        return None;
    }

    Some(FixedMsi {
        destination: ((entry.addr >> MSI_ADDR_DEST_SHIFT) & MSI_ADDR_DEST_MASK) as u32,
        vector: (entry.data & MSI_DATA_VECTOR_MASK) as u8,
    })
}

#[cfg(test)]
mod tests {
    use super::{FixedMsi, Vector, decode_fixed_msi};

    #[test]
    fn decodes_fixed_physical_msi() {
        assert_eq!(
            decode_fixed_msi(Vector {
                addr: 0xFEE0_3000,
                data: 0x45,
                masked: false,
            }),
            Some(FixedMsi {
                destination: 3,
                vector: 0x45,
            })
        );
    }

    #[test]
    fn rejects_non_lapic_msi_address() {
        assert!(
            decode_fixed_msi(Vector {
                addr: 0xDEAD_0000,
                data: 0x45,
                masked: false,
            })
            .is_none()
        );
    }
}
