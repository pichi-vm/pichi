//! dillo-vm PCI transport adapters.

use std::sync::Mutex;

use dillo_pci::{BarRegion, PciDevice};

/// Adapter wrapping a `virtio_pci::VirtioPciDevice` as a `PciDevice`.
pub(crate) struct VirtioPciAdapter {
    inner: Mutex<virtio_pci::VirtioPciDevice>,
    bar_regions: Vec<BarRegion>,
}

impl std::fmt::Debug for VirtioPciAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtioPciAdapter")
            .field("bar_regions", &self.bar_regions)
            .finish_non_exhaustive()
    }
}

impl VirtioPciAdapter {
    pub(crate) fn new(inner: virtio_pci::VirtioPciDevice) -> Self {
        let bar_regions = inner
            .bar_regions()
            .into_iter()
            .map(|(bar_idx, base_gpa, size)| BarRegion {
                bar_idx,
                base_gpa,
                size,
            })
            .collect();
        Self {
            inner: Mutex::new(inner),
            bar_regions,
        }
    }
}

impl PciDevice for VirtioPciAdapter {
    fn config_read(&self, reg_idx: usize) -> u32 {
        self.inner
            .lock()
            .expect("virtio PCI transport poisoned")
            .config_read(reg_idx)
    }

    fn config_write(&self, reg_idx: usize, offset: u64, data: &[u8]) {
        self.inner
            .lock()
            .expect("virtio PCI transport poisoned")
            .config_write(reg_idx, offset, data);
    }

    fn name(&self) -> &str {
        "virtio-pci"
    }

    fn bar_regions(&self) -> &[BarRegion] {
        &self.bar_regions
    }

    fn bar_read(&self, bar_idx: u8, offset: u64, data: &mut [u8]) -> bool {
        self.inner
            .lock()
            .expect("virtio PCI transport poisoned")
            .bar_read(bar_idx, offset, data)
    }

    fn bar_write(&self, bar_idx: u8, offset: u64, data: &[u8]) -> bool {
        self.inner
            .lock()
            .expect("virtio PCI transport poisoned")
            .bar_write(bar_idx, offset, data)
    }
}
