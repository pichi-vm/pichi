#[cfg(target_os = "windows")]
mod cpuid_x86;
#[cfg(target_os = "windows")]
mod hypervisor;

/// Reasons a WHP vCPU run returned to backend code.
#[cfg(target_os = "windows")]
#[derive(Debug)]
enum VmExit {
    MmioRead { addr: u64, size: u8 },
    MmioWrite { addr: u64, data: [u8; 8], size: u8 },
    PioRead { port: u16, size: u8 },
    PioWrite { port: u16, data: [u8; 4], size: u8 },
    Shutdown,
    Interrupted,
    Halted,
    Unknown(String),
}

#[cfg(target_os = "windows")]
mod imp {
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    use dillo_machine::VcpuStop;
    use dillo_mmio::{
        Attach, Interrupt, InterruptError, InterruptLine, MessageInterrupt, MessageInterruptDomain,
        MmioAttachment, MmioBus, MmioDevice, MmioDeviceHandle, MmioInterrupt, MmioSpawnError,
        SharedMemory,
    };
    use dillo_x86::IoApic;
    use vm_memory::GuestMemoryMmap;

    use crate::VmExit;
    use crate::hypervisor::InterruptController;
    pub use crate::hypervisor::{Error, VcpuCancel};

    pub const HOST_ARCH: dillo_machine::HostArchitecture = dillo_machine::HostArchitecture::X86_64;

    type PioRead = Arc<dyn Fn(u16, u8) -> u32 + Send + Sync + 'static>;
    type PioWrite = Arc<dyn Fn(u16, &[u8]) + Send + Sync + 'static>;

    pub fn install_signal_watchers(_supervisor_shutdown: &'static AtomicBool) {}

    pub fn install_panic_terminal_restore() {}

    #[derive(Debug)]
    pub struct RawStdio;

    impl RawStdio {
        pub fn enter_if_tty() -> Self {
            Self
        }
    }

    pub struct Vm {
        inner: crate::hypervisor::Vm,
        mmio_bus: Arc<Mutex<MmioBus>>,
        vcpu_cancels: Arc<Mutex<Vec<VcpuCancel>>>,
        shared_memory: Vec<Arc<dyn SharedMemory>>,
    }

    impl std::fmt::Debug for Vm {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Vm")
                .field("inner", &self.inner)
                .field("mmio_bus", &self.mmio_bus)
                .finish_non_exhaustive()
        }
    }

    impl Vm {
        pub fn new() -> Result<Self, Error> {
            Ok(Self {
                inner: crate::hypervisor::Vm::new()?,
                mmio_bus: Arc::new(Mutex::new(MmioBus::new())),
                vcpu_cancels: Arc::new(Mutex::new(Vec::new())),
                shared_memory: Vec::new(),
            })
        }

        pub fn new_x86_64_with_local_apic_count(processor_count: u32) -> Result<Self, Error> {
            Ok(Self {
                inner: crate::hypervisor::Vm::new_x86_64_with_local_apic_count(processor_count)?,
                mmio_bus: Arc::new(Mutex::new(MmioBus::new())),
                vcpu_cancels: Arc::new(Mutex::new(Vec::new())),
                shared_memory: Vec::new(),
            })
        }

        fn set_memory(&mut self, memory: GuestMemoryMmap) -> Result<(), Error> {
            self.inner.set_memory(memory)
        }

        pub fn write_guest(&mut self, gpa: u64, data: &[u8]) -> Result<(), Error> {
            self.inner.write_guest(gpa, data)
        }

        pub fn region_mappings(&self) -> Vec<(u64, u64, u64)> {
            self.inner.region_mappings()
        }

        fn create_vcpu_with_pio(
            &self,
            idx: u32,
            cpu_profile: &str,
            pio_read: PioRead,
            pio_write: PioWrite,
        ) -> Result<Vcpu, Error> {
            let inner = self.inner.create_vcpu(idx, cpu_profile)?;
            self.vcpu_cancels
                .lock()
                .expect("vCPU cancel list poisoned")
                .push(inner.cancel_handle());
            Ok(Vcpu {
                inner,
                mmio_bus: Arc::clone(&self.mmio_bus),
                pio_read,
                pio_write,
            })
        }

