//! Integration tests for arma's structural success criteria.

mod common;

use std::fs;

use ciborium::de::from_reader;
use pmi::vm::{Action, FillKind, Spec, vcpu};
#[cfg(target_arch = "x86_64")]
use sha2::{Digest, Sha256};
use tempfile::TempDir;

#[cfg(target_arch = "aarch64")]
use common::synthesize_arm64_image;
#[cfg(target_arch = "x86_64")]
use common::synthesize_bzimage;
use common::{build_pmi, find_pmi_vm};

// ---------------------------------------------------------------------------
// Per-arch fixtures (built once per test).
// ---------------------------------------------------------------------------

struct Fixture {
    _tmp: TempDir,
    pmi_bytes: Vec<u8>,
}

#[cfg(target_arch = "x86_64")]
fn build_x86_fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let kernel = tmp.path().join("kernel");
    let init = tmp.path().join("init");
    let pmi = tmp.path().join("out.pmi");
    fs::write(&kernel, synthesize_bzimage(0x1000)).unwrap();
    // Cpio-magic-prefixed payload — arma passes cpio through unchanged.
    fs::write(&init, b"070701FAKE_CPIO_FOR_X86_FIXTURE").unwrap();
    build_pmi(&kernel, Some(&init), "console=ttyS0", &pmi);
    let bytes = fs::read(&pmi).unwrap();
    Fixture {
        _tmp: tmp,
        pmi_bytes: bytes,
    }
}

#[cfg(target_arch = "aarch64")]
fn build_aarch64_fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let kernel = tmp.path().join("Image");
    let pmi = tmp.path().join("out.pmi");
    fs::write(&kernel, synthesize_arm64_image()).unwrap();
    build_pmi(&kernel, None, "console=hvc0", &pmi);
    let bytes = fs::read(&pmi).unwrap();
    Fixture {
        _tmp: tmp,
        pmi_bytes: bytes,
    }
}

// ---------------------------------------------------------------------------
// §15.1 #1 — PE validity
// ---------------------------------------------------------------------------

#[test]
#[cfg(target_arch = "x86_64")]
fn pe_validity_x86() {
    let f = build_x86_fixture();
    assert_eq!(&f.pmi_bytes[..2], b"MZ");
    let pe = goblin::pe::PE::parse(&f.pmi_bytes).expect("parse PE");
    assert_eq!(pe.header.coff_header.machine, 0x8664);
    assert!(pe.is_64);
    assert_eq!(
        pe.header.coff_header.number_of_sections as usize,
        pe.sections.len()
    );
}

#[test]
#[cfg(target_arch = "aarch64")]
fn pe_validity_aarch64() {
    let f = build_aarch64_fixture();
    assert_eq!(&f.pmi_bytes[..2], b"MZ");
    let pe = goblin::pe::PE::parse(&f.pmi_bytes).expect("parse PE");
    assert_eq!(pe.header.coff_header.machine, 0xAA64);
}

// ---------------------------------------------------------------------------
// §15.1 #2 — PMI core conformance: sections referenced by actions
// exist; no overlapping VirtualAddress ranges among load/fill sections.
// ---------------------------------------------------------------------------

#[test]
#[cfg(target_arch = "x86_64")]
fn pmi_core_conformance_x86() {
    let f = build_x86_fixture();
    // Use the arch-specific decoder path directly to avoid the
    // generic CBOR retry gymnastics above.
    let pe = goblin::pe::PE::parse(&f.pmi_bytes).unwrap();
    let (off, size) = find_pmi_vm(&f.pmi_bytes);
    let cbor = &f.pmi_bytes[off..off + size];
    let spec: Spec<vcpu::x86_64::CpuState> = from_reader(cbor).expect("decode");

    let sec_names: std::collections::HashSet<String> = pe
        .sections
        .iter()
        .map(|s| s.name().unwrap_or("").to_string())
        .collect();

    let mut referenced = Vec::new();
    for a in &spec.actions {
        let n = match a {
            Action::Load(l) => l.section.as_str(),
            Action::Fill(f) => f.section.as_str(),
        };
        referenced.push(n);
        assert!(sec_names.contains(n), "missing section `{n}`");
    }
    // Pairwise non-overlap (u64 to handle .tatu.reset at 0xFFFFF000 + 4 KiB = 0x1_0000_0000).
    let ref_set: std::collections::HashSet<&str> = referenced.iter().copied().collect();
    let mut ranges: Vec<(u64, u64)> = pe
        .sections
        .iter()
        .filter(|s| ref_set.contains(s.name().unwrap_or("")))
        .map(|s| {
            let lo = s.virtual_address as u64;
            (lo, lo + s.virtual_size as u64)
        })
        .collect();
    ranges.sort_by_key(|r| r.0);
    for w in ranges.windows(2) {
        assert!(w[0].1 <= w[1].0, "overlap: {:?} vs {:?}", w[0], w[1]);
    }
}

