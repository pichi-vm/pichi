#[cfg(target_os = "windows")]
mod imp {
    use std::sync::{Arc, Mutex};

    use dillo_machine::VcpuStop;
    use dillo_mmio::{
        Attach, InterruptError, InterruptLine, MmioAttachment, MmioBus, MmioDevice,
        MmioDeviceHandle, MmioDeviceHost, MmioInterrupt, MmioSpawnError, SharedMemory,
    };
    use dillo_x86::IoApic;
    use vm_memory::GuestMemoryMmap;

    pub use dillo_hypervisor::{Error, VcpuCancel};
    use dillo_hypervisor::{InterruptController, VmExit};

    type PioRead = Arc<dyn Fn(u16, u8) -> u32 + Send + Sync + 'static>;
    type PioWrite = Arc<dyn Fn(u16, &[u8]) + Send + Sync + 'static>;

    pub struct Vm {
        inner: dillo_hypervisor::Vm,
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
                inner: dillo_hypervisor::Vm::new()?,
                mmio_bus: Arc::new(Mutex::new(MmioBus::new())),
                vcpu_cancels: Arc::new(Mutex::new(Vec::new())),
                shared_memory: Vec::new(),
            })
        }

        pub fn new_x86_64_with_local_apic_count(processor_count: u32) -> Result<Self, Error> {
            Ok(Self {
                inner: dillo_hypervisor::Vm::new_x86_64_with_local_apic_count(processor_count)?,
                mmio_bus: Arc::new(Mutex::new(MmioBus::new())),
                vcpu_cancels: Arc::new(Mutex::new(Vec::new())),
                shared_memory: Vec::new(),
            })
        }

        pub fn set_memory(&mut self, memory: GuestMemoryMmap) -> Result<(), Error> {
            self.inner.set_memory(memory)
        }

        pub fn write_guest(&mut self, gpa: u64, data: &[u8]) -> Result<(), Error> {
            self.inner.write_guest(gpa, data)
        }

        pub fn region_mappings(&self) -> Vec<(u64, u64, u64)> {
            self.inner.region_mappings()
        }

        pub fn create_vcpu(&self, idx: u32, cpu_profile: &str) -> Result<Vcpu, Error> {
            self.create_vcpu_with_pio(idx, cpu_profile, Arc::new(|_, _| 0), Arc::new(|_, _| {}))
        }

        pub fn create_vcpu_with_pio(
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

        pub fn request_fixed_interrupt(&self, destination: u32, vector: u8) -> Result<(), Error> {
            self.inner
                .interrupt_controller()
                .request_fixed_interrupt(destination, vector)
        }

        pub fn fixed_interrupt_requester(&self) -> FixedInterruptRequester {
            FixedInterruptRequester {
                interrupt_controller: self.inner.interrupt_controller(),
            }
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

    #[derive(Debug)]
    pub struct FixedInterruptRequester {
        interrupt_controller: InterruptController,
    }

    impl FixedInterruptRequester {
        pub fn request_fixed_interrupt(&self, destination: u32, vector: u8) -> Result<(), Error> {
            self.interrupt_controller
                .request_fixed_interrupt(destination, vector)
        }
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
            host: MmioDeviceHost,
        ) -> Result<MmioDeviceHandle, MmioSpawnError> {
            host.spawn_supervisor_model()
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
        inner: dillo_hypervisor::Vcpu,
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

        pub fn run(&mut self) -> Result<VcpuExit, Error> {
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
                    VmExit::MmioRead { .. } | VmExit::PioRead { .. } => continue,
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
                    VmExit::Debug => continue,
                    VmExit::Hvc { args } => {
                        log::warn!("unexpected WHP HVC exit: args={args:?}");
                    }
                    VmExit::Smc { args } => {
                        log::warn!("unexpected WHP SMC exit: args={args:?}");
                    }
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
}

#[cfg(target_os = "windows")]
pub use imp::*;
