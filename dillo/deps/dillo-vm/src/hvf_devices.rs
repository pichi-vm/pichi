//! macOS/HVF device glue for virtio-console over virtio-pci.
//!
//! Two pieces the Linux path provides differently:
//!   - [`HvfMsixNotifier`] — instead of routing irqfds through KVM, it records
//!     the MSI-X message (address + data) the guest programs per vector, and
//!     hands out an [`Interrupt`] closure that injects it through the in-kernel
//!     GIC (`hv_gic_send_msi`) when a queue completes.
//!   - [`build_guest_memory`] — a `vm-memory` view over HVF-mapped guest RAM,
//!     built from host pointers (no memfd).

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, anyhow};
use dillo_pci::{MsixNotifier, MsixTableEntry};
use dillo_virtio::Interrupt;
use vm_memory::mmap::MmapRegionBuilder;
use vm_memory::{GuestAddress, GuestMemoryMmap, GuestRegionMmap};

/// One MSI-X table vector as last programmed by the guest.
#[derive(Clone, Copy, Default)]
struct Vector {
    /// Full 64-bit message address (doorbell).
    addr: u64,
    /// Message data (the MBI SPI number for our GIC).
    data: u32,
    /// Per-vector mask bit.
    masked: bool,
}

/// MSI-X notifier for the HVF path: records guest-programmed vectors and
/// injects them through the in-kernel GIC. Implements [`MsixNotifier`]
/// (the same interface the KVM `IrqfdNotifier` implements).
pub(crate) struct HvfMsixNotifier {
    vectors: Mutex<Vec<Vector>>,
    /// MSI-X effectively enabled (enable bit set AND function-mask clear).
    enabled: AtomicBool,
}

impl HvfMsixNotifier {
    pub(crate) fn new(count: u16) -> Self {
        Self {
            vectors: Mutex::new(vec![Vector::default(); count as usize]),
            enabled: AtomicBool::new(false),
        }
    }

    /// An [`Interrupt`] that injects the MSI currently programmed for `vector`.
    /// The table is read at *signal* time, so a vector reprogrammed after the
    /// device is activated is still honored.
    pub(crate) fn interrupt_for(self: &Arc<Self>, vector: u16) -> Option<Interrupt> {
        let me = Arc::clone(self);
        Some(Interrupt::from_fn(move || {
            if let Some((addr, intid)) = me.msi_for(vector) {
                if let Err(e) = dillo_hypervisor::send_msi(addr, intid) {
                    log::warn!("hvf MSI-X inject (vector {vector}) failed: {e}");
                }
            }
        }))
    }

    fn msi_for(&self, vector: u16) -> Option<(u64, u32)> {
        if !self.enabled.load(Ordering::SeqCst) {
            return None;
        }
        let vectors = self.vectors.lock().expect("msix table poisoned");
        let v = vectors.get(vector as usize)?;
        if v.masked {
            return None;
        }
        Some((v.addr, v.data))
    }
}

impl MsixNotifier for HvfMsixNotifier {
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