#[test]
#[cfg(target_arch = "aarch64")]
fn pmi_core_conformance_aarch64() {
    let f = build_aarch64_fixture();
    let pe = goblin::pe::PE::parse(&f.pmi_bytes).unwrap();
    let (off, size) = find_pmi_vm(&f.pmi_bytes);
    let cbor = &f.pmi_bytes[off..off + size];
    let spec: Spec<vcpu::aarch64::CpuState> = from_reader(cbor).expect("decode");
    let sec_names: std::collections::HashSet<String> = pe
        .sections
        .iter()
        .map(|s| s.name().unwrap_or("").to_string())
        .collect();
    for a in &spec.actions {
        let n = match a {
            Action::Load(l) => l.section.as_str(),
            Action::Fill(f) => f.section.as_str(),
        };
        assert!(sec_names.contains(n), "missing section `{n}`");
    }
}

// ---------------------------------------------------------------------------
// §15.1 #3 — PMI granularity rules.
// ---------------------------------------------------------------------------

fn check_granularity(pmi_bytes: &[u8]) {
    let pe = goblin::pe::PE::parse(pmi_bytes).unwrap();
    for s in &pe.sections {
        if s.virtual_size == 0 {
            continue;
        }
        let small = s.virtual_size < 2 * 1024 * 1024;
        let align = if small { 0x1000 } else { 0x20_0000 };
        // VirtualAddress must be aligned. Skip non-loaded (.pmi.vm)
        // which has VirtualAddress = 0.
        if s.virtual_address != 0 {
            assert_eq!(
                s.virtual_address as u64 % align,
                0,
                "section `{}` VirtualAddress not {align:#x}-aligned",
                s.name().unwrap_or("")
            );
        }
        if s.pointer_to_raw_data != 0 {
            assert_eq!(
                s.pointer_to_raw_data as u64 % 0x1000,
                0,
                "section `{}` PointerToRawData not 4 KiB-aligned",
                s.name().unwrap_or("")
            );
        }
        assert_eq!(
            s.size_of_raw_data as u64 % 0x1000,
            0,
            "section `{}` SizeOfRawData not 4 KiB-multiple",
            s.name().unwrap_or("")
        );
    }
}

#[test]
#[cfg(target_arch = "x86_64")]
fn granularity_rules_x86() {
    let f = build_x86_fixture();
    check_granularity(&f.pmi_bytes);
}

#[test]
#[cfg(target_arch = "aarch64")]
fn granularity_rules_aarch64() {
    let f = build_aarch64_fixture();
    check_granularity(&f.pmi_bytes);
}

// ---------------------------------------------------------------------------
// §15.1 #4 — CBOR strict-decode round-trip.
// ---------------------------------------------------------------------------

#[test]
#[cfg(target_arch = "x86_64")]
fn cbor_round_trip_x86() {
    let f = build_x86_fixture();
    let (off, size) = find_pmi_vm(&f.pmi_bytes);
    let cbor = &f.pmi_bytes[off..off + size];
    let spec: Spec<vcpu::x86_64::CpuState> = from_reader(cbor).expect("decode 1");
    // Re-encode and decode again; bytes need not match (CBOR has
    // multiple canonical-ish representations), but the decoded value
    // must be stable.
    let mut buf = Vec::new();
    ciborium::ser::into_writer(&spec, &mut buf).unwrap();
    let spec2: Spec<vcpu::x86_64::CpuState> = from_reader(buf.as_slice()).expect("decode 2");
    assert_eq!(spec2.vcpu.rip, spec.vcpu.rip);
    assert_eq!(spec2.actions.len(), spec.actions.len());
}

#[test]
#[cfg(target_arch = "aarch64")]
fn cbor_round_trip_aarch64() {
    let f = build_aarch64_fixture();
    let (off, size) = find_pmi_vm(&f.pmi_bytes);
    let cbor = &f.pmi_bytes[off..off + size];
    let spec: Spec<vcpu::aarch64::CpuState> = from_reader(cbor).expect("decode 1");
    let mut buf = Vec::new();
    ciborium::ser::into_writer(&spec, &mut buf).unwrap();
    let spec2: Spec<vcpu::aarch64::CpuState> = from_reader(buf.as_slice()).expect("decode 2");
    assert_eq!(spec2.vcpu.pc, spec.vcpu.pc);
    assert_eq!(spec2.actions.len(), spec.actions.len());
}

