use std::io;
use std::sync::Arc;

use dillo_hypervisor::InterruptController;
use dillo_mmio_uart::UartTrigger;

use crate::ioapic::IoApic;

#[derive(Debug)]
pub(crate) struct WhpTrigger {
    interrupt_controller: InterruptController,
    ioapic: Arc<IoApic>,
    gsi: u32,
}

impl WhpTrigger {
    pub(crate) fn new(
        interrupt_controller: InterruptController,
        ioapic: Arc<IoApic>,
        gsi: u32,
    ) -> Self {
        Self {
            interrupt_controller,
            ioapic,
            gsi,
        }
    }
}

impl UartTrigger for WhpTrigger {
    type E = io::Error;

    fn trigger(&self) -> Result<(), Self::E> {
        self.ioapic
            .inject_gsi(&self.interrupt_controller, self.gsi)
            .map_err(|e| io::Error::other(e.to_string()))
    }
}
