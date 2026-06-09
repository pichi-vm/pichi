//! Legacy PCI configuration I/O ports (`0xCF8` / `0xCFC`).
//!
//! Linux's x86 `pci_legacy_init` probes config space via the
//! CF8/CFC pair. If a write of `0x8000_0000` to `0xCF8` reads back
//! intact, `pci_check_type1()` accepts it and the kernel runs
//! `pcibios_scan_root(0)` — even without an ACPI `PNP0A08`/`PNP0A03`
//! root-bridge device in DSDT. This is the simpler bus-enumeration
//! trigger compared to AML.
//!
//! After enumeration, the kernel will also use ECAM (MCFG-declared)
//! for extended (>256B) config reads, so both dispatch paths must
//! return the same bytes — we feed them from the same `PciRoot`.

use std::sync::{Arc, Mutex};

use crate::PciRoot;

pub const CF8_PORT: u16 = 0xCF8;
pub const CF8_PORT_END: u16 = 0xCFB;
/// CFC-CFF is the 4-byte data window. Guests may issue 1/2/4-byte
/// accesses at any offset within it.
pub const CFC_PORT_BASE: u16 = 0xCFC;
pub const CFC_PORT_END: u16 = 0xCFF;

/// Latched CF8 address — shared across vCPUs (one shared PCI bus).
#[derive(Debug, Default)]
pub struct LegacyPciState {
    cf8: Mutex<u32>,
}

impl LegacyPciState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Decode CF8 → `(bus, dev, fn, reg_idx, byte_off)` if the enable bit
/// is set. `byte_off` is bits 1..0 of the latched address (the dword-
/// internal byte the guest is naming).
fn decode_cf8(cf8: u32) -> Option<(u8, u8, u8, usize, usize)> {
    if cf8 & 0x8000_0000 == 0 {
        return None;
    }
    let bus = ((cf8 >> 16) & 0xFF) as u8;
    let dev = ((cf8 >> 11) & 0x1F) as u8;
    let func = ((cf8 >> 8) & 0x07) as u8;
    let reg_idx = ((cf8 >> 2) & 0x3F) as usize;
    let byte_off = (cf8 as usize) & 0x3;
    Some((bus, dev, func, reg_idx, byte_off))
}

/// Dispatch a guest PIO read on `port`. Returns the value with `size`
/// (1/2/4) low bytes meaningful.
pub fn pio_read(state: &Arc<LegacyPciState>, bus: &Arc<PciRoot>, port: u16, size: u8) -> u32 {
    match port {
        p if (CF8_PORT..=CF8_PORT_END).contains(&p) => {
            let cf8 = *state.cf8.lock().expect("cf8 mutex poisoned");
            let off = (p - CF8_PORT) as u32;
            let shifted = cf8.wrapping_shr(off * 8);
            match size {
                1 => shifted & 0xFF,
                2 => shifted & 0xFFFF,
                _ => shifted,
            }
        }
        p if (CFC_PORT_BASE..=CFC_PORT_END).contains(&p) => {
            let cf8 = *state.cf8.lock().expect("cf8 mutex poisoned");
            let Some((b, d, f, reg, base_off)) = decode_cf8(cf8) else {
                return 0xFFFF_FFFF;
            };
            let dword = bus.config_read(b, d, f, reg);
            let off = base_off + (p - CFC_PORT_BASE) as usize;
            let shifted = dword.wrapping_shr((off * 8) as u32);
            match size {
                1 => shifted & 0xFF,
                2 => shifted & 0xFFFF,
                _ => shifted,
            }
        }
        _ => 0,
    }
}

/// Dispatch a guest PIO write on `port`. `data` carries `size` bytes.
pub fn pio_write(state: &Arc<LegacyPciState>, bus: &Arc<PciRoot>, port: u16, data: &[u8]) {
    match port {
        p if (CF8_PORT..=CF8_PORT_END).contains(&p) => {
            let off = (p - CF8_PORT) as usize;
            let mut cf8 = state.cf8.lock().expect("cf8 mutex poisoned");
            let mut bytes = cf8.to_le_bytes();
            let n = data.len().min(bytes.len().saturating_sub(off));
            bytes[off..off + n].copy_from_slice(&data[..n]);
            *cf8 = u32::from_le_bytes(bytes);
        }
        p if (CFC_PORT_BASE..=CFC_PORT_END).contains(&p) => {
            let cf8 = *state.cf8.lock().expect("cf8 mutex poisoned");
            let Some((b, d, f, reg, base_off)) = decode_cf8(cf8) else {
                return;
            };
            let off = base_off + (p - CFC_PORT_BASE) as usize;
            bus.config_write(b, d, f, reg, off as u64, data);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dillo_mmio::{MmioDevice, MmioWindow};

    #[test]
    fn decode_cf8_disable_bit_clear() {
        assert!(decode_cf8(0x0000_0000).is_none());
        assert!(decode_cf8(0x7FFF_FFFF).is_none());
    }

    #[test]
    fn decode_cf8_byte_offset_in_low_bits() {
        let (_, _, _, _, off) = decode_cf8(0x8000_0003).unwrap();
        assert_eq!(off, 3);
    }

    #[test]
    fn decode_cf8_fields() {
        // bus=1, dev=2, func=3, reg=4
        let cf8 = 0x8000_0000 | (1 << 16) | (2 << 11) | (3 << 8) | (4 << 2);
        let (b, d, f, r, off) = decode_cf8(cf8).unwrap();
        assert_eq!((b, d, f, r, off), (1, 2, 3, 4, 0));
    }

    #[test]
    fn cf8_latch_accepts_byte_writes() {
        let state = Arc::new(LegacyPciState::new());
        let bus = Arc::new(PciRoot::new(MmioWindow {
            base: 0x3000_0000,
            size: 0x1000_0000,
        }));

        pio_write(&state, &bus, CF8_PORT, &[0, 0, 0, 0]);
        pio_write(&state, &bus, CF8_PORT_END, &[0x80]);

        assert_eq!(pio_read(&state, &bus, CF8_PORT, 4), 0x8000_0000);
        assert_eq!(pio_read(&state, &bus, CF8_PORT_END, 1), 0x80);
    }

    #[test]
    fn legacy_cfc_and_ecam_return_identical_config_bytes() {
        let state = Arc::new(LegacyPciState::new());
        let root = Arc::new(PciRoot::new(MmioWindow {
            base: 0x3000_0000,
            size: 0x1000_0000,
        }));
        let cf8 = 0x8000_0000u32;
        pio_write(&state, &root, CF8_PORT, &cf8.to_le_bytes());

        let mut ecam = [0u8; 4];
        let ecam_window = root.windows()[0];
        root.read(ecam_window, 0, &mut ecam)
            .expect("ECAM read routed");

        assert_eq!(
            pio_read(&state, &root, CFC_PORT_BASE, 4).to_le_bytes(),
            ecam
        );
    }
}