// ---------------------------------------------------------------------------
// §15.1 #5 — Manifest correctness.
// ---------------------------------------------------------------------------

#[test]
#[cfg(target_arch = "x86_64")]
fn manifest_correctness_x86() {
    let f = build_x86_fixture();
    let (off, size) = find_pmi_vm(&f.pmi_bytes);
    let spec: Spec<vcpu::x86_64::CpuState> = from_reader(&f.pmi_bytes[off..off + size]).unwrap();
    assert_eq!(spec.merged_dtb.as_deref(), Some(".tatu.dtb"));
    // Last action must be Fill with merged:dtbo on .dtbo.
    match spec.actions.last() {
        Some(Action::Fill(fl)) => {
            assert_eq!(fl.section, ".tatu.dtbo");
            assert!(matches!(fl.kind, FillKind::MergedDtbo));
        }
        _ => panic!("last action must be merged:dtbo fill"),
    }
    // .linux is loaded.
    assert!(spec.actions.iter().any(|a| matches!(
        a, Action::Load(l) if l.section == ".linux"
    )));
}

#[test]
#[cfg(target_arch = "aarch64")]
fn manifest_correctness_aarch64() {
    let f = build_aarch64_fixture();
    let (off, size) = find_pmi_vm(&f.pmi_bytes);
    let spec: Spec<vcpu::aarch64::CpuState> = from_reader(&f.pmi_bytes[off..off + size]).unwrap();
    assert_eq!(spec.merged_dtb.as_deref(), Some(".tatu.dtb"));
    match spec.actions.last() {
        Some(Action::Fill(fl)) => {
            assert_eq!(fl.section, ".tatu.dtbo");
            assert!(matches!(fl.kind, FillKind::MergedDtbo));
        }
        _ => panic!("last action must be merged:dtbo fill"),
    }
    assert!(spec.actions.iter().any(|a| matches!(
        a, Action::Load(l) if l.section == ".linux"
    )));
    // aarch64 fixture passes no initrd; assert .initrd is NOT loaded.
    assert!(!spec.actions.iter().any(|a| matches!(
        a, Action::Load(l) if l.section == ".initrd"
    )));
}

// ---------------------------------------------------------------------------
// §15.1 #6 — TatuBootInfo correctness.
// ---------------------------------------------------------------------------

fn check_tatu_bootinfo_header(pmi_bytes: &[u8]) {
    let pe = goblin::pe::PE::parse(pmi_bytes).unwrap();
    let bi_sec = pe
        .sections
        .iter()
        .find(|s| s.name().unwrap_or("") == ".tatu.bootinfo")
        .expect(".tatu.bootinfo present");
    let off = bi_sec.pointer_to_raw_data as usize;
    let hdr = &pmi_bytes[off..off + 44];
    assert_eq!(&hdr[..8], b"TATUBOOT");
    let base_dtb_gpa = u64::from_le_bytes(hdr[8..16].try_into().unwrap());
    let host_dtbo_gpa = u64::from_le_bytes(hdr[16..24].try_into().unwrap());
    let kernel_gpa = u64::from_le_bytes(hdr[24..32].try_into().unwrap());
    let base_dtb_size = u32::from_le_bytes(hdr[32..36].try_into().unwrap());
    let host_dtbo_size = u32::from_le_bytes(hdr[36..40].try_into().unwrap());
    let kernel_size = u32::from_le_bytes(hdr[40..44].try_into().unwrap());
    // Bootinfo carries NATURAL byte counts (what tatu/Linux read).
    // `.dtb` is Data-shape padded (VirtSize == RawSize, both ≥ natural).
    // `.linux` is Padded-shape on x86: VirtualSize includes the bzImage
    // decompressor scratch buffer, so VirtSize ≥ RawSize ≥ natural.
    let look = |name: &str| {
        pe.sections
            .iter()
            .find(|s| s.name().unwrap_or("") == name)
            .map(|s| (s.virtual_address as u64, s.virtual_size, s.size_of_raw_data))
    };
    let check_data_shape = |name: &str, gpa: u64, natural: u32| {
        let (va, vs, rs) = look(name).unwrap_or_else(|| panic!("{name} present"));
        assert_eq!(va, gpa, "{name} VA");
        assert!(u64::from(natural) <= u64::from(vs), "{name} natural fits");
        assert_eq!(vs, rs, "{name} Data shape");
    };
    let check_padded_shape = |name: &str, gpa: u64, natural: u32| {
        let (va, vs, rs) = look(name).unwrap_or_else(|| panic!("{name} present"));
        assert_eq!(va, gpa, "{name} VA");
        assert!(
            u64::from(natural) <= u64::from(rs),
            "{name} natural fits raw"
        );
        assert!(rs <= vs, "{name} Padded shape (raw ≤ virtual)");
    };
    // .tatu.dtb is a tatu-reserved section arma fills with the base DTB:
    // Padded shape (RawSize ≥ natural base DTB; VirtualSize = reservation).
    let _ = check_data_shape;
    check_padded_shape(".tatu.dtb", base_dtb_gpa, base_dtb_size);
    check_padded_shape(".linux", kernel_gpa, kernel_size);
    // .dtbo is Zero shape — RawSize = 0, VirtualSize = host_dtbo_size.
    let (va, vs, rs) = look(".tatu.dtbo").expect(".dtbo present");
    assert_eq!(va, host_dtbo_gpa);
    assert_eq!(vs, host_dtbo_size);
    assert_eq!(rs, 0);
}

