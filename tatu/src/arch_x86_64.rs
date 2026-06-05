//! x86_64 finalization: entry stub, e820, boot_params, ACPI
//! generation, halt/debug, kernel handoff. See ARCHITECTURE.md
//! Part II.
//!
//! Unsafe lives where natural: the reset-vector global_asm, the
//! halt sequence, the debug-port write, and the final kernel jmp.
//! Each block carries a `// SAFETY:` note (§7.1).

#![allow(unsafe_code)]

use devtree::{NodeView, TreeView};

use crate::validate::ValidationError;

/// Hardcoded OEM identity stamped into every emitted ACPI table.
/// Tatu doesn't accept per-image OEM strings; if a caller needs
/// vendor branding in dmesg, this is the lever.
const OEM: dtb2acpi::OemIdentity = dtb2acpi::OemIdentity {
    oem_id: *b"TATU  ",
    oem_table_id: *b"TATUOEM ",
    oem_revision: 1,
    creator_id: *b"TATU",
    creator_revision: 1,
};

/// Size of the ACPI output buffer (the `.tatu.acpi` section). It now owns
/// the entire reserved [640K,960K) zone — base 640K is the RSDP GPA, and
/// it ends at 960K, leaving only the [0xF0000,1M) rompad scan-window gap.
/// Far beyond the §4.5 maxima (2048 vCPUs + 32 NUMA domains ≈ 80 KiB); the
/// rest is unwritten zero tail. `AcpiBuffer<N>` is a transparent `[u8; N]`,
/// so `Paged<AcpiBuffer<N>>` is exactly `N` (N is a 4 KiB multiple).
pub const ACPI_WORKSPACE_SIZE: usize = 320 * 1024;

/// Page-aligned ACPI output buffer. `dtb2acpi::AcpiBuffer` is only
/// 16-byte aligned (RSDP requirement); the `Paged` wrapper bumps that
/// to 4 KiB so the e820 entry tatu emits starts on a page boundary.
pub type AcpiBuf = crate::Paged<dtb2acpi::AcpiBuffer<ACPI_WORKSPACE_SIZE>>;

/// The `.tatu.acpi` workspace. tatu generates the ACPI tables here and
/// hands the GPA to Linux via `boot_params.acpi_rsdp_addr`. Fixed at
/// link time in the reserved [640K,1M) zone (linker/x86_64.ld).
#[used]
#[unsafe(link_section = ".tatu.acpi")]
static ACPI_WORKSPACE: crate::workspace::PageCell<AcpiBuf> = crate::workspace::PageCell::new(
    crate::Paged(dtb2acpi::AcpiBuffer::<ACPI_WORKSPACE_SIZE>::new()),
);

/// The `.tatu.acpi` workspace as a unique `&'static mut`.
pub fn acpi_workspace() -> &'static mut AcpiBuf {
    // SAFETY: tatu runs on a single boot vCPU with no APs and no
    // concurrent access (see workspace.rs), so a single &mut to the
    // static is sound. Called once, from finalize.
    unsafe { &mut *ACPI_WORKSPACE.as_mut_ptr() }
}

// ---------------------------------------------------------------------------
// Reset vector + BOOTINFO storage. Both live in this private
// submodule so `BOOTINFO` is referenced only by the asm `sym`
// below — no other Rust code can read or write it, and the
// optimizer can't fold through the asm boundary.
// ---------------------------------------------------------------------------

mod reset {
    use crate::bootinfo::TatuBootInfo;
    use crate::sections::STACK_SIZE;

    /// The `.tatu.bootinfo` section. Arma overwrites the
    /// placeholder zeros at PMI build time. **Never read from
    /// Rust by name** — its address is materialized only through
    /// the asm `sym` below, and reaches the rest of tatu only via
    /// the `&TatuBootInfo` parameter to `rust_main`. Any direct
    /// read here would let the optimizer see the placeholder
    /// `TatuBootInfo::ZERO` and constant-fold the magic check to
    /// `false`.
    #[used]
    #[unsafe(link_section = ".tatu.bootinfo")]
    static BOOTINFO: TatuBootInfo = TatuBootInfo::ZERO;

