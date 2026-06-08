//! MMIO device boundary for dillo.
//!
//! This crate owns the narrow device-facing MMIO traits and resource shapes.
//! The current `MmioBus` remains a compatibility dispatcher while machine-owned
//! routing is introduced in later stages.

use std::process::Child;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

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

impl AddressRange {
    pub fn contains(&self, base: u64, size: u64) -> bool {
        let Some(container_end) = self.base.checked_add(self.size) else {
            return false;
        };
        let Some(end) = base.checked_add(size) else {
            return false;
        };
        base >= self.base && end <= container_end
    }
}

/// Access allowed through a shared-memory capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedAccess {
    ReadOnly,

    WriteOnly,

    ReadWrite,
}

impl SharedAccess {
    fn permits_read(self) -> bool {
        matches!(self, Self::ReadOnly | Self::ReadWrite)
    }

    fn permits_write(self) -> bool {
        matches!(self, Self::WriteOnly | Self::ReadWrite)
    }

    fn includes(self, requested: Self) -> bool {
        match requested {
            Self::ReadOnly => self.permits_read(),
            Self::WriteOnly => self.permits_write(),
            Self::ReadWrite => self.permits_read() && self.permits_write(),
        }
    }
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

    #[error("shared-memory access is not permitted by the capability")]
    AccessDenied,

    #[error("shared-memory region access is outside the claimed range")]
    OutOfRange,

    #[error("shared-memory backing access failed: {0}")]
    Backing(String),

    #[error("shared-memory access is unsupported")]
    Unsupported,
}

/// Opaque shared-memory region handle.
#[derive(Debug)]
pub struct SharedRegion {
    memory: GuestMemoryMmap,
    gpa: u64,
    size: u64,
    access: SharedAccess,
}

impl SharedRegion {
    pub fn read(&self, offset: u64, data: &mut [u8]) -> Result<(), SharedMemoryError> {
        if !self.access.permits_read() {
            return Err(SharedMemoryError::AccessDenied);
        }
        let addr = self.checked_access(offset, data.len())?;
        self.memory
            .read(data, GuestAddress(addr))
            .map_err(|e| SharedMemoryError::Backing(e.to_string()))
            .map(|_| ())
    }

    pub fn write(&self, offset: u64, data: &[u8]) -> Result<(), SharedMemoryError> {
        if !self.access.permits_write() {
            return Err(SharedMemoryError::AccessDenied);
        }
        let addr = self.checked_access(offset, data.len())?;
        self.memory
            .write(data, GuestAddress(addr))
            .map_err(|e| SharedMemoryError::Backing(e.to_string()))
            .map(|_| ())
    }

    fn checked_access(&self, offset: u64, len: usize) -> Result<u64, SharedMemoryError> {
        let len = u64::try_from(len).map_err(|_| SharedMemoryError::OutOfRange)?;
        let end = offset
            .checked_add(len)
            .ok_or(SharedMemoryError::OutOfRange)?;
        if end > self.size {
            return Err(SharedMemoryError::OutOfRange);
        }
        self.gpa
            .checked_add(offset)
            .ok_or(SharedMemoryError::OutOfRange)
    }
}

/// Attachment-scoped shared-memory capability.
pub trait SharedMemory: Send + Sync {
    fn region(&self, range: SharedRange) -> Result<SharedRegion, SharedMemoryError>;
}

/// Standard-VM shared-memory capability over mapped guest RAM.
///
/// Confidential backends may use a different implementation that updates
/// `shared` when guest shared/private conversion exits are handled internally.
#[derive(Debug, Clone)]
pub struct MappedSharedMemory {
    memory: GuestMemoryMmap,
    aperture: AddressRange,
    access: SharedAccess,
    shared: Vec<AddressRange>,
}

impl MappedSharedMemory {
    pub fn new(memory: GuestMemoryMmap, requirement: SharedMemoryRequirement) -> Self {
        Self {
            memory,
            aperture: requirement.range,
            access: requirement.access,
            shared: vec![requirement.range],
        }
    }

    pub fn with_shared_ranges(
        memory: GuestMemoryMmap,
        requirement: SharedMemoryRequirement,
        shared: Vec<AddressRange>,
    ) -> Self {
        Self {
            memory,
            aperture: requirement.range,
            access: requirement.access,
            shared,
        }
    }
}

impl SharedMemory for MappedSharedMemory {
    fn region(&self, range: SharedRange) -> Result<SharedRegion, SharedMemoryError> {
        if !self.access.includes(range.access) {
            return Err(SharedMemoryError::AccessDenied);
        }
        if !self.aperture.contains(range.gpa, range.size) {
            return Err(SharedMemoryError::OutOfAperture);
        }
        if !self
            .shared
            .iter()
            .any(|shared| shared.contains(range.gpa, range.size))
        {
            return Err(SharedMemoryError::NotShared);
        }
        Ok(SharedRegion {
            memory: self.memory.clone(),
            gpa: range.gpa,
            size: range.size,
            access: range.access,
        })
    }
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

