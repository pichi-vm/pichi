//! Range-based MMIO dispatcher.
//!
//! Each registered handler covers `[base, base+size)`. On a guest MMIO
//! exit (`VmExit::MmioRead` / `VmExit::MmioWrite`), the bus picks the
//! one matching range and forwards the access with its
//! GPA-relative offset.
//!
//! No locking inside the dispatcher itself — the bus is built at
//! startup and frozen for the VM's lifetime; handlers wrap whatever
//! internal mutability they need (e.g. `Mutex<PciBus>`).

use std::sync::Arc;

/// Read handler: takes the GPA-relative `offset` and writes the
/// guest-visible bytes into `data`. Returns `true` if handled.
pub(crate) type ReadFn = Arc<dyn Fn(u64, &mut [u8]) -> bool + Send + Sync>;

/// Write handler: takes the GPA-relative `offset` and the bytes the
/// guest wrote. Returns `true` if handled.
pub(crate) type WriteFn = Arc<dyn Fn(u64, &[u8]) -> bool + Send + Sync>;

struct Range {
    base: u64,
    size: u64,
    read: ReadFn,
    write: WriteFn,
    name: &'static str,
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
        let new_end = base.checked_add(size).expect("MMIO range size overflow");
        for r in &self.ranges {
            let end = r.base + r.size;
            let overlap = base < end && r.base < new_end;
            assert!(
                !overlap,
                "MMIO range overlap: new `{name}` [{:#x}..{:#x}) collides with `{}` [{:#x}..{:#x})",
                base, new_end, r.name, r.base, end
            );
        }
        self.ranges.push(Range {
            base,
            size,
            read,
            write,
            name,
        });
    }

    /// Dispatch a guest MMIO read. Returns `true` if a registered
    /// handler claimed the address (in which case `data` is filled).
    pub(crate) fn read(&self, addr: u64, data: &mut [u8]) -> bool {
        if let Some(r) = self.find(addr, data.len() as u64) {
            return (r.read)(addr - r.base, data);
        }
        false
    }

    /// Dispatch a guest MMIO write. Returns `true` if a registered
    /// handler claimed the address.
    pub(crate) fn write(&self, addr: u64, data: &[u8]) -> bool {
        if let Some(r) = self.find(addr, data.len() as u64) {
            return (r.write)(addr - r.base, data);
        }
        false
    }

    fn find(&self, addr: u64, len: u64) -> Option<&Range> {
        let end = addr.checked_add(len)?;
        self.ranges
            .iter()
            .find(|r| r.base <= addr && end <= r.base + r.size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

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
    #[should_panic(expected = "MMIO range overlap")]
    fn overlap_panics() {
        let mut bus = MmioBus::new();
        let nop_r: ReadFn = Arc::new(|_, _| true);
        let nop_w: WriteFn = Arc::new(|_, _| true);
        bus.register("a", 0x1000, 0x100, Arc::clone(&nop_r), Arc::clone(&nop_w));
        bus.register("b", 0x1080, 0x100, nop_r, nop_w);
    }
}
