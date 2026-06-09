use std::sync::Arc;

#[derive(Debug)]
pub(crate) struct KvmMsixNotifier {
    inner: Arc<dillo_machine_kvm::IrqfdNotifier>,
}

impl KvmMsixNotifier {
    pub(crate) fn new(inner: Arc<dillo_machine_kvm::IrqfdNotifier>) -> Self {
        Self { inner }
    }

    pub(crate) fn interrupt_for_vector(&self, vector: u16) -> Option<dillo_mmio::Interrupt> {
        self.inner.interrupt_for_vector(vector)
    }
}

impl dillo_pci::MsixNotifier for KvmMsixNotifier {
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
