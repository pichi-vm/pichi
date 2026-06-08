#[cfg(target_os = "linux")]
mod imp {
    use std::os::fd::{AsRawFd, RawFd};
    use std::sync::{Arc, Mutex};
    use std::sync::{OnceLock, atomic::AtomicBool, atomic::Ordering};

    use dillo_machine::VcpuStop;
    use dillo_mmio::{
        Attach, MmioAttachment, MmioBus, MmioDevice, MmioDeviceHandle, MmioDeviceHost,
        MmioInterrupt, MmioNotifyEvent, MmioSpawnError, QueueNotifier, SharedMemory,
    };

    use dillo_hypervisor::VmExit;
    pub use dillo_hypervisor::{Error, debug_flags, kvm_regs, kvm_sregs};
    use kvm_ioctls::{IoEventAddress, NoDatamatch};
    use vmm_sys_util::eventfd::EventFd;

    type PioRead = Arc<dyn Fn(u16, u8) -> u32 + Send + Sync + 'static>;
    type PioWrite = Arc<dyn Fn(u16, &[u8]) + Send + Sync + 'static>;

    const VCPU_KICK_SIGNAL: nix::sys::signal::Signal = nix::sys::signal::Signal::SIGUSR1;

    extern "C" fn vcpu_kick_signal_handler(_: libc::c_int) {}

    #[derive(Clone)]
    pub struct Vm {
        inner: dillo_hypervisor::Vm,
        mmio_bus: Arc<Mutex<MmioBus>>,
        exit_requester: VcpuExitRequester,
        shared_memory: Vec<Arc<dyn SharedMemory>>,
    }

