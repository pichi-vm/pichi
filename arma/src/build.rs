//! Top-level build pipeline.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::{
    TATU_AARCH64, TATU_X86_64, base_dtb, bootinfo, initrd, kconfig, kernel, manifest, pe, planner,
    tatu,
};

/// Resolve `(mmio_slots, pci_slots)` (device-model.md §6 Slot composition).
/// With `--config`, infer/validate against kernel support; without it, honor
/// explicit slots, else default to a PCI bridge (TODO C5: embedded IKCONFIG).
/// Load the kernel build config used for slot inference (C4) AND the profile
/// floor clamp (C2): `--config` if given, else the kernel's embedded
/// CONFIG_IKCONFIG (C5), else error (device-model §6).
fn load_kernel_config(args: &BuildArgs, kernel: &[u8]) -> Result<kconfig::KernelConfig> {
    match &args.config_path {
        Some(p) => {
            let text =
                fs::read_to_string(p).with_context(|| format!("read --config: {}", p.display()))?;
            Ok(kconfig::KernelConfig::parse(text))
        }
        None => kconfig::KernelConfig::from_ikconfig(kernel).ok_or_else(|| {
            anyhow::anyhow!(
                "no --config given and the kernel has no embedded CONFIG_IKCONFIG; \
                 pass --config <kernel .config> for slot inference"
            )
        }),
    }
}

/// Inputs to a single `arma build` invocation. All caller-supplied —
/// arma carries no policy defaults.
pub(crate) struct BuildArgs {
    pub(crate) kernel_path: PathBuf,
    pub(crate) initrd_path: Option<PathBuf>,
    pub(crate) cmdline: String,
    /// vCPU ISA baseline; `None` ⇒ the per-arch RHEL 9 default (C2).
    pub(crate) profile: Option<String>,
    pub(crate) serial: bool,
    pub(crate) output_path: PathBuf,
    /// Kernel build config (text Kconfig); `None` ⇒ no config supplied.
    pub(crate) config_path: Option<PathBuf>,
    /// Explicit `--mmio-slots` / `--pci-slots` overrides (`None` ⇒ infer).
    pub(crate) mmio_slots: Option<u32>,
    pub(crate) pci_slots: Option<u32>,
    /// `--pci-window <B>` / `--min-addr-space <X>` (bits); `None` ⇒ per-arch
    /// defaults (C4b, device-model §6).
    pub(crate) pci_window: Option<u32>,
    pub(crate) min_addr_space: Option<u32>,
}