    pub fn spawn_supervisor_model(self) -> Result<MmioDeviceHandle, MmioSpawnError> {
        match self {
            Self::Thread(host) => Ok(host.spawn()),
            Self::Process(host) => Ok(host.spawn()),
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
    child: Child,
}

impl MmioProcessHost {
    pub fn from_child(child: Child) -> Self {
        Self { child }
    }

    fn spawn(self) -> MmioDeviceHandle {
        MmioDeviceHandle {
            inner: MmioDeviceHandleInner::Process {
                child: Mutex::new(Some(self.child)),
            },
        }
    }
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

    Process {
        child: Mutex<Option<Child>>,
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
            MmioDeviceHandleInner::Process { child } => {
                if let Some(mut child) = child.lock().expect("MMIO process child poisoned").take() {
                    Self::terminate_process(&mut child)?;
                }
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
            MmioDeviceHandleInner::Process { child } => {
                if let Some(mut child) = child
                    .into_inner()
                    .expect("MMIO process child poisoned")
                    .take()
                {
                    Self::terminate_process(&mut child)
                        .map_err(|e| MmioJoinError::Host(e.to_string()))?;
                }
                Ok(())
            }
        }
    }

    fn terminate_process(child: &mut Child) -> Result<(), MmioShutdownError> {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {}
            Err(e) => return Err(MmioShutdownError::Io(e)),
        }
        child.kill().map_err(MmioShutdownError::Io)?;
        child.wait().map_err(MmioShutdownError::Io)?;
        Ok(())
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

    #[error("MMIO device-host shutdown I/O failed: {0}")]
    Io(std::io::Error),
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
    use vm_memory::{Bytes, GuestAddress};

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
    fn mapped_shared_memory_reads_and_writes_claimed_region() {
        let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x4000)]).unwrap();
        memory.write(&[1, 2, 3, 4], GuestAddress(0x1200)).unwrap();
        let shared = MappedSharedMemory::new(
            memory,
            SharedMemoryRequirement {
                range: AddressRange {
                    base: 0x1000,
                    size: 0x1000,
                },
                access: SharedAccess::ReadWrite,
            },
        );

        let region = shared
            .region(SharedRange {
                gpa: 0x1200,
                size: 4,
                access: SharedAccess::ReadWrite,
            })
            .unwrap();
        let mut buf = [0; 4];
        region.read(0, &mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);

        region.write(1, &[9, 8]).unwrap();
        region.read(0, &mut buf).unwrap();
        assert_eq!(buf, [1, 9, 8, 4]);
    }

    #[test]
    fn mapped_shared_memory_rejects_outside_aperture() {
        let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x4000)]).unwrap();
        let shared = MappedSharedMemory::new(
            memory,
            SharedMemoryRequirement {
                range: AddressRange {
                    base: 0x1000,
                    size: 0x1000,
                },
                access: SharedAccess::ReadWrite,
            },
        );

        let err = shared
            .region(SharedRange {
                gpa: 0x1ff0,
                size: 0x20,
                access: SharedAccess::ReadOnly,
            })
            .expect_err("range crosses aperture");
        assert!(matches!(err, SharedMemoryError::OutOfAperture));
    }

    #[test]
    fn mapped_shared_memory_rejects_private_range() {
        let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x4000)]).unwrap();
        let shared = MappedSharedMemory::with_shared_ranges(
            memory,
            SharedMemoryRequirement {
                range: AddressRange {
                    base: 0x1000,
                    size: 0x1000,
                },
                access: SharedAccess::ReadWrite,
            },
            vec![AddressRange {
                base: 0x1000,
                size: 0x100,
            }],
        );

        let err = shared
            .region(SharedRange {
                gpa: 0x1200,
                size: 0x10,
                access: SharedAccess::ReadOnly,
            })
            .expect_err("range is not currently shared");
        assert!(matches!(err, SharedMemoryError::NotShared));
    }

    #[test]
    fn mapped_shared_memory_enforces_access_mode() {
        let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x4000)]).unwrap();
        let shared = MappedSharedMemory::new(
            memory,
            SharedMemoryRequirement {
                range: AddressRange {
                    base: 0x1000,
                    size: 0x1000,
                },
                access: SharedAccess::ReadOnly,
            },
        );

        let err = shared
            .region(SharedRange {
                gpa: 0x1100,
                size: 0x10,
                access: SharedAccess::ReadWrite,
            })
            .expect_err("capability is read-only");
        assert!(matches!(err, SharedMemoryError::AccessDenied));

        let region = shared
            .region(SharedRange {
                gpa: 0x1100,
                size: 0x10,
                access: SharedAccess::ReadOnly,
            })
            .unwrap();
        let err = region.write(0, &[1]).expect_err("region is read-only");
        assert!(matches!(err, SharedMemoryError::AccessDenied));
    }

    #[test]
    fn mapped_shared_memory_region_bounds_are_enforced() {
        let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x4000)]).unwrap();
        let shared = MappedSharedMemory::new(
            memory,
            SharedMemoryRequirement {
                range: AddressRange {
                    base: 0x1000,
                    size: 0x1000,
                },
                access: SharedAccess::ReadWrite,
            },
        );
        let region = shared
            .region(SharedRange {
                gpa: 0x1100,
                size: 4,
                access: SharedAccess::ReadWrite,
            })
            .unwrap();

        let err = region.write(3, &[1, 2]).expect_err("write crosses region");
        assert!(matches!(err, SharedMemoryError::OutOfRange));
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

        let handle = host.spawn_supervisor_model().expect("thread host spawned");
        started_rx.recv().expect("thread host started");
        handle.shutdown().expect("shutdown requested");
        handle.join().expect("thread host joined");
    }

    #[test]
    fn thread_host_error_reaches_join() {
        let handle = MmioDeviceHost::thread(|_| Err(MmioJoinError::Host("boom".into())))
            .spawn_supervisor_model()
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

    #[test]
    fn process_host_joins_exited_child() {
        let child = exited_child();
        let handle = MmioDeviceHost::process(MmioProcessHost::from_child(child))
            .spawn_supervisor_model()
            .expect("process host spawned");

        handle.join().expect("process host joined");
    }

    #[cfg(unix)]
    fn exited_child() -> std::process::Child {
        std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn exited child")
    }

    #[cfg(windows)]
    fn exited_child() -> std::process::Child {
        std::process::Command::new("cmd")
            .args(["/C", "exit", "/B", "0"])
            .spawn()
            .expect("spawn exited child")
    }
}
