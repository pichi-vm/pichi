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

/// Build a synthetic bzImage that passes arma's validation.
pub fn synthesize_bzimage(payload_size: usize) -> Vec<u8> {
    let mut v = vec![0u8; payload_size.max(0x1000)];
    v[0x1F1] = 1; // setup_sects
    // HdrS at 0x202
    v[0x202..0x206].copy_from_slice(&0x5372_6448u32.to_le_bytes());
    // protocol 0x020F
    v[0x206..0x208].copy_from_slice(&0x020Fu16.to_le_bytes());
    // loadflags LOADED_HIGH
    v[0x211] = 0x01;
    // kernel_alignment at 0x230 (2 MiB)
    v[0x230..0x234].copy_from_slice(&0x0020_0000u32.to_le_bytes());
    // init_size at 0x260 (size of compressed image + decompressor
    // scratch). Pick payload_size + 4 MiB headroom so synthetic
    // bzImages get a sensible alloc footprint without overflowing.
    let init_size = (payload_size as u32).saturating_add(0x40_0000);
    v[0x260..0x264].copy_from_slice(&init_size.to_le_bytes());
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
