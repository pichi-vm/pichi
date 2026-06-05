//! PMI CBOR manifest construction.
//!
//! Builds a `pmi::vm::Spec<V>` populated with arma's chosen layout
//! and serializes it via `ciborium`. The boot vCPU register map
//! varies per arch (x86_64 long-mode entry vs aarch64 EL1h entry).

use anyhow::{Context, Result};
use ciborium::ser::into_writer;
use pmi::Version;
use pmi::cpu::Profile;
use pmi::vm::{Action, Fill, Load, LoadKind, Spec, vcpu};

use crate::kernel::Arch;
use crate::tatu::TatuImage;

/// Names assigned to arma's own PE sections (tatu's `.tatu.*` names
/// come straight from tatu's ELF).
pub(crate) const SECTION_LINUX: &str = ".linux";
pub(crate) const SECTION_INITRD: &str = ".initrd";
// The base DTB and host-DTBO are tatu-defined sections (arma fills
// `.tatu.dtb`, dillo fills `.tatu.dtbo`); arma synthesizes neither.
pub(crate) const SECTION_DTB: &str = ".tatu.dtb";
pub(crate) const SECTION_DTBO: &str = ".tatu.dtbo";
// The boot CPU tables are tatu-defined sections (const-fn-baked into the
// tatu binary); arma reads their GPAs from the ELF for cr3 / gdtr.base.
pub(crate) const SECTION_PGTABLE: &str = ".tatu.pgt";
pub(crate) const SECTION_GDT: &str = ".tatu.gdt";

/// `IMAGE_SCN_MEM_DISCARDABLE` per the PMI spec for the non-loaded
/// target spec section.
pub(crate) const SECTION_PMI_VM: &str = ".pmi.vm";

/// Build the CBOR bytes for the `.pmi.vm` section. The placement [`Layout`]
/// isn't needed here — the manifest references sections by name and reads the
/// boot-CPU-table GPAs from the tatu ELF; GPAs live in the PE section table.
pub(crate) fn build_pmi_vm(
    arch: Arch,
    tatu: &TatuImage,
    has_initrd: bool,
    cpu_profile: &str,
) -> Result<Vec<u8>> {
    let profile = Profile::new(cpu_profile);
    match arch {
        Arch::X86_64 => {
            let spec = build_spec_x86(tatu, has_initrd, profile)?;
            encode(&spec)
        }
        Arch::Aarch64 => {
            let spec = build_spec_aarch64(tatu, has_initrd, profile);
            encode(&spec)
        }
    }
}

#[cfg(test)]
/// PE section name carrying the target spec (`.pmi.vm`).
pub(crate) fn target_section_name() -> &'static str {
    use pmi::Target;
    <Spec<vcpu::x86_64::CpuState> as Target>::SECTION
}

fn encode<T: pmi::Target>(t: &T) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    into_writer(t, &mut buf).context("CBOR encode of .pmi.vm spec")?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Action list builder, shared between arches.
// ---------------------------------------------------------------------------

fn build_actions(_arch: Arch, tatu: &TatuImage, has_initrd: bool) -> Vec<Action> {
    let mut out = Vec::with_capacity(16);

    // Tatu sections — one load per SHF_ALLOC section, in vaddr order
    // (already sorted by parse()). `.tatu.dtbo` is the exception: it's the
    // unmeasured host-DTBO fill target (Fill action below), not loaded
    // from the PMI. Everything else loads here, including `.tatu.dtb`
    // (arma fills it with the measured base DTB) and the x86 boot CPU
    // tables `.tatu.pgt` / `.tatu.gdt` (const-fn-baked by tatu).
    for s in &tatu.sections {
        if s.name == SECTION_DTBO {
            continue;
        }
        out.push(Action::Load(Load {
            section: s.name.clone(),
            kind: LoadKind::default(),
        }));
    }

    // Kernel, then optional initrd. The base DTB (`.tatu.dtb`) is loaded
    // by the tatu-sections loop above.
    out.push(Action::Load(Load {
        section: SECTION_LINUX.into(),
        kind: LoadKind::default(),
    }));
    if has_initrd {
        out.push(Action::Load(Load {
            section: SECTION_INITRD.into(),
            kind: LoadKind::default(),
        }));
    }

    // Host-DTBO fill — the unmeasured half of the merged extension.
    out.push(Action::Fill(Fill {
        section: SECTION_DTBO.into(),
        kind: pmi::vm::FillKind::MergedDtbo,
    }));

    out
}