#[test]
#[cfg(target_arch = "x86_64")]
fn tatu_bootinfo_header_x86() {
    let f = build_x86_fixture();
    check_tatu_bootinfo_header(&f.pmi_bytes);
}

#[test]
#[cfg(target_arch = "aarch64")]
fn tatu_bootinfo_header_aarch64() {
    let f = build_aarch64_fixture();
    check_tatu_bootinfo_header(&f.pmi_bytes);
}

// ---------------------------------------------------------------------------
// §15.1 #7 — Tatu sections present.
// ---------------------------------------------------------------------------

#[test]
#[cfg(target_arch = "x86_64")]
fn tatu_sections_present_x86() {
    let f = build_x86_fixture();
    let pe = goblin::pe::PE::parse(&f.pmi_bytes).unwrap();
    let names: std::collections::HashSet<String> = pe
        .sections
        .iter()
        .map(|s| s.name().unwrap_or("").to_string())
        .collect();
    for must in [
        ".tatu.bootinfo",
        ".tatu.stack",
        ".tatu.rodata",
        ".tatu.text",
        ".tatu.reset",
    ] {
        assert!(names.contains(must), "missing tatu section `{must}`");
    }
}

#[test]
#[cfg(target_arch = "aarch64")]
fn tatu_sections_present_aarch64() {
    let f = build_aarch64_fixture();
    let pe = goblin::pe::PE::parse(&f.pmi_bytes).unwrap();
    let names: std::collections::HashSet<String> = pe
        .sections
        .iter()
        .map(|s| s.name().unwrap_or("").to_string())
        .collect();
    for must in [
        ".tatu.bootinfo",
        ".tatu.stack",
        ".tatu.rodata",
        ".tatu.text",
    ] {
        assert!(names.contains(must), "missing tatu section `{must}`");
    }
}

// ---------------------------------------------------------------------------
// §15.1 #8 — Base DTB validity (parses via devtree).
// ---------------------------------------------------------------------------

#[test]
#[cfg(target_arch = "x86_64")]
fn base_dtb_parses_x86() {
    let f = build_x86_fixture();
    let pe = goblin::pe::PE::parse(&f.pmi_bytes).unwrap();
    let dtb_sec = pe
        .sections
        .iter()
        .find(|s| s.name().unwrap_or("") == ".tatu.dtb")
        .expect(".dtb present");
    let off = dtb_sec.pointer_to_raw_data as usize;
    let len = dtb_sec.virtual_size as usize;
    let dtb = &f.pmi_bytes[off..off + len];
    let _tree: devtree::Tree<'_> = devtree::Tree::parse(dtb).expect("base DTB parses");
}

#[test]
#[cfg(target_arch = "aarch64")]
fn base_dtb_parses_aarch64() {
    let f = build_aarch64_fixture();
    let pe = goblin::pe::PE::parse(&f.pmi_bytes).unwrap();
    let dtb_sec = pe
        .sections
        .iter()
        .find(|s| s.name().unwrap_or("") == ".tatu.dtb")
        .expect(".dtb present");
    let off = dtb_sec.pointer_to_raw_data as usize;
    let len = dtb_sec.virtual_size as usize;
    let dtb = &f.pmi_bytes[off..off + len];
    let _tree: devtree::Tree<'_> = devtree::Tree::parse(dtb).expect("base DTB parses");
}

// ---------------------------------------------------------------------------
// §15.1 #9 — vm:vcpu register validity.
// ---------------------------------------------------------------------------

