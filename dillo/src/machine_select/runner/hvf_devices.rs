//! macOS/HVF guest-memory glue.
//!
//! [`build_guest_memory`] creates a `vm-memory` view over HVF-mapped guest RAM,
//! built from host pointers rather than a memfd.

use anyhow::{Result, anyhow};
use vm_memory::mmap::MmapRegionBuilder;
use vm_memory::{GuestAddress, GuestMemoryMmap, GuestRegionMmap};

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