    /// 16-byte reset trampoline placed at 0xFFFFFFF0 by the linker
    /// script. Its section is `.tatu.reset.x86_64` (not the shared
    /// `.tatu.reset`) so the single `linker/tatu.ld` can route the x86
    /// stub to the architectural reset vector while the aarch64 stub
    /// folds into `.tatu.text`.
    ///
    /// SAFETY: STACK, BOOTINFO, and rust_main all live in the low
    /// 4 GiB of address space (the main image is in the low 1 MiB).
    /// `mov esp/edi, imm32` zero-extends into RSP/RDI
    /// (a long-mode rule for 32-bit destination registers).
    /// `push imm32` in long mode sign-extends to 64 bits and
    /// pushes 8 bytes; the main image's top bit is clear, so
    /// sign-extension matches zero-extension. `ret` pops 8 bytes
    /// into RIP, transferring control to rust_main with RSP back
    /// where `mov esp` left it. System V x86_64 ABI: first arg in
    /// RDI, so rust_main receives `bootinfo: &TatuBootInfo`. The
    /// stub is exactly 5 + 5 + 5 + 1 = 16 bytes — no padding nops.
    /// `#[unsafe(no_mangle)]` is the keep-alive anchor: rustc
    /// drops unreachable naked fns in `#![no_main]` binaries
    /// regardless of `pub` visibility, and `#[used]` doesn't
    /// apply to functions. no_mangle exposes the symbol as an
    /// external linkage point, which both prevents DCE and
    /// makes the name available to the linker's `ENTRY()`.
    #[unsafe(naked)]
    #[unsafe(no_mangle)]
    #[unsafe(link_section = ".tatu.reset.x86_64")]
    extern "C" fn reset() -> ! {
        core::arch::naked_asm!(
            "mov  esp, offset {stack} + {size}",
            "mov  edi, offset {bootinfo}",
            "push offset {main}",
            "ret",
            stack    = sym crate::sections::STACK,
            size     = const STACK_SIZE,
            bootinfo = sym BOOTINFO,
            main     = sym crate::rust_main,
        )
    }
}

// ---------------------------------------------------------------------------
// Halt: load null IDT + ud2 → #UD → #DF → triple fault.
// ---------------------------------------------------------------------------

#[inline(never)]
pub fn halt() -> ! {
    // SAFETY: lidt with a zero-limit descriptor disables all
    // exception handlers; ud2 then triggers #UD which escalates
    // via #DF to triple-fault → KVM_EXIT_SHUTDOWN. The vCPU
    // cannot resume.
    unsafe {
        let null_idtr: [u8; 10] = [0; 10];
        core::arch::asm!(
            "lidt [{p}]",
            p = in(reg) &null_idtr,
            options(nostack, preserves_flags),
        );
        core::arch::asm!("ud2", options(noreturn));
    }
}

// ---------------------------------------------------------------------------
// e820 entry list.
//
// DT→e820 translation (memory / reserved-memory / pci-host-ecam-generic)
// lives in `dtb2e820`; we re-export its types. tatu owns the storage
// (a stack-resident fixed array sized for §4.5 maxima) and prepends
// one ACPI-workspace entry (type 3) before delegating the rest of the
// slice to `ExtractE820::extract_e820`.
// ---------------------------------------------------------------------------

pub use dtb2e820::{E820Entry, E820Type, ExtractE820};

/// Cap on the number of e820 entries tatu ever emits. Sized for
/// `pmi/spec` §4.5 maxima (≤16 memory regions + a handful of reserved
/// + ECAM windows + 1 ACPI workspace entry).
pub const E820_MAX_ENTRIES: usize = 128;

// ---------------------------------------------------------------------------
// /chosen readers: CmdLine and Initrd are typed wrappers per
// ARCHITECTURE.md §12.6.
// ---------------------------------------------------------------------------

/// Kernel command-line pointer/length, derived from
/// `/chosen/bootargs`.
#[derive(Debug, Copy, Clone)]
pub struct CmdLine {
    ptr: u64,
}

impl CmdLine {
    pub fn from_dtb<T: TreeView>(tree: &T) -> Option<Self> {
        let chosen = tree.find_path("/chosen")?;
        let bootargs = chosen.property("bootargs")?;
        Some(Self {
            ptr: bootargs.as_ref().as_ptr() as u64,
        })
    }

    pub fn ptr(self) -> u64 {
        self.ptr
    }
}

/// Initial-ramdisk region, derived from `/chosen/linux,initrd-{start,end}`.
/// Absent when those properties aren't present.
#[derive(Debug, Copy, Clone)]
pub struct Initrd {
    gpa: u64,
    size: u64,
}

