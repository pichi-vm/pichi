//! macOS/HVF device glue for virtio-console over virtio-pci.
//!
//! Two pieces the Linux path provides differently:
//!   - [`HvfMsixNotifier`] — adapts PCI MSI-X table writes onto a backend-owned
//!     message-interrupt domain.
//!   - [`build_guest_memory`] — a `vm-memory` view over HVF-mapped guest RAM,
//!     built from host pointers (no memfd).

use std::sync::Arc;

use anyhow::{Result, anyhow};
use dillo_mmio::{Interrupt, MessageInterrupt, MessageInterruptDomain};
use dillo_pci::{MsixNotifier, MsixTableEntry};
use vm_memory::mmap::MmapRegionBuilder;
use vm_memory::{GuestAddress, GuestMemoryMmap, GuestRegionMmap};

/// MSI-X notifier for the HVF path: converts PCI MSI-X table changes into the
/// backend-neutral message-interrupt domain that `dillo-machine-hvf` owns.
pub(crate) struct HvfMsixNotifier {
    domain: Arc<dyn MessageInterruptDomain>,
}

impl HvfMsixNotifier {
    pub(crate) fn new(domain: Arc<dyn MessageInterruptDomain>) -> Self {
        Self { domain }
    }

    /// An [`Interrupt`] that injects the MSI currently programmed for `vector`.
    /// The table is read at *signal* time, so a vector reprogrammed after the
    /// device is activated is still honored.
    pub(crate) fn interrupt_for(self: &Arc<Self>, vector: u16) -> Option<Interrupt> {
        self.domain.interrupt(vector)
    }
}

impl MsixNotifier for HvfMsixNotifier {
    fn vector_updated(&self, vector: u16, entry: &MsixTableEntry) {
        if let Err(e) = self.domain.update(
            vector,
            MessageInterrupt {
                address: (u64::from(entry.msg_addr_hi) << 32) | u64::from(entry.msg_addr_lo),
                data: entry.msg_data,
                masked: entry.is_masked(),
            },
        ) {
            log::warn!("HVF MSI-X vector {vector} update failed: {e}");
        }
    }

    fn msix_enabled(&self, enabled: bool) {
        if let Err(e) = self.domain.enabled(enabled) {
            log::warn!("HVF MSI-X enable={enabled} failed: {e}");
        }
    }
}

/// Build a `vm-memory` view over HVF-mapped guest RAM. `regions` are
/// `(gpa, host_addr, size)` from `Vm::region_mappings()`. The host pointers are
/// owned by HVF (mapped for the VM's lifetime); the regions are non-owning so
/// Drop won't unmap them.
pub(crate) fn build_guest_memory(regions: &[(u64, u64, u64)]) -> Result<GuestMemoryMmap> {
    let mut built: Vec<GuestRegionMmap> = Vec::with_capacity(regions.len());
    for &(gpa, host_addr, size) in regions {
        // SAFETY: host_addr is an HVF-mapped region alive for the VM's
        // lifetime, of exactly `size` bytes; `owned=false` (raw pointer) means
        // Drop will not munmap it.
        #[allow(unsafe_code)]
        let region = unsafe {
            MmapRegionBuilder::new(size as usize).with_raw_mmap_pointer(host_addr as *mut u8)
        }
        .build()
        .map_err(|e| anyhow!("MmapRegionBuilder: {e}"))?;
        let gr = GuestRegionMmap::new(region, GuestAddress(gpa))
            .ok_or_else(|| anyhow!("GuestRegionMmap: gpa+size overflow at {gpa:#x}+{size}"))?;
        built.push(gr);
    }
    GuestMemoryMmap::from_regions(built).map_err(|e| anyhow!("GuestMemoryMmap: {e:?}"))
}
