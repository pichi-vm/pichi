//! MMIO device boundary for dillo.
//!
//! This crate owns the narrow device-facing MMIO traits and resource shapes.
//! The current `MmioBus` remains a compatibility dispatcher while machine-owned
//! routing is introduced in later stages.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

/// Device host execution model selected by one machine backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceModel {
    /// Device hosts run as threads in the supervisor process.
    Thread,

    /// Device hosts run outside the supervisor process.
    Process,
}

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

/// Backend-neutral launch request for an already-attached MMIO device host.
pub enum MmioDeviceHost {
    Thread(MmioThreadHost),

    Process(MmioProcessHost),
}

impl std::fmt::Debug for MmioDeviceHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Thread(_) => f.debug_tuple("Thread").finish(),
            Self::Process(_) => f.debug_tuple("Process").finish(),
        }
    }
}

impl MmioDeviceHost {
    pub fn thread(
        run: impl FnOnce(MmioRunToken) -> Result<(), MmioJoinError> + Send + 'static,
    ) -> Self {
        Self::Thread(MmioThreadHost { run: Box::new(run) })
    }

    pub fn process(spec: MmioProcessHost) -> Self {
        Self::Process(spec)
    }

    pub fn model(&self) -> DeviceModel {
        match self {
            Self::Thread(_) => DeviceModel::Thread,
            Self::Process(_) => DeviceModel::Process,
        }
    }

    pub fn spawn_thread_model(self) -> Result<MmioDeviceHandle, MmioSpawnError> {
        match self {
            Self::Thread(host) => Ok(host.spawn()),
            Self::Process(_) => Err(MmioSpawnError::UnsupportedModel(DeviceModel::Process)),
        }
    }
}

/// Thread-backed MMIO device host request.
pub struct MmioThreadHost {
    run: Box<dyn FnOnce(MmioRunToken) -> Result<(), MmioJoinError> + Send + 'static>,
}

impl std::fmt::Debug for MmioThreadHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MmioThreadHost").finish_non_exhaustive()
    }
}

impl MmioThreadHost {
    fn spawn(self) -> MmioDeviceHandle {
        let shutdown = Arc::new(AtomicBool::new(false));
        let token = MmioRunToken {
            shutdown: Arc::clone(&shutdown),
        };
        let join = thread::spawn(move || (self.run)(token));
        MmioDeviceHandle {
            inner: MmioDeviceHandleInner::Thread { shutdown, join },
        }
    }
}

/// Process-backed MMIO device host request.
#[derive(Debug)]
pub struct MmioProcessHost {
    _priv: (),
}

/// Token passed to a running MMIO thread host.
#[derive(Clone, Debug)]
pub struct MmioRunToken {
    shutdown: Arc<AtomicBool>,
}

impl MmioRunToken {
    pub fn is_shutdown_requested(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }
}

/// Handle for one running MMIO device host.
#[derive(Debug)]
pub struct MmioDeviceHandle {
    inner: MmioDeviceHandleInner,
}

#[derive(Debug)]
enum MmioDeviceHandleInner {
    Noop,

    Thread {
        shutdown: Arc<AtomicBool>,
        join: thread::JoinHandle<Result<(), MmioJoinError>>,
    },
}

impl MmioDeviceHandle {
    pub fn noop() -> Self {
        Self {
            inner: MmioDeviceHandleInner::Noop,
        }
    }

    pub fn shutdown(&self) -> Result<(), MmioShutdownError> {
        match &self.inner {
            MmioDeviceHandleInner::Noop => {}
            MmioDeviceHandleInner::Thread { shutdown, .. } => {
                shutdown.store(true, Ordering::Release);
            }
        }
        Ok(())
    }

    pub fn join(self) -> Result<(), MmioJoinError> {
        match self.inner {
            MmioDeviceHandleInner::Noop => Ok(()),
            MmioDeviceHandleInner::Thread { join, .. } => match join.join() {
                Ok(result) => result,
                Err(_) => Err(MmioJoinError::Panicked),
            },
        }
    }
}

/// Error from spawning an MMIO device host.
#[derive(Debug, thiserror::Error)]
pub enum MmioSpawnError {
    #[error("MMIO host model {0:?} is unsupported by this attachment")]
    UnsupportedModel(DeviceModel),
}

/// Error from requesting MMIO device-host shutdown.
#[derive(Debug, thiserror::Error)]
pub enum MmioShutdownError {
    #[error("MMIO device-host shutdown is unsupported")]
    Unsupported,
}

/// Error from joining an MMIO device host.
#[derive(Debug, thiserror::Error)]
pub enum MmioJoinError {
    #[error("MMIO device host panicked")]
    Panicked,

    #[error("MMIO device host failed: {0}")]
    Host(String),
}

/// Backend-owned services for one successfully attached MMIO device.
pub trait MmioAttachment: Send + Sync + std::fmt::Debug {
    fn interrupts(&self) -> &[MmioInterrupt];

    fn shared_memory(&self) -> &[Arc<dyn SharedMemory>];

    fn spawn(self: Arc<Self>, host: MmioDeviceHost) -> Result<MmioDeviceHandle, MmioSpawnError>;
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
    use std::sync::mpsc;

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

    #[test]
    fn thread_host_observes_shutdown_and_joins() {
        let (started_tx, started_rx) = mpsc::channel();
        let host = MmioDeviceHost::thread(move |token| {
            started_tx.send(()).expect("send start");
            while !token.is_shutdown_requested() {
                std::thread::yield_now();
            }
            Ok(())
        });
        assert_eq!(host.model(), DeviceModel::Thread);

        let handle = host.spawn_thread_model().expect("thread host spawned");
        started_rx.recv().expect("thread host started");
        handle.shutdown().expect("shutdown requested");
        handle.join().expect("thread host joined");
    }

    #[test]
    fn thread_host_error_reaches_join() {
        let handle = MmioDeviceHost::thread(|_| Err(MmioJoinError::Host("boom".into())))
            .spawn_thread_model()
            .expect("thread host spawned");

        let err = handle.join().expect_err("host error");
        assert_eq!(err.to_string(), "MMIO device host failed: boom");
    }

    #[test]
    fn noop_handle_shutdown_and_join_are_harmless() {
        let handle = MmioDeviceHandle::noop();

        handle.shutdown().expect("noop shutdown");
        handle.join().expect("noop join");
    }
}