impl Initrd {
    pub fn from_dtb<T: TreeView>(tree: &T) -> Self {
        let Some(chosen) = tree.find_path("/chosen") else {
            return Self::none();
        };
        let (Some(start_p), Some(end_p)) = (
            chosen.property("linux,initrd-start"),
            chosen.property("linux,initrd-end"),
        ) else {
            return Self::none();
        };
        let (Some(start), Some(end)) = (read_uint(start_p.as_ref()), read_uint(end_p.as_ref()))
        else {
            return Self::none();
        };
        if end < start {
            return Self::none();
        }
        Self {
            gpa: start,
            size: end - start,
        }
    }

    pub const fn none() -> Self {
        Self { gpa: 0, size: 0 }
    }

    pub fn gpa(self) -> u64 {
        self.gpa
    }
    pub fn size(self) -> u64 {
        self.size
    }
}

fn read_uint(bytes: &[u8]) -> Option<u64> {
    match bytes.len() {
        4 => Some(u64::from(u32::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3],
        ]))),
        8 => Some(u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ])),
        _ => None,
    }
}

fn map_e820_err(e: dtb2e820::Error) -> ValidationError {
    match e {
        dtb2e820::Error::BufferFull => ValidationError::TooManyMemoryRegions,
        dtb2e820::Error::BadRegShape => ValidationError::BadPropertyShape,
    }
}

// ---------------------------------------------------------------------------
// Typed mirror of Linux's `struct boot_params` (the "zero page") and
// its nested `struct setup_header`. Field offsets are pinned by sized
// `_pad*` arrays — there is no `[u8; 4096]` byte slice underneath,
// so all writes from `fill` are typed field assignments, and
// `dtb2e820::populate` writes directly into the [`e820_table`]
// field rather than into an intermediate buffer.
//
// Both structs are byte-for-byte wire-compatible with Linux's
// definitions; `static_assert!(size_of::<LinuxBootParams>() == 4096)`
// catches layout drift.
// ---------------------------------------------------------------------------

/// `struct setup_header` from `arch/x86/include/uapi/asm/bootparam.h`,
/// pinned to the protocol-2.15 layout tatu targets. Only the fields
/// tatu writes are typed; the rest are padding. Lives at offset
/// `0x1F1` within [`LinuxBootParams`]; total size 127 bytes.
#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct SetupHeader {
    pub setup_sects: u8,
    _pad_to_boot_flag: [u8; 0x0D - 0x01],
    pub boot_flag: u16,
    _pad_to_header: [u8; 0x11 - 0x0F],
    /// Setup magic — "HdrS".
    pub header: u32,
    _pad_to_type_of_loader: [u8; 0x1F - 0x15],
    pub type_of_loader: u8,
    pub loadflags: u8,
    _pad_to_code32_start: [u8; 0x23 - 0x21],
    pub code32_start: u32,
    pub ramdisk_image: u32,
    pub ramdisk_size: u32,
    _pad_to_cmd_line_ptr: [u8; 0x37 - 0x2F],
    pub cmd_line_ptr: u32,
    _pad_to_end: [u8; 0x7F - 0x3B],
}

/// `struct boot_params` (Linux's "zero page"), 4096 bytes, with only
/// the fields tatu writes typed and the rest as named pads.
#[repr(C)]
pub struct LinuxBootParams {
    _pad_to_acpi_rsdp: [u8; 0x70],
    pub acpi_rsdp_addr: u64,
    _pad_to_e820_entries: [u8; 0x1E8 - 0x78],
    pub e820_entries: u8,
    _pad_to_setup_header: [u8; 0x1F1 - 0x1E9],
    pub setup_header: SetupHeader,
    _pad_to_e820_table: [u8; 0x2D0 - 0x270],
    pub e820_table: [E820Entry; E820_MAX_ENTRIES],
    _pad_to_end: [u8; 0x1000 - 0x2D0 - E820_MAX_ENTRIES * 20],
}

const _: () = assert!(core::mem::size_of::<LinuxBootParams>() == 4096);
const _: () = assert!(core::mem::size_of::<SetupHeader>() == 0x7F);

