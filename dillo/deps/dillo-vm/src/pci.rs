//! PCI device trait + single-bus dispatcher + minimal host bridge.
//!
//! Slimmed from the PoC's `dillo-vmm/src/pci.rs` — no hotplug, no
//! secondary-bus / root-port, no multi-function. Per ARCH §11.2 MVP
//! "single-function devices only, no hot-plug, no multi-function,
//! no PCIe-PM, no AER."

use std::sync::Mutex;

use crate::mmio_bus::{MmioDevice, MmioWindow};
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

/// PCIe root complex declared by the base DTB.
///
/// The root owns the ECAM MMIO window and the single downstream PCI bus.
/// x86 legacy CF8/CFC access is a backend PIO decoder onto this same config
/// accessor; it is not a second PCI fabric.
pub(crate) struct PciRoot {
    window: MmioWindow,
    bus: PciBus,
}

impl std::fmt::Debug for PciRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PciRoot")
            .field("window", &self.window)
            .field("bus", &self.bus)
            .finish()
    }
}

impl PciRoot {
    pub(crate) fn new(window: MmioWindow) -> Self {
        Self {
            window,
            bus: PciBus::new_with_host_bridge(),
        }
    }

    pub(crate) fn register(&mut self, slot: u8, device: Box<dyn PciDevice>) {
        self.bus.register(slot, device);
    }

    pub(crate) fn config_read(&self, bus: u8, device: u8, function: u8, reg_idx: usize) -> u32 {
        self.bus.config_read(bus, device, function, reg_idx)
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
        self.bus
            .config_write(bus, device, function, reg_idx, offset, data);
    }

    pub(crate) fn enumerate_bars(&self) -> Vec<(u8, BarRegion)> {
        self.bus.enumerate_bars()
    }

    pub(crate) fn bar_read(&self, slot: u8, bar_idx: u8, offset: u64, data: &mut [u8]) -> bool {
        self.bus.bar_read(slot, bar_idx, offset, data)
    }

    pub(crate) fn bar_write(&self, slot: u8, bar_idx: u8, offset: u64, data: &[u8]) -> bool {
        self.bus.bar_write(slot, bar_idx, offset, data)
    }

    fn decode_ecam(offset: u64) -> (u8, u8, u8, usize, usize) {
        let (bus, device, function, register) = vm_pci::parse_ecam_offset(offset);
        let reg_byte = register as usize;
        (bus, device, function, reg_byte >> 2, reg_byte & 0x3)
    }

    fn bar_window(_slot: u8, bar: &BarRegion) -> MmioWindow {
        MmioWindow {
            name: "pci-bar",
            base: bar.base_gpa,
            size: bar.size,
        }
    }

    fn bar_route(&self, window: MmioWindow) -> Option<(u8, u8)> {
        self.enumerate_bars()
            .into_iter()
            .find(|(_, bar)| bar.base_gpa == window.base && bar.size == window.size)
            .map(|(slot, bar)| (slot, bar.bar_idx))
    }
}

impl MmioDevice for PciRoot {
    fn windows(&self) -> Vec<MmioWindow> {
        let mut windows = vec![self.window];
        windows.extend(
            self.enumerate_bars()
                .into_iter()
                .map(|(slot, bar)| Self::bar_window(slot, &bar)),
        );
        windows
    }

    fn read(&self, window: MmioWindow, offset: u64, data: &mut [u8]) -> bool {
        if window.base == self.window.base && window.size == self.window.size {
            let (bus, device, function, reg_idx, in_dword) = Self::decode_ecam(offset);
            let value = self.config_read(bus, device, function, reg_idx);
            let bytes = value.to_le_bytes();
            for (i, slot) in data.iter_mut().enumerate() {
                *slot = *bytes.get(in_dword + i).unwrap_or(&0xFF);
            }
            return true;
        }

        if let Some((slot, bar_idx)) = self.bar_route(window) {
            return self.bar_read(slot, bar_idx, offset, data);
        }

        false
    }

