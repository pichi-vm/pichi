use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::Mutex;

use dillo_mmio_uart::Ns16550;
use dillo_pci::MsixNotifier;
use vm_memory::GuestMemoryMmap;

#[cfg(any(target_os = "linux", target_os = "windows"))]
use dillo_mmio::Interrupt;
#[cfg(target_os = "macos")]
use dillo_mmio::MmioBus;
use dillo_mmio::{Attach, MmioAttachment, MmioDevice, MmioWindow, SharedMemory};

#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(crate) type PioRead = Arc<dyn Fn(u16, u8) -> u32 + Send + Sync + 'static>;
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(crate) type PioWrite = Arc<dyn Fn(u16, &[u8]) + Send + Sync + 'static>;

#[cfg(target_os = "linux")]
use crate::RunError;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use crate::backend_select::machine as backend_machine;
#[cfg(target_os = "macos")]
use crate::{RunError, hvf_devices};
#[cfg(target_os = "windows")]
use crate::{RunError, whp_devices::WhpMsixNotifier};
#[cfg(target_os = "linux")]
use backend_machine::{IrqManager, IrqfdNotifier};
#[cfg(target_os = "windows")]
use dillo_x86::IoApic;
use dillo_x86::syscon;

#[cfg(target_os = "linux")]
#[derive(Debug)]
pub(crate) struct KvmMsixNotifier {
    inner: Arc<IrqfdNotifier>,
}

#[cfg(target_os = "linux")]
impl KvmMsixNotifier {
    fn new(inner: Arc<IrqfdNotifier>) -> Self {
        Self { inner }
    }

    #[cfg(feature = "process-isolation-spawn")]
    pub(crate) fn eventfd_for_vector(&self, vector: u16) -> Option<backend_machine::EventFd> {
        self.inner.eventfd_for_vector(vector)
    }

    #[cfg(not(feature = "process-isolation-spawn"))]
    pub(crate) fn interrupt_for_vector(&self, vector: u16) -> Option<Interrupt> {
        self.inner.interrupt_for_vector(vector)
    }
}

#[cfg(target_os = "linux")]
impl MsixNotifier for KvmMsixNotifier {
    fn vector_updated(&self, vector: u16, entry: &dillo_pci::MsixTableEntry) {
        self.inner.vector_updated(
            vector,
            entry.msg_addr_lo,
            entry.msg_addr_hi,
            entry.msg_data,
            entry.vector_ctl & 1 != 0,
        );
    }

    fn msix_enabled(&self, enabled: bool) {
        self.inner.set_enabled(enabled);
    }
}

#[cfg(target_os = "linux")]
pub(crate) struct Memslot {
    pub(crate) index: u32,
    pub(crate) gpa: u64,
    pub(crate) host_addr: u64,
    pub(crate) size: u64,
}

#[cfg(target_os = "linux")]
pub(crate) struct VmOptions {
    pub(crate) memslots: Vec<Memslot>,
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) enum VcpuSeed<'a> {
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    X86_64Boot(&'a pmi::vm::vcpu::x86_64::CpuState),
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    X86_64Secondary,
    #[cfg(target_os = "macos")]
    Aarch64 {
        mpidr: u64,
        state: &'a pmi::vm::vcpu::aarch64::CpuState,
    },
}

#[allow(dead_code)]
pub(crate) trait BackendVm {
    type Options;
    type Vcpu;
    type InterruptState: Clone;
    type SerialIrq;
    type SerialDevice: MmioDevice + 'static;
    type WiredIrq;
    type MsiNotifier: MsixNotifier + 'static;

    fn new(opts: Self::Options) -> Result<Self, RunError>
    where
        Self: Sized;

    fn interrupt_state(&self) -> Result<Self::InterruptState, RunError>;

    fn msix_notifier(
        &self,
        interrupt_state: Self::InterruptState,
        count: u16,
    ) -> Arc<Self::MsiNotifier>;

