#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod cpuid_x86;
#[cfg(target_os = "linux")]
mod hypervisor;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod irq;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod msi;

/// Re-export the KVM debug-control flags so dillo can configure guest-debug
/// modes without depending on `kvm-bindings` directly.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub mod debug_flags {
    pub use kvm_bindings::{
        KVM_GUESTDBG_ENABLE, KVM_GUESTDBG_SINGLESTEP, KVM_GUESTDBG_USE_HW_BP,
        KVM_GUESTDBG_USE_SW_BP,
    };
}

/// Re-export raw KVM register structures for the gdb stub.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub use kvm_bindings::{kvm_regs, kvm_sregs};

/// Reasons a KVM vCPU run returned to backend code.
#[cfg(target_os = "linux")]
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
    Debug,
}

#[cfg(target_os = "linux")]
mod imp {
    use std::os::fd::{AsRawFd, RawFd};
    use std::sync::{Arc, Mutex};
    use std::sync::{OnceLock, atomic::AtomicBool, atomic::AtomicU8, atomic::Ordering};
    use std::thread;
    use std::time::Duration;

    #[cfg(target_arch = "aarch64")]
    use dillo_devtree::{
        FromDevTree,
        devtree::{NodeView, OwnedTree, PropertyView, Tree},
    };
    use dillo_machine::VcpuStop;
    use dillo_mmio::{
        Attach, InterruptError, InterruptLine, MmioAttachment, MmioBus, MmioDevice,
        MmioDeviceHandle, MmioInterrupt, MmioSpawnError, SharedMemory,
    };

    #[cfg(target_arch = "x86_64")]
    use crate::{kvm_regs, kvm_sregs};
    pub use vmm_sys_util::eventfd::EventFd;

    use crate::VmExit;
    pub use crate::hypervisor::Error;
    #[cfg(target_arch = "x86_64")]
    pub use crate::irq::{IrqError, IrqManager};
    #[cfg(target_arch = "x86_64")]
    pub use crate::msi::IrqfdNotifier;

    #[cfg(target_arch = "x86_64")]
    pub const HOST_ARCH: dillo_machine::HostArchitecture = dillo_machine::HostArchitecture::X86_64;
    #[cfg(target_arch = "aarch64")]
    pub const HOST_ARCH: dillo_machine::HostArchitecture = dillo_machine::HostArchitecture::Aarch64;

    pub type PioRead = Arc<dyn Fn(u16, u8) -> u32 + Send + Sync + 'static>;
    pub type PioWrite = Arc<dyn Fn(u16, &[u8]) + Send + Sync + 'static>;

    const VCPU_KICK_SIGNAL: nix::sys::signal::Signal = nix::sys::signal::Signal::SIGUSR1;

    extern "C" fn vcpu_kick_signal_handler(_: libc::c_int) {}

    /// Install KVM/Linux host signal handling for the dillo supervisor.
    pub fn install_signal_watchers(supervisor_shutdown: &'static AtomicBool) {
        use nix::sys::signal::{SigSet, Signal};
        use nix::sys::signalfd::{SfdFlags, SignalFd};

        let mut mask = SigSet::empty();
        mask.add(Signal::SIGINT);
        mask.add(Signal::SIGTERM);
        mask.add(Signal::SIGQUIT);
        mask.add(Signal::SIGWINCH);
        mask.thread_block().expect("block signals on main thread");

        let sfd = SignalFd::with_flags(&mask, SfdFlags::SFD_CLOEXEC).expect("signalfd creation");

        thread::Builder::new()
            .name("dillo-signals".into())
            .spawn(move || {
                static SEEN: AtomicU8 = AtomicU8::new(0);
                loop {
                    match sfd.read_signal() {
                        Ok(Some(sig)) => {
                            let signo = sig.ssi_signo as i32;
                            if signo == Signal::SIGWINCH as i32 {
                                log::trace!("SIGWINCH - no console child to forward to yet");
                                continue;
                            }
                            let count = SEEN.fetch_add(1, Ordering::SeqCst);
                            let name = match signo {
                                n if n == Signal::SIGINT as i32 => "SIGINT",
                                n if n == Signal::SIGTERM as i32 => "SIGTERM",
                                n if n == Signal::SIGQUIT as i32 => "SIGQUIT",
                                _ => "signal",
                            };
                            if signo == Signal::SIGQUIT as i32 || count >= 1 {
                                log::warn!("{name} - hard exit");
                                std::process::exit(128 + signo);
                            }
                            log::warn!(
                                "{name} - graceful shutdown requested; waiting 5s for guest before hard exit"
                            );
                            supervisor_shutdown.store(true, Ordering::Release);

                            let signo_for_timer = signo;
                            thread::Builder::new()
                                .name("dillo-shutdown-watchdog".into())
                                .spawn(move || {
                                    thread::sleep(Duration::from_secs(5));
                                    log::warn!("guest did not shut down within 5s - hard exit");
                                    std::process::exit(128 + signo_for_timer);
                                })
                                .expect("spawn shutdown watchdog");
                        }
                        Ok(None) => continue,
                        Err(e) => {
                            log::error!("signalfd read: {e}");
                            return;
                        }
                    }
                }
            })
            .expect("spawn dillo-signals thread");
    }

