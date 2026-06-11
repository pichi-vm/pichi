//! Kernel input handling: arch inference + format validation.
//!
//! Arma accepts only direct-boot kernel formats that tatu can hand
//! off to:
//!
//! - **x86_64**: a Linux bzImage (HdrS magic at offset 0x202, boot
//!   protocol >= 2.12, LOADED_HIGH set). Passed through whole; tatu
//!   reads `setup_sects` at runtime to compute the 64-bit entry.
//! - **aarch64**: a raw arm64 `Image` (ARM\x64 magic at offset 56).
//!   Passed through whole; tatu jumps to offset 0. An arm64 EFI-zboot
//!   wrapper (`CONFIG_EFI_ZBOOT`: `MZ` + `zimg`, a gzip-compressed Image —
//!   the form distro `vmlinuz` ships, e.g. Alpine `vmlinuz-virt`) is
//!   unwrapped to its raw Image first; see [`unwrap_zboot`].
//!
//! A PE-wrapped Linux `vmlinuz.efi` that is *not* zboot is rejected with a
//! hint to extract the raw Image.

use std::io::Read;

use flate2::read::GzDecoder;
use goblin::elf::{Elf, header::EM_X86_64, program_header::PT_LOAD};
use thiserror::Error;

/// Target guest architecture, inferred from the kernel file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    /// PE `FileHeader.Machine` value for this arch.
    pub(crate) const fn pe_machine(self) -> u16 {
        match self {
            Arch::X86_64 => 0x8664,
            Arch::Aarch64 => 0xAA64,
        }
    }
}

/// Errors from kernel parsing.
#[derive(Debug, Error)]
pub(crate) enum KernelError {
    #[error("kernel file too small ({0} bytes) to determine format")]
    TooSmall(usize),

    #[error(
        "kernel format not recognized. Expected a raw ELF vmlinux (x86), an arm64 Image \
         (ARM\\x64 at offset 56), or a gzip bzImage/EFI-zboot wrapper arma can unwrap first."
    )]
    Unrecognized,

    #[error(
        "EFI-zboot payload out of bounds (offset {offset:#x}, size {size:#x}, file {file} bytes)"
    )]
    ZbootMalformed {
        offset: usize,
        size: usize,
        file: usize,
    },

    #[error("EFI-zboot uses unsupported compression {0:?}; only gzip and zstd are supported")]
    ZbootCompression(String),

    #[error("EFI-zboot payload decompression failed: {0}")]
    ZbootDecompress(String),

    #[error("ELF kernel malformed: {0}")]
    ElfMalformed(&'static str),

    #[error("ELF kernel entry {entry:#x} is outside the loaded image (base {min_paddr:#x})")]
    ElfEntryOutOfRange { entry: u64, min_paddr: u64 },

    #[error(
        "bzImage (vmlinuz) carries no gzip-compressed vmlinux arma can extract; \
         only gzip-compressed kernels are supported — pass a raw vmlinux instead"
    )]
    BzImageNoVmlinux,
}

// bzImage "HdrS" magic at 0x202 — used only to detect a bzImage so
// [`extract_vmlinux`] can unwrap its embedded ELF; arma never boots a bzImage
// directly (tatu only ever sees the unwrapped vmlinux).
const BZIMAGE_HDRS_MAGIC: u32 = 0x5372_6448; // "HdrS" LE at 0x202
const BZIMAGE_HDRS_OFFSET: usize = 0x202;

const ARM64_IMAGE_MAGIC: u32 = 0x644D_5241; // "ARM\x64" LE at offset 56
const ARM64_IMAGE_MAGIC_OFFSET: usize = 56;
const ARM64_IMAGE_SIZE_OFFSET: usize = 16; // u64 LE: effective image size (text + BSS)

// arm64 EFI-zboot header (`CONFIG_EFI_ZBOOT`). Offsets verified against a
// real Alpine `vmlinuz-virt`: "MZ" at 0, "zimg" at 4, u32 payload offset at
// 8, u32 payload size at 12, NUL-padded compression name at 24.
const ZBOOT_ZIMG_OFFSET: usize = 4;
const ZBOOT_PAYLOAD_OFFSET_FIELD: usize = 8;
const ZBOOT_PAYLOAD_SIZE_FIELD: usize = 12;
const ZBOOT_COMP_OFFSET: usize = 24;
const ZBOOT_COMP_LEN: usize = 32;
const ZBOOT_HEADER_MIN: usize = ZBOOT_COMP_OFFSET + ZBOOT_COMP_LEN; // 56

/// ELF magic (`\x7fELF`) at offset 0 — cheap discriminator before handing the
/// buffer to goblin (which would error on a non-ELF bzImage / arm64 Image).
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];

/// Result of parsing a kernel file. A bzImage is converted to its embedded ELF
/// by [`extract_vmlinux`] *before* `parse`, so `parse` only ever sees a raw ELF
/// `vmlinux` (x86) or an arm64 `Image`.
#[derive(Debug, Clone)]
pub(crate) struct Parsed {
    pub(crate) arch: Arch,
    /// aarch64 only: the Image header's `image_size` (text + BSS) — the RAM
    /// the kernel needs at runtime, which exceeds the file when the BSS isn't
    /// in the file. `0` if the header leaves it unspecified. The `.linux`
    /// footprint must be `max(file_size, image_size)` or the BSS is unbacked.
    pub(crate) aarch64_image_size: Option<u64>,
    /// x86 only: present when the input is a raw ELF `vmlinux`. arma lowers the
    /// ELF to a flat loaded-segment image (see [`elf_load_image`]) so tatu only
    /// ever places bytes and jumps — it never parses ELF.
    pub(crate) elf: Option<ElfMeta>,
}

