#![forbid(unsafe_code)]
#![deny(
    missing_debug_implementations,
    rust_2018_idioms,
    unreachable_pub,
    trivial_casts,
    trivial_numeric_casts
)]
#![warn(clippy::all, clippy::pedantic)]
// PE/FDT code routinely splits u64 GPAs into u32 cell pairs and packs
// known-bounded sizes into PE u32 fields. Each `as` truncation is
// either intentional (high/low pair) or guarded by a prior bounds
// check elsewhere in the pipeline. Allowing these crate-wide keeps
// pedantic catching genuine issues without forcing a `try_from` at
// every PE/FDT field write.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::module_name_repetitions,
    clippy::similar_names,
    clippy::too_many_lines
)]

mod base_dtb;
mod bootinfo;
mod build;
mod check;
mod initrd;
mod kconfig;
mod kernel;
mod manifest;
mod pe;
mod planner;
mod tatu;

// The native-arch tatu ELF comes from the artifact dependency declared in
// Cargo.toml. Cargo injects `CARGO_BIN_FILE_<DEP>_<bin>` for it.
#[cfg(target_arch = "x86_64")]
pub(crate) const TATU_X86_64: &[u8] = include_bytes!(env!("CARGO_BIN_FILE_TATU_tatu"));
#[cfg(target_arch = "aarch64")]
pub(crate) const TATU_AARCH64: &[u8] = include_bytes!(env!("CARGO_BIN_FILE_TATU_tatu"));

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "arma", version, about = "PMI builder for pichi-vm")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build a PMI from a kernel, optional initrd, and cmdline.
    Build(BuildArgs),

    /// Lint a built PMI's guest-physical layout (read-only). Renders the map
    /// (device island, payload, the PCIe BAR window + burned buddy) and flags
    /// fragmentation, alignment, and the window invariants. See device-model §6.
    Check(CheckArgs),
}

#[derive(Args)]
struct CheckArgs {
    /// The PMI to inspect.
    pmi: PathBuf,
}

#[derive(Args)]
struct BuildArgs {
    /// Kernel image (bzImage on x86_64; raw arm64 Image on aarch64).
    #[arg(long)]
    kernel: PathBuf,

    /// Initramfs. Auto-detected: cpio newc passed through; any other
    /// binary wrapped in a single-entry cpio archive at /init.
    #[arg(long)]
    initrd: Option<PathBuf>,

    /// Kernel command line. Required — arma does not pick defaults
    /// (operator/test-harness chooses; arma is a faithful translator,
    /// not a policy maker).
    #[arg(long)]
    cmdline: String,

    /// vCPU ISA baseline written to `cpu:profile` (the VMM validates it
    /// against the host). Optional: defaults to the RHEL 9 baseline so a stock
    /// RHEL guest runs — `armv8.0-a` (aarch64) / `x86-64-v2` (x86-64).
    #[arg(long)]
    profile: Option<String>,

    /// Kernel build config (text Kconfig). Drives slot inference and
    /// drivability checks. If omitted, Arma falls back to a PCI-bridge
    /// default (TODO C5: extract the kernel's embedded CONFIG_IKCONFIG).
    #[arg(long)]
    config: Option<PathBuf>,

    /// virtio-mmio transport slot count. Default: inferred from `--config`
    /// (see device-model.md §6 Slot composition).
    #[arg(long = "mmio-slots")]
    mmio_slots: Option<u32>,

    /// PCIe slot count; `0` ⇒ no host bridge and no 64-bit window. Default:
    /// inferred from `--config`.
    #[arg(long = "pci-slots")]
    pci_slots: Option<u32>,

    /// 64-bit BAR window size in bits (window = `2^B` bytes). Default per-arch:
    /// 34 (aarch64) / 37 (x86-64). See device-model.md §6.
    #[arg(long = "pci-window")]
    pci_window: Option<u32>,

    /// Minimum guest-physical address bits `X` (the compatibility watermark).
    /// Default per-arch: 36 (aarch64) / 39 (x86-64). Invariant `X ≥ B+2`.
    #[arg(long = "min-addr-space")]
    min_addr_space: Option<u32>,

    /// Declare a canonical serial port in the DTB for early-boot debug
    /// output. Absent = no serial node, no UART emulation in the VMM,
    /// no ttyS0 in the guest (rely on virtio-console once it binds).
    /// Present = one MMIO `ns16550a` port at the per-arch canonical
    /// address: 0x09000000 IO-APIC pin 4 on x86_64; 0x0A110000 SPI 1
    /// on aarch64. Also declares `/aliases/serial0`, sets
    /// `/chosen/stdout-path = "serial0:115200n8"`, and prepends
    /// `earlycon` to the kernel command line. It does not choose the
    /// normal console; pass `console=...` explicitly if desired.
    #[arg(long = "serial")]
    serial: bool,

    /// Output PMI path (positional).
    #[arg(value_name = "OUTPUT")]
    output: PathBuf,
}

impl From<BuildArgs> for build::BuildArgs {
    fn from(a: BuildArgs) -> Self {
        build::BuildArgs {
            kernel_path: a.kernel,
            initrd_path: a.initrd,
            cmdline: a.cmdline,
            profile: a.profile,
            serial: a.serial,
            output_path: a.output,
            config_path: a.config,
            mmio_slots: a.mmio_slots,
            pci_slots: a.pci_slots,
            pci_window: a.pci_window,
            min_addr_space: a.min_addr_space,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Build(a) => build::run(&a.into()),
        Command::Check(a) => check::run(&a.pmi),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("arma: {e:#}");
            ExitCode::FAILURE
        }
    }
}