    fn create_vcpu(
        &self,
        idx: u32,
        cpu_profile: &str,
        seed: VcpuSeed<'_>,
        #[cfg(any(target_os = "linux", target_os = "windows"))] pio_read: PioRead,
        #[cfg(any(target_os = "linux", target_os = "windows"))] pio_write: PioWrite,
    ) -> Result<Self::Vcpu, RunError>;

    fn current_thread_vcpu(
        seed: VcpuSeed<'_>,
        #[cfg(target_os = "macos")] mmio_bus: Arc<std::sync::Mutex<MmioBus>>,
    ) -> Result<Self::Vcpu, RunError>
    where
        Self: Sized;

    fn attach_mmio<D>(&mut self, device: Arc<D>) -> Result<Arc<dyn MmioAttachment>, RunError>
    where
        D: MmioDevice + 'static;

    fn set_shared_memory_capabilities(&mut self, capabilities: Vec<Arc<dyn SharedMemory>>);

    fn attach_x86_syscon_devices(
        &mut self,
        poweroff: dillo_platform::Syscon,
        reboot: Option<dillo_platform::Syscon>,
        state: Arc<syscon::SysconState>,
    ) -> Result<Vec<Arc<dyn MmioAttachment>>, RunError>;

    fn ns16550(
        &self,
        window: MmioWindow,
        reg_shift: u32,
        irq: Self::SerialIrq,
        out: Box<dyn std::io::Write + Send>,
    ) -> Result<Self::SerialDevice, RunError>;

    fn guest_memory(&self) -> Result<GuestMemoryMmap, RunError>;

    fn wired_irq(&self, intid: u32) -> Self::WiredIrq;
}

#[cfg(target_os = "linux")]
impl BackendVm for backend_machine::Vm {
    type Options = VmOptions;
    type Vcpu = backend_machine::Vcpu;
    type InterruptState = Arc<Mutex<IrqManager>>;
    type SerialIrq = (Arc<Mutex<IrqManager>>, u32);
    type SerialDevice = Ns16550;
    type WiredIrq = ();
    type MsiNotifier = KvmMsixNotifier;

    fn new(opts: Self::Options) -> Result<Self, RunError> {
        let vm = backend_machine::Vm::new()?;
        for memslot in opts.memslots {
            log::info!(
                "registering memslot {}: GPA {:#x}..{:#x} -> host {:#x} ({} bytes)",
                memslot.index,
                memslot.gpa,
                memslot.gpa + memslot.size,
                memslot.host_addr,
                memslot.size
            );
            vm.add_memslot(memslot.index, memslot.gpa, memslot.host_addr, memslot.size)?;
        }
        Ok(vm)
    }

    fn interrupt_state(&self) -> Result<Self::InterruptState, RunError> {
        let manager = self.create_irq_manager().map_err(|e| {
            RunError::Kvm(backend_machine::Error::RunVcpu(
                0,
                std::io::Error::other(format!("irq manager: {e}")),
            ))
        })?;
        Ok(Arc::new(Mutex::new(manager)))
    }

    fn msix_notifier(
        &self,
        irq_manager: Self::InterruptState,
        count: u16,
    ) -> Arc<Self::MsiNotifier> {
        Arc::new(KvmMsixNotifier::new(Arc::new(IrqfdNotifier::new(
            irq_manager,
            count,
        ))))
    }

    fn create_vcpu(
        &self,
        idx: u32,
        cpu_profile: &str,
        seed: VcpuSeed<'_>,
        pio_read: PioRead,
        pio_write: PioWrite,
    ) -> Result<Self::Vcpu, RunError> {
        let mut vcpu =
            backend_machine::Vm::create_vcpu_with_pio(self, idx, cpu_profile, pio_read, pio_write)
                .map_err(RunError::Kvm)?;
        match seed {
            VcpuSeed::X86_64Boot(state) => {
                #[cfg(target_arch = "x86_64")]
                vcpu.set_x86_64_state(state)?;
                #[cfg(not(target_arch = "x86_64"))]
                {
                    let _ = state;
                    return Err(RunError::ArchMismatch);
                }
            }
            VcpuSeed::X86_64Secondary => {}
        }
        Ok(vcpu)
    }

