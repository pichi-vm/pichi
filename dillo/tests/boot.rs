//! End-to-end boot tests (feature `vm-tests`; KVM on Linux, WHP on Windows).
//!
//! For each config: build a PMI with arma (Alpine host-arch kernel +
//! the `snuffler` guest probe as initrd), boot it under dillo, capture the
//! console, extract `snuffler`'s `Report` from between its sentinels, and
//! assert what the guest actually saw matches what we asked dillo for.
//!
//! Gated on the `vm-tests` feature. On Linux a missing/inaccessible `/dev/kvm`
//! is a clean skip so a plain local `cargo test` doesn't fail — but once the
//! suite actually runs a boot, a guest that does not report back is a hard
//! failure, never a silent skip. CI runs this on the platforms it claims to
//! support (Linux/KVM, Windows/WHP), so there it must genuinely boot.
#![cfg(feature = "vm-tests")]

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use rstest::rstest;
use snuffler::{REPORT_BEGIN, REPORT_END, Report};
use tempfile::TempDir;
use wait_timeout::ChildExt;

const ALPINE: &str = "https://dl-cdn.alpinelinux.org/alpine/latest-stable/releases";

// dillo boots a same-arch guest, so the host arch picks the kernel.
#[cfg(target_arch = "x86_64")]
mod host {
    pub(crate) const ARCH: &str = "x86_64";
    pub(crate) const PROFILE: &str = "x86-64-v2";
    pub(crate) const CONFIG: &str = "CONFIG_PCI=y\nCONFIG_VIRTIO_PCI=y\nCONFIG_VIRTIO_MMIO=y\n";
}
#[cfg(target_arch = "aarch64")]
mod host {
    pub(crate) const ARCH: &str = "aarch64";
    pub(crate) const PROFILE: &str = "armv8.0-a";
    pub(crate) const CONFIG: &str =
        "CONFIG_PCI=y\nCONFIG_PCI_HOST_GENERIC=y\nCONFIG_VIRTIO_PCI=y\nCONFIG_VIRTIO_MMIO=y\n";
}

fn kernel_url() -> String {
    format!("{ALPINE}/{}/netboot/vmlinuz-virt", host::ARCH)
}

/// Whether the host hypervisor is usable; otherwise these tests no-op skip.
/// On Linux we can cheaply probe `/dev/kvm`; HVF (macOS) and WHP (Windows)
/// have no equivalent file gate, and the CI runners provide them, so we
/// assume present and let a genuine absence surface as a boot failure.
fn hypervisor_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::fs::OpenOptions;
        Path::new("/dev/kvm").exists()
            && OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/kvm")
                .is_ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
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

/// Boot `pmi` under dillo, returning the combined console output. dillo's
/// stdout/stderr are redirected to files (no pipe-buffer deadlock) and the
/// child is killed if it overruns the timeout. Cross-platform (no `timeout`
/// coreutil).
fn boot(pmi: &Path, mem_mib: u32, cpus: u32, dir: &Path) -> String {
    let out_path = dir.join("console.out");
    let err_path = dir.join("console.err");
    let mut child = Command::new(env!("CARGO_BIN_EXE_dillo"))
        .arg("--pmi")
        .arg(pmi)
        .args(["--memory", &mem_mib.to_string()])
        .args(["--cpus", &cpus.to_string()])
        .stdout(File::create(&out_path).unwrap())
        .stderr(File::create(&err_path).unwrap())
        .spawn()
        .expect("spawn dillo");
    if child
        .wait_timeout(Duration::from_secs(120))
        .expect("wait dillo")
        .is_none()
    {
        let _ = child.kill();
        let _ = child.wait();
        panic!("dillo boot timed out");
    }
    let mut s = std::fs::read_to_string(&out_path).unwrap_or_default();
    s.push_str(&std::fs::read_to_string(&err_path).unwrap_or_default());
    s
}

/// Extract snuffler's JSON `Report` from the console output, if present.
fn parse_report(output: &str) -> Option<Report> {
    let begin = output.find(REPORT_BEGIN)? + REPORT_BEGIN.len();
    let end = output[begin..].find(REPORT_END)? + begin;
    serde_json::from_str(&output[begin..end]).ok()
}

#[rstest]
fn boots_and_reports(#[values(256, 1024)] mem_mib: u32, #[values(1, 2)] cpus: u32) {
    // Local convenience only: if there is no usable /dev/kvm (Linux dev box
    // without KVM), skip rather than fail. This is the *only* skip; once we
    // attempt a boot below, anything short of a guest report is a failure.
    if !hypervisor_available() {
        eprintln!("skip: no usable /dev/kvm on this host (local dev only)");
        return;
    }
    let tmp = TempDir::new().unwrap();
    let pmi = build_pmi(tmp.path());
    let output = boot(&pmi, mem_mib, cpus, tmp.path());
    // Surface the hypervisor's own enlightenment/serial setup so the host's
    // actual capability level is visible in CI (under --nocapture) rather than
    // hidden in the captured child output.
    for line in output.lines() {
        if line.contains("enlightened") || line.contains("ns16550a") {
            eprintln!("[dillo] {line}");
        }
    }
    let r = parse_report(&output)
        .unwrap_or_else(|| panic!("boot produced no snuffler report (guest did not boot):\n{output}"));

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