/// ELF `vmlinux` placement metadata, derived from the program headers.
///
/// A `CONFIG_RELOCATABLE` kernel entered at `startup_64` runs at any 2 MiB
/// aligned physical base via `phys_base` *without* relocations, so long as the
/// virtual link base is unchanged. arma therefore loads the whole segment span
/// contiguously at a planner-chosen base; KASLR (a randomized base + applied
/// relocations) is a later phase.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ElfMeta {
    /// Runtime RAM footprint: `max(p_paddr + p_memsz) - min(p_paddr)`, the
    /// VirtualSize of the `.linux` section (file-backed prefix + BSS tail).
    pub(crate) alloc_size: u64,
    /// Lowest `p_paddr` = the kernel's link (load) physical address. A vmlinux
    /// entered at `startup_64` computes `phys_base = load - link`; loading below
    /// `link` underflows it and the kernel faults. arma therefore floors the
    /// `.linux` GPA to this (the ELF analog of bzImage `pref_address`), so the
    /// common case loads exactly here with `phys_base == 0`.
    pub(crate) min_paddr: u64,
    /// `e_entry - min_paddr`: the entry's byte offset within the loaded image.
    /// The entry need not be the image base (firecracker's is; Alpine's is far
    /// in), so arma records it and tatu jumps to `kernel_gpa + entry_offset`.
    pub(crate) entry_offset: u64,
}

/// Detect and validate the guest architecture from a kernel image's
/// header bytes. Arma never holds onto the kernel bytes itself; the
/// caller passes the original buffer through to layout and PE
/// emission.
pub(crate) fn parse(bytes: &[u8]) -> Result<Parsed, KernelError> {
    if bytes.len() < 64 {
        return Err(KernelError::TooSmall(bytes.len()));
    }

    // Raw ELF `vmlinux` (x86): magic at offset 0 is unambiguous — neither a
    // bzImage (real-mode boot code) nor an arm64 Image (a branch) starts with
    // it. arma lowers the ELF to a flat loaded image; tatu never parses ELF.
    if bytes[0..4] == ELF_MAGIC {
        let elf =
            Elf::parse(bytes).map_err(|_| KernelError::ElfMalformed("goblin parse failed"))?;
        if !elf.is_64 || elf.header.e_machine != EM_X86_64 {
            return Err(KernelError::ElfMalformed("not a 64-bit x86-64 ELF"));
        }
        let span = elf_span(&elf)?;
        let entry_offset = elf
            .entry
            .checked_sub(span.min_paddr)
            .filter(|&off| off < span.max_mem_end - span.min_paddr)
            .ok_or(KernelError::ElfEntryOutOfRange {
                entry: elf.entry,
                min_paddr: span.min_paddr,
            })?;
        return Ok(Parsed {
            arch: Arch::X86_64,
            aarch64_image_size: None,
            elf: Some(ElfMeta {
                alloc_size: span.max_mem_end - span.min_paddr,
                min_paddr: span.min_paddr,
                entry_offset,
            }),
        });
    }

    // Check arm64 Image first (cheap, single u32 at fixed offset).
    let arm64_magic = u32::from_le_bytes(
        bytes[ARM64_IMAGE_MAGIC_OFFSET..ARM64_IMAGE_MAGIC_OFFSET + 4]
            .try_into()
            .expect("slice is 4 bytes"),
    );
    if arm64_magic == ARM64_IMAGE_MAGIC {
        let image_size = u64::from_le_bytes(
            bytes[ARM64_IMAGE_SIZE_OFFSET..ARM64_IMAGE_SIZE_OFFSET + 8]
                .try_into()
                .expect("slice is 8 bytes"),
        );
        return Ok(Parsed {
            arch: Arch::Aarch64,
            aarch64_image_size: Some(image_size),
            elf: None,
        });
    }

    // A bzImage reaching `parse` is a bug: `extract_vmlinux` runs first and
    // converts it to its embedded ELF (handled above), so anything that still
    // has the "HdrS" magic here was not gzip-unwrappable.
    Err(KernelError::Unrecognized)
}

/// The physical span of an ELF's loadable segments.
struct ElfSpan {
    /// Lowest `p_paddr` across PT_LOAD segments — the image's load base.
    min_paddr: u64,
    /// Highest `p_paddr + p_filesz` — end of the file-backed prefix.
    max_file_end: u64,
    /// Highest `p_paddr + p_memsz` — end of the runtime image (incl. BSS).
    max_mem_end: u64,
}

/// Compute the loadable span over all non-empty `PT_LOAD` segments.
fn elf_span(elf: &Elf<'_>) -> Result<ElfSpan, KernelError> {
    let mut min_paddr = u64::MAX;
    let mut max_file_end = 0u64;
    let mut max_mem_end = 0u64;
    let mut any = false;
    for p in &elf.program_headers {
        if p.p_type != PT_LOAD || p.p_memsz == 0 {
            continue;
        }
        any = true;
        min_paddr = min_paddr.min(p.p_paddr);
        let file_end = p
            .p_paddr
            .checked_add(p.p_filesz)
            .ok_or(KernelError::ElfMalformed("p_paddr + p_filesz overflows"))?;
        let mem_end = p
            .p_paddr
            .checked_add(p.p_memsz)
            .ok_or(KernelError::ElfMalformed("p_paddr + p_memsz overflows"))?;
        max_file_end = max_file_end.max(file_end);
        max_mem_end = max_mem_end.max(mem_end);
    }
    if !any {
        return Err(KernelError::ElfMalformed("no PT_LOAD segments"));
    }
    Ok(ElfSpan {
        min_paddr,
        max_file_end,
        max_mem_end,
    })
}

