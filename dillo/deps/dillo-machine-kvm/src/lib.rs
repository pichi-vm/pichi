#[cfg(target_os = "linux")]
mod imp {
    use std::os::fd::{AsRawFd, RawFd};
    use std::sync::{Arc, Mutex};

    use dillo_mmio::{Attach, MmioAttachment, MmioBus, MmioDevice, MmioInterrupt, SharedMemory};

    use dillo_hypervisor::VmExit;
    pub use dillo_hypervisor::{Error, debug_flags, kvm_regs, kvm_sregs};

    type PioRead = Arc<dyn Fn(u16, u8) -> u32 + Send + Sync + 'static>;
    type PioWrite = Arc<dyn Fn(u16, &[u8]) + Send + Sync + 'static>;

    #[derive(Clone, Debug)]
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

        pub fn vm_fd_arc(&self) -> Arc<kvm_ioctls::VmFd> {
            self.inner.vm_fd_arc()
        }

        pub fn add_memslot(
            &self,
            slot: u32,
            gpa: u64,
            host_addr: u64,
            size: u64,
        ) -> Result<(), Error> {
            self.inner.add_memslot(slot, gpa, host_addr, size)
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
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum DebugExit {
        Debug,

        Halted,

        MmioWrite { addr: u64, data: [u8; 8], size: u8 },

        Shutdown,

        Interrupted,

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

        pub fn set_x86_64_state(
            &mut self,
            state: &pmi::vm::vcpu::x86_64::CpuState,
        ) -> Result<(), Error> {
            self.inner.set_x86_64_state(state)
        }

        pub fn set_guest_debug_flags(&self, flags: u32) -> Result<(), Error> {
            self.inner.set_guest_debug_flags(flags)
        }

        pub fn get_regs(&self) -> Result<kvm_regs, Error> {
            self.inner.get_regs()
        }

        pub fn set_regs(&self, regs: &kvm_regs) -> Result<(), Error> {
            self.inner.set_regs(regs)
        }

        pub fn get_sregs(&self) -> Result<kvm_sregs, Error> {
            self.inner.get_sregs()
        }

        pub fn set_sregs(&self, sregs: &kvm_sregs) -> Result<(), Error> {
            self.inner.set_sregs(sregs)
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
                                "MMIO read from unmapped {:#x} (size {}); returning zeros",
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
                                "MMIO write to unmapped {:#x} (size {}, data {:02x?})",
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
                        log::warn!("unexpected KVM HVC exit: args={args:?}");
                    }
                    VmExit::Smc { args } => {
                        log::warn!("unexpected KVM SMC exit: args={args:?}");
                    }
                    VmExit::Unknown(reason) => return Err(Error::UnhandledExit(reason)),
                }
            }
        }

        pub fn run_debug(&mut self) -> Result<DebugExit, Error> {
            loop {
                let bus = Arc::clone(&self.mmio_bus);
                let pio_read = Arc::clone(&self.pio_read);
                let exit = self.inner.run(
                    move |port, size| pio_read(port, size),
                    move |addr, data| {
                        let handled = bus.lock().expect("MMIO bus lock poisoned").read(addr, data);
                        if !handled {
                            log::debug!(
                                "MMIO read from unmapped {:#x} (size {}); returning zeros",
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
                                "MMIO write to unmapped {:#x} (size {}, data {:02x?})",
                                addr,
                                size,
                                &data[..size as usize],
                            );
                        }
                        return Ok(DebugExit::MmioWrite { addr, data, size });
                    }
                    VmExit::Interrupted => return Ok(DebugExit::Interrupted),
                    VmExit::Halted => return Ok(DebugExit::Halted),
                    VmExit::Shutdown => return Ok(DebugExit::Shutdown),
                    VmExit::Debug => return Ok(DebugExit::Debug),
                    VmExit::Hvc { args } => {
                        log::warn!("unexpected KVM HVC exit while debugging: args={args:?}");
                    }
                    VmExit::Smc { args } => {
                        log::warn!("unexpected KVM SMC exit while debugging: args={args:?}");
                    }
                    VmExit::Unknown(reason) => return Ok(DebugExit::Unknown(reason)),
                }
            }
        }
    }

    impl AsRawFd for Vcpu {
        fn as_raw_fd(&self) -> RawFd {
            self.inner.as_raw_fd()
        }
    }
}

#[cfg(target_os = "linux")]
pub use imp::*;