pub(crate) fn run(args: &BuildArgs) -> Result<()> {
    // ---- Step 1: read inputs ----
    let kernel_bytes = fs::read(&args.kernel_path)
        .with_context(|| format!("read kernel: {}", args.kernel_path.display()))?;
    let initrd_bytes = match args.initrd_path.as_ref() {
        Some(p) => Some(fs::read(p).with_context(|| format!("read initrd: {}", p.display()))?),
        None => None,
    };

    // ---- Step 2: arch inference + kernel validation ----
    let parsed_kernel = kernel::parse(&kernel_bytes).context("kernel format validation")?;
    let arch = parsed_kernel.arch;

    // Per-arch device-model defaults (§6). TODO(C4/C4b): drive these from
    // `--mmio-slots`/`--pci-slots`/`--pci-window`/`--min-addr-space` + slot
    // inference. For now: emit the PCIe bridge, no virtio-mmio yet.
    // C4b: window/space bits from --pci-window/--min-addr-space, else per-arch
    // defaults (§6). The X ≥ B+2 invariant is enforced in base_dtb::build.
    let (def_x, def_b) = match arch {
        kernel::Arch::Aarch64 => (36u32, 34u32),
        kernel::Arch::X86_64 => (39u32, 37u32),
    };
    let addr_space_bits = args.min_addr_space.unwrap_or(def_x);
    let window_bits = args.pci_window.unwrap_or(def_b);
    let kcfg = load_kernel_config(args, &kernel_bytes)?;
    let (mmio_slots, pci_slots) = kcfg
        .infer_slots(arch, args.mmio_slots, args.pci_slots)
        .context("slot inference")?;

    // ---- Step 3: cpio auto-detect / wrap ----
    let initrd_materialized = initrd_bytes
        .as_deref()
        .map(initrd::materialize)
        .transpose()
        .context("materialize initrd")?;
    let initrd_size = initrd_materialized.as_ref().map(|v| v.len() as u64);

    // ---- Step 4: select embedded tatu ----
    let tatu_elf: &[u8] = match arch {
        kernel::Arch::X86_64 => TATU_X86_64,
        kernel::Arch::Aarch64 => TATU_AARCH64,
    };

    // ---- Step 5: parse tatu ELF ----
    let tatu_img = tatu::parse(tatu_elf, arch).context("parse embedded tatu ELF")?;

    // ---- Step 6: kernel metadata for placement ----
    //
    // `kernel_file_size` is the raw byte count we load from disk.
    // `kernel_alloc_size` is the RAM footprint the kernel needs at runtime —
    // for bzImage the decompressor scratch buffer, for aarch64 Image the
    // header's image_size (see compute_kernel_alloc_size).
    let kernel_file_size = kernel_bytes.len() as u64;
    let kernel_alloc_size = compute_kernel_alloc_size(
        kernel_file_size,
        parsed_kernel.bzimage,
        parsed_kernel.aarch64_image_size,
    );
    let kernel_alignment = parsed_kernel
        .bzimage
        .map(|bz| u64::from(bz.kernel_alignment))
        .unwrap_or(2 * 1024 * 1024);
    // x86: a relocatable kernel runs at its preferred (link) address if loaded
    // lower, so the planner floors the load GPA to it. 0 on aarch64.
    let kernel_pref_addr = parsed_kernel.bzimage.map_or(0, |bz| bz.pref_address);

    // ---- Step 7: plan the guest-physical layout ----
    // The planner avoids tatu's fixed sections plus the architecture
    // fixed/specific devices (LAPIC/IOAPIC/syscon, GIC/v2m), and assigns the
    // kernel, initrd, and generic device MMIO (serial/virtio/ECAM/window).
    let mut reserved: Vec<core::ops::Range<u64>> = tatu_img.reserved().collect();
    reserved.extend(base_dtb::arch_reserved(arch));
    let lay = planner::Planner {
        args: planner::ArgsSpec {
            serial: args.serial,
            mmio_slots,
            pci_slots,
            window_bits,
            addr_space_bits,
        },
        reserved: &reserved,
        kernel: planner::KernelSpec {
            size: kernel_alloc_size,
            align: kernel_alignment,
            min_gpa: kernel_pref_addr,
        },
        initrd_size,
    }
    .plan()
    .context("plan guest-physical layout")?;

    // ---- Step 8: base DTB (single pass) ----
    // Device addresses and the initrd extent both come from the plan, so the
    // DTB is built once — no sizing/final two-pass dance.
    let cmdline = args.cmdline.as_str();
    let initrd_extent = lay.initrd.as_ref().map(|r| (r.start, r.end - r.start));
    let dtb_bytes = base_dtb::build(&base_dtb::Inputs {
        arch,
        cmdline,
        initrd: initrd_extent,
        serial: lay.serial.clone(),
        virtio: &lay.virtio,
        ecam: lay.ecam.clone(),
        pci_window: lay.pci_window.clone(),
    })
    .context("build base DTB")?;
    let dtb_size = dtb_bytes.len() as u64;

    // `.tatu.dtb` / `.tatu.dtbo` are tatu-defined sections: arma fills
    // `.tatu.dtb` with the base DTB (must fit the reserved capacity) and dillo
    // fills `.tatu.dtbo` at launch. Their GPAs come from tatu's own layout.
    let dtb_sec = tatu_img
        .section(".tatu.dtb")
        .context("tatu ELF missing .tatu.dtb section")?;
    if dtb_size > dtb_sec.virtual_size {
        bail!(
            "base DTB ({dtb_size:#x} bytes) exceeds .tatu.dtb capacity ({:#x})",
            dtb_sec.virtual_size
        );
    }
    let dtb_gpa = dtb_sec.vaddr;
    let dtbo_sec = tatu_img
        .section(".tatu.dtbo")
        .context("tatu ELF missing .tatu.dtbo section")?;
    let dtbo_gpa = dtbo_sec.vaddr;
    let dtbo_size = dtbo_sec.virtual_size;

    // ---- Step 9: TatuBootInfo ----
    // kernel_size here is the FILE size — tatu reads it to find the setup
    // header, NOT the alloc size.
    let bi = bootinfo::TatuBootInfo::new(
        dtb_gpa,
        dtb_size as u32,
        dtbo_gpa,
        dtbo_size as u32,
        lay.linux.start,
        kernel_file_size as u32,
    );
    let bootinfo_section_bytes = bi.to_section_bytes();

    // ---- Step 10: CBOR manifest ----
    // C2: --profile defaults to RHEL 9's baseline, then is clamped UP to the
    // kernel's ISA build floor (so a higher-built kernel can't get a profile
    // below what it requires); an explicit --profile may raise it further.
    let baseline = match arch {
        kernel::Arch::Aarch64 => "armv8.0-a",
        kernel::Arch::X86_64 => "x86-64-v2",
    };
    let chosen = args.profile.as_deref().unwrap_or(baseline);
    let cpu_profile = kconfig::raise_to_floor(chosen, kcfg.isa_floor(arch), arch);
    let has_initrd = initrd_materialized.is_some();
    let cbor = manifest::build_pmi_vm(arch, &tatu_img, has_initrd, &cpu_profile)
        .context("build CBOR manifest")?;

    // ---- Step 11: assemble PE section list ----
    let sections = assemble_sections(
        &tatu_img,
        &bootinfo_section_bytes,
        &kernel_bytes,
        initrd_materialized.as_deref(),
        &dtb_bytes,
        &lay,
        &cbor,
    );

    // ---- Step 12: emit PE; atomic write ----
    let pe_bytes = pe::build_pe(arch.pe_machine(), &sections).context("emit PE")?;

    atomic_write(&args.output_path, &pe_bytes)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn assemble_sections<'a>(
    tatu_img: &'a tatu::TatuImage,
    bootinfo_bytes: &'a [u8],
    kernel_bytes: &'a [u8],
    initrd_bytes: Option<&'a [u8]>,
    dtb_bytes: &'a [u8],
    lay: &planner::Layout,
    cbor: &'a [u8],
) -> Vec<pe::Section<'a>> {
    use std::borrow::Cow;

    let mut out: Vec<pe::Section<'a>> = Vec::with_capacity(16);

    // Tatu sections in their ELF-resolved GPA order. arma overrides two
    // tatu-defined zero sections with computed bytes: `.tatu.bootinfo`
    // (the header) and `.tatu.dtb` (the measured base DTB). `.tatu.dtbo`
    // stays a zero section — dillo fills it at launch (fill action in the
    // manifest). arma synthesizes no `.dtb`/`.dtbo` of its own.
    for s in &tatu_img.sections {
        let data: Cow<'a, [u8]> = if s.is_bootinfo {
            // Tatu's section is 4 KiB virtual; arma fills the same 4 KiB
            // with header + zero pad.
            Cow::Borrowed(bootinfo_bytes)
        } else if s.is_dtb {
            // Fill the reserved .tatu.dtb section with the base DTB; the
            // rest of the section stays zero (virtual_size > dtb len).
            Cow::Borrowed(dtb_bytes)
        } else if s.is_nobits {
            Cow::Borrowed(&[])
        } else {
            Cow::Borrowed(s.data.as_slice())
        };
        out.push(pe::Section {
            name: s.name.clone(),
            vaddr: s.vaddr,
            virtual_size: s.virtual_size,
            data,
            characteristics: tatu_section_characteristics(&s.name, s.is_nobits),
            non_loaded: false,
        });
    }

    // .linux (LARGE) — borrowed from the caller's owned buffer. Sits
    // in its own 2 MiB-aligned file region per PMI granularity. The
    // virtual_size exceeds the file size on x86 to declare the bzImage
    // decompressor's scratch buffer (Padded shape per PMI §"Section
    // Shapes"); dillo's placement uses virtual_size when sizing
    // memslots, which is exactly the RAM the kernel will touch.
    out.push(pe::Section {
        name: manifest::SECTION_LINUX.into(),
        vaddr: lay.linux.start,
        virtual_size: lay.linux.end - lay.linux.start,
        data: Cow::Borrowed(kernel_bytes),
        characteristics: pe::IMAGE_SCN_CNT_INITIALIZED_DATA | pe::IMAGE_SCN_MEM_READ,
        non_loaded: false,
    });

    // SMALL pool. Emitted in guest-VA order, with Data-shape sections
    // first and the Zero-shape `.dtbo` last (matches layout::compute).
    // File offsets follow declaration order in pe::build_pe, so the
    // Data SMALLs end up as one contiguous file run aligned with their
    // guest-VA run — letting a VMM mmap them as a single unit.

    // .initrd — borrowed.
    if let (Some(bytes), Some(r)) = (initrd_bytes, &lay.initrd) {
        out.push(pe::Section {
            name: manifest::SECTION_INITRD.into(),
            vaddr: r.start,
            virtual_size: r.end - r.start,
            data: Cow::Borrowed(bytes),
            characteristics: pe::IMAGE_SCN_CNT_INITIALIZED_DATA | pe::IMAGE_SCN_MEM_READ,
            non_loaded: false,
        });
    }

    // The measured base DTB, the host-DTBO, and the x86 boot CPU tables
    // (`.tatu.pgt` / `.tatu.gdt`) are NOT synthesized here: they are all
    // tatu-defined sections. arma fills `.tatu.dtb` with the base DTB
    // bytes in the tatu-sections loop above (via `is_dtb`); `.tatu.dtbo`
    // is emitted as the tatu zero section that dillo fills; the boot CPU
    // tables ship as const-fn-baked PROGBITS straight from tatu's ELF.

    // .pmi.vm (non-loaded; CBOR manifest)
    out.push(pe::Section {
        name: manifest::SECTION_PMI_VM.into(),
        vaddr: 0,
        virtual_size: cbor.len() as u64,
        data: Cow::Borrowed(cbor),
        characteristics: pe::IMAGE_SCN_CNT_INITIALIZED_DATA
            | pe::IMAGE_SCN_MEM_READ
            | pe::IMAGE_SCN_MEM_DISCARDABLE,
        non_loaded: true,
    });

    out
}