// ---------------------------------------------------------------------------
// x86_64 vm:vcpu.
// ---------------------------------------------------------------------------

fn build_spec_x86(
    tatu: &TatuImage,
    has_initrd: bool,
    cpu_profile: Profile,
) -> Result<Spec<vcpu::x86_64::CpuState>> {
    // cr3 / gdtr.base come straight from tatu's ELF: the boot CPU tables
    // are tatu-defined sections, not arma-allocated. arma never assumes
    // their addresses — it reads them by name from the binary.
    let pgtable_gpa = tatu
        .section(SECTION_PGTABLE)
        .with_context(|| format!("tatu x86 ELF missing `{SECTION_PGTABLE}`"))?
        .vaddr;
    let gdt_gpa = tatu
        .section(SECTION_GDT)
        .with_context(|| format!("tatu x86 ELF missing `{SECTION_GDT}`"))?
        .vaddr;
    Ok(Spec {
        version: Version::default(),
        actions: build_actions(Arch::X86_64, tatu, has_initrd),
        vcpu: x86_vcpu(pgtable_gpa, gdt_gpa),
        cpu_profile,
        merged_dtb: Some(SECTION_DTB.into()),
    })
}

fn x86_vcpu(pgtable_gpa: u64, gdt_gpa: u64) -> vcpu::x86_64::CpuState {
    use vcpu::x86_64::{CpuState, Dtr, SegReg};

    // Code descriptor at selector 0x08 — must match tatu's
    // bootmem::gdt_page() (0x9B / 0xAF). Attributes
    // encode P=1, S=1, type=Code/RX, L=1, G=1.
    let cs_attributes: u16 = 0xA09B; // L=1 (bit 9), P=1, type=11(code, R, A)
    // Data descriptor at selector 0x10 — matches 0x93 / 0xCF.
    let ds_attributes: u16 = 0xC093; // G=1, D/B=1, P=1, type=3(data, RW, A)

    CpuState {
        rip: 0xFFFF_FFF0,
        rsp: 0,
        rflags: 0x2,
        cr0: 0x8000_0001, // PG | PE
        cr3: pgtable_gpa,
        cr4: 0x20,   // PAE
        efer: 0x500, // LME | LMA
        cs: SegReg {
            selector: 0x08,
            attributes: cs_attributes,
            limit: 0xFFFF_FFFF,
            base: 0,
        },
        ds: data_seg(0x10, ds_attributes),
        es: data_seg(0x10, ds_attributes),
        fs: data_seg(0x10, ds_attributes),
        gs: data_seg(0x10, ds_attributes),
        ss: data_seg(0x10, ds_attributes),
        gdtr: Dtr {
            limit: 0x17, // 3 entries × 8 bytes - 1
            base: gdt_gpa,
        },
        idtr: Dtr { limit: 0, base: 0 },
        rax: 0,
        rbx: 0,
        rcx: 0,
        rdx: 0,
        rsi: 0,
        rdi: 0,
        rbp: 0,
        r8: 0,
        r9: 0,
        r10: 0,
        r11: 0,
        r12: 0,
        r13: 0,
        r14: 0,
        r15: 0,
    }
}

fn data_seg(selector: u16, attributes: u16) -> vcpu::x86_64::SegReg {
    vcpu::x86_64::SegReg {
        selector,
        attributes,
        limit: 0xFFFF_FFFF,
        base: 0,
    }
}