        pub fn request_vcpu_exit(&self) -> Result<(), Error> {
            self.exit_requester().request_vcpu_exit()
        }

        pub fn exit_requester(&self) -> VcpuExitRequester {
            VcpuExitRequester {
                vcpu_cancels: Arc::clone(&self.vcpu_cancels),
            }
        }

        fn fixed_interrupt_requester(&self) -> FixedInterruptRequester {
            FixedInterruptRequester {
                interrupt_controller: self.inner.interrupt_controller(),
            }
        }

        pub fn create_message_interrupt_domain(
            &self,
            count: u16,
        ) -> Arc<dyn MessageInterruptDomain> {
            Arc::new(FixedMessageInterruptDomain::new(
                self.fixed_interrupt_requester(),
                count,
            ))
        }

        pub fn create_ioapic_interrupt_line(
            &self,
            ioapic: Arc<IoApic>,
            gsi: u32,
        ) -> IoApicInterruptLine {
            IoApicInterruptLine::new(self.inner.interrupt_controller(), ioapic, gsi)
        }

        pub fn set_shared_memory_capabilities(
            &mut self,
            shared_memory: Vec<Arc<dyn SharedMemory>>,
        ) {
            self.shared_memory = shared_memory;
        }
    }

    impl dillo_machine::Machine for Vm {
        type Error = Error;
        type Vcpu = Vcpu;
        type Cpu = Cpu;
        type Memory = Memory;

        fn request_vcpu_exit(&self) -> Result<(), Self::Error> {
            Vm::request_vcpu_exit(self)
        }
    }

    /// WHP guest RAM mapping selected by dillo from the merged DTB memory plan.
    #[derive(Clone)]
    pub struct Memory {
        guest_memory: GuestMemoryMmap,
    }

