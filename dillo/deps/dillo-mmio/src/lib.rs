//! MMIO device boundary for dillo.
//!
//! This crate owns the narrow device-facing MMIO traits and resource shapes.
//! The current `MmioBus` remains a compatibility dispatcher while machine-owned
//! routing is introduced in later stages.

use std::sync::Arc;

/// A guest-physical MMIO window owned by one device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MmioWindow {
    pub name: &'static str,
    pub base: u64,
    pub size: u64,
}

/// A guest-physical address range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddressRange {
    pub base: u64,
    pub size: u64,
}

/// Access allowed through a shared-memory capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedAccess {
    ReadOnly,

    WriteOnly,

    ReadWrite,
}

/// One DTB-derived shared-memory aperture requirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SharedMemoryRequirement {
    pub range: AddressRange,
    pub access: SharedAccess,
}

/// DTB-derived interrupt source for one wired interrupt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterruptSource {
    pub controller: u32,
    pub cells: [u32; 4],
    pub cell_count: u8,
}

/// DTB-derived interrupt source for one message-interrupt domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageInterruptSource {
    pub controller: u32,
}

/// One DTB-derived interrupt requirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MmioInterruptRequirement {
    Line {
        source: InterruptSource,
    },

    MessageDomain {
        source: MessageInterruptSource,
        vectors: u16,
    },
}

/// Error returned by a routed MMIO device access.
#[derive(Debug, thiserror::Error)]
pub enum MmioError {
    #[error("MMIO access is unsupported by the routed device")]
    Unsupported,
}

/// A device with one or more guest-visible MMIO windows.
pub trait MmioDevice: Send + Sync {
    fn windows(&self) -> &[MmioWindow];

    fn interrupts(&self) -> &[MmioInterruptRequirement] {
        &[]
    }

    fn shared_memory(&self) -> &[SharedMemoryRequirement] {
        &[]
    }

    fn read(&self, window: MmioWindow, offset: u64, data: &mut [u8]) -> bool;

    fn write(&self, window: MmioWindow, offset: u64, data: &[u8]) -> bool;
}

/// Generic registration into a constructed owner.
pub trait Attach<T> {
    type Error: std::error::Error + Send + Sync + 'static;
    type Output;

    fn attach(&mut self, item: T) -> Result<Self::Output, Self::Error>;
}

/// Runtime shared range requested by a device protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SharedRange {
    pub gpa: u64,
    pub size: u64,
    pub access: SharedAccess,
}

/// Error from attachment-scoped shared-memory access.
#[derive(Debug, thiserror::Error)]
pub enum SharedMemoryError {
    #[error("shared-memory range is outside the attached aperture")]
    OutOfAperture,

    #[error("shared-memory range is not currently shared")]
    NotShared,

    #[error("shared-memory access is unsupported")]
    Unsupported,
}

/// Opaque shared-memory region handle.
#[derive(Debug)]
pub struct SharedRegion {
    _priv: (),
}

impl SharedRegion {
    pub fn read(&self, _offset: u64, _data: &mut [u8]) -> Result<(), SharedMemoryError> {
        Err(SharedMemoryError::Unsupported)
    }

    pub fn write(&self, _offset: u64, _data: &[u8]) -> Result<(), SharedMemoryError> {
        Err(SharedMemoryError::Unsupported)
    }
}

/// Attachment-scoped shared-memory capability.
pub trait SharedMemory: Send + Sync {
    fn region(&self, range: SharedRange) -> Result<SharedRegion, SharedMemoryError>;
}

/// Resolved interrupt handle.
#[derive(Clone)]
pub struct Interrupt(Arc<dyn InterruptLine>);

impl std::fmt::Debug for Interrupt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Interrupt").finish()
    }
}

impl Interrupt {
    pub fn new(line: Arc<dyn InterruptLine>) -> Self {
        Self(line)
    }

    pub fn signal(&self) {
        self.0.signal();
    }

    pub fn set_level(&self, level: bool) -> Result<(), InterruptError> {
        self.0.set_level(level)
    }
}

/// Backend-resolved line interrupt.
pub trait InterruptLine: Send + Sync + std::fmt::Debug {
    fn signal(&self);

    fn set_level(&self, level: bool) -> Result<(), InterruptError>;
}

/// Error from interrupt delivery.
#[derive(Debug, thiserror::Error)]
pub enum InterruptError {
    #[error("interrupt deassert is unsupported")]
    UnsupportedDeassert,

    #[error("interrupt delivery failed: {0}")]
    Delivery(String),
}

/// Resolved message interrupt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageInterrupt {
    pub address: u64,
    pub data: u32,
    pub masked: bool,
}

/// Backend-resolved message-interrupt domain.
pub trait MessageInterruptDomain: Send + Sync {
    fn update(&self, vector: u16, msg: MessageInterrupt) -> Result<(), InterruptError>;

    fn enabled(&self, enabled: bool) -> Result<(), InterruptError>;

    fn interrupt(&self, vector: u16) -> Option<Interrupt>;
}

/// Backend-resolved interrupt resource for an attached MMIO device.
#[derive(Clone)]
pub enum MmioInterrupt {
    Line(Interrupt),

    MessageDomain(Arc<dyn MessageInterruptDomain>),
}

impl std::fmt::Debug for MmioInterrupt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Line(line) => f.debug_tuple("Line").field(line).finish(),
            Self::MessageDomain(_) => f.debug_tuple("MessageDomain").finish(),
        }
    }
}

/// Backend-owned services for one successfully attached MMIO device.
pub trait MmioAttachment: Send + Sync + std::fmt::Debug {
    fn interrupts(&self) -> &[MmioInterrupt];

    fn shared_memory(&self) -> &[Arc<dyn SharedMemory>];
}

struct Range {
    window: MmioWindow,
    device: Arc<dyn MmioDevice>,
}

/// Compatibility MMIO bus.
///
/// Built at startup via [`MmioBus::register_device`] and queried per guest exit.
/// Later machine crates will own this routing state behind `Attach`.
#[derive(Default)]
pub struct MmioBus {
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
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an MMIO device.
    pub fn register_device<D>(&mut self, device: Arc<D>)
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
                window: *window,
                device: Arc::clone(&device),
            });
        }
    }

    /// Dispatch a guest MMIO read.
    pub fn read(&self, addr: u64, data: &mut [u8]) -> bool {
        if let Some(r) = self.find(addr, data.len() as u64) {
            return r.device.read(r.window, addr - r.window.base, data);
        }
        false
    }

    /// Dispatch a guest MMIO write.
    pub fn write(&self, addr: u64, data: &[u8]) -> bool {
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
        window: [MmioWindow; 1],
        written: AtomicU64,
    }

    impl TestDevice {
        fn new(name: &'static str, base: u64, size: u64) -> Self {
            Self {
                window: [MmioWindow { name, base, size }],
                written: AtomicU64::new(0),
            }
        }
    }

    impl MmioDevice for TestDevice {
        fn windows(&self) -> &[MmioWindow] {
            &self.window
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
