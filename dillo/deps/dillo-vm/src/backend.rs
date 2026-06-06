#[cfg(target_os = "windows")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::{Arc, Mutex};

#[cfg(target_os = "linux")]
use virtio_pci::QueueNotifier;
#[cfg(target_os = "macos")]
use vm_memory::GuestMemoryMmap;
#[cfg(target_os = "windows")]
use vm_memory::GuestMemoryMmap;

#[cfg(target_os = "macos")]
use crate::{RunError, hvf_devices};
#[cfg(target_os = "windows")]
use crate::{RunError, ioapic::IoApic, mmio_bus::MmioWindow, uart, whp_devices::WhpMsixNotifier};
#[cfg(target_os = "linux")]
use crate::{RunError, irq::IrqManager, pci_notify::KvmQueueNotifier};

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

#[cfg(target_os = "linux")]
pub(crate) trait BackendVm {
    fn new(opts: VmOptions) -> Result<Self, RunError>
    where
        Self: Sized;

    fn irq_manager(&self) -> Result<Arc<Mutex<IrqManager>>, RunError>;
    fn queue_notifier(&self) -> Box<dyn QueueNotifier>;
}

#[cfg(target_os = "linux")]
impl BackendVm for dillo_hypervisor::Vm {
    fn new(opts: VmOptions) -> Result<Self, RunError> {
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

    fn irq_manager(&self) -> Result<Arc<Mutex<IrqManager>>, RunError> {
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
pub(crate) trait BackendVm {
    fn new(opts: VmOptions) -> Result<Self, RunError>
    where
        Self: Sized;

    fn guest_memory(&self) -> Result<GuestMemoryMmap, RunError>;

    fn current_thread_vcpu() -> Result<dillo_hypervisor::Vcpu, RunError>;
}

#[cfg(target_os = "macos")]
impl BackendVm for dillo_hypervisor::Vm {
    fn new(opts: VmOptions) -> Result<Self, RunError> {
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

    fn current_thread_vcpu() -> Result<dillo_hypervisor::Vcpu, RunError> {
        dillo_hypervisor::create_vcpu_current_thread().map_err(RunError::Kvm)
    }
}

#[cfg(target_os = "windows")]
pub(crate) struct VmOptions {
    pub(crate) vcpus: u32,
    pub(crate) guest_memory: GuestMemoryMmap,
}

#[cfg(target_os = "windows")]
pub(crate) trait BackendVm {
    fn new(opts: VmOptions) -> Result<Self, RunError>
    where
        Self: Sized;

    fn log_guest_memory_mappings(&self);

    fn msix_notifier(&self, count: u16) -> Arc<WhpMsixNotifier>;

    fn ns16550(
        &self,
        window: MmioWindow,
        reg_shift: u32,
        ioapic: Arc<IoApic>,
        gsi: u32,
        out: Box<dyn std::io::Write + Send>,
    ) -> uart::Ns16550;
}

#[cfg(target_os = "windows")]
impl BackendVm for dillo_hypervisor::Vm {
    fn new(opts: VmOptions) -> Result<Self, RunError> {
        let mut vm = dillo_hypervisor::Vm::new_x86_64_with_local_apic_count(opts.vcpus)?;
        vm.set_memory(opts.guest_memory)?;
        vm.log_guest_memory_mappings();
        Ok(vm)
    }

    fn log_guest_memory_mappings(&self) {
        for (gpa, host, size) in self.region_mappings() {
            log::info!(
                "  WHP GPA mapping [{:#x}..{:#x}) -> host {:#x} ({} bytes)",
                gpa,
                gpa + size,
                host,
                size,
            );
        }
    }

    fn msix_notifier(&self, count: u16) -> Arc<WhpMsixNotifier> {
        Arc::new(WhpMsixNotifier::new(self.interrupt_controller(), count))
    }

    fn ns16550(
        &self,
        window: MmioWindow,
        reg_shift: u32,
        ioapic: Arc<IoApic>,
        gsi: u32,
        out: Box<dyn std::io::Write + Send>,
    ) -> uart::Ns16550 {
        uart::Ns16550::new_whp(
            window,
            reg_shift,
            self.interrupt_controller(),
            ioapic,
            gsi,
            out,
        )
    }
}
