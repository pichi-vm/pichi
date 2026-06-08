use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::Mutex;

use virtio_pci::QueueNotifier;
use vm_memory::GuestMemoryMmap;

use dillo_mmio::{MmioBus, MmioDevice, MmioWindow};

#[cfg(target_os = "macos")]
use crate::{RunError, hvf_devices, syscon, uart, virtio_mmio};
#[cfg(target_os = "windows")]
use crate::{RunError, ioapic::IoApic, syscon, uart, whp_devices::WhpMsixNotifier};
#[cfg(target_os = "linux")]
use crate::{
    RunError, irq::IrqManager, pci_irq::IrqfdNotifier, pci_notify::KvmQueueNotifier, syscon, uart,
};

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
    type WiredIrq;
    type MsiNotifier: vm_pci::MsixNotifier + 'static;

    fn new(opts: Self::Options) -> Result<Self, RunError>
    where
        Self: Sized;

    fn interrupt_state(&self) -> Result<Self::InterruptState, RunError>;

    fn queue_notifier(&self) -> Box<dyn QueueNotifier>;

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
    ) -> Result<Self::Vcpu, RunError>;

    fn current_thread_vcpu(seed: VcpuSeed<'_>) -> Result<Self::Vcpu, RunError>
    where
        Self: Sized;

    fn attach_mmio<D>(&self, bus: &mut MmioBus, device: Arc<D>)
    where
        D: MmioDevice + 'static;

    fn attach_x86_syscon_devices(
        &self,
        bus: &mut MmioBus,
        poweroff: dillo_platform::Syscon,
        reboot: Option<dillo_platform::Syscon>,
        state: Arc<syscon::SysconState>,
    );

    fn ns16550(
        &self,
        window: MmioWindow,
        reg_shift: u32,
        irq: Self::SerialIrq,
        out: Box<dyn std::io::Write + Send>,
    ) -> Result<uart::Ns16550, RunError>;

    fn guest_memory(&self) -> Result<GuestMemoryMmap, RunError>;

    fn wired_irq(&self, intid: u32) -> Self::WiredIrq;
}

#[allow(dead_code)]
struct NoopQueueNotifier;

impl QueueNotifier for NoopQueueNotifier {
    fn register(
        &mut self,
        _queue_index: usize,
        _addr: u64,
        _kick: &virtio::Kick,
    ) -> Result<(), String> {
        Ok(())
    }

    fn unregister_all(&mut self) {}
}

#[cfg(target_os = "linux")]
impl BackendVm for dillo_hypervisor::Vm {
    type Options = VmOptions;
    type Vcpu = dillo_hypervisor::Vcpu;
    type InterruptState = Arc<Mutex<IrqManager>>;
    type SerialIrq = (Arc<Mutex<IrqManager>>, u32);
    type WiredIrq = ();
    type MsiNotifier = IrqfdNotifier;

    fn new(opts: Self::Options) -> Result<Self, RunError> {
        let vm = dillo_hypervisor::Vm::new()?;
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
        let manager = IrqManager::new(self.vm_fd_arc()).map_err(|e| {
            RunError::Kvm(dillo_hypervisor::Error::RunVcpu(
                0,
                std::io::Error::other(format!("irq manager: {e}")),
            ))
        })?;
        Ok(Arc::new(Mutex::new(manager)))
    }

    fn queue_notifier(&self) -> Box<dyn QueueNotifier> {
        Box::new(KvmQueueNotifier::new(self.vm_fd_arc()))
    }

    fn msix_notifier(
        &self,
        irq_manager: Self::InterruptState,
        count: u16,
    ) -> Arc<Self::MsiNotifier> {
        Arc::new(IrqfdNotifier::new(irq_manager, count))
    }

