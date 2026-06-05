//! Real-kernel image-generation tests.
//!
//! Builds PMIs from the actual Alpine `vmlinuz-virt` kernels — x86 bzImage
//! and arm64 EFI-zboot (exercising `kernel::unwrap_zboot`) — across a matrix
//! of device configurations, and asserts the produced PMI is well-formed.
//! Pure image construction (no VM), so it runs on every host OS. Kernels are
//! fetched once via `burrow` and cached under `target/`.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use ciborium::de::from_reader;
use pmi::vm::{Spec, vcpu};
use rstest::rstest;
use tempfile::TempDir;

use common::{arma_bin, find_pmi_vm};

const GRAN: u64 = 2 * 1024 * 1024;
const ALPINE: &str = "https://dl-cdn.alpinelinux.org/alpine/latest-stable/releases";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    fn kernel_url(self) -> String {
        let a = match self {
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "aarch64",
        };
        format!("{ALPINE}/{a}/netboot/vmlinuz-virt")
    }

    fn pe_machine(self) -> u16 {
        match self {
            Arch::X86_64 => 0x8664,
            Arch::Aarch64 => 0xAA64,
        }
    }

    /// Minimal kernel config exposing the transports the Alpine `virt`
    /// kernels carry — enough for arma's slot inference.
    fn config(self) -> &'static str {
        match self {
            Arch::X86_64 => "CONFIG_PCI=y\nCONFIG_VIRTIO_PCI=y\nCONFIG_VIRTIO_MMIO=y\n",
            Arch::Aarch64 => {
                "CONFIG_PCI=y\nCONFIG_PCI_HOST_GENERIC=y\nCONFIG_VIRTIO_PCI=y\nCONFIG_VIRTIO_MMIO=y\n"
            }
        }
    }

    fn profile(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86-64-v2",
            Arch::Aarch64 => "armv8.0-a",
        }
    }

    fn cmdline(self) -> &'static str {
        match self {
            Arch::X86_64 => "console=ttyS0",
            Arch::Aarch64 => "console=ttyAMA0",
        }
    }
}

/// Fetch (and cache) the Alpine kernel for `arch`.
fn kernel(arch: Arch) -> PathBuf {
    burrow::fetch(&arch.kernel_url()).expect("fetch alpine kernel")
}

#[derive(Clone, Copy, Debug)]
struct Case {
    serial: bool,
    initrd: bool,
    pci_slots: Option<u32>,
    mmio_slots: Option<u32>,
}

/// Run `arma build` for `arch`+`case` in `dir`, returning the PMI bytes.
fn build(arch: Arch, case: Case, dir: &Path) -> Vec<u8> {
    std::fs::create_dir_all(dir).unwrap();
    let cfg = dir.join("kernel.config");
    std::fs::write(&cfg, arch.config()).unwrap();
    let pmi = dir.join("out.pmi");

    let mut cmd = Command::new(arma_bin());
    cmd.arg("build")
        .arg("--kernel")
        .arg(kernel(arch))
        .arg("--config")
        .arg(&cfg)
        .arg("--cmdline")
        .arg(arch.cmdline())
        .arg("--profile")
        .arg(arch.profile());
    if case.serial {
        cmd.arg("--serial");
    }
    if let Some(n) = case.pci_slots {
        cmd.arg("--pci-slots").arg(n.to_string());
    }
    if let Some(n) = case.mmio_slots {
        cmd.arg("--mmio-slots").arg(n.to_string());
    }
    if case.initrd {
        let init = dir.join("init");
        std::fs::write(&init, b"070701FAKE_CPIO_INITRD_PAYLOAD").unwrap();
        cmd.arg("--initrd").arg(&init);
    }
    cmd.arg(&pmi);

    let st = cmd.status().expect("spawn arma");
    assert!(st.success(), "arma build failed: {arch:?} {case:?}");
    std::fs::read(&pmi).unwrap()
}

/// The whole matrix builds a well-formed PMI: valid PE for the arch, every
/// LARGE (>=2 MiB) section 2 MiB-aligned in both VA and file offset (the PMI
/// granularity contract), and a parseable `.pmi.vm` manifest.
#[rstest]
fn builds_well_formed_pmi(
    #[values(Arch::X86_64, Arch::Aarch64)] arch: Arch,
    #[values(false, true)] serial: bool,
    #[values(false, true)] initrd: bool,
    #[values((None, None), (Some(4), Some(2)), (Some(8), Some(0)))] slots: (Option<u32>, Option<u32>),
) {
    let tmp = TempDir::new().unwrap();
    let case = Case {
        serial,
        initrd,
        pci_slots: slots.0,
        mmio_slots: slots.1,
    };
    let bytes = build(arch, case, tmp.path());

    assert_eq!(&bytes[..2], b"MZ", "not a PE");
    let pe = goblin::pe::PE::parse(&bytes).expect("parse PE");
    assert_eq!(pe.header.coff_header.machine, arch.pe_machine());

    for s in &pe.sections {
        if u64::from(s.virtual_size) >= GRAN {
            let name = s.name().unwrap_or("?");
            assert_eq!(
                u64::from(s.virtual_address) % GRAN,
                0,
                "section {name} VA not 2 MiB-aligned"
            );
            assert_eq!(
                u64::from(s.pointer_to_raw_data) % GRAN,
                0,
                "section {name} file offset not 2 MiB-aligned"
            );
        }
    }

    let (off, size) = find_pmi_vm(&bytes);
    let manifest = &bytes[off..off + size];
    let actions = match arch {
        Arch::X86_64 => {
            from_reader::<Spec<vcpu::x86_64::CpuState>, _>(manifest)
                .expect("decode x86 .pmi.vm")
                .actions
                .len()
        }
        Arch::Aarch64 => {
            from_reader::<Spec<vcpu::aarch64::CpuState>, _>(manifest)
                .expect("decode arm .pmi.vm")
                .actions
                .len()
        }
    };
    assert!(actions > 0, "manifest has no actions");
}

/// Same inputs → byte-identical PMI (arma is a deterministic translator).
#[rstest]
fn deterministic(#[values(Arch::X86_64, Arch::Aarch64)] arch: Arch) {
    let tmp = TempDir::new().unwrap();
    let case = Case {
        serial: true,
        initrd: true,
        pci_slots: Some(4),
        mmio_slots: Some(2),
    };
    let a = build(arch, case, &tmp.path().join("a"));
    let b = build(arch, case, &tmp.path().join("b"));
    assert_eq!(a, b, "{arch:?} build is not deterministic");
}
