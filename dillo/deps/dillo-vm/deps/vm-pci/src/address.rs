// SPDX-License-Identifier: Apache-2.0

// Portions derived from Firecracker (https://github.com/firecracker-microvm/firecracker),
// licensed under Apache-2.0 OR BSD-3-Clause.

//! CF8/CFC legacy PIO and ECAM MMIO address decoding for PCI config space.

/// Parse a CF8 (legacy PCI config address) register value.
///
/// Returns `Some((bus, device, function, register_index))` if the enable bit
/// (bit 31) is set, or `None` otherwise. Register index is a dword index
/// (bits 7:2 shifted right by 2).
pub fn parse_cf8(addr: u32) -> Option<(u8, u8, u8, u8)> {
    // Bit 31 must be set (enable bit)
    if addr & 0x8000_0000 == 0 {
        return None;
    }
    let bus = ((addr >> 16) & 0xFF) as u8;
    let device = ((addr >> 11) & 0x1F) as u8;
    let function = ((addr >> 8) & 0x07) as u8;
    let register = ((addr >> 2) & 0x3F) as u8;
    Some((bus, device, function, register))
}

/// Parse an ECAM (PCIe enhanced config) byte offset into BDF + register offset.
///
/// Returns `(bus, device, function, register_byte_offset)`.
/// ECAM layout: each function gets 4KB (4096 bytes).
/// Offset bits: [27:20] = bus, [19:15] = device, [14:12] = function, [11:0] = register offset.
pub fn parse_ecam_offset(offset: u64) -> (u8, u8, u8, u16) {
    let bus = ((offset >> 20) & 0xFF) as u8;
    let device = ((offset >> 15) & 0x1F) as u8;
    let function = ((offset >> 12) & 0x07) as u8;
    let register = (offset & 0xFFF) as u16;
    (bus, device, function, register)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn cf8_enable_bit_unset_returns_none() {
        assert!(parse_cf8(0x0000_0000).is_none());
        assert!(parse_cf8(0x7FFF_FFFF).is_none());
    }

    #[test]
    fn cf8_enable_bit_set_basic() {
        let result = parse_cf8(0x8000_0000).unwrap();
        assert_eq!(result, (0, 0, 0, 0));
    }

    #[test]
    fn cf8_parses_bus_device_function_register() {
        let addr: u32 = 0x8000_0000 | (1 << 16) | (2 << 11) | (3 << 8) | (4 << 2);
        let (bus, dev, func, reg) = parse_cf8(addr).unwrap();
        assert_eq!(bus, 1);
        assert_eq!(dev, 2);
        assert_eq!(func, 3);
        assert_eq!(reg, 4);
    }

    #[test]
    fn ecam_offset_zero() {
        assert_eq!(parse_ecam_offset(0), (0, 0, 0, 0));
    }

    #[test]
    fn ecam_offset_device1() {
        // Device 1: offset = 1 << 15 = 0x8000 = 32768
        assert_eq!(parse_ecam_offset(0x8000), (0, 1, 0, 0));
    }

    #[test]
    fn ecam_offset_with_register() {
        assert_eq!(parse_ecam_offset(4), (0, 0, 0, 4));
    }
}
