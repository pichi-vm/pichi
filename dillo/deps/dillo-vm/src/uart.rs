//! ns16550a UART emulation (MMIO), `Platform`-driven attach.
//!
//! A 16550 backed by `vm-superio`'s `Serial`. The driver sees a register
//! file that responds to LCR/DLAB, the MCR loopback probe, IIR/FCR FIFO
//! control, and the LSR TX-ready bit — a hand-rolled stub fails one of
//! those probes and the kernel silently disables the console. Writes to
//! THR go to the host (stdout on Linux, stderr elsewhere).
//!
//! Per the arma device model (see `arma/docs/device-model.md` §"Serial
//! port") the serial port is an **MMIO `ns16550a`** on every arch — there
//! is no legacy x86 `0x3f8` port-I/O UART. When the PMI Platform declares a
//! UART, dillo constructs an owned [`Ns16550`] with the node's register
//! window and attaches it to the MMIO bus (register `N` at offset
//! `N << reg_shift`). The IRQ is delivered per host: a KVM irqfd on Linux,
//! WHP's fixed-interrupt injection through the userspace IOAPIC on Windows,
//! and polled (no IRQ) on macOS/HVF.
//!
//! ## THR-empty interrupt on enable
//!
//! A real 16550 asserts the THR-empty (THRE) interrupt *whenever* that
//! interrupt is enabled in IER while the transmit holding register is empty
//! — not only when the driver writes a byte. `vm-superio` raises THRE only
//! from the THR-write path (it never re-asserts on the IER-enable edge), so
//! interrupt-driven `ttyS0` TX never receives its kick: once the polled
//! earlycon is disabled the guest blocks forever in `serial8250_start_tx`,
//! waiting for a THRE interrupt that never comes. We layer the missing
//! behaviour on top of `vm-superio` ([`Ns16550`]): the THR is always empty
//! for this virtual device, so whenever the driver enables THRI we pulse the
//! interrupt, and we surface THRE in IIR while THRI stays enabled and
//! `vm-superio` has nothing else pending. This is what makes the serial a
//! fully usable console rather than an early-boot-only channel.

use std::io::{self, Write};
use std::sync::Mutex;

use vm_superio::Serial;
use vm_superio::Trigger;
use vm_superio::serial::NoEvents;
#[cfg(target_os = "linux")]
use vmm_sys_util::eventfd::EventFd;
#[cfg(target_os = "windows")]
use {crate::ioapic::IoApic, dillo_hypervisor::InterruptController, std::sync::Arc};

use crate::mmio_bus::{MmioDevice, MmioWindow};

// 16550 register offsets and bits we post-process on top of vm-superio.
// Offsets are pre-`reg_shift` register indices (0..=7).
/// Transmit holding register (write, DLAB=0).
const REG_THR: u8 = 0;
/// Interrupt enable register (DLAB=0).
const REG_IER: u8 = 1;
/// Interrupt identification register (read).
const REG_IIR: u8 = 2;
/// Line control register (holds the DLAB bit).
const REG_LCR: u8 = 3;
/// IER bit: enable the THR-empty interrupt.
const IER_THRE: u8 = 0b0000_0010;
/// IIR bit0: set when *no* interrupt is pending.
const IIR_NO_INT: u8 = 0b0000_0001;
/// IIR id: THR-empty interrupt pending.
const IIR_THRE: u8 = 0b0000_0010;
/// IIR high bits reported when the FIFO (16550A) is enabled.
const IIR_FIFO: u8 = 0b1100_0000;
/// LCR bit: divisor-latch access (remaps offsets 0/1 to the baud divisor).
const LCR_DLAB: u8 = 0b1000_0000;

/// `vm-superio` Trigger that fires a KVM irqfd. Cloned EventFd; writes
/// of 1 cause KVM's in-kernel IOAPIC to inject the configured ISA IRQ.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub(crate) struct IrqfdTrigger(EventFd);

#[cfg(target_os = "linux")]
impl IrqfdTrigger {
    pub(crate) fn new(efd: EventFd) -> Self {
        Self(efd)
    }
}

#[cfg(target_os = "linux")]
impl vm_superio::Trigger for IrqfdTrigger {
    type E = io::Error;
    fn trigger(&self) -> io::Result<()> {
        self.0.write(1)
    }
}

#[cfg(target_os = "windows")]
#[derive(Debug)]
pub(crate) struct WhpTrigger {
    interrupt_controller: InterruptController,
    ioapic: Arc<IoApic>,
    gsi: u32,
}

#[cfg(target_os = "windows")]
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

#[cfg(target_os = "windows")]
impl vm_superio::Trigger for WhpTrigger {
    type E = io::Error;

