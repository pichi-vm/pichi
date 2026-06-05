//! Serial Port Console Redirection table.
//!
//! Microsoft SPCR, revision 2 (80 bytes): SDT header + a 16550
//! description — interface type, a [`GenericAddress`] for the register
//! block, and the interrupt routing. The HW-Reduced no-legacy platform
//! exposes its UART as an MMIO `ns16550a` (device-model §4, serial
//! port), so there is no ISA `Device(COMn)` AML and no I/O-port `_CRS`;
//! SPCR is the table that points firmware/OS console redirection at the
//! MMIO 16550.
//!
//! Per `dtb2acpi/lib.rs`'s "DT binding scope" policy this consumer
//! restricts the serial node's property set to the ones with an ACPI
//! representation and rejects any other — silently dropping
//! `clock-frequency` etc. would couple dtb2acpi to a driver default.

use devtree::{NodeView, PropertyView, TreeView};
use zerocopy::little_endian::{U16, U32};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::sdt::{GenericAddress, SYSTEM_MEMORY, SdtHeader};
use super::set_sdt_checksum;
use crate::count::base_name_is;
use crate::dtb::DtbNode;
use crate::error::{DtbError, Site};
use crate::oem::OemIdentity;

/// SPCR revision per the Microsoft SPCR spec (the 80-byte form).
pub(crate) const REVISION: u8 = 2;

/// `interface_type` — full 16550 compatible (SPCR Table "Interface
/// Type", value 0). The MMIO UART arma emits is register-compatible
/// with the original 16550.
const INTERFACE_TYPE_FULL_16550: u8 = 0x00;

/// `interrupt_type` bit 3 — interrupt routed through an I/O-APIC.
/// x86 wires the UART line to the IO-APIC (device-model §4), so the
/// global system interrupt in `gsi` is authoritative and the legacy
/// PC-AT `irq` byte is left zero.
const INTERRUPT_TYPE_IOAPIC: u8 = 0x08;

/// "Vendor-defined / unknown" sentinels for the fields SPCR requires
/// but a generic MMIO 16550 doesn't constrain. Per the SPCR spec a
/// baud-rate of 0 means "the current/firmware setting", parity 0 = no
/// parity, stop-bits 1, and `0x00FF` PCI device/vendor IDs mean "not a
/// PCI device".
const BAUD_RATE_AS_IS: u8 = 0;
const PARITY_NONE: u8 = 0;
const STOP_BITS_ONE: u8 = 1;
const FLOW_CONTROL_NONE: u8 = 0;
const TERMINAL_TYPE_VT_UTF8: u8 = 3;
const PCI_NOT_PRESENT_ID: u16 = 0xFFFF;

/// Allowed property names on the `ns16550a` serial node. Anything else
/// triggers `DtbError::UnsupportedProperty` per the strict-reject rule.
/// `reg-shift` / `reg-io-width` are read (they shape the GAS access);
/// `clock-frequency` / `interrupt-parent` are accepted as present-only
/// (ACPI's single GSI space + the UART's own clock make them inert).
const SERIAL_ALLOWED: &[&str] = &[
    "compatible",
    "reg",
    "interrupts",
    "interrupt-parent",
    "reg-shift",
    "reg-io-width",
    "clock-frequency",
];

/// SPCR table, revision 2 (80 bytes). Field order and offsets are the
/// wire format — do not reorder.
#[derive(Debug, Clone, Copy, IntoBytes, FromBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub(crate) struct Spcr {
    pub header: SdtHeader,
    pub interface_type: u8,
    pub reserved0: [u8; 3],
    pub base_address: GenericAddress,
    pub interrupt_type: u8,
    pub irq: u8,
    pub gsi: U32,
    pub baud_rate: u8,
    pub parity: u8,
    pub stop_bits: u8,
    pub flow_control: u8,
    pub terminal_type: u8,
    pub language: u8,
    pub pci_device_id: U16,
    pub pci_vendor_id: U16,
    pub pci_bus: u8,
    pub pci_device: u8,
    pub pci_function: u8,
    pub pci_flags: U32,
    pub pci_segment: u8,
    pub reserved1: U32,
}

impl Spcr {
    /// Total bytes (SPCR revision 2).
    pub const SIZE: usize = 80;
}

const _: () = assert!(core::mem::size_of::<Spcr>() == Spcr::SIZE);

/// `true` iff the tree declares a `serial@…` (`ns16550a`) node — and
/// validates its property set when it does. Drives whether the SPCR
/// slot is allocated. A node present but malformed is an error, not a
/// silent omission.
pub(crate) fn present<T: TreeView>(tree: &T) -> Result<bool, DtbError> {
    Ok(find_serial(tree)?.is_some())
}

/// Locate the top-level `serial@…` node compatible with `ns16550a`,
/// validating its property set if found.
fn find_serial<T: TreeView>(tree: &T) -> Result<Option<DtbNode<<T as TreeView>::Node>>, DtbError> {
    let root = DtbNode::root_of(tree.root());
    for child in root.children()? {
        if !base_name_is(child.name(), "serial") {
            continue;
        }
        if !child.has_compatible("ns16550a")? {
            return Err(DtbError::UnsupportedNode { site: Site::Serial });
        }
        validate_property_set(&child)?;
        return Ok(Some(child));
    }
    Ok(None)
}

