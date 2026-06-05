//! PCI device trait + single-bus dispatcher + minimal host bridge.
//!
//! Slimmed from the PoC's `dillo-vmm/src/pci.rs` — no hotplug, no
//! secondary-bus / root-port, no multi-function. Per ARCH §11.2 MVP
//! "single-function devices only, no hot-plug, no multi-function,
//! no PCIe-PM, no AER."

use std::sync::Mutex;

use vm_pci::PciConfiguration;

/// One BAR exposed by a PCI device, in GPA terms — used by the MMIO
/// bus to wire the BAR's range to the device's `bar_read`/`bar_write`.
#[derive(Debug, Clone)]
pub(crate) struct BarRegion {
    pub bar_idx: u8,
    pub base_gpa: u64,
    pub size: u64,
}

/// PCI device attached to the bus. One per slot.
pub(crate) trait PciDevice: Send + std::fmt::Debug {
    /// Read a 32-bit configuration register (`reg_idx` is the dword
    /// offset, not the byte offset).
    fn config_read(&self, reg_idx: usize) -> u32;

    /// Write to a configuration register. `offset` is the byte offset
    /// within the dword (for unaligned writes like a single u8 to the
    /// command register).
    fn config_write(&mut self, reg_idx: usize, offset: u64, data: &[u8]);

    /// Logging name (e.g. `"virtio-console"`).
    fn name(&self) -> &str;

    /// BARs this device exposes — used at startup to wire MMIO ranges.
    fn bar_regions(&self) -> Vec<BarRegion> {
        Vec::new()
    }

    /// MMIO read on a BAR range. Default: ignored.
    fn bar_read(&self, _bar_idx: u8, _offset: u64, _data: &mut [u8]) -> bool {
        false
    }

    /// MMIO write on a BAR range. Default: ignored.
    fn bar_write(&mut self, _bar_idx: u8, _offset: u64, _data: &[u8]) -> bool {
        false
    }
}

/// Number of slots on the single PCI bus. PCIe spec allows 32 device
/// numbers per bus.
pub(crate) const NUM_SLOTS: usize = 32;

/// Single-bus PCI fabric. Slot 0 typically holds a host bridge; slots
/// 1..NUM_SLOTS hold device endpoints in CLI-declaration order.
#[derive(Default)]
pub(crate) struct PciBus {
    slots: Vec<Option<Mutex<Box<dyn PciDevice>>>>,
}

impl std::fmt::Debug for PciBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<_> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                s.as_ref()
                    .map(|m| (i, m.lock().ok().map(|d| d.name().to_string())))
            })
            .collect();
        f.debug_struct("PciBus")
            .field("populated_slots", &names)
            .finish()
    }
}

impl PciBus {
    pub(crate) fn new() -> Self {
        let mut slots = Vec::with_capacity(NUM_SLOTS);
        for _ in 0..NUM_SLOTS {
            slots.push(None);
        }
        Self { slots }
    }

    /// Create with a pre-registered host bridge at slot 0. The bridge
    /// gives the kernel something to find at 00:00.0 during its PCIe
    /// enumeration walk so it doesn't conclude the bus is empty.
    pub(crate) fn new_with_host_bridge() -> Self {
        let mut bus = Self::new();
        bus.register(0, Box::new(HostBridge::new()));
        bus
    }

    /// Place `device` at `slot`. Panics if the slot is already
    /// occupied or out of range — both are wiring bugs.
    pub(crate) fn register(&mut self, slot: u8, device: Box<dyn PciDevice>) {
        assert!((slot as usize) < NUM_SLOTS, "PCI slot {slot} out of range");
        assert!(
            self.slots[slot as usize].is_none(),
            "PCI slot {slot} already occupied"
        );
        log::info!("PCI: registered '{}' at 00:{:02x}.0", device.name(), slot);
        self.slots[slot as usize] = Some(Mutex::new(device));
    }

    /// Read a config register. Single-bus, single-function only:
    /// `bus != 0` or `function != 0` returns the all-1s "no device"
    /// response, matching real hardware for unpopulated addresses.
    pub(crate) fn config_read(&self, bus: u8, device: u8, function: u8, reg_idx: usize) -> u32 {
        if bus != 0 || function != 0 || (device as usize) >= NUM_SLOTS {
            return 0xFFFF_FFFF;
        }
        match &self.slots[device as usize] {
            None => 0xFFFF_FFFF,
            Some(m) => m
                .lock()
                .expect("PCI device mutex poisoned")
                .config_read(reg_idx),
        }
    }

    pub(crate) fn config_write(
        &self,
        bus: u8,
        device: u8,
        function: u8,
        reg_idx: usize,
        offset: u64,
        data: &[u8],
    ) {
        if bus != 0 || function != 0 || (device as usize) >= NUM_SLOTS {
            return;
        }
        if let Some(m) = &self.slots[device as usize] {
            m.lock()
                .expect("PCI device mutex poisoned")
                .config_write(reg_idx, offset, data);
        }
    }