// Protocol constants stay associated with LinuxBootParams.
impl LinuxBootParams {
    pub const HEADER_MAGIC_VAL: u32 = 0x5372_6448; // "HdrS"
    pub const BOOT_FLAG_VAL: u16 = 0xAA55;
    pub const LOADED_HIGH: u8 = 0x01;
    pub const KEEP_SEGMENTS: u8 = 0x40;
    pub const CAN_USE_HEAP: u8 = 0x80;
    pub const LOADER_TYPE_TATU: u8 = 0xE0; // unallocated loader-type tag for tatu

    /// Range of the bzImage file that the Linux x86 boot protocol
    /// (§1.5) requires the loader to copy verbatim into the
    /// `setup_header` field.
    pub const BZIMAGE_SETUP_HEADER_RANGE: core::ops::Range<usize> = 0x1F1..0x270;

    pub fn zeroed() -> Self {
        // SAFETY: all-zero is a valid bit pattern for LinuxBootParams
        // (every field is a u8 array, scalar with `Default == 0`, or a
        // struct/array of the same). E820Entry's all-zero gives
        // `kind = Invalid`, which is the legal default.
        unsafe { core::mem::zeroed() }
    }

    pub fn as_ptr(&self) -> *const Self {
        self as *const _
    }

    /// Copy the bzImage's setup header verbatim into [`Self::setup_header`].
    /// Linux's boot protocol requires the loader to preserve every
    /// field the bzImage shipped (`version`, `xloadflags`,
    /// `kernel_alignment`, `init_size`, `pref_address`, ...) — with
    /// any of these zero, the kernel relocates outside the e820 map
    /// and triple-faults.
    pub fn copy_setup_header_from_bzimage(&mut self, kernel_bytes: &[u8]) {
        let src = &kernel_bytes[Self::BZIMAGE_SETUP_HEADER_RANGE];
        // SAFETY: the bzImage bytes at this range are (per Linux's
        // x86 boot protocol) the wire layout of `struct setup_header`.
        // `SetupHeader` mirrors that layout byte-for-byte (repr(C,
        // packed), 127 bytes), so reading the bytes as a SetupHeader
        // value is valid. `read_unaligned` handles any byte alignment.
        self.setup_header =
            unsafe { core::ptr::read_unaligned(src.as_ptr().cast::<SetupHeader>()) };
    }

