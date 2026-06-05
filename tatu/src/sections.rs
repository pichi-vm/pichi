//! Guest-memory layout — one typed static per output section.
//!
//! The linker script (`linker/tatu.ld`) only *orders* these sections;
//! every address falls out of their sizes by contiguous packing. Which
//! sections exist is controlled purely by `#[cfg]`, so the same linker
//! script yields the optimal layout on each architecture: the x86-only
//! legacy/padding sections vanish on aarch64 (an absent section's
//! placement evaporates — the linker drops it without perturbing the
//! location counter), and the rest pack tight from 0.
//!
//! Sizes live in the *types*, so `size_of_val` over the instances derives
//! the few offsets tatu needs (notably [`PGT_BASE`], the prefix sum to
//! `.tatu.pgt`). Change a section's type and the layout reflows; the
//! compile-time `assert!`s pin the x86 architectural boundaries (640 KiB,
//! 1 MiB) so a size change that would push ACPI out of the reserved zone
//! fails the build. Addresses are never hardcoded (ARCHITECTURE.md §6.3):
//! arma reads each section's GPA from tatu's ELF and cross-checks.

// `link_section` overrides trip the unsafe-code lint; this whole module is
// the documented placement contract for `linker/tatu.ld`, not a soundness
// hazard — same pattern as the other reserved input sections.
#![allow(unsafe_code)]

// Only the x86_64 layout arithmetic (flex stack, PGT_BASE) reads instance
// sizes; aarch64 packs with no derived offsets.
#[cfg(target_arch = "x86_64")]
use core::mem::size_of_val;

use crate::Paged;
use crate::workspace::PageCell;

// ---------------------------------------------------------------------------
// Shared sections (both architectures).
// ---------------------------------------------------------------------------

/// `.tatu.dtb` — measured base DTB. arma fills it at PMI build time; tatu
/// reads it via the bootinfo GPA. The static only reserves the section.
#[used]
#[allow(dead_code)]
#[unsafe(link_section = ".tatu.dtb")]
pub static DTB: PageCell<Paged<[u8; 8 * 1024]>> = PageCell::new(Paged([0; 8 * 1024]));

/// `.tatu.dtbo` — host overlay. dillo fills it at launch (the unmeasured
/// half); tatu reads it via the bootinfo GPA.
#[used]
#[allow(dead_code)]
#[unsafe(link_section = ".tatu.dtbo")]
pub static DTBO: PageCell<Paged<[u8; 64 * 1024]>> = PageCell::new(Paged([0; 64 * 1024]));

/// `.tatu.dtbm` — merged-DTB buffer. tatu writes the merge here. On aarch64
/// this is the output Linux reads via `x0`; on x86 it is throwaway scratch
/// (Linux only reads the cmdline pointer into it, early). Bounded by base
/// (`.tatu.dtb` 8 KiB) + overlay (`.tatu.dtbo` 64 KiB) = 72 KiB.
#[used]
#[unsafe(link_section = ".tatu.dtbm")]
pub static DTBM: PageCell<Paged<[u8; 72 * 1024]>> = PageCell::new(Paged([0; 72 * 1024]));

/// Unique `&'static mut` view of the merged-DTB buffer. Called once, from
/// `rust_main`.
pub fn dtbm_buf() -> &'static mut [u8] {
    // SAFETY: tatu runs on a single boot vCPU with no APs, so a mutable
    // static is sound for the same reason as STACK (see workspace.rs).
    let buf: &'static mut [u8; 72 * 1024] = unsafe { &mut (*DTBM.as_mut_ptr()).0 };
    buf
}

// ---------------------------------------------------------------------------
// x86_64-only sections: the legacy low-memory map. Each is a zero pad over
// an architectural scan window, or a fixed boot-CPU structure. All vanish
// on aarch64, so the shared sections above pack contiguously from 0 there.
// ---------------------------------------------------------------------------

/// `.tatu.guard` — 4 KiB at GPA 0. The MP/BDA scan window [0,1K) must read
/// zero; this keeps the stack off it. Absent on aarch64, so the stack
/// becomes the first section at GPA 0.
#[cfg(target_arch = "x86_64")]
#[used]
#[allow(dead_code)]
#[unsafe(link_section = ".tatu.guard")]
pub static GUARD: PageCell<Paged<[u8; 4 * 1024]>> = PageCell::new(Paged([0; 4 * 1024]));

