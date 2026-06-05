//! UART emulation, DTB-driven attach.
//!
//! 8250/16550 backed by `vm-superio`'s `Serial`. The driver sees a
//! register file that responds to LCR/DLAB, the MCR loopback probe,
//! IIR/FCR FIFO control, and the LSR TX-ready bit — a hand-rolled
//! stub fails one of those probes and the kernel silently disables
//! ttyS0. Writes to THR go to host stderr. The IRQ trigger is wired
//! to a KVM irqfd so guest writes that arm IER's THRE/RX bits cause
//! the kernel to receive a real ISA IRQ from the in-kernel IOAPIC.
//!
//! Attach is **DTB-driven** per the `#12` device-allocation contract:
//! `init_8250` is called once from `dillo_vm::run` after walking the
//! PMI's DTB for `isa@*/serial@*` nodes. If the DTB declares no
//! serial node, this module stays dormant and the bus dispatcher
//! never claims port 0x3F8 — there's no UART on the bus at all.

use std::io::{self, Write};
use std::sync::{Mutex, OnceLock};

use vm_superio::Serial;
use vm_superio::serial::NoEvents;
#[cfg(target_os = "linux")]
use vmm_sys_util::eventfd::EventFd;
#[cfg(target_os = "windows")]
use {crate::ioapic::IoApic, dillo_hypervisor::InterruptController, std::sync::Arc};

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

#[cfg(target_os = "linux")]
type SerialDev = Serial<IrqfdTrigger, NoEvents, Box<dyn Write + Send>>;
#[cfg(target_os = "windows")]
type SerialDev = Serial<WhpTrigger, NoEvents, Box<dyn Write + Send>>;

/// State for the (at most one) declared 8250. None until `init_8250`
/// fires; set-once thereafter.
#[cfg(any(target_os = "linux", target_os = "windows"))]
struct SerialState {
    base: u16,
    size: u16,
    serial: Mutex<SerialDev>,
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
static SERIAL_8250: OnceLock<SerialState> = OnceLock::new();

/// Attach an 8250 emulator at the given I/O port base, with `irqfd`
/// (already registered with KVM at the right GSI) as the IRQ trigger.
/// Idempotent on duplicate call (returns the original; second call's
/// arguments are dropped — `dillo_vm::run` calls this exactly once).
#[cfg(target_os = "linux")]
pub(crate) fn init_8250(base: u16, irqfd: EventFd) {
    let stderr: Box<dyn Write + Send> = Box::new(io::stderr());
    let serial = Serial::new(IrqfdTrigger::new(irqfd), stderr);
    let _ = SERIAL_8250.set(SerialState {
        base,
        size: 8,
        serial: Mutex::new(serial),
    });
}

/// Attach an 8250 emulator at the given I/O port base and route its
/// DTB-declared ISA IRQ through the userspace IOAPIC model to WHP's
/// fixed-interrupt injection primitive.
#[cfg(target_os = "windows")]
pub(crate) fn init_8250(
    base: u16,
    interrupt_controller: InterruptController,
    ioapic: Arc<IoApic>,
    gsi: u32,
) {
    let stderr: Box<dyn Write + Send> = Box::new(io::stderr());
    let serial = Serial::new(WhpTrigger::new(interrupt_controller, ioapic, gsi), stderr);
    let _ = SERIAL_8250.set(SerialState {
        base,
        size: 8,
        serial: Mutex::new(serial),
    });
}

/// Dispatch a PIO write to the attached 8250. Returns `true` if the
/// port was claimed, `false` otherwise (no UART attached or port
/// outside the claimed range).
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(crate) fn try_pio_write(port: u16, data: &[u8]) -> bool {
    let Some(st) = SERIAL_8250.get() else {
        return false;
    };
    if !(st.base..st.base + st.size).contains(&port) || data.is_empty() {
        return false;
    }
    let offset = (port - st.base) as u8;
    if let Ok(mut s) = st.serial.lock() {
        let _ = s.write(offset, data[0]);
        if offset == 0 {
            // THR write — flush stderr now so host sees the byte as it
            // is produced rather than waiting for buffer growth.
            let _ = s.writer_mut().flush();
        }
    }
    true
}

/// Dispatch a PIO read to the attached 8250. Returns `Some(byte)` if
/// the port was claimed, `None` otherwise.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(crate) fn try_pio_read(port: u16) -> Option<u8> {
    let st = SERIAL_8250.get()?;
    if !(st.base..st.base + st.size).contains(&port) {
        return None;
    }
    let offset = (port - st.base) as u8;
    Some(st.serial.lock().ok()?.read(offset))
}