    /// Fill the e820 table from `tree` + `acpi`, writing directly
    /// into [`Self::e820_table`] with no intermediate buffer.
    pub fn fill_e820<T: TreeView>(
        &mut self,
        tree: &T,
        acpi: &AcpiTables,
    ) -> Result<(), ValidationError> {
        // Slot 0: ACPI workspace entry (type 3). Linux preserves
        // exactly this range; the surrounding stack reverts to
        // type-1 (reclaimable) once Linux processes the e820 map.
        *self
            .e820_table
            .get_mut(0)
            .ok_or(ValidationError::TooManyMemoryRegions)? = E820Entry {
            addr: acpi.rsdp_gpa(),
            size: acpi.size(),
            kind: E820Type::Acpi,
        };
        // dtb2e820 writes directly into the boot_params storage.
        let dt_slice = self
            .e820_table
            .get_mut(1..)
            .ok_or(ValidationError::TooManyMemoryRegions)?;
        let n = tree.extract_e820(dt_slice).map_err(map_e820_err)?;
        self.e820_entries =
            u8::try_from(n.saturating_add(1)).map_err(|_| ValidationError::TooManyMemoryRegions)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn fill(
        &mut self,
        kernel_gpa: u64,
        kernel_bytes: &[u8],
        cmdline: &CmdLine,
        initrd: &Initrd,
        acpi: &AcpiTables,
        tree: &impl TreeView,
    ) -> Result<(), ValidationError> {
        self.copy_setup_header_from_bzimage(kernel_bytes);

        let setup_sects = {
            // setup_sects defaults to 4 if the bzImage shipped 0.
            let v = self.setup_header.setup_sects;
            if v == 0 { 4 } else { v }
        };
        // code32_start = kernel_gpa + (setup_sects+1)*512.
        let setup_bytes = u32::from(setup_sects).saturating_add(1) * 512;
        let code32 = u32::try_from(kernel_gpa.saturating_add(u64::from(setup_bytes)))
            .map_err(|_| ValidationError::AddressOverflow)?;
        let cmd_line_ptr =
            u32::try_from(cmdline.ptr()).map_err(|_| ValidationError::AddressOverflow)?;
        let ramdisk_image =
            u32::try_from(initrd.gpa()).map_err(|_| ValidationError::AddressOverflow)?;
        let ramdisk_size =
            u32::try_from(initrd.size()).map_err(|_| ValidationError::AddressOverflow)?;

        // Required boot-protocol magic + tatu identification. These
        // overlap with values the bzImage header copy may already
        // have set (HEADER_MAGIC, BOOT_FLAG); re-writing them
        // documents the loader's required values.
        self.setup_header.setup_sects = setup_sects;
        self.setup_header.boot_flag = Self::BOOT_FLAG_VAL;
        self.setup_header.header = Self::HEADER_MAGIC_VAL;
        self.setup_header.type_of_loader = Self::LOADER_TYPE_TATU;
        self.setup_header.loadflags = Self::LOADED_HIGH | Self::KEEP_SEGMENTS | Self::CAN_USE_HEAP;
        self.setup_header.code32_start = code32;
        self.setup_header.ramdisk_image = ramdisk_image;
        self.setup_header.ramdisk_size = ramdisk_size;
        self.setup_header.cmd_line_ptr = cmd_line_ptr;
        self.acpi_rsdp_addr = acpi.rsdp_gpa();

        self.fill_e820(tree, acpi)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// AcpiTables — typed wrapper around the ACPI workspace after
// dtb2acpi has populated it. Carries the RSDP GPA.
// ---------------------------------------------------------------------------

pub struct AcpiTables {
    rsdp_gpa: u64,
}

impl AcpiTables {
    /// Populate the caller-supplied ACPI buffer (a stack-local in
    /// `finalize`). The buffer's GPA is page-aligned (via `Paged`);
    /// the returned `AcpiTables` records it so e820 + boot_params
    /// get the right RSDP address.
    pub fn generate<T: TreeView>(tree: &T, buf: &mut AcpiBuf) -> Result<Self, ValidationError> {
        let gpa = buf as *const AcpiBuf as u64;
        buf.0
            .populate(tree, &OEM, gpa)
            .map_err(|_| ValidationError::BadPropertyShape)?;
        Ok(Self { rsdp_gpa: gpa })
    }

    pub fn rsdp_gpa(&self) -> u64 {
        self.rsdp_gpa
    }

    pub fn size(&self) -> u64 {
        ACPI_WORKSPACE_SIZE as u64
    }
}

// ---------------------------------------------------------------------------
// build_boot_params — composes LinuxBootParams from the typed inputs
// per ARCHITECTURE.md §12.6 step 8d.
// ---------------------------------------------------------------------------

pub fn build_boot_params<T: TreeView>(
    kernel_bytes: &[u8],
    cmdline: &CmdLine,
    initrd: &Initrd,
    acpi: &AcpiTables,
    tree: &T,
) -> Result<LinuxBootParams, ValidationError> {
    let kernel_gpa = kernel_bytes.as_ptr() as u64;
    let mut bp = LinuxBootParams::zeroed();
    bp.fill(kernel_gpa, kernel_bytes, cmdline, initrd, acpi, tree)?;
    Ok(bp)
}

// ---------------------------------------------------------------------------
// Kernel handoff. Reads setup_sects from the populated boot_params,
// computes the 64-bit entry point, loads rsi with &boot_params,
// jmps. Never returns.
// ---------------------------------------------------------------------------

/// Architecturally final operation: load `&boot_params` into RSI
/// and jump to the kernel's 64-bit entry point. Never returns.
pub fn boot_kernel(kernel_bytes: &[u8], bp: LinuxBootParams) -> ! {
    // Copy out the packed field by value — `&bp.setup_header.setup_sects`
    // would be UB on the packed struct.
    let setup_sects: u8 = bp.setup_header.setup_sects;
    let entry = (kernel_bytes.as_ptr() as u64)
        .saturating_add(u64::from(setup_sects).saturating_add(1).saturating_mul(512))
        .saturating_add(0x200);
    let bp_ptr = bp.as_ptr() as u64;

    // SAFETY: this is the architecturally final instruction tatu
    // executes. Per Linux x86 boot protocol the kernel receives
    // boot_params in `%rsi` and execution at the 64-bit entry
    // point. After this jmp, tatu's code is no longer running.
    unsafe {
        core::arch::asm!(
            "mov rsi, {bp}",
            "jmp {entry}",
            bp = in(reg) bp_ptr,
            entry = in(reg) entry,
            options(noreturn),
        );
    }
}