    fn create_vcpu(
        &self,
        idx: u32,
        cpu_profile: &str,
        seed: VcpuSeed<'_>,
    ) -> Result<Self::Vcpu, RunError> {
        let mut vcpu =
            dillo_hypervisor::Vm::create_vcpu(self, idx, cpu_profile).map_err(RunError::Kvm)?;
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

    fn attach_mmio<D>(&self, bus: &mut MmioBus, device: Arc<D>)
    where
        D: MmioDevice + 'static,
    {
        bus.register_device(device);
    }

    fn attach_x86_syscon_devices(
        &self,
        bus: &mut MmioBus,
        poweroff: dillo_platform::Syscon,
        reboot: Option<dillo_platform::Syscon>,
        state: Arc<syscon::SysconState>,
    ) {
        self.attach_mmio(
            bus,
            Arc::new(syscon::SysconDevice::new(
                "syscon-poweroff",
                poweroff,
                syscon::SysconAction::Poweroff,
                Arc::clone(&state),
            )),
        );
        if let Some(reboot) = reboot {
            self.attach_mmio(
                bus,
                Arc::new(syscon::SysconDevice::new(
                    "syscon-reboot",
                    reboot,
                    syscon::SysconAction::Reboot,
                    state,
                )),
            );
        }
    }

    fn ns16550(
        &self,
        window: MmioWindow,
        reg_shift: u32,
        irq: Self::SerialIrq,
        out: Box<dyn std::io::Write + Send>,
    ) -> Result<uart::Ns16550, RunError> {
        let (irq_manager, gsi) = irq;
        let eventfd = {
            let mut manager = irq_manager.lock().expect("irq mgr poisoned");
            manager
                .register_irqfd_at_gsi(gsi)
                .map_err(|e| RunError::SerialInit {
                    source: anyhow::anyhow!("irqfd for serial GSI {gsi}: {e}"),
                })?
        };
        Ok(uart::Ns16550::new_irqfd(window, reg_shift, eventfd, out))
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
    pub(crate) gic_params: dillo_hypervisor::GicParams,
    pub(crate) min_addr_space_bits: u32,
    pub(crate) vcpus: u32,
    pub(crate) memory_regions: Vec<MemoryRegion>,
}

#[cfg(target_os = "macos")]
impl BackendVm for dillo_hypervisor::Vm {
    type Options = VmOptions;
    type Vcpu = dillo_hypervisor::Vcpu;
    type InterruptState = ();
    type SerialIrq = ();
    type WiredIrq = virtio_mmio::WiredIrq;
    type MsiNotifier = hvf_devices::HvfMsixNotifier;

    fn new(opts: Self::Options) -> Result<Self, RunError> {
        let mut vm = dillo_hypervisor::Vm::new(&opts.gic_params, opts.min_addr_space_bits)?;
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

    fn queue_notifier(&self) -> Box<dyn QueueNotifier> {
        Box::new(NoopQueueNotifier)
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

    fn current_thread_vcpu(seed: VcpuSeed<'_>) -> Result<Self::Vcpu, RunError> {
        let vcpu = dillo_hypervisor::create_vcpu_current_thread().map_err(RunError::Kvm)?;
        match seed {
            VcpuSeed::Aarch64 { mpidr, state } => {
                vcpu.set_mpidr(mpidr)?;
                vcpu.set_aarch64_state(state)?;
            }
        }
        Ok(vcpu)
    }

    fn attach_mmio<D>(&self, bus: &mut MmioBus, device: Arc<D>)
    where
        D: MmioDevice + 'static,
    {
        bus.register_device(device);
    }

    fn attach_x86_syscon_devices(
        &self,
        _bus: &mut MmioBus,
        _poweroff: dillo_platform::Syscon,
        _reboot: Option<dillo_platform::Syscon>,
        _state: Arc<syscon::SysconState>,
    ) {
    }

    fn ns16550(
        &self,
        window: MmioWindow,
        reg_shift: u32,
        _irq: Self::SerialIrq,
        out: Box<dyn std::io::Write + Send>,
    ) -> Result<uart::Ns16550, RunError> {
        Ok(uart::Ns16550::new_polled(window, reg_shift, out))
    }

    fn wired_irq(&self, intid: u32) -> Self::WiredIrq {
        virtio_mmio::WiredIrq::new(
            intid,
            Arc::new(|intid, level| {
                if let Err(e) = dillo_hypervisor::set_spi(intid, level) {
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
impl BackendVm for dillo_hypervisor::Vm {
    type Options = VmOptions;
    type Vcpu = dillo_hypervisor::Vcpu;
    type InterruptState = ();
    type SerialIrq = (Arc<IoApic>, u32);
    type WiredIrq = ();
    type MsiNotifier = WhpMsixNotifier;

    fn new(opts: Self::Options) -> Result<Self, RunError> {
        let mut vm = dillo_hypervisor::Vm::new_x86_64_with_local_apic_count(opts.vcpus)?;
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

    fn queue_notifier(&self) -> Box<dyn QueueNotifier> {
        Box::new(NoopQueueNotifier)
    }

    fn msix_notifier(
        &self,
        _interrupt_state: Self::InterruptState,
        count: u16,
    ) -> Arc<Self::MsiNotifier> {
        Arc::new(WhpMsixNotifier::new(self.interrupt_controller(), count))
    }

    fn create_vcpu(
        &self,
        idx: u32,
        cpu_profile: &str,
        seed: VcpuSeed<'_>,
    ) -> Result<Self::Vcpu, RunError> {
        let mut vcpu =
            dillo_hypervisor::Vm::create_vcpu(self, idx, cpu_profile).map_err(RunError::Kvm)?;
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

    fn attach_mmio<D>(&self, bus: &mut MmioBus, device: Arc<D>)
    where
        D: MmioDevice + 'static,
    {
        bus.register_device(device);
    }

    fn attach_x86_syscon_devices(
        &self,
        bus: &mut MmioBus,
        poweroff: dillo_platform::Syscon,
        reboot: Option<dillo_platform::Syscon>,
        state: Arc<syscon::SysconState>,
    ) {
        self.attach_mmio(
            bus,
            Arc::new(syscon::SysconDevice::new(
                "syscon-poweroff",
                poweroff,
                syscon::SysconAction::Poweroff,
                Arc::clone(&state),
            )),
        );
        if let Some(reboot) = reboot {
            self.attach_mmio(
                bus,
                Arc::new(syscon::SysconDevice::new(
                    "syscon-reboot",
                    reboot,
                    syscon::SysconAction::Reboot,
                    state,
                )),
            );
        }
    }

    fn ns16550(
        &self,
        window: MmioWindow,
        reg_shift: u32,
        irq: Self::SerialIrq,
        out: Box<dyn std::io::Write + Send>,
    ) -> Result<uart::Ns16550, RunError> {
        let (ioapic, gsi) = irq;
        Ok(uart::Ns16550::new_whp(
            window,
            reg_shift,
            self.interrupt_controller(),
            ioapic,
            gsi,
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