    impl std::fmt::Debug for Memory {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Memory").finish_non_exhaustive()
        }
    }

    impl Memory {
        pub fn new(guest_memory: GuestMemoryMmap) -> Self {
            Self { guest_memory }
        }
    }

    impl Attach<Memory> for Vm {
        type Error = Error;
        type Output = ();

        fn attach(&mut self, item: Memory) -> Result<Self::Output, Self::Error> {
            self.set_memory(item.guest_memory)
        }
    }

    /// One WHP x86 vCPU creation request.
    pub struct Cpu {
        idx: u32,
        cpu_profile: String,
        pio_read: PioRead,
        pio_write: PioWrite,
        state: Option<pmi::vm::vcpu::x86_64::CpuState>,
    }

    impl std::fmt::Debug for Cpu {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Cpu")
                .field("idx", &self.idx)
                .field("cpu_profile", &self.cpu_profile)
                .field("has_state", &self.state.is_some())
                .finish_non_exhaustive()
        }
    }

    impl Cpu {
        pub fn x86_64(
            idx: u32,
            cpu_profile: impl Into<String>,
            pio_read: Arc<dyn Fn(u16, u8) -> u32 + Send + Sync + 'static>,
            pio_write: Arc<dyn Fn(u16, &[u8]) + Send + Sync + 'static>,
            state: Option<pmi::vm::vcpu::x86_64::CpuState>,
        ) -> Self {
            Self {
                idx,
                cpu_profile: cpu_profile.into(),
                pio_read,
                pio_write,
                state,
            }
        }
    }

    impl Attach<Cpu> for Vm {
        type Error = Error;
        type Output = Vcpu;

        fn attach(&mut self, item: Cpu) -> Result<Self::Output, Self::Error> {
            let mut vcpu = self.create_vcpu_with_pio(
                item.idx,
                &item.cpu_profile,
                item.pio_read,
                item.pio_write,
            )?;
            if let Some(state) = item.state {
                vcpu.set_x86_64_state(&state)?;
            }
            Ok(vcpu)
        }
    }

    impl<D> Attach<Arc<D>> for Vm
    where
        D: MmioDevice + 'static,
    {
        type Error = Error;
        type Output = Arc<dyn MmioAttachment>;

        fn attach(&mut self, item: Arc<D>) -> Result<Self::Output, Self::Error> {
            if !item.shared_memory().is_empty() {
                return Err(Error::UnhandledExit(format!(
                    "MMIO device requested {} fixed shared-memory requirement(s), but WHP attachment does not realize machine-mediated shared-memory capabilities yet",
                    item.shared_memory().len()
                )));
            }
            self.mmio_bus
                .lock()
                .expect("MMIO bus lock poisoned")
                .register_device(item);
            Ok(Arc::new(MachineMmioAttachment {
                shared_memory: self.shared_memory.clone(),
            }))
        }
    }

    #[derive(Clone, Debug)]
    struct FixedInterruptRequester {
        interrupt_controller: InterruptController,
    }

    impl FixedInterruptRequester {
        fn request_fixed_interrupt(&self, destination: u32, vector: u8) -> Result<(), Error> {
            self.interrupt_controller
                .request_fixed_interrupt(destination, vector)
        }
    }

    #[derive(Clone, Copy, Debug, Default)]
    struct FixedMessage {
        address: u64,
        data: u32,
        masked: bool,
    }

    #[derive(Debug)]
    struct FixedMessageInterruptDomain {
        inner: Arc<FixedMessageInterruptDomainInner>,
    }

    #[derive(Debug)]
    struct FixedMessageInterruptDomainInner {
        fixed_interrupts: FixedInterruptRequester,
        vectors: Mutex<Vec<FixedMessage>>,
        enabled: AtomicBool,
    }

    impl FixedMessageInterruptDomain {
        fn new(fixed_interrupts: FixedInterruptRequester, count: u16) -> Self {
            Self {
                inner: Arc::new(FixedMessageInterruptDomainInner {
                    fixed_interrupts,
                    vectors: Mutex::new(vec![FixedMessage::default(); count as usize]),
                    enabled: AtomicBool::new(false),
                }),
            }
        }
    }

    impl FixedMessageInterruptDomainInner {
        fn message_for(&self, vector: u16) -> Option<FixedMsi> {
            if !self.enabled.load(std::sync::atomic::Ordering::SeqCst) {
                return None;
            }
            let vectors = self.vectors.lock().expect("message table poisoned");
            let message = *vectors.get(vector as usize)?;
            if message.masked {
                return None;
            }
            decode_fixed_msi(message)
        }
    }

    impl MessageInterruptDomain for FixedMessageInterruptDomain {
        fn update(&self, vector: u16, msg: MessageInterrupt) -> Result<(), InterruptError> {
            let mut vectors = self.inner.vectors.lock().expect("message table poisoned");
            if let Some(slot) = vectors.get_mut(vector as usize) {
                *slot = FixedMessage {
                    address: msg.address,
                    data: msg.data,
                    masked: msg.masked,
                };
            }
            Ok(())
        }

        fn enabled(&self, enabled: bool) -> Result<(), InterruptError> {
            self.inner
                .enabled
                .store(enabled, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        fn interrupt(&self, vector: u16) -> Option<Interrupt> {
            let domain = Arc::clone(&self.inner);
            Some(Interrupt::from_fn(move || {
                let Some(msi) = domain.message_for(vector) else {
                    return;
                };
                if let Err(e) = domain
                    .fixed_interrupts
                    .request_fixed_interrupt(msi.destination, msi.vector)
                {
                    log::warn!(
                        "WHP MSI inject failed for table vector {vector}, APIC destination {}, vector {:#x}: {e}",
                        msi.destination,
                        msi.vector,
                    );
                }
            }))
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FixedMsi {
        destination: u32,
        vector: u8,
    }

    fn decode_fixed_msi(message: FixedMessage) -> Option<FixedMsi> {
        const MSI_ADDR_BASE_MASK: u64 = 0xFFF0_0000;
        const MSI_ADDR_BASE: u64 = 0xFEE0_0000;
        const MSI_ADDR_DEST_SHIFT: u64 = 12;
        const MSI_ADDR_DEST_MASK: u64 = 0xFF;
        const MSI_DATA_VECTOR_MASK: u32 = 0xFF;
        const MSI_DATA_DELIVERY_MODE_MASK: u32 = 0x700;
        const MSI_DATA_LEVEL_ASSERT: u32 = 1 << 14;
        const MSI_DATA_TRIGGER_LEVEL: u32 = 1 << 15;

        if (message.address & MSI_ADDR_BASE_MASK) != MSI_ADDR_BASE {
            log::warn!(
                "WHP MSI entry has non-local-APIC address {:#x}",
                message.address
            );
            return None;
        }
        if message.data & MSI_DATA_DELIVERY_MODE_MASK != 0 {
            log::warn!(
                "WHP MSI entry uses unsupported delivery mode data={:#x}",
                message.data
            );
            return None;
        }
        if message.data & (MSI_DATA_LEVEL_ASSERT | MSI_DATA_TRIGGER_LEVEL) != 0 {
            log::warn!(
                "WHP MSI entry uses unsupported level/trigger data={:#x}",
                message.data
            );
            return None;
        }

        Some(FixedMsi {
            destination: ((message.address >> MSI_ADDR_DEST_SHIFT) & MSI_ADDR_DEST_MASK) as u32,
            vector: (message.data & MSI_DATA_VECTOR_MASK) as u8,
        })
    }

    #[derive(Debug)]
    pub struct IoApicInterruptLine {
        interrupt_controller: InterruptController,
        ioapic: Arc<IoApic>,
        gsi: u32,
    }

    impl IoApicInterruptLine {
        fn new(interrupt_controller: InterruptController, ioapic: Arc<IoApic>, gsi: u32) -> Self {
            Self {
                interrupt_controller,
                ioapic,
                gsi,
            }
        }

        fn inject(&self) -> Result<(), InterruptError> {
            let Some(route) = self.ioapic.route(self.gsi) else {
                return Ok(());
            };
            self.interrupt_controller
                .request_fixed_interrupt(route.destination, route.vector)
                .map_err(|e| InterruptError::Delivery(e.to_string()))
        }
    }

    impl InterruptLine for IoApicInterruptLine {
        fn signal(&self) {
            if let Err(e) = self.inject() {
                log::warn!(
                    "WHP IOAPIC interrupt signal failed for GSI {}: {e}",
                    self.gsi
                );
            }
        }

        fn set_level(&self, level: bool) -> Result<(), InterruptError> {
            if level {
                self.inject()?;
            }
            Ok(())
        }
    }

    struct MachineMmioAttachment {
        shared_memory: Vec<Arc<dyn SharedMemory>>,
    }

    impl std::fmt::Debug for MachineMmioAttachment {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("MachineMmioAttachment")
                .field("shared_memory", &self.shared_memory.len())
                .finish()
        }
    }

    impl MmioAttachment for MachineMmioAttachment {
        fn interrupts(&self) -> &[MmioInterrupt] {
            &[]
        }

        fn shared_memory(&self) -> &[Arc<dyn SharedMemory>] {
            &self.shared_memory
        }

        fn spawn(
            self: Arc<Self>,
            run: dillo_mmio::MmioDeviceRun,
        ) -> Result<MmioDeviceHandle, MmioSpawnError> {
            Ok(MmioDeviceHandle::thread(run))
        }
    }

    #[derive(Clone)]
    pub struct VcpuExitRequester {
        vcpu_cancels: Arc<Mutex<Vec<VcpuCancel>>>,
    }

    impl std::fmt::Debug for VcpuExitRequester {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("VcpuExitRequester").finish_non_exhaustive()
        }
    }

    impl VcpuExitRequester {
        pub fn request_vcpu_exit(&self) -> Result<(), Error> {
            for cancel in self
                .vcpu_cancels
                .lock()
                .expect("vCPU cancel list poisoned")
                .iter()
            {
                cancel.cancel()?;
            }
            Ok(())
        }
    }

    pub struct Vcpu {
        inner: crate::hypervisor::Vcpu,
        mmio_bus: Arc<Mutex<MmioBus>>,
        pio_read: PioRead,
        pio_write: PioWrite,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum VcpuExit {
        MmioWrite { addr: u64, data: [u8; 8], size: u8 },

        Interrupted,

        Shutdown,
    }

    impl std::fmt::Debug for Vcpu {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Vcpu")
                .field("inner", &self.inner)
                .field("mmio_bus", &self.mmio_bus)
                .finish_non_exhaustive()
        }
    }

    impl Vcpu {
        pub fn index(&self) -> u32 {
            self.inner.index()
        }

        pub fn set_x86_64_state(
            &mut self,
            state: &pmi::vm::vcpu::x86_64::CpuState,
        ) -> Result<(), Error> {
            self.inner.set_x86_64_state(state)
        }

        fn run(&mut self) -> Result<VcpuExit, Error> {
            loop {
                let bus = Arc::clone(&self.mmio_bus);
                let pio_read = Arc::clone(&self.pio_read);
                let exit = self.inner.run(
                    move |port, size| pio_read(port, size),
                    move |addr, data| {
                        let handled = bus.lock().expect("MMIO bus lock poisoned").read(addr, data);
                        if !handled {
                            log::debug!(
                                "WHP MMIO read from unmapped {:#x} (size {}); returning zeros",
                                addr,
                                data.len(),
                            );
                        }
                        handled
                    },
                )?;
                match exit {
                    VmExit::MmioRead { addr, size } => {
                        let _ = (addr, size);
                        continue;
                    }
                    VmExit::PioRead { port, size } => {
                        let _ = (port, size);
                        continue;
                    }
                    VmExit::PioWrite { port, data, size } => {
                        (self.pio_write)(port, &data[..size as usize]);
                    }
                    VmExit::MmioWrite { addr, data, size } => {
                        if !self
                            .mmio_bus
                            .lock()
                            .expect("MMIO bus lock poisoned")
                            .write(addr, &data[..size as usize])
                        {
                            log::warn!(
                                "WHP MMIO write to unmapped {:#x} (size {}, data {:02x?})",
                                addr,
                                size,
                                &data[..size as usize],
                            );
                        }
                        return Ok(VcpuExit::MmioWrite { addr, data, size });
                    }
                    VmExit::Interrupted => return Ok(VcpuExit::Interrupted),
                    VmExit::Halted => continue,
                    VmExit::Shutdown => return Ok(VcpuExit::Shutdown),
                    VmExit::Unknown(reason) => return Err(Error::UnhandledExit(reason)),
                }
            }
        }

        pub fn run_until_stop<F>(&mut self, mut stop: F) -> Result<VcpuStop, Error>
        where
            F: FnMut() -> Option<VcpuStop>,
        {
            loop {
                if let Some(stop) = stop() {
                    return Ok(stop);
                }
                match self.run()? {
                    VcpuExit::MmioWrite { .. } | VcpuExit::Interrupted => {
                        if let Some(stop) = stop() {
                            return Ok(stop);
                        }
                    }
                    VcpuExit::Shutdown => {
                        log::warn!("guest shutdown via WHP shutdown exit");
                        return Ok(VcpuStop::GuestPoweroff);
                    }
                }
            }
        }
    }

    impl dillo_machine::Vcpu for Vcpu {
        type Error = Error;

        fn run(&mut self) -> Result<VcpuStop, Self::Error> {
            self.run_until_stop(|| None)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{FixedMessage, FixedMsi, decode_fixed_msi};

        #[test]
        fn decodes_fixed_physical_msi() {
            assert_eq!(
                decode_fixed_msi(FixedMessage {
                    address: 0xFEE0_3000,
                    data: 0x45,
                    masked: false,
                }),
                Some(FixedMsi {
                    destination: 3,
                    vector: 0x45,
                })
            );
        }

        #[test]
        fn rejects_non_lapic_msi_address() {
            assert!(
                decode_fixed_msi(FixedMessage {
                    address: 0xDEAD_0000,
                    data: 0x45,
                    masked: false,
                })
                .is_none()
            );
        }
    }
}

#[cfg(target_os = "windows")]
pub use imp::*;