/// Lower an ELF `vmlinux` to its flat loaded-segment image: each `PT_LOAD`'s
/// file bytes copied to `p_paddr - min_paddr`, gaps zero-filled. The BSS tail
/// (`max_mem_end - max_file_end`) is left to the `.linux` section's VirtualSize,
/// which the VMM zero-fills. The result is what tatu places at the planner's
/// kernel GPA and jumps into at offset 0 — no ELF awareness in tatu.
pub(crate) fn elf_load_image(bytes: &[u8]) -> Result<Vec<u8>, KernelError> {
    let elf = Elf::parse(bytes).map_err(|_| KernelError::ElfMalformed("goblin parse failed"))?;
    let span = elf_span(&elf)?;
    let len = usize::try_from(span.max_file_end - span.min_paddr)
        .map_err(|_| KernelError::ElfMalformed("loaded image exceeds usize"))?;
    let mut image = vec![0u8; len];
    for p in &elf.program_headers {
        if p.p_type != PT_LOAD || p.p_filesz == 0 {
            continue;
        }
        let dst = (p.p_paddr - span.min_paddr) as usize;
        let src = p.p_offset as usize;
        let n = p.p_filesz as usize;
        let src_end =
            src.checked_add(n)
                .filter(|&e| e <= bytes.len())
                .ok_or(KernelError::ElfMalformed(
                    "p_offset + p_filesz out of file bounds",
                ))?;
        image[dst..dst + n].copy_from_slice(&bytes[src..src_end]);
    }
    Ok(image)
}

/// x86 KASLR relocation tables extracted from a `CONFIG_X86_NEED_RELOCS` vmlinux,
/// expressed as **byte offsets into the flat loaded image** ([`elf_load_image`]).
///
/// For KASLR, tatu picks a random *virtual* base offset (`delta`) and adjusts
/// every absolute reference into the kernel image accordingly. There are three
/// tables, exactly as the kernel's own decompressor consumes
/// (`arch/x86/boot/compressed/misc.c::handle_relocations`), built by the same
/// filter as `arch/x86/tools/relocs.c` (`walk_relocs` + `do_reloc64`, v6.1):
///
/// - [`relocs64`](Self::relocs64): `*(u64*)p += delta`
/// - [`relocs32`](Self::relocs32): `*(u32*)p += delta`
/// - [`relocs32neg`](Self::relocs32neg): `*(i32*)p -= delta` (PC-relative refs to
///   per-CPU symbols, whose displacement shrinks as the referent base grows)
///
/// The kernel runs at any *physical* base for free (`phys_base`); these
/// relocations are what make the *virtual* base randomizable.
#[derive(Debug, Clone, Default)]
pub(crate) struct Relocs {
    /// Image offsets of `R_X86_64_32`/`32S` sites (patch 4 bytes: `*p += delta`).
    pub(crate) relocs32: Vec<u32>,
    /// Image offsets of per-CPU PC-relative sites (patch 4 bytes: `*p -= delta`).
    pub(crate) relocs32neg: Vec<u32>,
    /// Image offsets of `R_X86_64_64` sites (patch 8 bytes: `*p += delta`).
    pub(crate) relocs64: Vec<u32>,
}

impl Relocs {
    /// Serialize as the PMI relocs-section payload: the three `u32`-LE arrays
    /// back to back in the order `relocs64`, `relocs32neg`, `relocs32`. Each
    /// array's length is carried separately in `TatuBootInfo`, so tatu can slice
    /// the regions apart. Returns an empty vector when there are no relocations.
    pub(crate) fn to_section_bytes(&self) -> Vec<u8> {
        let total = self.relocs64.len() + self.relocs32neg.len() + self.relocs32.len();
        let mut v = Vec::with_capacity(total * 4);
        for &o in self
            .relocs64
            .iter()
            .chain(&self.relocs32neg)
            .chain(&self.relocs32)
        {
            v.extend_from_slice(&o.to_le_bytes());
        }
        v
    }
}

// x86-64 relocation types arma must classify (see relocs.c::do_reloc64).
const R_X86_64_NONE: u32 = 0;
const R_X86_64_64: u32 = 1;
const R_X86_64_PC32: u32 = 2;
const R_X86_64_PLT32: u32 = 4;
const R_X86_64_32: u32 = 10;
const R_X86_64_32S: u32 = 11;
const R_X86_64_PC64: u32 = 24;
const R_X86_64_REX_GOTPCRELX: u32 = 42;

const SHN_UNDEF: usize = 0;
const SHN_ABS: usize = 0xfff1;
const SHN_XINDEX: usize = 0xffff;
const SHT_NOTE: u32 = 7;
const SHF_ALLOC: u64 = 0x2;