    fn current_thread_vcpu(_seed: VcpuSeed<'_>) -> Result<Self::Vcpu, RunError> {
        Err(RunError::Unimplemented(
            "current-thread vCPU factory is HVF-only",
        ))
    }

    fn attach_mmio<D>(&mut self, device: Arc<D>) -> Result<Arc<dyn MmioAttachment>, RunError>
    where
        D: MmioDevice + 'static,
    {
        Attach::attach(self, device).map_err(RunError::Kvm)
    }

    fn set_shared_memory_capabilities(&mut self, capabilities: Vec<Arc<dyn SharedMemory>>) {
        backend_machine::Vm::set_shared_memory_capabilities(self, capabilities);
    }

    fn attach_x86_syscon_devices(
        &mut self,
        poweroff: dillo_platform::Syscon,
        reboot: Option<dillo_platform::Syscon>,
        state: Arc<syscon::SysconState>,
    ) -> Result<Vec<Arc<dyn MmioAttachment>>, RunError> {
        let mut attachments = Vec::new();
        attachments.push(self.attach_mmio(Arc::new(syscon::SysconDevice::new(
            "syscon-poweroff",
            poweroff,
            syscon::SysconAction::Poweroff,
            Arc::clone(&state),
        )))?);
        if let Some(reboot) = reboot {
            attachments.push(self.attach_mmio(Arc::new(syscon::SysconDevice::new(
                "syscon-reboot",
                reboot,
                syscon::SysconAction::Reboot,
                state,
            )))?);
        }
        Ok(attachments)
    }

    fn ns16550(
        &self,
        window: MmioWindow,
        reg_shift: u32,
        irq: Self::SerialIrq,
        out: Box<dyn std::io::Write + Send>,
    ) -> Result<Self::SerialDevice, RunError> {
        let (irq_manager, gsi) = irq;
        let eventfd = {
            let mut manager = irq_manager.lock().expect("irq mgr poisoned");
            manager
                .register_irqfd_at_gsi(gsi)
                .map_err(|e| RunError::SerialInit {
                    source: anyhow::anyhow!("irqfd for serial GSI {gsi}: {e}"),
                })?
        };
        Ok(Ns16550::new(
            window,
            reg_shift,
            Some(Interrupt::new(Arc::new(
                backend_machine::EventFdInterruptLine::new(eventfd),
            ))),
            out,
        ))
    }

    fn guest_memory(&self) -> Result<GuestMemoryMmap, RunError> {
        Err(RunError::Unimplemented(
            "Linux/KVM guest memory is provided from the launcher memfd map",
        ))
    }

    fn wired_irq(&self, _intid: u32) -> Self::WiredIrq {}
}

#[cfg(target_os = "macos")]
pub(crate) struct MemoryRegion {
    pub(crate) gpa: u64,
    pub(crate) size: u64,
}

#[cfg(target_os = "macos")]
pub(crate) struct VmOptions {
    pub(crate) gic_params: backend_machine::GicParams,
    pub(crate) min_addr_space_bits: u32,
    pub(crate) vcpus: u32,
    pub(crate) memory_regions: Vec<MemoryRegion>,
}

#[cfg(target_os = "macos")]
impl BackendVm for backend_machine::Vm {
    type Options = VmOptions;
    type Vcpu = backend_machine::Vcpu;
    type InterruptState = ();
    type SerialIrq = ();
    type SerialDevice = Ns16550;
    type WiredIrq = dillo_mmio_virtio::WiredIrq;
    type MsiNotifier = hvf_devices::HvfMsixNotifier;