    /// Walk every populated slot and return each BAR's `(slot, region)`.
    /// The startup wiring uses this to register MMIO bus handlers.
    pub(crate) fn enumerate_bars(&self) -> Vec<(u8, BarRegion)> {
        let mut out = Vec::new();
        for (slot, m) in self.slots.iter().enumerate() {
            if let Some(m) = m {
                let dev = m.lock().expect("PCI device mutex poisoned");
                for r in dev.bar_regions() {
                    out.push((slot as u8, r));
                }
            }
        }
        out
    }

    /// BAR-range MMIO read. Caller has already matched the address to
    /// `(slot, bar_idx, base_gpa)`.
    pub(crate) fn bar_read(&self, slot: u8, bar_idx: u8, offset: u64, data: &mut [u8]) -> bool {
        match &self.slots.get(slot as usize).and_then(|s| s.as_ref()) {
            None => false,
            Some(m) => m
                .lock()
                .expect("PCI device mutex poisoned")
                .bar_read(bar_idx, offset, data),
        }
    }

    pub(crate) fn bar_write(&self, slot: u8, bar_idx: u8, offset: u64, data: &[u8]) -> bool {
        match &self.slots.get(slot as usize).and_then(|s| s.as_ref()) {
            None => false,
            Some(m) => m
                .lock()
                .expect("PCI device mutex poisoned")
                .bar_write(bar_idx, offset, data),
        }
    }
}

/// Minimal Intel-440FX-style host bridge. Pure config-space placeholder
/// so the kernel's PCIe enumeration walk finds something at 00:00.0.
pub(crate) struct HostBridge {
    config: PciConfiguration,
}

impl std::fmt::Debug for HostBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostBridge").finish_non_exhaustive()
    }
}

impl Default for HostBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl HostBridge {
    pub(crate) fn new() -> Self {
        let mut config = PciConfiguration::new();
        // Intel 440FX — vendor 0x8086, device 0x1237.
        config.set_vendor_device(0x8086, 0x1237);
        // Class code: 0x06 (bridge) / 0x00 (host bridge).
        config.set_class(0x06, 0x00, 0x00, 0x00);
        config.set_header_type(0x00);
        Self { config }
    }
}

impl PciDevice for HostBridge {
    fn config_read(&self, reg_idx: usize) -> u32 {
        self.config.read_reg(reg_idx)
    }
    fn config_write(&mut self, reg_idx: usize, offset: u64, data: &[u8]) {
        self.config.write_reg(reg_idx, offset, data);
    }
    fn name(&self) -> &str {
        "host-bridge"
    }
}

/// Adapter wrapping a `virtio_pci::VirtioPciDevice` as a `PciDevice`.
/// Forwards every call directly — the virtio-pci transport already
/// knows how to format both config-space and BAR-MMIO bytes.
pub(crate) struct VirtioPciAdapter {
    inner: virtio_pci::VirtioPciDevice,
}

impl std::fmt::Debug for VirtioPciAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtioPciAdapter").finish_non_exhaustive()
    }
}

impl VirtioPciAdapter {
    pub(crate) fn new(inner: virtio_pci::VirtioPciDevice) -> Self {
        Self { inner }
    }
}

impl PciDevice for VirtioPciAdapter {
    fn config_read(&self, reg_idx: usize) -> u32 {
        self.inner.config_read(reg_idx)
    }
    fn config_write(&mut self, reg_idx: usize, offset: u64, data: &[u8]) {
        self.inner.config_write(reg_idx, offset, data);
    }
    fn name(&self) -> &str {
        "virtio-pci"
    }
    fn bar_regions(&self) -> Vec<BarRegion> {
        self.inner
            .bar_regions()
            .into_iter()
            .map(|(bar_idx, base_gpa, size)| BarRegion {
                bar_idx,
                base_gpa,
                size,
            })
            .collect()
    }
    fn bar_read(&self, bar_idx: u8, offset: u64, data: &mut [u8]) -> bool {
        self.inner.bar_read(bar_idx, offset, data)
    }
    fn bar_write(&mut self, bar_idx: u8, offset: u64, data: &[u8]) -> bool {
        self.inner.bar_write(bar_idx, offset, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_slots_return_all_ones() {
        let bus = PciBus::new();
        assert_eq!(bus.config_read(0, 0, 0, 0), 0xFFFF_FFFF);
        assert_eq!(bus.config_read(0, 31, 0, 0), 0xFFFF_FFFF);
    }

    #[test]
    fn host_bridge_vendor_device() {
        let bus = PciBus::new_with_host_bridge();
        // Reg 0 = vendor (low 16) + device (high 16).
        let val = bus.config_read(0, 0, 0, 0);
        assert_eq!(val, 0x1237_8086);
    }

    #[test]
    fn non_zero_bus_function_returns_all_ones() {
        let bus = PciBus::new_with_host_bridge();
        assert_eq!(bus.config_read(1, 0, 0, 0), 0xFFFF_FFFF);
        assert_eq!(bus.config_read(0, 0, 1, 0), 0xFFFF_FFFF);
    }

    #[test]
    fn out_of_range_device_returns_all_ones() {
        let bus = PciBus::new_with_host_bridge();
        assert_eq!(bus.config_read(0, 32, 0, 0), 0xFFFF_FFFF);
    }
}