fn tatu_section_characteristics(name: &str, is_nobits: bool) -> u32 {
    // Conservative mapping. Tatu's linker emits:
    //   .tatu.text   — code, RX
    //   .tatu.rodata — read-only data
    //   .tatu.data   — RW data (rare; tatu's release binary minimizes this)
    //   .tatu.bootinfo — RW (arma writes header at build time; tatu reads)
    //   .tatu.bss    — RW zero (BSS)
    //   .tatu.reset  — code, RX (x86 reset trampoline at 0xFFFF_F000;
    //                  on aarch64 it's collapsed into .tatu.text by
    //                  the linker and never appears here)
    if is_nobits {
        pe::IMAGE_SCN_CNT_UNINITIALIZED_DATA | pe::IMAGE_SCN_MEM_READ | pe::IMAGE_SCN_MEM_WRITE
    } else if name.contains(".text") || name == ".tatu.reset" {
        pe::IMAGE_SCN_CNT_CODE | pe::IMAGE_SCN_MEM_READ | pe::IMAGE_SCN_MEM_EXECUTE
    } else if name.contains(".rodata") {
        pe::IMAGE_SCN_CNT_INITIALIZED_DATA | pe::IMAGE_SCN_MEM_READ
    } else {
        pe::IMAGE_SCN_CNT_INITIALIZED_DATA | pe::IMAGE_SCN_MEM_READ | pe::IMAGE_SCN_MEM_WRITE
    }
}

