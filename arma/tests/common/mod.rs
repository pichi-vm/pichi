//! Shared helpers for integration tests.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// Path to the built `arma` binary. Cargo sets `CARGO_BIN_EXE_<name>`
/// for integration tests.
pub fn arma_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_arma"))
}

pub fn build_pmi(kernel: &Path, initrd: Option<&Path>, cmdline: &str, out: &Path) {
    build_pmi_with_profile(kernel, initrd, cmdline, "x86-64-v3", out);
}

pub fn build_pmi_with_profile(
    kernel: &Path,
    initrd: Option<&Path>,
    cmdline: &str,
    cpu_profile: &str,
    out: &Path,
) {
    let mut cmd = Command::new(arma_bin());
    cmd.arg("build")
        .arg("--kernel")
        .arg(kernel)
        .arg("--cmdline")
        .arg(cmdline)
        .arg("--profile")
        .arg(cpu_profile);
    if let Some(p) = initrd {
        cmd.arg("--initrd").arg(p);
    }
    cmd.arg(out); // positional <output>, last
    let st = cmd.status().expect("spawn arma");
    assert!(st.success(), "arma build failed: {st:?}");
}

/// An embedded `CONFIG_IKCONFIG` blob (PCI-only) so `arma build` without
/// `--config` can infer slots (C5). PCI + virtio-pci + host-generic, no
/// virtio-mmio ⇒ a PCIe bridge and no virtio-mmio nodes (the shape the DTB
/// tests expect).
fn ikconfig_blob() -> Vec<u8> {
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;
    let cfg = b"CONFIG_PCI=y\nCONFIG_VIRTIO_PCI=y\nCONFIG_PCI_HOST_GENERIC=y\n";
    let mut e = GzEncoder::new(Vec::new(), Compression::fast());
    e.write_all(cfg).unwrap();
    let gz = e.finish().unwrap();
    let mut out = Vec::with_capacity(gz.len() + 16);
    out.extend_from_slice(b"IKCFG_ST");
    out.extend_from_slice(&gz);
    out.extend_from_slice(b"IKCFG_ED");
    out
}

/// Build a synthetic x86 ELF `vmlinux` that passes arma's validation, with the
/// IKCONFIG blob appended (outside any PT_LOAD) so `arma build` without
/// `--config` can still infer slots. One PT_LOAD at the conventional 16 MiB
/// link base, entry at the base.
pub fn synthesize_vmlinux(payload_size: usize) -> Vec<u8> {
    const EHSIZE: usize = 64;
    const PHENTSIZE: usize = 56;
    const BASE: u64 = 0x100_0000;
    let filesz = payload_size.max(0x1000) as u64;

    let mut ehdr = [0u8; EHSIZE];
    ehdr[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    ehdr[4] = 2; // ELFCLASS64
    ehdr[5] = 1; // ELFDATA2LSB
    ehdr[6] = 1; // EI_VERSION
    ehdr[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    ehdr[18..20].copy_from_slice(&62u16.to_le_bytes()); // EM_X86_64
    ehdr[20..24].copy_from_slice(&1u32.to_le_bytes()); // e_version
    ehdr[24..32].copy_from_slice(&BASE.to_le_bytes()); // e_entry (== base)
    ehdr[32..40].copy_from_slice(&(EHSIZE as u64).to_le_bytes()); // e_phoff
    ehdr[52..54].copy_from_slice(&(EHSIZE as u16).to_le_bytes()); // e_ehsize
    ehdr[54..56].copy_from_slice(&(PHENTSIZE as u16).to_le_bytes()); // e_phentsize
    ehdr[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum

    let data_off = (EHSIZE + PHENTSIZE) as u64;
    let mut ph = [0u8; PHENTSIZE];
    ph[0..4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
    ph[8..16].copy_from_slice(&data_off.to_le_bytes()); // p_offset
    ph[16..24].copy_from_slice(&BASE.to_le_bytes()); // p_vaddr
    ph[24..32].copy_from_slice(&BASE.to_le_bytes()); // p_paddr
    ph[32..40].copy_from_slice(&filesz.to_le_bytes()); // p_filesz
    ph[40..48].copy_from_slice(&filesz.to_le_bytes()); // p_memsz

    let mut v = Vec::new();
    v.extend_from_slice(&ehdr);
    v.extend_from_slice(&ph);
    v.extend(std::iter::repeat_n(0u8, filesz as usize));
    v.extend_from_slice(&ikconfig_blob());
    v
}

/// Build a synthetic arm64 Image that passes arma's validation.
pub fn synthesize_arm64_image() -> Vec<u8> {
    let mut v = vec![0u8; 4096];
    v[56..60].copy_from_slice(&0x644D_5241u32.to_le_bytes());
    v.extend_from_slice(&ikconfig_blob());
    v
}

/// Locate the `.pmi.vm` section in a PE file's section table and
/// return (offset, size). Panics if not found.
pub fn find_pmi_vm(bytes: &[u8]) -> (usize, usize) {
    let pe = goblin::pe::PE::parse(bytes).expect("parse PE");
    for s in &pe.sections {
        // Section name padded with NULs to 8 bytes; goblin may also
        // resolve long names via the string table.
        let name = s.name().unwrap_or("");
        if name == ".pmi.vm" {
            return (s.pointer_to_raw_data as usize, s.virtual_size as usize);
        }
    }
    panic!("no .pmi.vm section found");
}
