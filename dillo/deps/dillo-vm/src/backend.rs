#[cfg(target_os = "linux")]
use std::sync::{Arc, Mutex};

#[cfg(target_os = "linux")]
use virtio_pci::QueueNotifier;

#[cfg(target_os = "linux")]
use crate::{RunError, irq::IrqManager, pci_notify::KvmQueueNotifier};

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