/// Symbols the linker marks absolute but which still move with the kernel image
/// (defined outside any section in the linker script). References to these *must*
/// be relocated for KASLR — this is `relocs.c`'s `S_REL` whitelist (v6.1). Any
/// other `SHN_ABS` symbol is a true constant (`S_ABS`) and is skipped; `relocs.c`
/// would `die()` on a non-whitelisted absolute reloc, so a valid vmlinux has none.
fn is_s_rel(name: &str) -> bool {
    // `__(start|end)_pci_.*` and `init_per_cpu__.*` — prefix matches.
    if name.starts_with("__start_pci_")
        || name.starts_with("__end_pci_")
        || name.starts_with("init_per_cpu__")
    {
        return true;
    }
    matches!(
        name,
        "__init_begin"
            | "__init_end"
            | "__x86_cpu_dev_start"
            | "__x86_cpu_dev_end"
            | "__parainstructions"
            | "__parainstructions_end"
            | "__alt_instructions"
            | "__alt_instructions_end"
            | "__iommu_table"
            | "__iommu_table_end"
            | "__apicdrivers"
            | "__apicdrivers_end"
            | "__smp_locks"
            | "__smp_locks_end"
            | "__start_builtin_fw"
            | "__end_builtin_fw"
            | "__start___ksymtab"
            | "__stop___ksymtab"
            | "__start___ksymtab_gpl"
            | "__stop___ksymtab_gpl"
            | "__start___kcrctab"
            | "__stop___kcrctab"
            | "__start___kcrctab_gpl"
            | "__stop___kcrctab_gpl"
            | "__start___param"
            | "__stop___param"
            | "__start___modver"
            | "__stop___modver"
            | "__start___bug_table"
            | "__stop___bug_table"
            | "__tracedata_start"
            | "__tracedata_end"
            | "__start_notes"
            | "__stop_notes"
            | "__end_rodata"
            | "__end_rodata_aligned"
            | "__initramfs_start"
            | "jiffies"
            | "jiffies_64"
            | "__per_cpu_load"
            | "__end_rodata_hpage_align"
            | "__vvar_page"
            | "_end"
    )
}

/// The `.data..percpu` section state needed to relocate per-CPU references, per
/// `relocs.c::percpu_init`. For an SMP kernel the section is linked at VMA 0 and
/// loaded at `__per_cpu_load`; reloc offsets within it are rebased by that LMA,
/// and references *to* per-CPU symbols are handled specially (skipped, or pushed
/// to `relocs32neg`). A non-SMP kernel (section VMA != 0) has no special case.
struct PerCpu {
    /// Section index of `.data..percpu`, or `None` for a non-SMP kernel.
    shndx: Option<usize>,
    /// `st_value` of `__per_cpu_load` (the section's load address).
    load_addr: u64,
}

/// Locate the `.data..percpu` section and `__per_cpu_load` symbol (`percpu_init`).
fn find_percpu(elf: &Elf<'_>) -> Result<PerCpu, KernelError> {
    for (i, sh) in elf.section_headers.iter().enumerate() {
        if elf.shdr_strtab.get_at(sh.sh_name) != Some(".data..percpu") {
            continue;
        }
        if sh.sh_addr != 0 {
            return Ok(PerCpu {
                shndx: None,
                load_addr: 0,
            }); // non-SMP: no special handling
        }
        let load_addr = elf
            .syms
            .iter()
            .find(|s| elf.strtab.get_at(s.st_name) == Some("__per_cpu_load"))
            .map(|s| s.st_value)
            .ok_or(KernelError::ElfMalformed(
                "percpu section but no __per_cpu_load",
            ))?;
        return Ok(PerCpu {
            shndx: Some(i),
            load_addr,
        });
    }
    Ok(PerCpu {
        shndx: None,
        load_addr: 0,
    })
}

/// Whether a symbol lies in the per-CPU section and is a genuine per-CPU variable
/// (not one of the linker-misattributed boundary/init symbols). Mirrors
/// `relocs.c::is_percpu_sym`.
fn is_percpu_sym(st_shndx: usize, name: &str, percpu: &PerCpu) -> bool {
    percpu.shndx == Some(st_shndx)
        && name != "__init_begin"
        && name != "__per_cpu_load"
        && !name.starts_with("init_per_cpu_")
}

