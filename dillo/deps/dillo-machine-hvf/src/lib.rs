#[cfg(target_os = "macos")]
mod imp {
    use std::sync::{Arc, Mutex};

    use dillo_mmio::{Attach, MmioAttachment, MmioBus, MmioDevice, MmioInterrupt, SharedMemory};

    pub use dillo_hypervisor::{
        Error, GicParams, VcpuHandle, VmExit, force_vcpus_exit, send_msi, set_spi,
    };

    #[derive(Debug)]
    pub struct Vm {
        inner: dillo_hypervisor::Vm,
        mmio_bus: Arc<Mutex<MmioBus>>,
    }

    impl Vm {
        pub fn new(gic: &GicParams, min_addr_space_bits: u32) -> Result<Self, Error> {
            Ok(Self {
                inner: dillo_hypervisor::Vm::new(gic, min_addr_space_bits)?,
                mmio_bus: Arc::new(Mutex::new(MmioBus::new())),
            })
        }

        pub fn max_vcpus(&self) -> Result<u32, Error> {
            self.inner.max_vcpus()
        }

        pub fn add_memory(&mut self, base: u64, size: u64) -> Result<(), Error> {
            self.inner.add_memory(base, size)
        }

        pub fn write_guest(&mut self, gpa: u64, data: &[u8]) -> Result<(), Error> {
            self.inner.write_guest(gpa, data)
        }

        pub fn region_mappings(&self) -> Vec<(u64, u64, u64)> {
            self.inner.region_mappings()
        }

        pub fn reset_gic(&mut self) -> Result<(), Error> {
            self.inner.reset_gic()
        }

        pub fn mmio_bus(&self) -> Arc<Mutex<MmioBus>> {
            Arc::clone(&self.mmio_bus)
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

    #[derive(Debug)]
    pub struct Vcpu {
        inner: dillo_hypervisor::Vcpu,
        mmio_bus: Arc<Mutex<MmioBus>>,
    }

    pub fn create_vcpu_current_thread(mmio_bus: Arc<Mutex<MmioBus>>) -> Result<Vcpu, Error> {
        Ok(Vcpu {
            inner: dillo_hypervisor::create_vcpu_current_thread()?,
            mmio_bus,
        })
    }

    impl Vcpu {
        pub fn set_aarch64_state(
            &self,
            state: &pmi::vm::vcpu::aarch64::CpuState,
        ) -> Result<(), Error> {
            self.inner.set_aarch64_state(state)
        }

        pub fn set_mpidr(&self, mpidr: u64) -> Result<(), Error> {
            self.inner.set_mpidr(mpidr)
        }

        pub fn set_gpr(&self, n: u8, value: u64) -> Result<(), Error> {
            self.inner.set_gpr(n, value)
        }

        pub fn el1_exception_state(&self) -> (u64, u64, u64) {
            self.inner.el1_exception_state()
        }

        pub fn handle(&self) -> VcpuHandle {
            self.inner.handle()
        }

        pub fn run(&self) -> Result<VmExit, Error> {
            loop {
                match self.inner.run()? {
                    VmExit::MmioRead { addr, size } => {
                        let mut data = [0u8; 8];
                        let size = (size as usize).min(8);
                        self.mmio_bus
                            .lock()
                            .expect("MMIO bus lock poisoned")
                            .read(addr, &mut data[..size]);
                        self.inner.complete_mmio_read(u64::from_le_bytes(data))?;
                    }
                    VmExit::MmioWrite { addr, data, size } => {
                        let size = (size as usize).min(8);
                        if !self
                            .mmio_bus
                            .lock()
                            .expect("MMIO bus lock poisoned")
                            .write(addr, &data[..size])
                        {
                            log::warn!(
                                "HVF MMIO write to unmapped {:#x} (size {}, data {:02x?})",
                                addr,
                                size,
                                &data[..size],
                            );
                        }
                        return Ok(VmExit::MmioWrite {
                            addr,
                            data,
                            size: size as u8,
                        });
                    }
                    other => return Ok(other),
                }
            }
        }
    }
}

#[cfg(target_os = "macos")]
pub use imp::*;