/// Strict-reject: every property on the serial node must be allowed.
fn validate_property_set<N: NodeView + Copy>(node: &DtbNode<N>) -> Result<(), DtbError> {
    for prop in node.node.properties() {
        if !SERIAL_ALLOWED.iter().any(|a| *a == prop.name()) {
            return Err(DtbError::UnsupportedProperty { site: Site::Serial });
        }
    }
    Ok(())
}

/// Emit a complete, checksummed SPCR into `slot`.
///
/// Precondition: the tree has a validated `serial@…` node (the slot is
/// only carved when [`present`] returned `true`). The re-walk's
/// per-node errors are defense-in-depth on a count-validated tree.
pub(crate) fn emit<T: TreeView>(
    slot: &mut [u8],
    oem: &OemIdentity,
    tree: &T,
) -> Result<(), DtbError> {
    let node = find_serial(tree)?.ok_or(DtbError::Internal)?;
    let (base, access_size, gsi) = decode_serial(&node)?;
    let length = super::sdt_length_from_slot(slot)?;

    let spcr = Spcr {
        header: SdtHeader::new(*b"SPCR", length, REVISION, oem),
        interface_type: INTERFACE_TYPE_FULL_16550,
        reserved0: [0; 3],
        base_address: GenericAddress {
            address_space_id: SYSTEM_MEMORY,
            // `reg-io-width = 4` → 32-bit register access width.
            register_bit_width: 32,
            register_bit_offset: 0,
            access_size,
            address: zerocopy::little_endian::U64::new(base),
        },
        interrupt_type: INTERRUPT_TYPE_IOAPIC,
        irq: 0,
        gsi: U32::new(gsi),
        baud_rate: BAUD_RATE_AS_IS,
        parity: PARITY_NONE,
        stop_bits: STOP_BITS_ONE,
        flow_control: FLOW_CONTROL_NONE,
        terminal_type: TERMINAL_TYPE_VT_UTF8,
        language: 0,
        pci_device_id: U16::new(PCI_NOT_PRESENT_ID),
        pci_vendor_id: U16::new(PCI_NOT_PRESENT_ID),
        pci_bus: 0,
        pci_device: 0,
        pci_function: 0,
        pci_flags: U32::new(0),
        pci_segment: 0,
        reserved1: U32::new(0),
    };

    super::write_header(slot, &spcr)?;
    set_sdt_checksum(slot)
}

/// Decode `(base, access_size, gsi)` from the serial node.
///
/// `base` is the MMIO `reg[0]` base. `access_size` is the GAS
/// access-size code derived from `reg-io-width` (4 → 3 = dword).
/// `gsi` is the IO-APIC pin from `interrupts = <pin, sense>` — under
/// identity GSI routing the pin IS the global system interrupt.
fn decode_serial<N: NodeView + Copy>(node: &DtbNode<N>) -> Result<(u64, u8, u32), DtbError> {
    let base = node
        .reg(Site::Serial)?
        .next()
        .ok_or(DtbError::MalformedProperty {
            site: Site::Serial,
            property: "reg",
        })?
        .0;

    // `reg-io-width` maps to the GAS access-size code: 1→1 (byte),
    // 2→2 (word), 4→3 (dword), 8→4 (qword). arma emits 4.
    let io_width = node.property_u32("reg-io-width", Site::Serial)?;
    let access_size = match io_width {
        1 => 1,
        2 => 2,
        4 => 3,
        8 => 4,
        _ => {
            return Err(DtbError::ValueOutOfRange {
                site: Site::Serial,
                property: "reg-io-width",
            });
        }
    };

    // `interrupts = <pin, sense>` (2-cell IO-APIC binding). The first
    // cell is the IO-APIC pin; the second is the sense, which ACPI's
    // SPCR has no field for under a single identity-routed IOAPIC.
    let int_prop = node
        .node
        .property("interrupts")
        .ok_or(DtbError::MissingProperty {
            site: Site::Serial,
            property: "interrupts",
        })?;
    let mut cells = int_prop.as_u32s().ok_or(DtbError::MalformedProperty {
        site: Site::Serial,
        property: "interrupts",
    })?;
    let pin = cells.next().ok_or(DtbError::MalformedProperty {
        site: Site::Serial,
        property: "interrupts",
    })?;
    let _sense = cells.next().ok_or(DtbError::MalformedProperty {
        site: Site::Serial,
        property: "interrupts",
    })?;
    if cells.next().is_some() {
        return Err(DtbError::MalformedProperty {
            site: Site::Serial,
            property: "interrupts",
        });
    }

    Ok((base, access_size, pin))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spcr_size_pinned() {
        assert_eq!(Spcr::SIZE, 80);
        assert_eq!(core::mem::size_of::<Spcr>(), 80);
    }
}