/// Extract the KASLR relocation tables from an x86 `vmlinux` ELF.
///
/// Faithfully ports `arch/x86/tools/relocs.c` (v6.1): walk every `SHT_RELA`
/// section whose applied section (`sh_info`) is `SHF_ALLOC` and not a NOTE;
/// rebase per-CPU offsets by `__per_cpu_load`; classify each entry by type
/// (`do_reloc64`) — skipping PC-relative and absolute-constant sites, routing
/// per-CPU PC-relative refs to `relocs32neg`, and recording the rest. Offsets are
/// stored as positions in the flat loaded image: `VA - text_vaddr`, the same
/// translation the kernel applies (`stored_VA - __START_KERNEL_map -
/// LOAD_PHYSICAL_ADDR`). A `CONFIG_RELOCATABLE` kernel built with `--emit-relocs`
/// (`CONFIG_X86_NEED_RELOCS=y`) retains the `.rela.*` sections this needs.
pub(crate) fn extract_relocs(bytes: &[u8]) -> Result<Relocs, KernelError> {
    let elf = Elf::parse(bytes).map_err(|_| KernelError::ElfMalformed("goblin parse failed"))?;
    let span = elf_span(&elf)?;
    let percpu = find_percpu(&elf)?;

    // The kernel image's link virtual base = p_vaddr of the segment at min_paddr.
    // Every reloc location VA maps to image offset `VA - text_vaddr`.
    let text_vaddr = elf
        .program_headers
        .iter()
        .find(|p| p.p_type == PT_LOAD && p.p_memsz != 0 && p.p_paddr == span.min_paddr)
        .map(|p| p.p_vaddr)
        .ok_or(KernelError::ElfMalformed("no PT_LOAD at min_paddr"))?;

    let to_image_off = |va: u64| -> Result<u32, KernelError> {
        va.checked_sub(text_vaddr)
            .and_then(|o| u32::try_from(o).ok())
            .ok_or(KernelError::ElfMalformed("reloc location outside image"))
    };

    let mut out = Relocs::default();

    for (sec_idx, rels) in &elf.shdr_relocs {
        // Section these relocations apply to. Skip non-allocated (debug, symtab)
        // and NOTE sections, exactly as relocs.c::walk_relocs.
        let applies_idx = elf.section_headers[*sec_idx].sh_info as usize;
        let applies = elf
            .section_headers
            .get(applies_idx)
            .ok_or(KernelError::ElfMalformed("reloc sh_info out of range"))?;
        if applies.sh_flags & SHF_ALLOC == 0 || applies.sh_type == SHT_NOTE {
            continue;
        }
        // Relocs applied within the per-CPU section carry section-relative
        // offsets (VMA 0); rebase them to the load address.
        let applies_percpu = percpu.shndx == Some(applies_idx);

        for rel in rels.iter() {
            let sym = elf
                .syms
                .get(rel.r_sym)
                .ok_or(KernelError::ElfMalformed("reloc symbol index out of range"))?;
            if sym.st_shndx == SHN_UNDEF {
                continue;
            }
            if sym.st_shndx == SHN_XINDEX {
                return Err(KernelError::ElfMalformed("SHN_XINDEX symbols unsupported"));
            }
            let name = elf.strtab.get_at(sym.st_name).unwrap_or("");
            let shn_abs = sym.st_shndx == SHN_ABS && !is_s_rel(name);
            let percpu_sym = is_percpu_sym(sym.st_shndx, name, &percpu);

            let offset = if applies_percpu {
                rel.r_offset.wrapping_add(percpu.load_addr)
            } else {
                rel.r_offset
            };

            match rel.r_type {
                R_X86_64_NONE => {}
                R_X86_64_PC32 | R_X86_64_PLT32 | R_X86_64_REX_GOTPCRELX => {
                    // PC-relative: only per-CPU refs need adjusting (inverse).
                    if percpu_sym {
                        out.relocs32neg.push(to_image_off(offset)?);
                    }
                }
                R_X86_64_PC64 => {
                    if percpu_sym {
                        return Err(KernelError::ElfMalformed(
                            "R_X86_64_PC64 against per-CPU symbol",
                        ));
                    }
                }
                R_X86_64_64 | R_X86_64_32 | R_X86_64_32S => {
                    // References into the per-CPU area use GS-relative addressing
                    // at runtime; their stored value is an offset, not a VA.
                    if percpu_sym {
                        continue;
                    }
                    // SHN_ABS-and-not-S_REL is a true constant (S_ABS): no move.
                    if shn_abs {
                        continue;
                    }
                    let off = to_image_off(offset)?;
                    if rel.r_type == R_X86_64_64 {
                        out.relocs64.push(off);
                    } else {
                        out.relocs32.push(off);
                    }
                }
                _ => {
                    return Err(KernelError::ElfMalformed(
                        "unsupported x86-64 relocation type",
                    ));
                }
            }
        }
    }

    Ok(out)
}

/// Unwrap an arm64 EFI-zboot kernel to its raw `Image`.
///
/// `CONFIG_EFI_ZBOOT` kernels are a tiny EFI stub (`MZ`) tagged `zimg` at
/// offset 4, wrapping a compressed raw `Image`. Distro arm64 `vmlinuz`
/// ships this form (e.g. Alpine `vmlinuz-virt`). Input that is not zboot
/// passes through unchanged, so this is safe to call on any kernel before
/// [`parse`]. gzip (Alpine `vmlinuz-virt`) and zstd (Fedora arm64 `vmlinuz`)
/// are handled; other compression schemes are an error rather than a silent
/// pass.
pub(crate) fn unwrap_zboot(input: Vec<u8>) -> Result<Vec<u8>, KernelError> {
    let is_zboot = input.len() >= ZBOOT_HEADER_MIN
        && &input[0..2] == b"MZ"
        && &input[ZBOOT_ZIMG_OFFSET..ZBOOT_ZIMG_OFFSET + 4] == b"zimg";
    if !is_zboot {
        return Ok(input);
    }

    let offset = u32::from_le_bytes(
        input[ZBOOT_PAYLOAD_OFFSET_FIELD..ZBOOT_PAYLOAD_OFFSET_FIELD + 4]
            .try_into()
            .expect("slice is 4 bytes"),
    ) as usize;
    let size = u32::from_le_bytes(
        input[ZBOOT_PAYLOAD_SIZE_FIELD..ZBOOT_PAYLOAD_SIZE_FIELD + 4]
            .try_into()
            .expect("slice is 4 bytes"),
    ) as usize;
    let end = offset
        .checked_add(size)
        .filter(|&e| e <= input.len())
        .ok_or(KernelError::ZbootMalformed {
            offset,
            size,
            file: input.len(),
        })?;

    let comp_field = &input[ZBOOT_COMP_OFFSET..ZBOOT_COMP_OFFSET + ZBOOT_COMP_LEN];
    let comp_len = comp_field
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(comp_field.len());
    let comp = &comp_field[..comp_len];
    let payload = &input[offset..end];
    let mut image = Vec::new();
    match comp {
        b"gzip" => {
            GzDecoder::new(payload)
                .read_to_end(&mut image)
                .map_err(|e| KernelError::ZbootDecompress(e.to_string()))?;
        }
        b"zstd" => {
            let mut dec = ruzstd::decoding::StreamingDecoder::new(payload)
                .map_err(|e| KernelError::ZbootDecompress(e.to_string()))?;
            dec.read_to_end(&mut image)
                .map_err(|e| KernelError::ZbootDecompress(e.to_string()))?;
        }
        _ => {
            return Err(KernelError::ZbootCompression(
                String::from_utf8_lossy(comp).into_owned(),
            ));
        }
    }
    Ok(image)
}

