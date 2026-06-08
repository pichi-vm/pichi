use std::sync::Arc;

use dillo_mmio::{MmioNotifyEvent, QueueNotifier};
use kvm_ioctls::{IoEventAddress, NoDatamatch, VmFd};
use vmm_sys_util::eventfd::EventFd;

pub(crate) struct KvmQueueNotifier {
    vm_fd: Arc<VmFd>,
    registered: Vec<(usize, u64, EventFd)>,
}

impl KvmQueueNotifier {
    pub(crate) fn new(vm_fd: Arc<VmFd>) -> Self {
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
            if let Err(e) =
                self.vm_fd
                    .unregister_ioevent(&eventfd, &IoEventAddress::Mmio(addr), NoDatamatch)
            {
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
