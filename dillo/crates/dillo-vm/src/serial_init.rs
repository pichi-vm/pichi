// SPDX-License-Identifier: Apache-2.0

//! DTB-driven 8250 attach.
//!
//! Walks the PMI's base DTB for `isa@*/serial@*` declarations and
//! attaches the matching emulator in [`crate::uart`]. On Linux/KVM it
//! also wires a KVM irqfd at the declared GSI. On Windows/WHP, the UART
//! trigger raises the DTB-declared GSI through Dillo's userspace IOAPIC
//! and WHP's fixed-interrupt API.
//! Per the `#12` device-allocation
//! contract this is the single place that decides whether dillo
//! emulates an 8250 at all — no `--serial` flag, no implicit attach,
//! no hardcoded 0x3F8.
//!
//! Today we recognize exactly the shape arma emits with `--serial`:
//!
//! ```dts
//! isa@0 {
//!     compatible = "isa";
//!     serial@3f8 {
//!         compatible = "ns16550a";
//!         reg = <1 0x3f8 0x8>;
//!         interrupts = <4>;
//!     };
//! };
//! ```
//!
//! Strict-reject: a child of `isa@*` with a non-`ns16550a` compatible
//! errors fail-loud. At most one serial port is supported in this
//! commit; >1 errors with a pointer to the limitation. The aarch64
//! MMIO ns16550a attach lives in `uart.rs`.

#[cfg(target_os = "linux")]
use crate::irq::{IrqError, IrqManager};
use crate::uart;
use devtree::{NodeView, PropertyView, Tree, TreeView};
use thiserror::Error;
#[cfg(target_os = "windows")]
use {crate::ioapic::IoApic, dillo_hypervisor::InterruptController, std::sync::Arc};

#[derive(Debug, Error)]
pub(crate) enum Error {
    #[error("failed to parse base DTB for serial-init walk: {0:?}")]
    DtbParse(devtree::Error),
    #[error("`{node}` declares unsupported child `compatible`; expected `ns16550a`")]
    UnsupportedSerial { node: String },
    #[error(
        "`{node}` is missing required property `{property}`; \
         dillo emulates only the DTB-declared port"
    )]
    MissingProperty {
        node: String,
        property: &'static str,
    },
    #[error("`{node}.{property}` has wrong encoding ({reason})")]
    MalformedProperty {
        node: String,
        property: &'static str,
        reason: &'static str,
    },
    #[error(
        "DTB declares {count} serial ports; dillo supports at most one. \
         Multi-port serial pending a real use case."
    )]
    TooManyPorts { count: usize },
    #[error(
        "DTB declares serial at I/O port {port:#x} IRQ {irq} — outside the \
         GSI 0-15 IOAPIC ISA range dillo wires today."
    )]
    IrqOutOfRange { port: u16, irq: u32 },
    #[cfg(target_os = "linux")]
    #[error("KVM irqfd setup for GSI {gsi} failed: {source}")]
    Irqfd {
        gsi: u32,
        #[source]
        source: IrqError,
    },
}

/// Walk `dtb`, attach (at most) one serial port to [`crate::uart`].
/// Returns `Ok(())` for zero or one matching serial node; errors
/// otherwise.
#[cfg(target_os = "linux")]
pub(crate) fn init_from_dtb(dtb: &[u8], irq_manager: &mut IrqManager) -> Result<(), Error> {
    match serial_nodes(dtb)?.as_slice() {
        [] => Ok(()),
        &[(port, _len, irq)] => {
            if irq > 15 {
                return Err(Error::IrqOutOfRange { port, irq });
            }
            let gsi = irq;
            let eventfd = irq_manager
                .register_irqfd_at_gsi(gsi)
                .map_err(|e| Error::Irqfd { gsi, source: e })?;
            uart::init_8250(port, eventfd);
            log::info!("serial: attached ns16550a at I/O {port:#x} IRQ {irq} (GSI {gsi})");
            Ok(())
        }
        many => Err(Error::TooManyPorts { count: many.len() }),
    }
}