    fn trigger(&self) -> Result<(), Self::E> {
        self.ioapic
            .inject_gsi(&self.interrupt_controller, self.gsi)
            .map_err(|e| io::Error::other(e.to_string()))
    }
}

// On macOS/HVF the serial is polled: the kernel's console write path polls
// LSR-THRE (which vm-superio drives correctly), so boot-time output works
// without wiring the GIC SPI. The trigger is a no-op.
#[cfg(target_os = "macos")]
#[derive(Debug)]
pub(crate) struct NoopTrigger;

#[cfg(target_os = "macos")]
impl vm_superio::Trigger for NoopTrigger {
    type E = io::Error;
    fn trigger(&self) -> io::Result<()> {
        Ok(())
    }
}

// The concrete trigger differs per host; everything else about the device is
// identical, so the register logic below is written once over `Trig`.
#[cfg(target_os = "linux")]
type Trig = IrqfdTrigger;
#[cfg(target_os = "macos")]
type Trig = NoopTrigger;
#[cfg(target_os = "windows")]
type Trig = WhpTrigger;

type Mmio16550 = Serial<Trig, NoEvents, Box<dyn Write + Send>>;

/// MMIO ns16550a: a `vm-superio` `Serial` plus the THR-empty-on-enable
/// emulation it lacks (see module docs).
pub(crate) struct Ns16550 {
    window: MmioWindow,
    state: Mutex<Ns16550State>,
}

impl Ns16550 {
    fn with_serial(window: MmioWindow, reg_shift: u32, serial: Mmio16550) -> Self {
        Self {
            window,
            state: Mutex::new(Ns16550State::new(reg_shift, serial)),
        }
    }

    /// Build the MMIO ns16550a, wiring its IRQ to a KVM irqfd at the declared
    /// GSI. Console output (THR) uses the supplied host sink.
    #[cfg(target_os = "linux")]
    pub(crate) fn new_irqfd(
        window: MmioWindow,
        reg_shift: u32,
        irqfd: EventFd,
        out: Box<dyn Write + Send>,
    ) -> Self {
        Self::with_serial(
            window,
            reg_shift,
            Serial::new(IrqfdTrigger::new(irqfd), out),
        )
    }

    /// Build the MMIO ns16550a in polled mode. Console output (THR) uses the
    /// supplied host sink.
    #[cfg(target_os = "macos")]
    pub(crate) fn new_polled(
        window: MmioWindow,
        reg_shift: u32,
        out: Box<dyn Write + Send>,
    ) -> Self {
        Self::with_serial(window, reg_shift, Serial::new(NoopTrigger, out))
    }

    /// Build the MMIO ns16550a, routing its IRQ through the userspace IOAPIC to
    /// WHP's fixed-interrupt injection at the declared GSI. Console output
    /// (THR) uses the supplied host sink.
    #[cfg(target_os = "windows")]
    pub(crate) fn new_whp(
        window: MmioWindow,
        reg_shift: u32,
        interrupt_controller: InterruptController,
        ioapic: Arc<IoApic>,
        gsi: u32,
        out: Box<dyn Write + Send>,
    ) -> Self {
        Self::with_serial(
            window,
            reg_shift,
            Serial::new(WhpTrigger::new(interrupt_controller, ioapic, gsi), out),
        )
    }
}

impl MmioDevice for Ns16550 {
    fn window(&self) -> MmioWindow {
        self.window
    }

    fn read(&self, offset: u64, data: &mut [u8]) -> bool {
        data.fill(0);
        if let Some(slot) = data.first_mut()
            && let Ok(mut state) = self.state.lock()
        {
            *slot = state.read(offset);
        }
        true
    }

    fn write(&self, offset: u64, data: &[u8]) -> bool {
        if let Ok(mut state) = self.state.lock() {
            state.write(offset, data);
        }
        true
    }
}

struct Ns16550State {
    reg_shift: u32,
    serial: Mmio16550,
    /// Mirror of IER.THRE (DLAB-aware). When set, the THR-empty interrupt is
    /// enabled, so — the THR being permanently empty for this virtual device
    /// — we treat THRE as continuously assertable.
    thri_enabled: bool,
}

impl Ns16550State {
    fn new(reg_shift: u32, serial: Mmio16550) -> Self {
        Self {
            reg_shift,
            serial,
            thri_enabled: false,
        }
    }

    /// True when the divisor-latch is mapped over offsets 0/1.
    fn dlab(&mut self) -> bool {
        self.serial.read(REG_LCR) & LCR_DLAB != 0
    }