// ---------------------------------------------------------------------------
// aarch64 vm:vcpu.
// ---------------------------------------------------------------------------

fn build_spec_aarch64(
    tatu: &TatuImage,
    has_initrd: bool,
    cpu_profile: Profile,
) -> Spec<vcpu::aarch64::CpuState> {
    Spec {
        version: Version::default(),
        actions: build_actions(Arch::Aarch64, tatu, has_initrd),
        vcpu: aarch64_vcpu(tatu.entry),
        cpu_profile,
        merged_dtb: Some(SECTION_DTB.into()),
    }
}

fn aarch64_vcpu(pc: u64) -> vcpu::aarch64::CpuState {
    vcpu::aarch64::CpuState {
        pc,
        pstate: 0x3C5,          // EL1h, DAIF masked
        sctlr_el1: 0x00C5_0838, // typical reset; MMU off, caches off
        vbar_el1: 0,
        // FPEN=0b11 (bits 21:20): enable FP/SIMD at EL1/EL0 from reset.
        // The bare-metal tatu (and compiler-emitted memcpy/SIMD) uses FP
        // registers; leaving CPACR_EL1=0 traps the first such access to the
        // (zero) EL1 vector. Boot with FP/SIMD enabled.
        cpacr_el1: 0x30_0000,
        // sp_el1 = 0 (tatu's entry stub sets SP itself)
        ..Default::default()
    }
}

// Quiet warnings about unused fields/imports — pmi's Default is enough.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::TATU_X86_64;
    use crate::tatu::parse as parse_tatu;
    use ciborium::de::from_reader;

    #[test]
    fn x86_manifest_round_trips_via_strict_decoder() {
        let tatu = parse_tatu(TATU_X86_64, Arch::X86_64).unwrap();
        let cbor = build_pmi_vm(Arch::X86_64, &tatu, true, "x86-64-v3").unwrap();
        let decoded: Spec<vcpu::x86_64::CpuState> =
            from_reader(cbor.as_slice()).expect("strict round-trip decode");
        assert_eq!(decoded.vcpu.rip, 0xFFFF_FFF0);
        // cr3 / gdtr.base must equal the tatu ELF's boot-CPU-table GPAs.
        let pgt = tatu
            .section(SECTION_PGTABLE)
            .expect(".tatu.pgt in tatu ELF");
        let gdt = tatu.section(SECTION_GDT).expect(".tatu.gdt in tatu ELF");
        assert_eq!(decoded.vcpu.cr3, pgt.vaddr);
        assert_eq!(decoded.vcpu.gdtr.base, gdt.vaddr);
        assert!(matches!(decoded.merged_dtb, Some(s) if s == ".tatu.dtb"));
        // Last action must be the fill for .dtbo.
        let last = decoded.actions.last().unwrap();
        match last {
            Action::Fill(f) => {
                assert_eq!(f.section, SECTION_DTBO);
                assert!(matches!(f.kind, pmi::vm::FillKind::MergedDtbo));
            }
            Action::Load(_) => panic!("last action must be Fill"),
        }
    }

    #[test]
    fn aarch64_manifest_pc_matches_tatu_entry() {
        use crate::TATU_AARCH64;
        let tatu = parse_tatu(TATU_AARCH64, Arch::Aarch64).unwrap();
        let cbor = build_pmi_vm(Arch::Aarch64, &tatu, false, "armv8.2-a").unwrap();
        let decoded: Spec<vcpu::aarch64::CpuState> =
            from_reader(cbor.as_slice()).expect("strict round-trip decode");
        assert_eq!(decoded.vcpu.pc, tatu.entry);
        assert_eq!(decoded.vcpu.pstate & 0xF, 0x5); // EL1h
        assert_eq!((decoded.vcpu.pstate >> 4) & 1, 0); // AArch64
    }

    #[test]
    fn target_section_name_is_pmi_vm() {
        assert_eq!(target_section_name(), SECTION_PMI_VM);
    }
}
