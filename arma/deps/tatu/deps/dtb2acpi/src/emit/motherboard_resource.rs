//! DSDT motherboard resource emitter.
//!
//! Linux's late ECAM validation accepts an MCFG allocation only when the ECAM
//! range is reserved by firmware, commonly through a `PNP0C02` motherboard
//! resource device. MCFG names the ECAM base; this DSDT device reserves the
//! same DTB-derived `reg` window so an ECAM-only x86 guest does not need legacy
//! CF8/CFC config-space probing.

use devtree::TreeView;

use super::aml;
use crate::dtb::DtbNode;
use crate::error::{DtbError, Site};

/// `_HID` + `_UID`.
const FIXED_NAMES_BYTES: usize = (1 + 4 + 1 + 4) + (1 + 4 + 1 + 1);

/// `_CRS` wrapper bytes around a QWordMemory descriptor and EndTag:
/// `Name(_CRS, Buffer(PkgLength, WordPrefix+u16, <descriptors>))`.
const CRS_WRAPPER_BYTES: usize = 1 + 4 + 1 + aml::PKG_LENGTH_BYTES + 3;

const fn motherboard_aml_bytes() -> usize {
    let descriptors = aml::QWORD_MEMORY_BYTES + aml::END_TAG_BYTES;
    let crs = CRS_WRAPPER_BYTES + descriptors;
    let body = FIXED_NAMES_BYTES + crs;
    2 + aml::PKG_LENGTH_BYTES + 4 + body
}

pub(crate) fn dsdt_total_bytes<T: TreeView>(tree: &T) -> Result<usize, DtbError> {
    let root = DtbNode::root_of(tree.root());
    let mut total = 0usize;
    for child in root.children()? {
        if !child.has_compatible("pci-host-ecam-generic")? {
            continue;
        }
        total = total
            .checked_add(motherboard_aml_bytes())
            .ok_or(DtbError::Internal)?;
    }
    Ok(total)
}

pub(crate) fn emit<T: TreeView>(slot: &mut [u8], pos: usize, tree: &T) -> Result<usize, DtbError> {
    let root = DtbNode::root_of(tree.root());
    let mut cursor = pos;
    let mut index: u8 = 0;
    for child in root.children()? {
        if !child.has_compatible("pci-host-ecam-generic")? {
            continue;
        }
        let (base, size) = child.reg(Site::PciHost)?.next().ok_or(DtbError::Internal)?;
        cursor = write_one_device(slot, cursor, index, base, size)?;
        index = index.checked_add(1).ok_or(DtbError::Internal)?;
    }
    Ok(cursor)
}

fn write_one_device(
    slot: &mut [u8],
    pos: usize,
    index: u8,
    base: u64,
    size: u64,
) -> Result<usize, DtbError> {
    let total = motherboard_aml_bytes();

    let pos = aml::write_bytes(slot, pos, &[aml::EXT_OP_PREFIX, aml::DEVICE_OP])?;
    let pkg_value = total.checked_sub(2).ok_or(DtbError::Internal)?;
    let pos = aml::write_pkg_length(slot, pos, pkg_value)?;
    let name = motherboard_name_seg(index);
    let pos = aml::write_name_seg(slot, pos, &name)?;

    let pos = aml::write_name_dword(slot, pos, b"_HID", aml::eisaid(b"PNP0C02"))?;
    let pos = aml::write_name_byte(slot, pos, b"_UID", index)?;

    let descriptors_bytes = aml::QWORD_MEMORY_BYTES + aml::END_TAG_BYTES;
    let buffer_size = u16::try_from(descriptors_bytes).map_err(|_| DtbError::Internal)?;
    let buf_pkg_value = aml::PKG_LENGTH_BYTES + 3 + descriptors_bytes;

    let pos = aml::write_bytes(slot, pos, &[aml::NAME_OP])?;
    let pos = aml::write_name_seg(slot, pos, b"_CRS")?;
    let pos = aml::write_bytes(slot, pos, &[aml::BUFFER_OP])?;
    let pos = aml::write_pkg_length(slot, pos, buf_pkg_value)?;
    let pos = aml::write_bytes(slot, pos, &[aml::WORD_PREFIX])?;
    let pos = aml::write_bytes(slot, pos, &buffer_size.to_le_bytes())?;
    let pos = aml::write_qword_memory(slot, pos, base, size)?;
    aml::write_end_tag(slot, pos)
}

fn motherboard_name_seg(index: u8) -> [u8; 4] {
    let d0 = if index < 10 {
        b'0' + index
    } else {
        b'A' + (index - 10)
    };
    [b'M', b'B', b'R', d0]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn motherboard_device_size_is_stable() {
        assert_eq!(motherboard_aml_bytes(), 84);
    }

    #[test]
    fn motherboard_name_first_few() {
        assert_eq!(motherboard_name_seg(0), *b"MBR0");
        assert_eq!(motherboard_name_seg(9), *b"MBR9");
        assert_eq!(motherboard_name_seg(10), *b"MBRA");
    }
}
