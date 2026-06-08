use std::io;
use std::sync::Arc;

use dillo_machine_backend::InterruptController;
use dillo_mmio_uart::UartTrigger;
use dillo_x86::IoApic;

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
        let Some(route) = self.ioapic.route(self.gsi) else {
            return Ok(());
        };
        self.interrupt_controller
            .request_fixed_interrupt(route.destination, route.vector)
            .map_err(|e| io::Error::other(e.to_string()))
    }
}