    fn new(opts: Self::Options) -> Result<Self, RunError> {
        let mut vm = backend_machine::Vm::new(&opts.gic_params, opts.min_addr_space_bits)?;
        let max_vcpus = vm.max_vcpus()?;
        if opts.vcpus > max_vcpus {
            return Err(RunError::TooManyVcpus {
                requested: opts.vcpus,
                max: max_vcpus,
            });
        }
        for region in opts.memory_regions {
            log::info!(
                "  memslot [{:#x}..{:#x}) ({} bytes)",
                region.gpa,
                region.gpa + region.size,
                region.size
            );
            vm.add_memory(region.gpa, region.size)?;
        }
        Ok(vm)
    }

    fn guest_memory(&self) -> Result<GuestMemoryMmap, RunError> {
        hvf_devices::build_guest_memory(&self.region_mappings()).map_err(RunError::MemfdSetup)
    }

    fn interrupt_state(&self) -> Result<Self::InterruptState, RunError> {
        Ok(())
    }

    fn msix_notifier(
        &self,
        _interrupt_state: Self::InterruptState,
        count: u16,
    ) -> Arc<Self::MsiNotifier> {
        Arc::new(hvf_devices::HvfMsixNotifier::new(count))
    }

    fn create_vcpu(
        &self,
        _idx: u32,
        _cpu_profile: &str,
        _seed: VcpuSeed<'_>,
    ) -> Result<Self::Vcpu, RunError> {
        Err(RunError::Unimplemented(
            "HVF creates vCPUs on their owning threads",
        ))
    }

    fn current_thread_vcpu(
        seed: VcpuSeed<'_>,
        mmio_bus: Arc<std::sync::Mutex<MmioBus>>,
    ) -> Result<Self::Vcpu, RunError> {
        let vcpu = backend_machine::create_vcpu_current_thread(mmio_bus).map_err(RunError::Kvm)?;
        match seed {
            VcpuSeed::Aarch64 { mpidr, state } => {
                vcpu.set_mpidr(mpidr)?;
                vcpu.set_aarch64_state(state)?;
            }
        }
        Ok(vcpu)
    }

    fn attach_mmio<D>(&mut self, device: Arc<D>) -> Result<Arc<dyn MmioAttachment>, RunError>
    where
        D: MmioDevice + 'static,
    {
        Attach::attach(self, device).map_err(RunError::Kvm)
    }

    fn set_shared_memory_capabilities(&mut self, capabilities: Vec<Arc<dyn SharedMemory>>) {
        backend_machine::Vm::set_shared_memory_capabilities(self, capabilities);
    }

    fn attach_x86_syscon_devices(
        &mut self,
        _poweroff: dillo_platform::Syscon,
        _reboot: Option<dillo_platform::Syscon>,
        _state: Arc<syscon::SysconState>,
    ) -> Result<Vec<Arc<dyn MmioAttachment>>, RunError> {
        Ok(Vec::new())
    }

    fn ns16550(
        &self,
        window: MmioWindow,
        reg_shift: u32,
        _irq: Self::SerialIrq,
        out: Box<dyn std::io::Write + Send>,
    ) -> Result<Self::SerialDevice, RunError> {
        Ok(Ns16550::new(window, reg_shift, None, out))
    }

    fn wired_irq(&self, intid: u32) -> Self::WiredIrq {
        dillo_mmio_virtio::WiredIrq::new(
            intid,
            Arc::new(|intid, level| {
                if let Err(e) = backend_machine::set_spi(intid, level) {
                    log::warn!("virtio-mmio SPI {intid} inject failed: {e}");
                }
            }),
        )
    }
}

#[cfg(target_os = "windows")]
pub(crate) struct VmOptions {
    pub(crate) vcpus: u32,
    pub(crate) guest_memory: GuestMemoryMmap,
}

#[cfg(target_os = "windows")]
impl BackendVm for backend_machine::Vm {
    type Options = VmOptions;
    type Vcpu = backend_machine::Vcpu;
    type InterruptState = ();
    type SerialIrq = (Arc<IoApic>, u32);
    type SerialDevice = Ns16550;
    type WiredIrq = ();
    type MsiNotifier = WhpMsixNotifier;

