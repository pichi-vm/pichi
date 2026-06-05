//! Bootstrap input wire format — the channel from arma to tatu.
//!
//! A single `.tatu.bootinfo` PE section holds a `TatuBootInfo`
//! struct: magic + GPA/size pairs for the three image-adjacent
//! regions tatu needs at boot time (measured base DTB, host-filled
//! DTBO, kernel image). The struct is the entire section — 4 KiB,
//! page-aligned — so PMI's small-section granularity is satisfied
//! by the type's own layout, not by the linker script.
//!
//! Arma writes the bytes at PMI build time. Tatu's per-arch reset
//! vector loads `&BOOTINFO` into the first-argument register and
//! calls `rust_main(&TatuBootInfo)`; the optimizer cannot fold
//! through that asm boundary, so no `UnsafeCell` is needed.
//!
//! `base_dtb_bytes`, `host_dtbo_bytes`, and `kernel_bytes` each
//! construct a raw slice from a (gpa, size) pair in the header.
//! They are the three `unsafe` sites in this module, with a single
//! shared `// SAFETY:` block on the helper they all call.

/// Magic value identifying a populated `TatuBootInfo` (i.e. one
/// arma actually filled in, vs the all-zero placeholder).
pub const MAGIC: [u8; 8] = *b"TATUBOOT";

/// Wire format. Fields are naturally aligned (8-byte u64s come
/// before 4-byte u32s), so plain `#[repr(C)]` would already give
/// the 44-byte payload layout with no internal padding. We use
/// `align(4096)` + an explicit `_reserved` tail to make the type
/// itself exactly one page, encoding PMI small-section granularity
/// (`pmi/spec/granularity.md`) in the type instead of the linker
/// script.
#[repr(C, align(4096))]
pub struct TatuBootInfo {
    pub magic: [u8; 8],
    pub base_dtb_gpa: u64,
    pub host_dtbo_gpa: u64,
    pub kernel_gpa: u64,
    pub base_dtb_size: u32,
    pub host_dtbo_size: u32,
    pub kernel_size: u32,
    _reserved: [u8; 4096 - 44],
}

impl TatuBootInfo {
    /// All-zero initializer used as the placeholder for the
    /// `.tatu.bootinfo` static. Arma overwrites this before tatu
    /// runs; a stale image (unfilled) fails the magic check at
    /// boot.
    pub const ZERO: Self = Self {
        magic: [0; 8],
        base_dtb_gpa: 0,
        host_dtbo_gpa: 0,
        kernel_gpa: 0,
        base_dtb_size: 0,
        host_dtbo_size: 0,
        kernel_size: 0,
        _reserved: [0; 4096 - 44],
    };
}

const _: () = {
    assert!(core::mem::size_of::<TatuBootInfo>() == 4096);
    assert!(core::mem::align_of::<TatuBootInfo>() == 4096);
};

// ---------------------------------------------------------------------------
// Region accessors. Each turns a (gpa, size) pair from the header
// into a typed `&'static [u8]`. Three call sites; one shared
// `// SAFETY:` helper.
// ---------------------------------------------------------------------------

/// Borrow the measured base-DTB region.
pub fn base_dtb_bytes(info: &TatuBootInfo) -> &'static [u8] {
    region(info.base_dtb_gpa, info.base_dtb_size)
}

/// Borrow the host-supplied DTBO region.
pub fn host_dtbo_bytes(info: &TatuBootInfo) -> &'static [u8] {
    region(info.host_dtbo_gpa, info.host_dtbo_size)
}

/// Borrow the loaded kernel image.
pub fn kernel_bytes(info: &TatuBootInfo) -> &'static [u8] {
    region(info.kernel_gpa, info.kernel_size)
}

#[allow(unsafe_code)]
fn region(gpa: u64, size: u32) -> &'static [u8] {
    // SAFETY: arma wrote the (gpa, size) pair to point at a PE
    // section loaded by the VMM per PMI's `load` / `merged:dtbo`
    // action before vCPU entry; pmi/spec/merged.md §2 makes the
    // VMM responsible for `/memory@*` covering each region. Tatu
    // runs on a single boot vCPU with no IRQs or DMA — no
    // concurrent writer. The slice lives for the whole boot.
    unsafe { core::slice::from_raw_parts(gpa as *const u8, size as usize) }
}