/// `.tatu.scanpad` — covers the [639K,640K) MP scan window. Sized so ACPI
/// lands exactly at the 640 KiB reserved-zone boundary.
#[cfg(target_arch = "x86_64")]
#[used]
#[allow(dead_code)]
#[unsafe(link_section = ".tatu.scanpad")]
pub static SCANPAD: PageCell<Paged<[u8; 4 * 1024]>> = PageCell::new(Paged([0; 4 * 1024]));

/// `.tatu.rompad` — covers the [0xF0000,1M) MP+DMI scan window and pushes
/// the code sections up to the 1 MiB floor.
#[cfg(target_arch = "x86_64")]
#[used]
#[allow(dead_code)]
#[unsafe(link_section = ".tatu.rompad")]
pub static ROMPAD: PageCell<Paged<[u8; 64 * 1024]>> = PageCell::new(Paged([0; 64 * 1024]));

/// `.tatu.reset.x86_64.pad` — 0xFF0 zero bytes that position the 16-byte
/// reset stub at the architectural reset vector 0xFFFFFFF0 (the stub
/// follows this pad in `.tatu.reset`). A static rather than a linker
/// `. = . + 0xFF0` so the whole `.tatu.reset` section is *genuinely* empty
/// on aarch64 (no nonzero location-counter advance to keep it alive) and
/// the linker drops it.
#[cfg(target_arch = "x86_64")]
#[used]
#[allow(dead_code)]
#[unsafe(link_section = ".tatu.reset.x86_64.pad")]
pub static RESET_PAD: PageCell<[u8; 0xFF0]> = PageCell::new([0; 0xFF0]);

// ---------------------------------------------------------------------------
// Stack — its own section on both arches, placed first (after the x86 guard)
// so an underflow walks toward GPA 0. On x86 its size is the flex filler
// that makes conventional RAM end exactly at 640 KiB (maximising the stack);
// aarch64 has no such boundary, so it is a plain fixed size.
// ---------------------------------------------------------------------------

/// x86 640 KiB conventional/reserved split — the only bare architectural
/// constant in the layout; everything else is a section size.
#[cfg(target_arch = "x86_64")]
const CONV_TOP: usize = 640 * 1024;

/// Stack size. x86: fills conventional RAM up to the 640 KiB barrier, so
/// the stack is as large as possible and ACPI starts exactly at 640 KiB.
#[cfg(target_arch = "x86_64")]
pub const STACK_SIZE: usize = CONV_TOP
    - (size_of_val(&GUARD)
        + core::mem::size_of::<crate::bootinfo::TatuBootInfo>()
        + size_of_val(&DTB)
        + size_of_val(&DTBO)
        + size_of_val(&DTBM)
        + crate::bootmem::PGT_SIZE
        + crate::bootmem::GDT_SIZE
        + size_of_val(&SCANPAD));

/// Stack size. aarch64: no legacy boundary, so a plain fixed size.
#[cfg(target_arch = "aarch64")]
pub const STACK_SIZE: usize = 456 * 1024;

/// `.tatu.stack` — the runtime stack (SP starts at the top, grows down).
#[used]
#[unsafe(link_section = ".tatu.stack")]
pub static STACK: PageCell<Paged<[u8; STACK_SIZE]>> = PageCell::new(Paged([0; STACK_SIZE]));

// ---------------------------------------------------------------------------
// Derived offsets + boundary assertions (x86_64).
// ---------------------------------------------------------------------------

/// Load GPA of `.tatu.pgt` — derived, not declared: the prefix sum of the
/// sections that precede it (guard, stack, bootinfo, dtb, dtbo, dtbm). The
/// page-table self-pointers (`bootmem`) embed this at const-eval time; arma
/// reads the actual section GPA from the ELF and cross-checks. Change any
/// preceding section's size and this follows.
#[cfg(target_arch = "x86_64")]
pub const PGT_BASE: u64 = (size_of_val(&GUARD)
    + size_of_val(&STACK)
    + core::mem::size_of::<crate::bootinfo::TatuBootInfo>()
    + size_of_val(&DTB)
    + size_of_val(&DTBO)
    + size_of_val(&DTBM)) as u64;

#[cfg(target_arch = "x86_64")]
const _: () = {
    // Boot CPU tables land at PGT_BASE; the self-pointers depend on it.
    assert!(PGT_BASE == 0x9_8000);
    // ACPI fills [640K,960K); the rompad fills [960K,1M); code floor = 1 MiB.
    assert!(CONV_TOP + crate::arch_x86_64::ACPI_WORKSPACE_SIZE + size_of_val(&ROMPAD) == 0x10_0000);
};