/// Compute the kernel section's RAM footprint.
///
/// For aarch64 we don't yet parse equivalent metadata, so this is the
/// file size verbatim. For x86 bzImage, the decompressor (see
/// `arch/x86/boot/compressed/head_64.S`) relocates the protected-mode
/// kernel to `rbp = ceil(load_addr + setup_bytes, kernel_alignment)`
/// and then uses `[rbp, rbp + init_size)` as a scratch buffer that
/// includes its own stack at the top. The total RAM the kernel will
/// touch from `load_addr` is therefore `slack + init_size`, where
/// `slack = rbp − load_addr`.
///
/// arma's layout places `.linux` at a `kernel_alignment`-aligned GPA,
/// so the slack can be computed independently of the final address.
///
/// Returns `max(file_size, slack + init_size)` so the section is
/// always at least big enough to hold the on-disk bytes.
fn compute_kernel_alloc_size(
    file_size: u64,
    bzimage: Option<kernel::BzImageMeta>,
    aarch64_image_size: Option<u64>,
) -> u64 {
    let Some(bz) = bzimage else {
        // aarch64 Image: the runtime footprint is the header's image_size
        // (text + BSS), which exceeds the file when the BSS isn't in it. Back
        // `max(file, image_size)` as RAM or the BSS tail is unmapped.
        return aarch64_image_size.unwrap_or(0).max(file_size);
    };
    let kalign = u64::from(bz.kernel_alignment);
    let setup = bz.setup_bytes();
    let slack = setup.div_ceil(kalign).saturating_mul(kalign);
    let alloc = slack.saturating_add(u64::from(bz.init_size));
    alloc.max(file_size)
}

