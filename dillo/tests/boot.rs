//! End-to-end boot tests (feature `vm-tests`, Linux/KVM).
//!
//! For each config: build a PMI with arma (Alpine host-arch kernel +
//! the `snuffler` guest probe as initrd), boot it under dillo, capture the
//! console, extract `snuffler`'s `Report` from between its sentinels, and
//! assert what the guest actually saw matches what we asked dillo for.
//!
//! Gated on the `vm-tests` feature *and* a runtime `/dev/kvm` check, so a
//! plain `cargo test` (or a runner without KVM) is a clean skip.
#![cfg(feature = "vm-tests")]

use std::path::{Path, PathBuf};
use std::process::Command;

use rstest::rstest;
use snuffler::{REPORT_BEGIN, REPORT_END, Report};
use tempfile::TempDir;

const ALPINE: &str = "https://dl-cdn.alpinelinux.org/alpine/latest-stable/releases";

// dillo boots a same-arch guest, so the host arch picks the kernel.
#[cfg(target_arch = "x86_64")]
mod host {
    pub const ARCH: &str = "x86_64";
    pub const PROFILE: &str = "x86-64-v2";
    pub const CONFIG: &str = "CONFIG_PCI=y\nCONFIG_VIRTIO_PCI=y\nCONFIG_VIRTIO_MMIO=y\n";
}
#[cfg(target_arch = "aarch64")]
mod host {
    pub const ARCH: &str = "aarch64";
    pub const PROFILE: &str = "armv8.0-a";
    pub const CONFIG: &str =
        "CONFIG_PCI=y\nCONFIG_PCI_HOST_GENERIC=y\nCONFIG_VIRTIO_PCI=y\nCONFIG_VIRTIO_MMIO=y\n";
}

fn kernel_url() -> String {
    format!("{ALPINE}/{}/netboot/vmlinuz-virt", host::ARCH)
}

/// KVM must be present and usable, else these tests are a no-op skip.
fn kvm_available() -> bool {
    use std::fs::OpenOptions;
    Path::new("/dev/kvm").exists() && OpenOptions::new().read(true).write(true).open("/dev/kvm").is_ok()
}

/// Build a PMI: Alpine host kernel + snuffler initrd, console on hvc0.
fn build_pmi(dir: &Path) -> PathBuf {
    let kernel = burrow::fetch(&kernel_url()).expect("fetch kernel");
    let cfg = dir.join("kernel.config");
    std::fs::write(&cfg, host::CONFIG).unwrap();
    let pmi = dir.join("boot.pmi");
    let st = Command::new(env!("CARGO_BIN_FILE_ARMA_arma"))
        .arg("build")
        .args(["--cmdline", "console=hvc0"])
        .args(["--profile", host::PROFILE])
        .arg("--config")
        .arg(&cfg)
        .arg("--kernel")
        .arg(&kernel)
        .arg("--initrd")
        .arg(env!("CARGO_BIN_FILE_SNUFFLER_snuffler"))
        .arg(&pmi)
        .status()
        .expect("spawn arma");
    assert!(st.success(), "arma build failed");
    pmi
}

/// Boot `pmi` under dillo (wrapped in `timeout` so a hang fails fast),
/// returning the combined console output.
fn boot(pmi: &Path, mem_mib: u32, cpus: u32) -> String {
    let out = Command::new("timeout")
        .arg("120")
        .arg(env!("CARGO_BIN_EXE_dillo"))
        .arg("--pmi")
        .arg(pmi)
        .args(["--memory", &mem_mib.to_string()])
        .args(["--cpus", &cpus.to_string()])
        .output()
        .expect("spawn dillo");
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    s
}

/// Extract snuffler's JSON `Report` from the console output.
fn report(output: &str) -> Report {
    let begin = output
        .find(REPORT_BEGIN)
        .unwrap_or_else(|| panic!("no report sentinel in output:\n{output}"))
        + REPORT_BEGIN.len();
    let end = output[begin..]
        .find(REPORT_END)
        .expect("no report end sentinel")
        + begin;
    serde_json::from_str(&output[begin..end]).expect("parse Report json")
}

#[rstest]
fn boots_and_reports(#[values(256, 1024)] mem_mib: u32, #[values(1, 2)] cpus: u32) {
    if !kvm_available() {
        eprintln!("skip: /dev/kvm unavailable");
        return;
    }
    let tmp = TempDir::new().unwrap();
    let pmi = build_pmi(tmp.path());
    let r = report(&boot(&pmi, mem_mib, cpus));

    assert_eq!(r.arch, host::ARCH, "guest arch");
    assert_eq!(r.cpu.online_count, cpus as usize, "online cpus");
    // The kernel keeps some RAM for itself; total reported is a bit under.
    let asked_kib = u64::from(mem_mib) * 1024;
    assert!(
        r.memory.total_kib >= asked_kib * 7 / 10 && r.memory.total_kib <= asked_kib,
        "MemTotal {} kib not within [70%,100%] of asked {asked_kib}",
        r.memory.total_kib
    );
    assert!(
        r.consoles.iter().any(|c| c.starts_with("hvc0")),
        "hvc0 not among consoles: {:?}",
        r.consoles
    );
}