// ── ns16550a (aarch64/HVF) — MMIO 16550 backed by vm-superio (F3). ─────
// The same register file the x86 8250 path uses, but MMIO-mapped with the
// node's reg-shift (register N at offset `N << reg_shift`). Console output
// (THR) → host stderr. Polled mode: no IRQ trigger — the kernel's serial
// console write path polls LSR-THRE, which vm-superio drives correctly, so
// boot-time console output works without wiring the GIC SPI.

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct NoopTrigger;

#[cfg(target_os = "macos")]
impl vm_superio::Trigger for NoopTrigger {
    type E = io::Error;
    fn trigger(&self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(target_os = "macos")]
type Mmio16550 = Serial<NoopTrigger, NoEvents, Box<dyn Write + Send>>;

#[cfg(target_os = "macos")]
struct Ns16550State {
    reg_shift: u32,
    serial: Mutex<Mmio16550>,
}

#[cfg(target_os = "macos")]
static NS16550: OnceLock<Ns16550State> = OnceLock::new();

/// Attach the MMIO ns16550a at the serial node's reg with its `reg-shift`.
/// Set-once; `dillo_vm::run` calls this exactly once when the Platform
/// declares a UART.
#[cfg(target_os = "macos")]
pub(crate) fn init_ns16550(reg_shift: u32) {
    let out: Box<dyn Write + Send> = Box::new(io::stderr());
    let _ = NS16550.set(Ns16550State {
        reg_shift,
        serial: Mutex::new(Serial::new(NoopTrigger, out)),
    });
}

/// MMIO write to the ns16550a register file (`offset` within the node reg).
#[cfg(target_os = "macos")]
pub(crate) fn ns16550_write(offset: u64, data: &[u8]) -> bool {
    let Some(st) = NS16550.get() else {
        return true;
    };
    if data.is_empty() {
        return true;
    }
    let reg = (offset >> st.reg_shift) as u8;
    if let Ok(mut s) = st.serial.lock() {
        let _ = s.write(reg, data[0]);
        if reg == 0 {
            // THR write — flush so the host sees the byte immediately.
            let _ = s.writer_mut().flush();
        }
    }
    true
}

/// MMIO read from the ns16550a register file.
#[cfg(target_os = "macos")]
pub(crate) fn ns16550_read(offset: u64, data: &mut [u8]) -> bool {
    data.fill(0);
    let Some(st) = NS16550.get() else {
        return true;
    };
    let reg = (offset >> st.reg_shift) as u8;
    if let Ok(mut s) = st.serial.lock()
        && !data.is_empty()
    {
        data[0] = s.read(reg);
    }
    true
}

// ── ns16550a (Linux/KVM) — MMIO 16550 with a KVM irqfd. ───────────────
// The device-model serial is MMIO on Linux too (both arches). Console
// output (printk through the polled console driver) works regardless of
// the IRQ; the irqfd is what lets interrupt-driven ttyS0 RX/TX work.
// Output goes to stdout (the `--console stdio` endpoint).
#[cfg(target_os = "linux")]
type Mmio16550 = Serial<IrqfdTrigger, NoEvents, Box<dyn Write + Send>>;

#[cfg(target_os = "linux")]
struct Ns16550State {
    reg_shift: u32,
    serial: Mutex<Mmio16550>,
}

#[cfg(target_os = "linux")]
static NS16550: OnceLock<Ns16550State> = OnceLock::new();

/// Attach the MMIO ns16550a, wiring its IRQ to a KVM irqfd at the
/// declared GSI. Set-once; `dillo_vm::run` calls this exactly once when
/// the Platform declares a UART.
#[cfg(target_os = "linux")]
pub(crate) fn init_ns16550(reg_shift: u32, irqfd: EventFd) {
    let out: Box<dyn Write + Send> = Box::new(io::stdout());
    let _ = NS16550.set(Ns16550State {
        reg_shift,
        serial: Mutex::new(Serial::new(IrqfdTrigger::new(irqfd), out)),
    });
}

/// MMIO write to the ns16550a register file (`offset` within the node reg).
#[cfg(target_os = "linux")]
pub(crate) fn ns16550_write(offset: u64, data: &[u8]) -> bool {
    let Some(st) = NS16550.get() else {
        return true;
    };
    if data.is_empty() {
        return true;
    }
    let reg = (offset >> st.reg_shift) as u8;
    if let Ok(mut s) = st.serial.lock() {
        let _ = s.write(reg, data[0]);
        if reg == 0 {
            // THR write — flush so the host sees the byte immediately.
            let _ = s.writer_mut().flush();
        }
    }
    true
}

/// MMIO read from the ns16550a register file.
#[cfg(target_os = "linux")]
pub(crate) fn ns16550_read(offset: u64, data: &mut [u8]) -> bool {
    data.fill(0);
    let Some(st) = NS16550.get() else {
        return true;
    };
    let reg = (offset >> st.reg_shift) as u8;
    if let Ok(mut s) = st.serial.lock()
        && !data.is_empty()
    {
        data[0] = s.read(reg);
    }
    true
}