    static ORIGINAL_TERMIOS: OnceLock<libc::termios> = OnceLock::new();

    /// Raw terminal guard for KVM/Linux supervisor sessions.
    #[derive(Debug)]
    pub struct RawStdio {
        armed: bool,
    }

    impl RawStdio {
        pub fn enter_if_tty() -> Self {
            use std::os::fd::{AsFd, AsRawFd};
            let stdin = std::io::stdin();
            let fd = stdin.as_fd().as_raw_fd();
            #[allow(unsafe_code)]
            let is_tty = unsafe { libc::isatty(fd) } == 1;
            if !is_tty {
                return Self { armed: false };
            }
            #[allow(unsafe_code)]
            let original = unsafe {
                let mut t: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(fd, &mut t) != 0 {
                    return Self { armed: false };
                }
                t
            };
            let mut raw = original;
            raw.c_lflag &= !(libc::ECHO | libc::ICANON | libc::ISIG | libc::IEXTEN);
            #[allow(unsafe_code)]
            unsafe {
                if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
                    return Self { armed: false };
                }
            }
            if ORIGINAL_TERMIOS.set(original).is_ok() {
                #[allow(unsafe_code)]
                unsafe {
                    libc::atexit(restore_termios_atexit);
                }
            }
            Self { armed: true }
        }
    }

    impl Drop for RawStdio {
        fn drop(&mut self) {
            if self.armed {
                restore_termios();
            }
        }
    }

    fn restore_termios() {
        use std::os::fd::{AsFd, AsRawFd};
        if let Some(orig) = ORIGINAL_TERMIOS.get() {
            let stdin = std::io::stdin();
            let fd = stdin.as_fd().as_raw_fd();
            #[allow(unsafe_code)]
            unsafe {
                let _ = libc::tcsetattr(fd, libc::TCSANOW, orig);
            }
        }
    }

    extern "C" fn restore_termios_atexit() {
        restore_termios();
    }

    pub fn install_panic_terminal_restore() {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_termios();
            prev(info);
        }));
    }

    #[derive(Clone)]
    pub struct Vm {
        inner: crate::hypervisor::Vm,
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
        #[cfg(target_arch = "x86_64")]
        pub fn new() -> Result<Self, Error> {
            Self::try_from(Config {})
        }

        fn vm_fd_arc(&self) -> Arc<kvm_ioctls::VmFd> {
            self.inner.vm_fd_arc()
        }

        #[cfg(target_arch = "x86_64")]
        pub fn create_irq_manager(&self) -> Result<IrqManager, IrqError> {
            IrqManager::new(self.vm_fd_arc())
        }

        fn add_memslot(&self, slot: u32, gpa: u64, host_addr: u64, size: u64) -> Result<(), Error> {
            self.inner.add_memslot(slot, gpa, host_addr, size)
        }

        fn create_vcpu_with_pio(
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

    #[cfg(target_arch = "aarch64")]
    #[derive(Debug, Clone, Copy)]
    pub(crate) struct Aarch64Substrate {
        pub(crate) dist_base: u64,
        pub(crate) redist_base: u64,
        pub(crate) spi_count: u32,
    }

    #[derive(Debug, Clone)]
    pub struct Config {
        #[cfg(target_arch = "aarch64")]
        pub dtb: Vec<u8>,
    }

    impl TryFrom<Config> for Vm {
        type Error = Error;

        fn try_from(config: Config) -> Result<Self, Self::Error> {
            let exit_requester = VcpuExitRequester::new();
            #[cfg(target_arch = "x86_64")]
            let inner = {
                let _ = config;
                crate::hypervisor::Vm::new()?
            };
            #[cfg(target_arch = "aarch64")]
            let inner = {
                let parsed: Tree<'_> = Tree::parse(&config.dtb).map_err(Error::ParseDtb)?;
                let mut tree = OwnedTree::materialize(&parsed);
                let substrate = Aarch64Substrate::from_devtree(&mut tree)?
                    .ok_or(Error::MissingSubstrate("/interrupt-controller@*"))?;
                crate::hypervisor::Vm::new_with_gic(&substrate)?
            };
            Ok(Self {
                inner,
                mmio_bus: Arc::new(Mutex::new(MmioBus::new())),
                exit_requester,
                shared_memory: Vec::new(),
            })
        }
    }

    #[cfg(target_arch = "aarch64")]
    impl FromDevTree for Aarch64Substrate {
        type Error = Error;

        fn from_devtree(tree: &mut OwnedTree) -> Result<Option<Self>, Self::Error> {
            let root = tree.root_mut();
            let intc_name = root
                .children()
                .find(|node| {
                    node.name().starts_with("interrupt-controller@")
                        && compatible_contains(*node, "arm,gic-v3")
                })
                .map(|node| node.name().to_string())
                .ok_or(Error::MissingSubstrate("/interrupt-controller@*"))?;
            let mut intc = root
                .remove_child(&intc_name)
                .ok_or(Error::MissingSubstrate("/interrupt-controller@*"))?;
            consume_compatible(&mut intc, "/interrupt-controller", "arm,gic-v3")?;
            let reg = intc
                .remove_property("reg")
                .ok_or(Error::BadSubstrateProperty {
                    node: "/interrupt-controller",
                    prop: "reg",
                    reason: "missing",
                })?;
            let (dist_base, _) = reg_pair(&reg, 0).ok_or(Error::BadSubstrateProperty {
                node: "/interrupt-controller",
                prop: "reg",
                reason: "missing GICD pair",
            })?;
            let (redist_base, _) = reg_pair(&reg, 1).ok_or(Error::BadSubstrateProperty {
                node: "/interrupt-controller",
                prop: "reg",
                reason: "missing GICR pair",
            })?;
            let _ = intc.remove_property("#interrupt-cells");
            let _ = intc.remove_property("interrupt-controller");
            let _ = intc.remove_property("phandle");

            let v2m_name = root
                .children()
                .find(|node| {
                    node.name().starts_with("msi-controller@")
                        && compatible_contains(*node, "arm,gic-v2m-frame")
                })
                .map(|node| node.name().to_string())
                .ok_or(Error::MissingSubstrate("/msi-controller@*"))?;
            let mut v2m = root
                .remove_child(&v2m_name)
                .ok_or(Error::MissingSubstrate("/msi-controller@*"))?;
            consume_compatible(&mut v2m, "/msi-controller", "arm,gic-v2m-frame")?;
            let spi_count = v2m
                .remove_property("arm,msi-num-spis")
                .and_then(|prop| prop.as_u32())
                .ok_or(Error::BadSubstrateProperty {
                    node: "/msi-controller",
                    prop: "arm,msi-num-spis",
                    reason: "missing or not a u32",
                })?;
            let _ = v2m.remove_property("reg");
            let _ = v2m.remove_property("arm,msi-base-spi");
            let _ = v2m.remove_property("msi-controller");
            let _ = v2m.remove_property("phandle");

            Ok(Some(Self {
                dist_base,
                redist_base,
                spi_count,
            }))
        }
    }

    #[cfg(target_arch = "aarch64")]
    fn compatible_contains<N: NodeView>(node: N, value: &str) -> bool {
        let Some(prop) = node.property("compatible") else {
            return false;
        };
        stringlist_contains(prop.as_ref(), value)
    }

    #[cfg(target_arch = "aarch64")]
    fn consume_compatible(
        node: &mut dillo_devtree::devtree::OwnedNode,
        node_name: &'static str,
        value: &str,
    ) -> Result<(), Error> {
        let prop = node
            .remove_property("compatible")
            .ok_or(Error::BadSubstrateProperty {
                node: node_name,
                prop: "compatible",
                reason: "missing",
            })?;
        if stringlist_contains(prop.as_ref(), value) {
            Ok(())
        } else {
            Err(Error::BadSubstrateProperty {
                node: node_name,
                prop: "compatible",
                reason: "unsupported compatible",
            })
        }
    }

    #[cfg(target_arch = "aarch64")]
    fn stringlist_contains(bytes: &[u8], value: &str) -> bool {
        bytes
            .split(|byte| *byte == 0)
            .filter(|item| !item.is_empty())
            .any(|item| item == value.as_bytes())
    }

    #[cfg(target_arch = "aarch64")]
    fn reg_pair<P: PropertyView>(prop: P, pair_index: usize) -> Option<(u64, u64)> {
        let cells: Vec<u32> = prop.as_u32s()?.collect();
        let base = cells.get(pair_index * 4..pair_index * 4 + 2)?;
        let size = cells.get(pair_index * 4 + 2..pair_index * 4 + 4)?;
        Some((
            (u64::from(base[0]) << 32) | u64::from(base[1]),
            (u64::from(size[0]) << 32) | u64::from(size[1]),
        ))
    }

    impl dillo_machine::Machine for Vm {
        type Error = Error;
        type Config = Config;
        type Vcpu = Vcpu;
        type Cpu = Cpu;
        type Memory = Memory;

        fn request_vcpu_exit(&self) -> Result<(), Self::Error> {
            Vm::request_vcpu_exit(self);
            Ok(())
        }
    }

    /// One KVM memslot backed by host memory that dillo derived from the
    /// merged DTB and launch plan.
    #[derive(Debug, Clone, Copy)]
    pub struct Memory {
        slot: u32,
        gpa: u64,
        host_addr: u64,
        size: u64,
    }

    impl Memory {
        pub fn new(slot: u32, gpa: u64, host_addr: u64, size: u64) -> Self {
            Self {
                slot,
                gpa,
                host_addr,
                size,
            }
        }
    }

    impl Attach<Memory> for Vm {
        type Error = Error;
        type Output = ();

        fn attach(&mut self, item: Memory) -> Result<Self::Output, Self::Error> {
            self.add_memslot(item.slot, item.gpa, item.host_addr, item.size)
        }
    }

    /// One KVM vCPU creation request. Backend-specific setup stays inside KVM;
    /// dillo supplies only the launch facts it derived from PMI/DTB.
    pub struct Cpu {
        pub idx: u32,
        pub cpu_profile: String,
        #[cfg(target_arch = "x86_64")]
        pub pio_read: PioRead,
        #[cfg(target_arch = "x86_64")]
        pub pio_write: PioWrite,
        #[cfg(target_arch = "x86_64")]
        pub state: Option<pmi::vm::vcpu::x86_64::CpuState>,
        #[cfg(target_arch = "aarch64")]
        pub state: Option<pmi::vm::vcpu::aarch64::CpuState>,
    }

    impl std::fmt::Debug for Cpu {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let mut debug = f.debug_struct("Cpu");
            debug.field("idx", &self.idx);
            debug.field("cpu_profile", &self.cpu_profile);
            debug.field("has_state", &self.state.is_some());
            debug.finish_non_exhaustive()
        }
    }

    impl Attach<Cpu> for Vm {
        type Error = Error;
        type Output = Vcpu;

        fn attach(&mut self, item: Cpu) -> Result<Self::Output, Self::Error> {
            let mut vcpu = self.create_vcpu_with_pio(
                item.idx,
                &item.cpu_profile,
                #[cfg(target_arch = "x86_64")]
                item.pio_read,
                #[cfg(target_arch = "x86_64")]
                item.pio_write,
                #[cfg(target_arch = "aarch64")]
                Arc::new(|_, _| 0),
                #[cfg(target_arch = "aarch64")]
                Arc::new(|_, _| {}),
            )?;
            #[cfg(target_arch = "x86_64")]
            if let Some(state) = item.state {
                vcpu.set_x86_64_state(&state)?;
            }
            #[cfg(target_arch = "aarch64")]
            if let Some(state) = item.state {
                vcpu.set_aarch64_state(&state)?;
            }
            Ok(vcpu)
        }
    }

    #[derive(Debug)]
    pub struct EventFdInterruptLine {
        eventfd: EventFd,
    }

    impl EventFdInterruptLine {
        pub fn new(eventfd: EventFd) -> Self {
            Self { eventfd }
        }
    }

    impl InterruptLine for EventFdInterruptLine {
        fn signal(&self) {
            if let Err(e) = self.eventfd.write(1) {
                log::warn!("KVM irqfd interrupt signal failed: {e}");
            }
        }

        fn set_level(&self, level: bool) -> Result<(), InterruptError> {
            if level {
                self.eventfd
                    .write(1)
                    .map_err(|e| InterruptError::Delivery(e.to_string()))?;
            }
            Ok(())
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[derive(Debug)]
    pub struct SpiInterruptLine {
        vm: Arc<kvm_ioctls::VmFd>,
        intid: u32,
    }

    #[cfg(target_arch = "aarch64")]
    impl SpiInterruptLine {
        fn new(vm: Arc<kvm_ioctls::VmFd>, intid: u32) -> Self {
            Self { vm, intid }
        }

        fn irq_line(&self) -> u32 {
            (kvm_bindings::KVM_ARM_IRQ_TYPE_SPI << kvm_bindings::KVM_ARM_IRQ_TYPE_SHIFT)
                | (self.intid << kvm_bindings::KVM_ARM_IRQ_NUM_SHIFT)
        }
    }

    #[cfg(target_arch = "aarch64")]
    impl InterruptLine for SpiInterruptLine {
        fn signal(&self) {
            if let Err(e) = self.set_level(true) {
                log::warn!("KVM SPI interrupt signal failed: {e}");
            }
            if let Err(e) = self.set_level(false) {
                log::warn!("KVM SPI interrupt deassert failed: {e}");
            }
        }

        fn set_level(&self, level: bool) -> Result<(), InterruptError> {
            self.vm
                .set_irq_line(self.irq_line(), level)
                .map_err(|e| InterruptError::Delivery(e.to_string()))
        }
    }

    #[cfg(target_arch = "aarch64")]
    impl Vm {
        pub fn create_spi_interrupt_line(&self, intid: u32) -> SpiInterruptLine {
            SpiInterruptLine::new(self.vm_fd_arc(), intid)
        }

        pub fn init_gic(&self) -> Result<(), Error> {
            self.inner.init_gic()
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
            run: dillo_mmio::MmioDeviceRun,
        ) -> Result<MmioDeviceHandle, MmioSpawnError> {
            Ok(MmioDeviceHandle::thread(run))
        }
    }

    pub struct Vcpu {
        inner: crate::hypervisor::Vcpu,
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

        #[cfg(target_arch = "x86_64")]
        pub fn set_x86_64_state(
            &mut self,
            state: &pmi::vm::vcpu::x86_64::CpuState,
        ) -> Result<(), Error> {
            self.inner.set_x86_64_state(state)
        }

        #[cfg(target_arch = "aarch64")]
        pub fn set_aarch64_state(
            &mut self,
            state: &pmi::vm::vcpu::aarch64::CpuState,
        ) -> Result<(), Error> {
            self.inner.set_aarch64_state(state)
        }

        #[cfg(target_arch = "x86_64")]
        pub fn set_guest_debug_flags(&self, flags: u32) -> Result<(), Error> {
            self.inner.set_guest_debug_flags(flags)
        }

        #[cfg(target_arch = "x86_64")]
        pub fn get_regs(&self) -> Result<kvm_regs, Error> {
            self.inner.get_regs()
        }

        #[cfg(target_arch = "x86_64")]
        pub fn set_regs(&self, regs: &kvm_regs) -> Result<(), Error> {
            self.inner.set_regs(regs)
        }

        #[cfg(target_arch = "x86_64")]
        pub fn get_sregs(&self) -> Result<kvm_sregs, Error> {
            self.inner.get_sregs()
        }

        #[cfg(target_arch = "x86_64")]
        pub fn set_sregs(&self, sregs: &kvm_sregs) -> Result<(), Error> {
            self.inner.set_sregs(sregs)
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
                                "MMIO read from unmapped {:#x} (size {}); returning zeros",
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
                    VmExit::Unknown(reason) => return Ok(DebugExit::Unknown(reason)),
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

    impl AsRawFd for Vcpu {
        fn as_raw_fd(&self) -> RawFd {
            self.inner.as_raw_fd()
        }
    }
}

#[cfg(target_os = "linux")]
pub use imp::*;