#[cfg(test)]
mod tests_alloc {
    use super::*;

    #[test]
    fn x86_alloc_matches_decompressor_formula() {
        // Fedora 6.18 distro kernel observed values.
        let bz = kernel::BzImageMeta {
            init_size: 0x048E5000,
            kernel_alignment: 0x200000,
            setup_sects: 39,
            pref_address: 0x100_0000,
        };
        let alloc = compute_kernel_alloc_size(0x117E828, Some(bz), None);
        // slack = ceil(40*512, 2 MiB) = 2 MiB; alloc = 2 MiB + 73 MiB.
        assert_eq!(alloc, 0x200000 + 0x048E5000);
    }

    #[test]
    fn aarch64_alloc_is_max_file_and_image_size() {
        // No image_size header info → file size.
        assert_eq!(compute_kernel_alloc_size(0x800000, None, None), 0x800000);
        // image_size > file (BSS not in file, the Firecracker case) → image_size.
        assert_eq!(
            compute_kernel_alloc_size(0xF84800, None, Some(0x1080000)),
            0x1080000
        );
        // image_size < file (or padded equal) → file size.
        assert_eq!(
            compute_kernel_alloc_size(0x2110000, None, Some(0x2110000)),
            0x2110000
        );
    }

    #[test]
    fn alloc_never_smaller_than_file_size() {
        let bz = kernel::BzImageMeta {
            init_size: 0x1000,
            kernel_alignment: 0x200000,
            setup_sects: 1,
            pref_address: 0x100_0000,
        };
        let alloc = compute_kernel_alloc_size(0x10_000_000, Some(bz), None);
        assert_eq!(alloc, 0x10_000_000);
    }
}

fn atomic_write(dst: &Path, bytes: &[u8]) -> Result<()> {
    let dir = dst
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let tmp_name = format!(
        ".{}.arma.tmp",
        dst.file_name()
            .map_or_else(|| "out".to_string(), |n| n.to_string_lossy().into_owned())
    );
    let tmp = dir.join(tmp_name);
    fs::write(&tmp, bytes).with_context(|| format!("write tmp: {}", tmp.display()))?;
    fs::rename(&tmp, dst)
        .with_context(|| format!("rename {} -> {}", tmp.display(), dst.display()))?;
    Ok(())
}