    /// MMIO write to the register file (`offset` within the node reg window).
    fn write(&mut self, offset: u64, data: &[u8]) {
        let Some(&byte) = data.first() else {
            return;
        };
        let reg = (offset >> self.reg_shift) as u8;
        let dlab = self.dlab();
        let _ = self.serial.write(reg, byte);
        match reg {
            // THR write — flush so the host sees the byte immediately.
            // vm-superio already asserted THRE for this write.
            REG_THR if !dlab => {
                let _ = self.serial.writer_mut().flush();
            }
            // IER write — track THRI and emulate the 16550's assert-on-enable:
            // a real UART raises THRE the instant THRI is enabled while THR is
            // empty. Pulse the interrupt on the disabled→enabled edge.
            REG_IER if !dlab => {
                let now = byte & IER_THRE != 0;
                if now && !self.thri_enabled {
                    let _ = self.serial.interrupt_evt().trigger();
                }
                self.thri_enabled = now;
            }
            _ => {}
        }
    }

    /// MMIO read from the register file.
    fn read(&mut self, offset: u64) -> u8 {
        let reg = (offset >> self.reg_shift) as u8;
        let value = self.serial.read(reg);
        // If THRI is enabled and vm-superio has nothing else pending, surface
        // THRE: the THR is always empty, so the interrupt is level-asserted.
        if reg == REG_IIR && value & IIR_NO_INT != 0 && self.thri_enabled {
            return IIR_THRE | IIR_FIFO;
        }
        value
    }
}

// The THR-empty-on-enable emulation is identical across backends; we exercise
// it on Linux, where the trigger is a real `EventFd` we can observe directly.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use vmm_sys_util::eventfd::EventFd;

    /// Build an `Ns16550` whose trigger writes an observable `EventFd`.
    fn harness() -> (Ns16550State, EventFd) {
        let efd = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        let observe = efd.try_clone().unwrap();
        let out: Box<dyn Write + Send> = Box::new(io::sink());
        let serial = Serial::new(IrqfdTrigger::new(efd), out);
        (Ns16550State::new(0, serial), observe)
    }

    /// Enabling THRI while THR is empty must immediately raise the interrupt
    /// (the behaviour vm-superio omits) and IIR must then report THRE — the
    /// 16550 contract `serial8250_start_tx` relies on for interrupt-driven TX.
    #[test]
    fn thri_enable_raises_thre_interrupt() {
        let (mut uart, efd) = harness();

        // Nothing pending before THRI is enabled.
        assert!(efd.read().is_err(), "spurious interrupt before enable");
        assert_eq!(uart.read(u64::from(REG_IIR)) & IIR_NO_INT, IIR_NO_INT);

        // Enable the THR-empty interrupt.
        uart.write(u64::from(REG_IER), &[IER_THRE]);

        // The enable edge must have fired the trigger exactly once...
        assert_eq!(efd.read().unwrap(), 1, "THRI enable did not fire the IRQ");
        // ...and IIR must identify it as the THR-empty interrupt.
        assert_eq!(uart.read(u64::from(REG_IIR)), IIR_THRE | IIR_FIFO);
    }

    /// THRE stays asserted while THRI is enabled (level-triggered): every IIR
    /// read with nothing else pending reports it, so a guest that re-checks
    /// after draining its buffer keeps making progress.
    #[test]
    fn thre_is_level_asserted_while_enabled() {
        let (mut uart, _efd) = harness();
        uart.write(u64::from(REG_IER), &[IER_THRE]);
        assert_eq!(uart.read(u64::from(REG_IIR)), IIR_THRE | IIR_FIFO);
        assert_eq!(uart.read(u64::from(REG_IIR)), IIR_THRE | IIR_FIFO);
    }

    /// Re-asserting an already-enabled THRI must not fire a fresh interrupt
    /// (edge-detected), and disabling it stops THRE being reported.
    #[test]
    fn thri_enable_is_edge_detected_and_clears() {
        let (mut uart, efd) = harness();

        uart.write(u64::from(REG_IER), &[IER_THRE]);
        assert_eq!(efd.read().unwrap(), 1);

        // Writing the same enabled value again is not a fresh edge.
        uart.write(u64::from(REG_IER), &[IER_THRE]);
        assert!(efd.read().is_err(), "non-edge write fired a spurious IRQ");

        // Disabling THRI stops THRE from being surfaced.
        uart.write(u64::from(REG_IER), &[0]);
        assert_eq!(uart.read(u64::from(REG_IIR)) & IIR_NO_INT, IIR_NO_INT);
    }

    /// While DLAB is set, offset 1 is the divisor-latch high byte, not IER —
    /// it must never be mistaken for a THRI enable.
    #[test]
    fn dlab_high_byte_is_not_an_ier_write() {
        let (mut uart, efd) = harness();

        uart.write(u64::from(REG_LCR), &[LCR_DLAB]); // set DLAB
        uart.write(u64::from(REG_IER), &[IER_THRE]); // divisor high, not IER

        assert!(efd.read().is_err(), "divisor write fired the THRE IRQ");
    }
}
