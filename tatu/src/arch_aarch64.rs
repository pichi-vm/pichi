//! aarch64 finalization: entry stub, halt/debug, kernel handoff.
//! See ARCHITECTURE.md Part III.
//!
//! Unsafe lives where natural: entry asm, halt, MMIO debug write,
//! final kernel branch. Each block carries `// SAFETY:` (§7.1).

#![allow(unsafe_code)]

// ---------------------------------------------------------------------------
// Entry stub + BOOTINFO storage. Both live in this private
// submodule so `BOOTINFO` is referenced only by the asm `sym`
// below — no other Rust code can read or write it, and the
// optimizer can't fold through the asm boundary.
// ---------------------------------------------------------------------------

mod reset {
    use crate::bootinfo::TatuBootInfo;
    use crate::sections::STACK_SIZE;

    /// The `.tatu.bootinfo` section. Arma overwrites the
    /// placeholder zeros at PMI build time. **Never read from
    /// Rust by name** — see the matching note in arch_x86_64.
    #[used]
    #[unsafe(link_section = ".tatu.bootinfo")]
    static BOOTINFO: TatuBootInfo = TatuBootInfo::ZERO;

    /// Entry stub in `.tatu.reset.aarch64`, which the single
    /// `linker/tatu.ld` positions first inside `.tatu.text` so the
    /// `reset` symbol is the kernel-visible entry. (The x86 stub uses a
    /// distinct `.tatu.reset.x86_64` routed to the reset vector instead.)
    ///
    /// SAFETY: PC-relative adrp/add+:lo12: resolves STACK and
    /// BOOTINFO at link time; adding STACK_SIZE yields the top of
    /// the 64 KiB stack. rust_main is the cross-arch lifecycle
    /// entry; it never returns. AAPCS: first arg in X0, so
    /// rust_main receives `bootinfo: &TatuBootInfo`. Interrupts
    /// are masked (DAIF set by arma).
    /// `#[unsafe(no_mangle)]` is the keep-alive anchor — see
    /// the matching note in arch_x86_64.
    #[unsafe(naked)]
    #[unsafe(no_mangle)]
    #[unsafe(link_section = ".tatu.reset.aarch64")]
    extern "C" fn reset() -> ! {
        core::arch::naked_asm!(
            "adrp x0,  {bootinfo}",
            "add  x0,  x0,  :lo12:{bootinfo}",
            "adrp x16, {stack}",
            "add  x16, x16, :lo12:{stack}",
            "add  x16, x16, {size}",
            "mov  sp,  x16",
            "b    {main}",
            bootinfo = sym BOOTINFO,
            stack    = sym crate::sections::STACK,
            size     = const STACK_SIZE,
            main     = sym crate::rust_main,
        )
    }
}

// ---------------------------------------------------------------------------
// Halt: null VBAR_EL1 + UDF #0 → recursive synchronous abort →
// KVM_EXIT_FAIL_ENTRY (or equivalent).
// ---------------------------------------------------------------------------

#[inline(never)]
pub fn halt() -> ! {
    // SAFETY: writing xzr to VBAR_EL1 sets the exception vector
    // to address 0; UDF then raises a synchronous exception which
    // cannot be vectored anywhere valid. The vCPU cannot resume.
    unsafe {
        core::arch::asm!("msr vbar_el1, xzr", "udf #0", options(noreturn),);
    }
}

// ---------------------------------------------------------------------------
// MergedDtb — typed handle for the merged DTB workspace, the
// argument to `boot_kernel` on aarch64.
// ---------------------------------------------------------------------------

#[derive(Debug, Copy, Clone)]
pub struct MergedDtb {
    pub gpa: u64,
}

// ---------------------------------------------------------------------------
// Kernel handoff. Loads x0 with the merged DTB GPA, branches to
// the kernel entry. Never returns.
// ---------------------------------------------------------------------------

pub fn boot_kernel(kernel_bytes: &[u8], dtb: MergedDtb) -> ! {
    let entry = kernel_bytes.as_ptr() as u64;
    // SAFETY: this is the final instruction tatu executes. Per
    // the aarch64 Linux boot protocol the kernel expects
    // `x0 = DTB GPA`, `x1 = x2 = x3 = 0` (reserved, MBZ), and PC at
    // the Image header (offset 0). Tatu's code no longer runs after
    // this branch. Zeroing x1–x3 silences the kernel's "x1-x3 nonzero
    // in violation of boot protocol" warning.
    unsafe {
        core::arch::asm!(
            "mov x0, {dtb}",
            "mov x1, xzr",
            "mov x2, xzr",
            "mov x3, xzr",
            "br  {entry}",
            dtb = in(reg) dtb.gpa,
            entry = in(reg) entry,
            options(noreturn),
        );
    }
}
