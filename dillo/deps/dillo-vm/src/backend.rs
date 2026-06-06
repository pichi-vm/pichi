#[cfg(target_os = "windows")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::{Arc, Mutex};

#[cfg(target_os = "linux")]
use virtio_pci::QueueNotifier;
#[cfg(target_os = "macos")]
use vm_memory::GuestMemoryMmap;

#[cfg(target_os = "macos")]
use crate::{RunError, hvf_devices};
#[cfg(target_os = "linux")]
use crate::{RunError, irq::IrqManager, pci_notify::KvmQueueNotifier};
#[cfg(target_os = "windows")]
use crate::{ioapic::IoApic, mmio_bus::MmioWindow, uart, whp_devices::WhpMsixNotifier};

#[cfg(target_os = "linux")]
pub(crate) trait BackendVm {
    fn irq_manager(&self) -> Result<Arc<Mutex<IrqManager>>, RunError>;
    fn queue_notifier(&self) -> Box<dyn QueueNotifier>;
}

#[cfg(target_os = "linux")]
impl BackendVm for dillo_hypervisor::Vm {
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
pub(crate) trait BackendVm {
    fn guest_memory(&self) -> Result<GuestMemoryMmap, RunError>;
}

#[cfg(target_os = "macos")]
impl BackendVm for dillo_hypervisor::Vm {
    fn guest_memory(&self) -> Result<GuestMemoryMmap, RunError> {
        hvf_devices::build_guest_memory(&self.region_mappings()).map_err(RunError::MemfdSetup)
    }
}

#[cfg(target_os = "windows")]
pub(crate) trait BackendVm {
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