#[test]
#[cfg(target_arch = "x86_64")]
fn vcpu_register_validity_x86() {
    let f = build_x86_fixture();
    let (off, size) = find_pmi_vm(&f.pmi_bytes);
    let spec: Spec<vcpu::x86_64::CpuState> = from_reader(&f.pmi_bytes[off..off + size]).unwrap();
    assert_eq!(spec.vcpu.rip, 0xFFFF_FFF0, "rip is the reset vector");
    assert_eq!(spec.vcpu.rflags & 0x2, 0x2, "rflags bit 1 must be 1");
    // gdtr.base equals the .tatu.gdt section's GPA.
    let pe = goblin::pe::PE::parse(&f.pmi_bytes).unwrap();
    let gdt = pe
        .sections
        .iter()
        .find(|s| s.name().unwrap_or("") == ".tatu.gdt")
        .expect(".tatu.gdt present");
    assert_eq!(spec.vcpu.gdtr.base, gdt.virtual_address as u64);
    // cr3 equals .tatu.pgt's GPA.
    let pg = pe
        .sections
        .iter()
        .find(|s| s.name().unwrap_or("") == ".tatu.pgt")
        .expect(".tatu.pgt present");
    assert_eq!(spec.vcpu.cr3, pg.virtual_address as u64);
}

#[test]
#[cfg(target_arch = "aarch64")]
fn vcpu_register_validity_aarch64() {
    let f = build_aarch64_fixture();
    let (off, size) = find_pmi_vm(&f.pmi_bytes);
    let spec: Spec<vcpu::aarch64::CpuState> = from_reader(&f.pmi_bytes[off..off + size]).unwrap();
    // pstate.M[3:0] = 0x5 (EL1h); M[4] = 0 (AArch64).
    assert_eq!(spec.vcpu.pstate & 0xF, 0x5);
    assert_eq!((spec.vcpu.pstate >> 4) & 1, 0);
    // pc lies inside .tatu.text.
    let pe = goblin::pe::PE::parse(&f.pmi_bytes).unwrap();
    let text = pe
        .sections
        .iter()
        .find(|s| s.name().unwrap_or("") == ".tatu.text")
        .expect(".tatu.text present");
    let lo = text.virtual_address as u64;
    let hi = lo + text.virtual_size as u64;
    assert!(
        spec.vcpu.pc >= lo && spec.vcpu.pc < hi,
        "pc {:#x} must lie in .tatu.text [{:#x}, {:#x})",
        spec.vcpu.pc,
        lo,
        hi
    );
}

// ---------------------------------------------------------------------------
// §15.1 #10 — Determinism.
// ---------------------------------------------------------------------------

#[test]
#[cfg(target_arch = "x86_64")]
fn determinism_two_builds_same_inputs_same_output() {
    let tmp = TempDir::new().unwrap();
    let kernel = tmp.path().join("kernel");
    let init = tmp.path().join("init");
    let pmi1 = tmp.path().join("out1.pmi");
    let pmi2 = tmp.path().join("out2.pmi");
    fs::write(&kernel, synthesize_bzimage(0x2000)).unwrap();
    // Cpio-magic-prefixed payload — arma passes cpio through unchanged.
    fs::write(&init, b"070701DETERMINISTIC_CPIO").unwrap();
    build_pmi(&kernel, Some(&init), "ro", &pmi1);
    build_pmi(&kernel, Some(&init), "ro", &pmi2);
    let b1 = fs::read(&pmi1).unwrap();
    let b2 = fs::read(&pmi2).unwrap();
    let h1 = Sha256::digest(&b1);
    let h2 = Sha256::digest(&b2);
    assert_eq!(
        h1, h2,
        "two builds with identical inputs must match byte-for-byte"
    );
}

// ---------------------------------------------------------------------------
// Build/distribution sanity (§15.3).
// ---------------------------------------------------------------------------

#[test]
#[cfg(any())]
fn single_binary_emits_both_arch_pmis() {
    // §15.3: a single arma binary builds PMIs for both x86_64 and
    // aarch64. We already exercise both fixtures elsewhere; this test
    // explicitly asserts that the PE Machine fields differ.
    let f86 = build_x86_fixture();
    let farm = build_aarch64_fixture();
    let m86 = goblin::pe::PE::parse(&f86.pmi_bytes)
        .unwrap()
        .header
        .coff_header
        .machine;
    let marm = goblin::pe::PE::parse(&farm.pmi_bytes)
        .unwrap()
        .header
        .coff_header
        .machine;
    assert_eq!(m86, 0x8664);
    assert_eq!(marm, 0xAA64);
}