    impl std::fmt::Debug for Vm {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Vm")
                .field("inner", &self.inner)
                .field("mmio_bus", &self.mmio_bus)
                .field("exit_requester", &self.exit_requester)
                .field("shared_memory", &self.shared_memory.len())
                .finish()
        }
    }

    impl Vm {
        pub fn new() -> Result<Self, Error> {
            let exit_requester = VcpuExitRequester::new();
            Ok(Self {
                inner: dillo_hypervisor::Vm::new()?,
                mmio_bus: Arc::new(Mutex::new(MmioBus::new())),
                exit_requester,
                shared_memory: Vec::new(),
            })
        }

        pub fn vm_fd_arc(&self) -> Arc<kvm_ioctls::VmFd> {
            self.inner.vm_fd_arc()
        }

        pub fn create_queue_notifier(&self) -> KvmQueueNotifier {
            KvmQueueNotifier::new(self.vm_fd_arc())
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
                exit_requester: self.exit_requester(),
                registered_thread: AtomicBool::new(false),
            })
        }

        pub fn request_vcpu_exit(&self) {
            self.exit_requester.request_vcpu_exit();
        }

        pub fn exit_requester(&self) -> VcpuExitRequester {
            self.exit_requester.clone()
        }

        pub fn set_shared_memory_capabilities(
            &mut self,
            shared_memory: Vec<Arc<dyn SharedMemory>>,
        ) {
            self.shared_memory = shared_memory;
        }
    }

    #[derive(Debug)]
    pub struct KvmQueueNotifier {
        vm_fd: Arc<kvm_ioctls::VmFd>,
        registered: Vec<(usize, u64, EventFd)>,
    }

    impl KvmQueueNotifier {
        pub fn new(vm_fd: Arc<kvm_ioctls::VmFd>) -> Self {
            Self {
                vm_fd,
                registered: Vec::new(),
            }
        }
    }

    impl QueueNotifier for KvmQueueNotifier {
        fn register(
            &mut self,
            queue_index: usize,
            addr: u64,
            event: &dyn MmioNotifyEvent,
        ) -> Result<(), String> {
            let eventfd = event.as_eventfd().try_clone().map_err(|e| e.to_string())?;
            self.vm_fd
                .register_ioevent(event.as_eventfd(), &IoEventAddress::Mmio(addr), NoDatamatch)
                .map_err(|e| e.to_string())?;
            self.registered.push((queue_index, addr, eventfd));
            Ok(())
        }

        fn unregister_all(&mut self) {
            for (queue_index, addr, eventfd) in self.registered.drain(..) {
                if let Err(e) = self.vm_fd.unregister_ioevent(
                    &eventfd,
                    &IoEventAddress::Mmio(addr),
                    NoDatamatch,
                ) {
                    log::warn!(
                        "virtio-pci: failed to unregister ioeventfd for queue {queue_index} \
                         at {addr:#x}: {e}"
                    );
                } else {
                    log::debug!(
                        "virtio-pci: unregistered ioeventfd for queue {queue_index} at {addr:#x}"
                    );
                }
            }
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
                    "MMIO device requested {} fixed shared-memory requirement(s), but KVM attachment does not realize machine-mediated shared-memory capabilities yet",
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

    pub struct Vcpu {
        inner: dillo_hypervisor::Vcpu,
        mmio_bus: Arc<Mutex<MmioBus>>,
        pio_read: PioRead,
        pio_write: PioWrite,
        exit_requester: VcpuExitRequester,
        registered_thread: AtomicBool,
    }

    #[derive(Clone, Debug)]
    pub struct VcpuExitRequester {
        threads: Arc<Mutex<Vec<libc::pthread_t>>>,
    }

    impl VcpuExitRequester {
        fn new() -> Self {
            Self::install_kick_handler();
            Self {
                threads: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn install_kick_handler() {
            static INSTALLED: OnceLock<()> = OnceLock::new();
            INSTALLED.get_or_init(|| {
                let action = nix::sys::signal::SigAction::new(
                    nix::sys::signal::SigHandler::Handler(vcpu_kick_signal_handler),
                    nix::sys::signal::SaFlags::empty(),
                    nix::sys::signal::SigSet::empty(),
                );
                // SAFETY: installs a trivial async-signal-safe handler for a
                // process-local wake signal used only to interrupt KVM_RUN.
                #[allow(unsafe_code)]
                unsafe {
                    nix::sys::signal::sigaction(VCPU_KICK_SIGNAL, &action)
                        .expect("install vCPU kick signal handler");
                }
            });
        }

        fn register_current_thread(&self) {
            // SAFETY: pthread_self returns the current thread identifier. It is
            // stored only so request_vcpu_exit can send the wake signal while
            // the vCPU worker thread may be blocked in KVM_RUN.
            #[allow(unsafe_code)]
            let thread = unsafe { libc::pthread_self() };
            let mut threads = self.threads.lock().expect("vCPU thread list poisoned");
            if !threads.contains(&thread) {
                threads.push(thread);
            }
        }

        pub fn request_vcpu_exit(&self) {
            for thread in self
                .threads
                .lock()
                .expect("vCPU thread list poisoned")
                .iter()
            {
                // SAFETY: pthread_t values come from vCPU worker threads that
                // registered themselves before entering KVM_RUN. If a thread
                // has already exited, pthread_kill returns ESRCH and there is
                // nothing left to wake.
                #[allow(unsafe_code)]
                let rc = unsafe { libc::pthread_kill(*thread, VCPU_KICK_SIGNAL as libc::c_int) };
                if rc != 0 && rc != libc::ESRCH {
                    log::warn!("failed to kick vCPU thread with signal: errno {rc}");
                }
            }
        }
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

        pub fn run_until_stop<F>(&mut self, mut stop: F) -> Result<VcpuStop, Error>
        where
            F: FnMut() -> Option<VcpuStop>,
        {
            if !self.registered_thread.swap(true, Ordering::AcqRel) {
                self.exit_requester.register_current_thread();
            }
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
                        log::warn!(
                            "guest shutdown via KVM_EXIT_SHUTDOWN; treating as guest poweroff"
                        );
                        return Ok(VcpuStop::GuestPoweroff);
                    }
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
