//! Range-based MMIO dispatcher.
//!
//! Each registered handler covers `[base, base+size)`. On a guest MMIO
//! exit (`VmExit::MmioRead` / `VmExit::MmioWrite`), the bus picks the
//! one matching range and forwards the access with its
//! GPA-relative offset.
//!
//! No locking inside the dispatcher itself — the bus is built at
//! startup and frozen for the VM's lifetime; handlers wrap whatever
//! internal mutability they need.

use std::sync::Arc;

/// A guest-physical MMIO window owned by one device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MmioWindow {
    pub name: &'static str,
    pub base: u64,
    pub size: u64,
}

/// A device with one or more guest-visible MMIO windows.
pub(crate) trait MmioDevice: Send + Sync {
    fn windows(&self) -> Vec<MmioWindow>;
    fn read(&self, window: MmioWindow, offset: u64, data: &mut [u8]) -> bool;
    fn write(&self, window: MmioWindow, offset: u64, data: &[u8]) -> bool;
}

struct Range {
    window: MmioWindow,
    device: Arc<dyn MmioDevice>,
}

/// MMIO bus. Built at startup via [`MmioBus::register_device`]; queried per
/// guest exit via [`MmioBus::read`] / [`MmioBus::write`].
#[derive(Default)]
pub(crate) struct MmioBus {
    ranges: Vec<Range>,
}

impl std::fmt::Debug for MmioBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MmioBus")
            .field("range_count", &self.ranges.len())
            .finish()
    }
}

impl MmioBus {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Register an MMIO device.
    pub(crate) fn register_device<D>(&mut self, device: Arc<D>)
    where
        D: MmioDevice + 'static,
    {
        let device: Arc<dyn MmioDevice> = device;
        let windows = device.windows();
        assert!(
            !windows.is_empty(),
            "MMIO device must expose at least one window"
        );
        for window in windows {
            let new_end = window
                .base
                .checked_add(window.size)
                .expect("MMIO range size overflow");
            for r in &self.ranges {
                let end = r.window.base + r.window.size;
                let overlap = window.base < end && r.window.base < new_end;
                assert!(
                    !overlap,
                    "MMIO range overlap: new `{name}` [{:#x}..{:#x}) collides with `{}` [{:#x}..{:#x})",
                    window.base,
                    new_end,
                    r.window.name,
                    r.window.base,
                    end,
                    name = window.name,
                );
            }
            self.ranges.push(Range {
                window,
                device: Arc::clone(&device),
            });
        }
    }

    /// Dispatch a guest MMIO read. Returns `true` if a registered
    /// handler claimed the address (in which case `data` is filled).
    pub(crate) fn read(&self, addr: u64, data: &mut [u8]) -> bool {
        if let Some(r) = self.find(addr, data.len() as u64) {
            return r.device.read(r.window, addr - r.window.base, data);
        }
        false
    }

    /// Dispatch a guest MMIO write. Returns `true` if a registered
    /// handler claimed the address.
    pub(crate) fn write(&self, addr: u64, data: &[u8]) -> bool {
        if let Some(r) = self.find(addr, data.len() as u64) {
            return r.device.write(r.window, addr - r.window.base, data);
        }
        false
    }

    fn find(&self, addr: u64, len: u64) -> Option<&Range> {
        let end = addr.checked_add(len)?;
        self.ranges
            .iter()
            .find(|r| r.window.base <= addr && end <= r.window.base + r.window.size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TestDevice {
        window: MmioWindow,
        written: AtomicU64,
    }

    impl TestDevice {
        fn new(name: &'static str, base: u64, size: u64) -> Self {
            Self {
                window: MmioWindow { name, base, size },
                written: AtomicU64::new(0),
            }
        }
    }

    impl MmioDevice for TestDevice {
        fn windows(&self) -> Vec<MmioWindow> {
            vec![self.window]
        }

        fn read(&self, _window: MmioWindow, offset: u64, data: &mut [u8]) -> bool {
            data.fill(offset as u8);
            true
        }

        fn write(&self, _window: MmioWindow, offset: u64, data: &[u8]) -> bool {
            let mut buf = [0u8; 8];
            let n = data.len().min(8);
            buf[..n].copy_from_slice(&data[..n]);
            self.written
                .store(offset | (u64::from_le_bytes(buf) << 32), Ordering::SeqCst);
            true
        }
    }

    #[test]
    fn read_dispatches_with_offset() {
        let mut bus = MmioBus::new();
        bus.register_device(Arc::new(TestDevice::new("test", 0x1000, 0x100)));
        let mut buf = [0u8; 4];
        assert!(bus.read(0x1042, &mut buf));
        assert_eq!(buf, [0x42; 4]);
        assert!(!bus.read(0x900, &mut buf));
    }

    #[test]
    fn write_dispatches() {
        let device = Arc::new(TestDevice::new("test", 0x2000, 0x100));
        let mut bus = MmioBus::new();
        bus.register_device(Arc::clone(&device));
        assert!(bus.write(0x2080, &[0xAA, 0xBB]));
        assert_eq!(device.written.load(Ordering::SeqCst) & 0xFFFF_FFFF, 0x80);
    }

    #[test]
    fn device_read_dispatches_with_offset() {
        let mut bus = MmioBus::new();
        bus.register_device(Arc::new(TestDevice::new("device", 0x3000, 0x100)));

        let mut buf = [0u8; 4];
        assert!(bus.read(0x3042, &mut buf));
        assert_eq!(buf, [0x42; 4]);
        assert!(!bus.read(0x2fff, &mut buf));
    }

    #[test]
    fn device_write_dispatches_with_offset() {
        let device = Arc::new(TestDevice::new("device", 0x4000, 0x100));
        let mut bus = MmioBus::new();
        bus.register_device(Arc::clone(&device));

        assert!(bus.write(0x4080, &[0xAA, 0xBB]));
        assert_eq!(device.written.load(Ordering::SeqCst) & 0xFFFF_FFFF, 0x80);
    }

    #[test]
    #[should_panic(expected = "MMIO range overlap")]
    fn overlap_panics() {
        let mut bus = MmioBus::new();
        bus.register_device(Arc::new(TestDevice::new("a", 0x1000, 0x100)));
        bus.register_device(Arc::new(TestDevice::new("b", 0x1080, 0x100)));
    }

    #[test]
    #[should_panic(expected = "MMIO range overlap")]
    fn device_overlap_panics() {
        let mut bus = MmioBus::new();
        bus.register_device(Arc::new(TestDevice::new("a", 0x1000, 0x100)));
        bus.register_device(Arc::new(TestDevice::new("b", 0x1080, 0x100)));
    }
}