/// Windows/WHP attach: discover exactly the same DTB-declared 8250,
/// expose its PIO registers, and route its IRQ through the userspace IOAPIC.
#[cfg(target_os = "windows")]
pub(crate) fn init_from_dtb(
    dtb: &[u8],
    interrupt_controller: InterruptController,
    ioapic: Arc<IoApic>,
) -> Result<(), Error> {
    match serial_nodes(dtb)?.as_slice() {
        [] => Ok(()),
        &[(port, _len, irq)] => {
            if irq > 15 {
                return Err(Error::IrqOutOfRange { port, irq });
            }
            uart::init_8250(port, interrupt_controller, ioapic, irq);
            log::info!("serial: attached ns16550a at I/O {port:#x} IRQ {irq} (WHP IOAPIC)");
            Ok(())
        }
        many => Err(Error::TooManyPorts { count: many.len() }),
    }
}

fn serial_nodes(dtb: &[u8]) -> Result<Vec<(u16, u8, u32)>, Error> {
    let tree: Tree<'_> = Tree::parse(dtb).map_err(Error::DtbParse)?;
    let root = tree.root();

    let mut nodes: Vec<(u16, u8, u32)> = Vec::new();
    for child in root.children() {
        if !node_has_compatible(&child, "isa") {
            continue;
        }
        for sub in child.children() {
            if !node_has_compatible(&sub, "ns16550a") {
                return Err(Error::UnsupportedSerial {
                    node: sub.name().to_string(),
                });
            }
            nodes.push(decode_serial(&sub)?);
        }
    }
    Ok(nodes)
}

fn node_has_compatible<N: NodeView>(node: &N, want: &str) -> bool {
    let Some(prop) = node.property("compatible") else {
        return false;
    };
    let Some(mut strs) = prop.as_strs() else {
        return false;
    };
    strs.any(|s| s == want)
}

/// Decode `(port, length, irq)` from a ns16550a serial node. `reg` is
/// `<space addr size>` per the ISA bus binding (space = 1 = I/O);
/// `interrupts` is a single u32 (LAPIC binding pins #interrupt-cells = 1).
fn decode_serial<N: NodeView>(node: &N) -> Result<(u16, u8, u32), Error> {
    let name = node.name().to_string();
    let reg = node.property("reg").ok_or_else(|| Error::MissingProperty {
        node: name.clone(),
        property: "reg",
    })?;
    let mut cells = reg.as_u32s().ok_or_else(|| Error::MalformedProperty {
        node: name.clone(),
        property: "reg",
        reason: "expected multiple of 4 bytes",
    })?;
    let space = next_cell(&mut cells, &name, "reg")?;
    let addr = next_cell(&mut cells, &name, "reg")?;
    let size = next_cell(&mut cells, &name, "reg")?;
    if cells.next().is_some() {
        return Err(Error::MalformedProperty {
            node: name,
            property: "reg",
            reason: "more than 3 cells; only one fixed I/O range supported",
        });
    }
    if space != 1 {
        return Err(Error::MalformedProperty {
            node: name,
            property: "reg",
            reason: "ISA space code must be 1 (I/O); memory-space ns16550a not supported",
        });
    }
    let port = u16::try_from(addr).map_err(|_| Error::MalformedProperty {
        node: name.clone(),
        property: "reg",
        reason: "I/O port > 0xFFFF",
    })?;
    let len = u8::try_from(size).map_err(|_| Error::MalformedProperty {
        node: name.clone(),
        property: "reg",
        reason: "size > 0xFF",
    })?;
    let irq_prop = node
        .property("interrupts")
        .ok_or_else(|| Error::MissingProperty {
            node: name.clone(),
            property: "interrupts",
        })?;
    let irq = irq_prop.as_u32().ok_or_else(|| Error::MalformedProperty {
        node: name,
        property: "interrupts",
        reason: "expected single u32 (LAPIC #interrupt-cells = 1)",
    })?;
    Ok((port, len, irq))
}

fn next_cell(
    iter: &mut impl Iterator<Item = u32>,
    node: &str,
    property: &'static str,
) -> Result<u32, Error> {
    iter.next().ok_or_else(|| Error::MalformedProperty {
        node: node.to_string(),
        property,
        reason: "expected 3 cells (space, addr, size)",
    })
}