/// Convert a bzImage (`vmlinuz`) to its embedded ELF `vmlinux`.
///
/// arma keeps a single x86 boot path (ELF) and converts a bzImage to it here,
/// so tatu only ever receives a `vmlinux`. The bzImage embeds the kernel as a
/// gzip-compressed ELF after its real-mode setup; we scan for the gzip magic
/// and inflate each candidate, returning the first whose output is an ELF. A
/// non-bzImage (already a raw vmlinux, or an arm64 Image) passes through
/// unchanged, so this is safe to call on any kernel before [`parse`]. Only
/// gzip is handled (what the kernels arma targets use).
pub(crate) fn extract_vmlinux(input: Vec<u8>) -> Result<Vec<u8>, KernelError> {
    // bzImage? "HdrS" at 0x202. Anything else (raw vmlinux ELF, arm64 Image)
    // is already in a form `parse` handles.
    let is_bzimage = input.len() >= BZIMAGE_HDRS_OFFSET + 4
        && u32::from_le_bytes(
            input[BZIMAGE_HDRS_OFFSET..BZIMAGE_HDRS_OFFSET + 4]
                .try_into()
                .expect("slice is 4 bytes"),
        ) == BZIMAGE_HDRS_MAGIC;
    if !is_bzimage {
        return Ok(input);
    }

    const GZIP_MAGIC: [u8; 3] = [0x1f, 0x8b, 0x08];
    let mut from = 0usize;
    while let Some(rel) = input[from..]
        .windows(GZIP_MAGIC.len())
        .position(|w| w == GZIP_MAGIC)
    {
        let pos = from + rel;
        let mut out = Vec::new();
        if GzDecoder::new(&input[pos..]).read_to_end(&mut out).is_ok()
            && out.len() >= 4
            && out[0..4] == ELF_MAGIC
        {
            return Ok(out);
        }
        from = pos + 1; // spurious magic in setup/payload — keep scanning
    }
    Err(KernelError::BzImageNoVmlinux)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_arm64_image() -> Vec<u8> {
        let mut v = vec![0u8; 256];
        v[ARM64_IMAGE_MAGIC_OFFSET..ARM64_IMAGE_MAGIC_OFFSET + 4]
            .copy_from_slice(&ARM64_IMAGE_MAGIC.to_le_bytes());
        v
    }

    #[test]
    fn detects_arm64_image() {
        let bytes = make_arm64_image();
        let p = parse(&bytes).unwrap();
        assert_eq!(p.arch, Arch::Aarch64);
        assert!(p.elf.is_none());
    }

    // Wrap an already-compressed `payload` in an EFI-zboot header tagged with
    // `comp` (the NUL-padded compression name at offset 24).
    fn make_zboot_raw(payload: &[u8], comp: &[u8]) -> Vec<u8> {
        let offset = ZBOOT_HEADER_MIN; // payload right after the header
        let mut v = vec![0u8; offset];
        v[0..2].copy_from_slice(b"MZ");
        v[ZBOOT_ZIMG_OFFSET..ZBOOT_ZIMG_OFFSET + 4].copy_from_slice(b"zimg");
        v[ZBOOT_PAYLOAD_OFFSET_FIELD..ZBOOT_PAYLOAD_OFFSET_FIELD + 4]
            .copy_from_slice(&(offset as u32).to_le_bytes());
        v[ZBOOT_PAYLOAD_SIZE_FIELD..ZBOOT_PAYLOAD_SIZE_FIELD + 4]
            .copy_from_slice(&(payload.len() as u32).to_le_bytes());
        v[ZBOOT_COMP_OFFSET..ZBOOT_COMP_OFFSET + comp.len()].copy_from_slice(comp);
        v.extend_from_slice(payload);
        v
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write;
        let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    // A zstd frame of exactly `make_arm64_image()`'s 256 bytes (zeros + the
    // ARM\x64 magic at offset 56), precomputed with `zstd -19` — ruzstd is
    // decode-only, so the encoded frame is embedded rather than produced here.
    // Regenerate if `make_arm64_image` changes.
    const ZSTD_FRAME_OF_ARM64_IMAGE: &[u8] = &[
        0x28, 0xb5, 0x2f, 0xfd, 0x64, 0x00, 0x00, 0x7d, 0x00, 0x00, 0x30, 0x00, 0x41, 0x52, 0x4d,
        0x64, 0x00, 0x02, 0x00, 0x40, 0x40, 0x29, 0x4f, 0xc0, 0x06, 0xc3, 0x99, 0x53, 0x49,
    ];

    #[test]
    fn unwraps_gzip_efi_zboot_to_arm64_image() {
        let img = make_arm64_image();
        let wrapped = make_zboot_raw(&gzip(&img), b"gzip");
        let raw = unwrap_zboot(wrapped).unwrap();
        assert_eq!(raw, img);
        assert_eq!(parse(&raw).unwrap().arch, Arch::Aarch64);
    }

    #[test]
    fn unwraps_zstd_efi_zboot_to_arm64_image() {
        let img = make_arm64_image();
        let wrapped = make_zboot_raw(ZSTD_FRAME_OF_ARM64_IMAGE, b"zstd");
        let raw = unwrap_zboot(wrapped).unwrap();
        assert_eq!(raw, img);
        assert_eq!(parse(&raw).unwrap().arch, Arch::Aarch64);
    }

    #[test]
    fn unwrap_zboot_passes_through_non_zboot() {
        // Anything lacking the MZ+zimg header passes through untouched.
        let blob = vec![0x55u8; 0x100];
        assert_eq!(unwrap_zboot(blob.clone()).unwrap(), blob);
        let arm = make_arm64_image();
        assert_eq!(unwrap_zboot(arm.clone()).unwrap(), arm);
    }

    #[test]
    fn rejects_unsupported_zboot_compression() {
        let z = make_zboot_raw(&gzip(&make_arm64_image()), b"lzma");
        assert!(matches!(
            unwrap_zboot(z),
            Err(KernelError::ZbootCompression(_))
        ));
    }

    #[test]
    fn rejects_too_small() {
        let r = parse(&[0u8; 32]);
        assert!(matches!(r, Err(KernelError::TooSmall(_))));
    }

    #[test]
    fn rejects_random_bytes() {
        let v = vec![0xAAu8; 1024];
        assert!(matches!(parse(&v), Err(KernelError::Unrecognized)));
    }

    #[test]
    fn pe_machine_codes_match_pmi_spec() {
        assert_eq!(Arch::X86_64.pe_machine(), 0x8664);
        assert_eq!(Arch::Aarch64.pe_machine(), 0xAA64);
    }

    /// Build a minimal ELF64 x86-64 executable with the given PT_LOAD segments
    /// (paddr, file_bytes, memsz) and entry. Layout: [ehdr][phdrs][seg data…],
    /// with each segment's p_offset pointing at its bytes in the file.
    fn make_elf(entry: u64, segs: &[(u64, Vec<u8>, u64)]) -> Vec<u8> {
        const EHSIZE: usize = 64;
        const PHENTSIZE: usize = 56;
        let phoff = EHSIZE;
        let mut data_off = EHSIZE + segs.len() * PHENTSIZE;
        let mut phdrs = Vec::new();
        let mut blob = Vec::new();
        for (paddr, bytes, memsz) in segs {
            let mut ph = [0u8; PHENTSIZE];
            ph[0..4].copy_from_slice(&1u32.to_le_bytes()); // p_type = PT_LOAD
            ph[8..16].copy_from_slice(&(data_off as u64).to_le_bytes()); // p_offset
            ph[16..24].copy_from_slice(&paddr.to_le_bytes()); // p_vaddr (reuse)
            ph[24..32].copy_from_slice(&paddr.to_le_bytes()); // p_paddr
            ph[32..40].copy_from_slice(&(bytes.len() as u64).to_le_bytes()); // p_filesz
            ph[40..48].copy_from_slice(memsz.to_le_bytes().as_slice()); // p_memsz
            phdrs.extend_from_slice(&ph);
            blob.extend_from_slice(bytes);
            data_off += bytes.len();
        }
        let mut ehdr = [0u8; EHSIZE];
        ehdr[0..4].copy_from_slice(&ELF_MAGIC);
        ehdr[4] = 2; // EI_CLASS = ELFCLASS64
        ehdr[5] = 1; // EI_DATA = ELFDATA2LSB
        ehdr[6] = 1; // EI_VERSION
        ehdr[16..18].copy_from_slice(&2u16.to_le_bytes()); // e_type = ET_EXEC
        ehdr[18..20].copy_from_slice(&EM_X86_64.to_le_bytes()); // e_machine
        ehdr[20..24].copy_from_slice(&1u32.to_le_bytes()); // e_version
        ehdr[24..32].copy_from_slice(&entry.to_le_bytes()); // e_entry
        ehdr[32..40].copy_from_slice(&(phoff as u64).to_le_bytes()); // e_phoff
        ehdr[52..54].copy_from_slice(&(EHSIZE as u16).to_le_bytes()); // e_ehsize
        ehdr[54..56].copy_from_slice(&(PHENTSIZE as u16).to_le_bytes()); // e_phentsize
        ehdr[56..58].copy_from_slice(&(segs.len() as u16).to_le_bytes()); // e_phnum
        let mut out = Vec::new();
        out.extend_from_slice(&ehdr);
        out.extend_from_slice(&phdrs);
        out.extend_from_slice(&blob);
        out
    }

    #[test]
    fn detects_elf_vmlinux_as_x86_64() {
        // Two segments: text at 0x100_0000 (entry), data at 0x120_0000 with a
        // BSS tail (memsz > filesz). alloc_size spans both incl. BSS.
        let elf = make_elf(
            0x100_0000,
            &[
                (0x100_0000, vec![0x11; 0x1000], 0x1000),
                (0x120_0000, vec![0x22; 0x800], 0x4000),
            ],
        );
        let p = parse(&elf).unwrap();
        assert_eq!(p.arch, Arch::X86_64);
        let e = p.elf.unwrap();
        // max_mem_end = 0x120_0000 + 0x4000; min_paddr = 0x100_0000.
        assert_eq!(e.alloc_size, 0x120_0000 + 0x4000 - 0x100_0000);
        assert_eq!(e.min_paddr, 0x100_0000);
        assert_eq!(e.entry_offset, 0); // entry == base here
    }

    #[test]
    fn elf_entry_offset_when_entry_not_at_base() {
        // Entry deep in the image (Alpine-style): offset = entry - min_paddr.
        let elf = make_elf(
            0x130_0000,
            &[
                (0x100_0000, vec![0x11; 0x400_000], 0x400_000),
                (0x140_0000, vec![0x22; 0x1000], 0x1000),
            ],
        );
        let e = parse(&elf).unwrap().elf.unwrap();
        assert_eq!(e.min_paddr, 0x100_0000);
        assert_eq!(e.entry_offset, 0x30_0000);
    }

    #[test]
    fn elf_load_image_lays_segments_at_relative_paddr() {
        let elf = make_elf(
            0x100_0000,
            &[
                (0x100_0000, vec![0xAB; 4], 4),
                (0x100_2000, vec![0xCD; 2], 0x100), // gap before it stays zero
            ],
        );
        let img = elf_load_image(&elf).unwrap();
        // Image spans min_paddr..max_file_end = [0x100_0000, 0x100_2002).
        assert_eq!(img.len(), 0x2002);
        assert_eq!(&img[0..4], &[0xAB; 4]);
        assert_eq!(&img[4..0x2000], &vec![0u8; 0x2000 - 4][..]); // gap zero
        assert_eq!(&img[0x2000..0x2002], &[0xCD; 2]);
    }

    #[test]
    fn rejects_elf_entry_outside_image() {
        // Entry past the end of the loaded image.
        let elf = make_elf(0x900_0000, &[(0x100_0000, vec![0u8; 0x100], 0x100)]);
        assert!(matches!(
            parse(&elf),
            Err(KernelError::ElfEntryOutOfRange { .. })
        ));
    }

    #[test]
    fn extract_vmlinux_inflates_embedded_elf() {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write;
        // A real ELF vmlinux, gzip-compressed and embedded in a fake bzImage
        // after the setup header (with a decoy gzip magic that won't inflate).
        let elf = make_elf(0x100_0000, &[(0x100_0000, vec![0x5a; 0x400], 0x800)]);
        let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
        enc.write_all(&elf).unwrap();
        let payload = enc.finish().unwrap();

        let mut bz = vec![0u8; 0x600];
        bz[BZIMAGE_HDRS_OFFSET..BZIMAGE_HDRS_OFFSET + 4]
            .copy_from_slice(&BZIMAGE_HDRS_MAGIC.to_le_bytes());
        bz[0x300..0x303].copy_from_slice(&[0x1f, 0x8b, 0x08]); // decoy: not valid gzip
        bz.extend_from_slice(&payload);

        let out = extract_vmlinux(bz).unwrap();
        assert_eq!(&out[0..4], &ELF_MAGIC);
        assert_eq!(parse(&out).unwrap().arch, Arch::X86_64);
    }

    #[test]
    fn extract_vmlinux_passes_through_raw_vmlinux() {
        let elf = make_elf(0x100_0000, &[(0x100_0000, vec![0u8; 0x100], 0x100)]);
        assert_eq!(extract_vmlinux(elf.clone()).unwrap(), elf);
    }

    #[test]
    #[ignore = "needs a real vmlinux at PICHI_VMLINUX + ground-truth at PICHI_GT"]
    fn extract_relocs_matches_kernel_relocs_tool() {
        // PICHI_GT is the raw table the kernel's own `relocs` tool emits, format
        // (64-bit): 0, relocs64.., 0, relocs32neg.., 0, relocs32.. (each a u32 LE
        // = low 32 bits of the patch-site kernel VA). We convert to image offsets
        // and require an exact set match against arma's extractor.
        use std::collections::BTreeSet;
        let bytes = std::fs::read(std::env::var("PICHI_VMLINUX").unwrap()).unwrap();
        let raw = std::fs::read(std::env::var("PICHI_GT").unwrap()).unwrap();
        let words: Vec<u32> = raw
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        // Split on zero terminators: [stop] r64 [stop] r32neg [stop] r32.
        let mut groups: Vec<Vec<u32>> = vec![Vec::new()];
        for &w in &words[1..] {
            if w == 0 {
                groups.push(Vec::new());
            } else {
                groups.last_mut().unwrap().push(w);
            }
        }
        let (gt64, gt32n, gt32) = (&groups[0], &groups[1], &groups[2]);

        let r = extract_relocs(&bytes).unwrap();
        let elf = Elf::parse(&bytes).unwrap();
        let span = elf_span(&elf).unwrap();
        let text_vaddr = elf
            .program_headers
            .iter()
            .find(|p| p.p_type == PT_LOAD && p.p_memsz != 0 && p.p_paddr == span.min_paddr)
            .unwrap()
            .p_vaddr;
        let lo = (text_vaddr & 0xffff_ffff) as u32;

        let mine = |v: &[u32]| -> BTreeSet<u32> { v.iter().copied().collect() };
        // Ground-truth values are (image_off + lo) mod 2^32; invert to offsets.
        let theirs =
            |v: &[u32]| -> BTreeSet<u32> { v.iter().map(|x| x.wrapping_sub(lo)).collect() };
        eprintln!(
            "arma: r64={} r32neg={} r32={} | gt: r64={} r32neg={} r32={}",
            r.relocs64.len(),
            r.relocs32neg.len(),
            r.relocs32.len(),
            gt64.len(),
            gt32n.len(),
            gt32.len(),
        );
        assert_eq!(mine(&r.relocs64), theirs(gt64), "relocs64 mismatch");
        assert_eq!(mine(&r.relocs32neg), theirs(gt32n), "relocs32neg mismatch");
        assert_eq!(mine(&r.relocs32), theirs(gt32), "relocs32 mismatch");
    }

    #[test]
    fn rejects_non_x86_elf() {
        // EM_AARCH64 = 183; arma's ELF path is x86-only (arm64 ships raw Image).
        let mut elf = make_elf(0x100_0000, &[(0x100_0000, vec![0u8; 0x100], 0x100)]);
        elf[18..20].copy_from_slice(&183u16.to_le_bytes());
        assert!(matches!(parse(&elf), Err(KernelError::ElfMalformed(_))));
    }
}
