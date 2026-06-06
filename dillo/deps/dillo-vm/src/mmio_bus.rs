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

/// Read handler: takes the GPA-relative `offset` and writes the
/// guest-visible bytes into `data`. Returns `true` if handled.
pub(crate) type ReadFn = Arc<dyn Fn(u64, &mut [u8]) -> bool + Send + Sync>;

/// Write handler: takes the GPA-relative `offset` and the bytes the
/// guest wrote. Returns `true` if handled.
pub(crate) type WriteFn = Arc<dyn Fn(u64, &[u8]) -> bool + Send + Sync>;

/// A guest-physical MMIO window owned by one device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MmioWindow {
    pub name: &'static str,
    pub base: u64,
    pub size: u64,
}

/// A device with one guest-visible MMIO window.
pub(crate) trait MmioDevice: Send + Sync {
    fn window(&self) -> MmioWindow;
    fn read(&self, offset: u64, data: &mut [u8]) -> bool;
    fn write(&self, offset: u64, data: &[u8]) -> bool;
}

struct Range {
    window: MmioWindow,
    device: Arc<dyn MmioDevice>,
}

struct ClosureMmioDevice {
    window: MmioWindow,
    read: ReadFn,
    write: WriteFn,
}

impl MmioDevice for ClosureMmioDevice {
    fn window(&self) -> MmioWindow {
        self.window
    }

    fn read(&self, offset: u64, data: &mut [u8]) -> bool {
        (self.read)(offset, data)
    }

    fn write(&self, offset: u64, data: &[u8]) -> bool {
        (self.write)(offset, data)
    }
}

/// MMIO bus. Built at startup via [`MmioBus::register`]; queried per
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

    /// Register a range. Overlapping ranges panic at registration —
    /// guest-visible MMIO conflicts are a bug in the wiring, not
    /// something to silently last-writer-wins.
    pub(crate) fn register(
        &mut self,
        name: &'static str,
        base: u64,
        size: u64,
        read: ReadFn,
        write: WriteFn,
    ) {
        self.register_device(Arc::new(ClosureMmioDevice {
            window: MmioWindow { name, base, size },
            read,
            write,
        }));
    }

    /// Register an MMIO device. This is the typed attach path; closure-based
    /// registration remains while existing devices migrate.
    pub(crate) fn register_device<D>(&mut self, device: Arc<D>)
    where
        D: MmioDevice + 'static,
    {
        let window = device.window();
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
        self.ranges.push(Range { window, device });
    }

    /// Dispatch a guest MMIO read. Returns `true` if a registered
    /// handler claimed the address (in which case `data` is filled).
    pub(crate) fn read(&self, addr: u64, data: &mut [u8]) -> bool {
        if let Some(r) = self.find(addr, data.len() as u64) {
            return r.device.read(addr - r.window.base, data);
        }
        false
    }

    /// Dispatch a guest MMIO write. Returns `true` if a registered
    /// handler claimed the address.
    pub(crate) fn write(&self, addr: u64, data: &[u8]) -> bool {
        if let Some(r) = self.find(addr, data.len() as u64) {
            return r.device.write(addr - r.window.base, data);
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
        fn window(&self) -> MmioWindow {
            self.window
        }

        fn read(&self, offset: u64, data: &mut [u8]) -> bool {
            data.fill(offset as u8);
            true
        }

        fn write(&self, offset: u64, data: &[u8]) -> bool {
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
        bus.register(
            "test",
            0x1000,
            0x100,
            Arc::new(|off, data| {
                data.fill(off as u8);
                true
            }),
            Arc::new(|_, _| true),
        );
        let mut buf = [0u8; 4];
        assert!(bus.read(0x1042, &mut buf));
        assert_eq!(buf, [0x42; 4]);
        assert!(!bus.read(0x900, &mut buf));
    }

    #[test]
    fn write_dispatches() {
        let captured = Arc::new(AtomicU64::new(0));
        let captured_c = Arc::clone(&captured);
        let mut bus = MmioBus::new();
        bus.register(
            "test",
            0x2000,
            0x100,
            Arc::new(|_, _| true),
            Arc::new(move |off, data| {
                let mut buf = [0u8; 8];
                let n = data.len().min(8);
                buf[..n].copy_from_slice(&data[..n]);
                captured_c.store(off | (u64::from_le_bytes(buf) << 32), Ordering::SeqCst);
                true
            }),
        );
        assert!(bus.write(0x2080, &[0xAA, 0xBB]));
        assert_eq!(captured.load(Ordering::SeqCst) & 0xFFFF_FFFF, 0x80);
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
        let nop_r: ReadFn = Arc::new(|_, _| true);
        let nop_w: WriteFn = Arc::new(|_, _| true);
        bus.register("a", 0x1000, 0x100, Arc::clone(&nop_r), Arc::clone(&nop_w));
        bus.register("b", 0x1080, 0x100, nop_r, nop_w);
    }

    #[test]
    #[should_panic(expected = "MMIO range overlap")]
    fn device_overlap_panics() {
        let mut bus = MmioBus::new();
        let nop_r: ReadFn = Arc::new(|_, _| true);
        let nop_w: WriteFn = Arc::new(|_, _| true);
        bus.register("a", 0x1000, 0x100, nop_r, nop_w);
        bus.register_device(Arc::new(TestDevice::new("b", 0x1080, 0x100)));
    }
}