    fn write(&self, window: MmioWindow, offset: u64, data: &[u8]) -> bool {
        if window.base == self.window.base && window.size == self.window.size {
            let (bus, device, function, reg_idx, in_dword) = Self::decode_ecam(offset);
            self.config_write(bus, device, function, reg_idx, in_dword as u64, data);
            return true;
        }

        if let Some((slot, bar_idx)) = self.bar_route(window) {
            return self.bar_write(slot, bar_idx, offset, data);
        }

        false
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

    #[test]
    fn pci_root_ecam_reads_host_bridge_config() {
        let root = PciRoot::new(MmioWindow {
            name: "pcie-ecam",
            base: 0x3000_0000,
            size: 0x1000_0000,
        });
        let mut data = [0u8; 4];

        assert!(root.read(root.window, 0, &mut data));

        assert_eq!(u32::from_le_bytes(data), root.config_read(0, 0, 0, 0));
        assert_eq!(data, [0x86, 0x80, 0x37, 0x12]);
    }

    #[test]
    fn pci_root_ecam_reads_unaligned_bytes() {
        let root = PciRoot::new(MmioWindow {
            name: "pcie-ecam",
            base: 0x3000_0000,
            size: 0x1000_0000,
        });
        let mut data = [0u8; 2];

        assert!(root.read(root.window, 1, &mut data));

        assert_eq!(data, [0x80, 0x37]);
    }

    #[test]
    fn pci_root_windows_include_ecam_and_bars() {
        struct BarDevice;

        impl std::fmt::Debug for BarDevice {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct("BarDevice").finish()
            }
        }

        impl PciDevice for BarDevice {
            fn config_read(&self, _reg_idx: usize) -> u32 {
                0
            }

            fn config_write(&mut self, _reg_idx: usize, _offset: u64, _data: &[u8]) {}

            fn name(&self) -> &str {
                "bar-device"
            }

            fn bar_regions(&self) -> Vec<BarRegion> {
                vec![BarRegion {
                    bar_idx: 2,
                    base_gpa: 0x8000_0000,
                    size: 0x1000,
                }]
            }
        }

        let mut root = PciRoot::new(MmioWindow {
            name: "pcie-ecam",
            base: 0x3000_0000,
            size: 0x1000_0000,
        });
        root.register(1, Box::new(BarDevice));

        let windows = root.windows();

        assert!(windows.contains(&root.window));
        assert!(
            windows
                .iter()
                .any(|w| w.base == 0x8000_0000 && w.size == 0x1000)
        );
    }

    #[test]
    fn pci_root_routes_bar_window_reads() {
        struct BarDevice;

        impl std::fmt::Debug for BarDevice {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct("BarDevice").finish()
            }
        }

        impl PciDevice for BarDevice {
            fn config_read(&self, _reg_idx: usize) -> u32 {
                0
            }

            fn config_write(&mut self, _reg_idx: usize, _offset: u64, _data: &[u8]) {}

            fn name(&self) -> &str {
                "bar-device"
            }

            fn bar_regions(&self) -> Vec<BarRegion> {
                vec![BarRegion {
                    bar_idx: 2,
                    base_gpa: 0x8000_0000,
                    size: 0x1000,
                }]
            }

            fn bar_read(&self, bar_idx: u8, offset: u64, data: &mut [u8]) -> bool {
                data[0] = bar_idx;
                data[1] = offset as u8;
                true
            }
        }

        let mut root = PciRoot::new(MmioWindow {
            name: "pcie-ecam",
            base: 0x3000_0000,
            size: 0x1000_0000,
        });
        root.register(1, Box::new(BarDevice));
        let bar_window = root
            .windows()
            .into_iter()
            .find(|w| w.base == 0x8000_0000)
            .expect("BAR window");
        let mut data = [0u8; 2];

        assert!(root.read(bar_window, 0x42, &mut data));

        assert_eq!(data, [2, 0x42]);
    }
}