    fn new(opts: Self::Options) -> Result<Self, RunError> {
        let mut vm = backend_machine::Vm::new_x86_64_with_local_apic_count(opts.vcpus)?;
        vm.set_memory(opts.guest_memory)?;
        for (gpa, host, size) in vm.region_mappings() {
            log::info!(
                "  WHP GPA mapping [{:#x}..{:#x}) -> host {:#x} ({} bytes)",
                gpa,
                gpa + size,
                host,
                size,
            );
        }
        Ok(vm)
    }

    fn interrupt_state(&self) -> Result<Self::InterruptState, RunError> {
        Ok(())
    }

    fn msix_notifier(
        &self,
        _interrupt_state: Self::InterruptState,
        count: u16,
    ) -> Arc<Self::MsiNotifier> {
        Arc::new(WhpMsixNotifier::new(
            self.fixed_interrupt_requester(),
            count,
        ))
    }

    fn create_vcpu(
        &self,
        idx: u32,
        cpu_profile: &str,
        seed: VcpuSeed<'_>,
        pio_read: PioRead,
        pio_write: PioWrite,
    ) -> Result<Self::Vcpu, RunError> {
        let mut vcpu =
            backend_machine::Vm::create_vcpu_with_pio(self, idx, cpu_profile, pio_read, pio_write)
                .map_err(RunError::Kvm)?;
        match seed {
            VcpuSeed::X86_64Boot(state) => vcpu.set_x86_64_state(state)?,
            VcpuSeed::X86_64Secondary => {}
        }
        Ok(vcpu)
    }

    fn current_thread_vcpu(_seed: VcpuSeed<'_>) -> Result<Self::Vcpu, RunError> {
        Err(RunError::Unimplemented(
            "current-thread vCPU factory is HVF-only",
        ))
    }

    fn attach_mmio<D>(&mut self, device: Arc<D>) -> Result<Arc<dyn MmioAttachment>, RunError>
    where
        D: MmioDevice + 'static,
    {
        Attach::attach(self, device).map_err(RunError::Kvm)
    }

    fn set_shared_memory_capabilities(&mut self, capabilities: Vec<Arc<dyn SharedMemory>>) {
        backend_machine::Vm::set_shared_memory_capabilities(self, capabilities);
    }

    fn attach_x86_syscon_devices(
        &mut self,
        poweroff: dillo_platform::Syscon,
        reboot: Option<dillo_platform::Syscon>,
        state: Arc<syscon::SysconState>,
    ) -> Result<Vec<Arc<dyn MmioAttachment>>, RunError> {
        let mut attachments = Vec::new();
        attachments.push(self.attach_mmio(Arc::new(syscon::SysconDevice::new(
            "syscon-poweroff",
            poweroff,
            syscon::SysconAction::Poweroff,
            Arc::clone(&state),
        )))?);
        if let Some(reboot) = reboot {
            attachments.push(self.attach_mmio(Arc::new(syscon::SysconDevice::new(
                "syscon-reboot",
                reboot,
                syscon::SysconAction::Reboot,
                state,
            )))?);
        }
        Ok(attachments)
    }

    fn ns16550(
        &self,
        window: MmioWindow,
        reg_shift: u32,
        irq: Self::SerialIrq,
        out: Box<dyn std::io::Write + Send>,
    ) -> Result<Self::SerialDevice, RunError> {
        let (ioapic, gsi) = irq;
        Ok(Ns16550::new(
            window,
            reg_shift,
            Some(Interrupt::new(Arc::new(
                self.create_ioapic_interrupt_line(ioapic, gsi),
            ))),
            out,
        ))
    }

    fn guest_memory(&self) -> Result<GuestMemoryMmap, RunError> {
        Err(RunError::Unimplemented(
            "WHP guest memory is supplied at construction",
        ))
    }

    fn wired_irq(&self, _intid: u32) -> Self::WiredIrq {}
}
