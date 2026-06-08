#[cfg(target_os = "windows")]
mod imp {
    use std::sync::{Arc, Mutex};

    use dillo_mmio::{Attach, MmioAttachment, MmioBus, MmioDevice, MmioInterrupt, SharedMemory};
    use vm_memory::GuestMemoryMmap;

    use dillo_hypervisor::VmExit;
    pub use dillo_hypervisor::{Error, InterruptController, VcpuCancel};

    type PioRead = Arc<dyn Fn(u16, u8) -> u32 + Send + Sync + 'static>;
    type PioWrite = Arc<dyn Fn(u16, &[u8]) + Send + Sync + 'static>;

    #[derive(Debug)]
    pub struct Vm {
        inner: dillo_hypervisor::Vm,
        mmio_bus: Arc<Mutex<MmioBus>>,
    }

    impl Vm {
        pub fn new() -> Result<Self, Error> {
            Ok(Self {
                inner: dillo_hypervisor::Vm::new()?,
                mmio_bus: Arc::new(Mutex::new(MmioBus::new())),
            })
        }

        pub fn new_x86_64_with_local_apic_count(processor_count: u32) -> Result<Self, Error> {
            Ok(Self {
                inner: dillo_hypervisor::Vm::new_x86_64_with_local_apic_count(processor_count)?,
                mmio_bus: Arc::new(Mutex::new(MmioBus::new())),
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
            Ok(Vcpu {
                inner: self.inner.create_vcpu(idx, cpu_profile)?,
                mmio_bus: Arc::clone(&self.mmio_bus),
                pio_read,
                pio_write,
            })
        }

        pub fn interrupt_controller(&self) -> InterruptController {
            self.inner.interrupt_controller()
        }
    }

    impl<D> Attach<Arc<D>> for Vm
    where
        D: MmioDevice + 'static,
    {
        type Error = Error;
        type Output = Arc<dyn MmioAttachment>;

        fn attach(&mut self, item: Arc<D>) -> Result<Self::Output, Self::Error> {
            self.mmio_bus
                .lock()
                .expect("MMIO bus lock poisoned")
                .register_device(item);
            Ok(Arc::new(MachineMmioAttachment))
        }
    }

    #[derive(Debug)]
    struct MachineMmioAttachment;

    impl MmioAttachment for MachineMmioAttachment {
        fn interrupts(&self) -> &[MmioInterrupt] {
            &[]
        }

        fn shared_memory(&self) -> &[Arc<dyn SharedMemory>] {
            &[]
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

        Unknown(String),
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

        pub fn cancel_handle(&self) -> VcpuCancel {
            self.inner.cancel_handle()
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
                    VmExit::Unknown(reason) => return Ok(VcpuExit::Unknown(reason)),
                }
            }
        }
    }
}

#[cfg(target_os = "windows")]
pub use imp::*;
