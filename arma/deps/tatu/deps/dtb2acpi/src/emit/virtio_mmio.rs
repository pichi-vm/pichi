//! DSDT `Device(VMnn)` emitter for virtio-mmio transports.
//!
//! On a DT platform the kernel finds virtio-mmio devices straight from the
//! device tree. x86 has no DT at runtime, so without an ACPI namespace device
//! the kernel never probes the transport — the console/disk on virtio-mmio
//! simply do not appear. This emitter gives each `virtio_mmio@*` node from the
//! DTB an ACPI device with `_HID "LNRO0005"` (the ID Linux's `virtio_mmio`
//! driver matches in ACPI mode) and a `_CRS` carrying the MMIO window + GSI.
//!
//! One device is emitted per declared transport, in `root.children()` order.
//! Empty slots (no device attached by the VMM) are harmless: the driver reads
//! the virtio magic at probe time and skips a slot that has none.

use devtree::{NodeView, PropertyView, TreeView};

use super::aml;
use crate::dtb::DtbNode;
use crate::error::{DtbError, Site};

/// Fixed body bytes per `Device(VMnn)` before `_CRS`:
/// `_HID` (string-valued name, "LNRO0005" = 8 chars) + `_UID` (Byte-valued).
const FIXED_NAMES_BYTES: usize = (1 + 4 + aml::string_bytes(8)) + (1 + 4 + 1 + 1);

/// `_CRS` wrapper bytes: `Name(_CRS, Buffer(PkgLength, WordPrefix+u16, <descs>))`.
const CRS_WRAPPER_BYTES: usize = 1 + 4 + 1 + aml::PKG_LENGTH_BYTES + 3;

/// QWordMemory(window) + ExtendedInterrupt(GSI) + EndTag.
const CRS_DESCRIPTOR_BYTES: usize =
    aml::QWORD_MEMORY_BYTES + aml::EXTENDED_INTERRUPT_BYTES + aml::END_TAG_BYTES;

/// Total bytes for one virtio-mmio ACPI device.
pub(crate) const DEVICE_BYTES: usize = {
    let body = FIXED_NAMES_BYTES + CRS_WRAPPER_BYTES + CRS_DESCRIPTOR_BYTES;
    // DeviceOp(2) + PkgLength(2) + NameSeg(4) + body
    2 + aml::PKG_LENGTH_BYTES + 4 + body
};

/// Count the `virtio,mmio` transports declared in the DTB.
fn count_devices<T: TreeView>(tree: &T) -> Result<usize, DtbError> {
    let root = DtbNode::root_of(tree.root());
    let mut n = 0usize;
    for child in root.children()? {
        if child.has_compatible("virtio,mmio")? {
            n += 1;
        }
    }
    Ok(n)
}

/// DSDT byte cost contributed by the virtio-mmio devices.
pub(crate) fn dsdt_total_bytes<T: TreeView>(tree: &T) -> Result<usize, DtbError> {
    Ok(count_devices(tree)? * DEVICE_BYTES)
}

/// Emit a `Device(VMnn)` for every `virtio,mmio` node. Returns the new position.
pub(crate) fn emit<T: TreeView>(slot: &mut [u8], pos: usize, tree: &T) -> Result<usize, DtbError> {
    let root = DtbNode::root_of(tree.root());
    let mut cursor = pos;
    let mut index: u8 = 0;
    for child in root.children()? {
        if !child.has_compatible("virtio,mmio")? {
            continue;
        }
        cursor = write_one_device(slot, cursor, &child, index)?;
        index = index.checked_add(1).ok_or(DtbError::Internal)?;
    }
    Ok(cursor)
}

fn write_one_device<N: NodeView + Copy>(
    slot: &mut [u8],
    pos: usize,
    node: &DtbNode<N>,
    index: u8,
) -> Result<usize, DtbError> {
    let (base, size) = node
        .reg(Site::VirtioMmio)?
        .next()
        .ok_or(DtbError::MalformedProperty {
            site: Site::VirtioMmio,
            property: "reg",
        })?;
    let gsi = decode_gsi(node)?;

    let pos = aml::write_bytes(slot, pos, &[aml::EXT_OP_PREFIX, aml::DEVICE_OP])?;
    let pkg_value = DEVICE_BYTES.checked_sub(2).ok_or(DtbError::Internal)?;
    let pos = aml::write_pkg_length(slot, pos, pkg_value)?;
    let pos = aml::write_name_seg(slot, pos, &name_seg(index))?;

    // LNRO0005 = the ACPI _HID Linux's virtio_mmio driver matches; MMIO because
    // _CRS exposes a QWordMemory resource (not an I/O-port one).
    let pos = aml::write_name_string(slot, pos, b"_HID", b"LNRO0005")?;
    let pos = aml::write_name_byte(slot, pos, b"_UID", index)?;

    let buffer_size = u16::try_from(CRS_DESCRIPTOR_BYTES).map_err(|_| DtbError::Internal)?;
    let buf_pkg_value = aml::PKG_LENGTH_BYTES + 3 /* WordPrefix + u16 */ + CRS_DESCRIPTOR_BYTES;

    let pos = aml::write_bytes(slot, pos, &[aml::NAME_OP])?;
    let pos = aml::write_name_seg(slot, pos, b"_CRS")?;
    let pos = aml::write_bytes(slot, pos, &[aml::BUFFER_OP])?;
    let pos = aml::write_pkg_length(slot, pos, buf_pkg_value)?;
    let pos = aml::write_bytes(slot, pos, &[aml::WORD_PREFIX])?;
    let pos = aml::write_bytes(slot, pos, &buffer_size.to_le_bytes())?;

    let pos = aml::write_qword_memory(slot, pos, base, size)?;
    let pos = aml::write_extended_interrupt(slot, pos, gsi)?;
    aml::write_end_tag(slot, pos)
}

/// `interrupts = <pin, sense>` — the first cell is the IO-APIC pin, which under
/// identity GSI routing IS the global system interrupt (as for the serial node).
fn decode_gsi<N: NodeView + Copy>(node: &DtbNode<N>) -> Result<u32, DtbError> {
    let prop = node
        .node
        .property("interrupts")
        .ok_or(DtbError::MissingProperty {
            site: Site::VirtioMmio,
            property: "interrupts",
        })?;
    let mut cells = prop.as_u32s().ok_or(DtbError::MalformedProperty {
        site: Site::VirtioMmio,
        property: "interrupts",
    })?;
    cells.next().ok_or(DtbError::MalformedProperty {
        site: Site::VirtioMmio,
        property: "interrupts",
    })
}

/// `VMnn` NameSeg (two decimal digits); 16 mmio slots is the planner maximum.
fn name_seg(index: u8) -> [u8; 4] {
    [b'V', b'M', b'0' + (index / 10), b'0' + (index % 10)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_size_pinned() {
        // _HID(15) + _UID(7) + CRS wrapper(11) + descriptors(57) = 90 body;
        // + DeviceOp(2) + PkgLength(2) + NameSeg(4) = 98.
        assert_eq!(DEVICE_BYTES, 98);
    }

    #[test]
    fn name_seg_is_valid() {
        assert_eq!(&name_seg(0), b"VM00");
        assert_eq!(&name_seg(15), b"VM15");
    }
}
