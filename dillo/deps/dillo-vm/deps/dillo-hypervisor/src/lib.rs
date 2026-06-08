//! Hypervisor abstraction for dillo.
//!
//! A single concrete `Vm` type whose implementation is selected at
//! compile time per host OS: KVM on Linux, HVF on macOS, WHP on Windows.
//! No `Hypervisor` trait; no runtime polymorphism.
//!
//! See `dillo/ARCHITECTURE.md` §9.

#[cfg(target_os = "linux")]
mod kvm;

#[cfg(all(
    any(target_os = "linux", target_os = "windows"),
    target_arch = "x86_64"
))]
mod cpuid_x86;

#[cfg(target_os = "linux")]
pub use kvm::{Error, Vcpu, Vm};

#[cfg(target_os = "windows")]
mod whp;

#[cfg(target_os = "windows")]
pub use whp::{Error, InterruptController, Vcpu, VcpuCancel, Vm};

#[cfg(target_os = "macos")]
mod hvf;

#[cfg(target_os = "macos")]
pub use applevisor::prelude::VcpuHandle;
#[cfg(target_os = "macos")]
pub use hvf::{
    Error, GicParams, Vcpu, Vm, create_vcpu_current_thread, force_vcpus_exit, send_msi, set_spi,
};

/// Re-export the KVM debug-control flags so dillo-vm can configure
/// guest-debug modes without depending on `kvm-bindings` directly.
#[cfg(target_os = "linux")]
pub mod debug_flags {
    pub use kvm_bindings::{
        KVM_GUESTDBG_ENABLE, KVM_GUESTDBG_SINGLESTEP, KVM_GUESTDBG_USE_HW_BP,
        KVM_GUESTDBG_USE_SW_BP,
    };
}

/// Re-export raw KVM register structures for the gdb stub.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub use kvm_bindings::{kvm_regs, kvm_sregs};

/// Reasons a `Vcpu::run()` call returned to userspace.
#[derive(Debug)]
pub enum VmExit {
    /// MMIO read; dispatcher provides the data via [`Vcpu::mmio_read_complete`].
    MmioRead { addr: u64, size: u8 },
    /// MMIO write; data carries the bytes the guest wrote.
    MmioWrite { addr: u64, data: [u8; 8], size: u8 },
    /// PIO read (x86 only).
    PioRead { port: u16, size: u8 },
    /// PIO write (x86 only).
    PioWrite { port: u16, data: [u8; 4], size: u8 },
    /// HVC trap (aarch64).
    Hvc { args: [u64; 8] },
    /// SMC trap (aarch64).
    Smc { args: [u64; 8] },
    /// Platform-signaled guest halt (PSCI SYSTEM_OFF surfaced as
    /// SystemEvent by KVM on aarch64).
    Shutdown,

    /// Host interrupt of the vCPU run ioctl.
    Interrupted,

    /// vCPU executed HLT or WFI.
    Halted,
    /// Hypervisor returned an exit reason dillo does not handle.
    Unknown(String),
    /// Single-step Debug exit (when guest_debug is enabled).
    Debug,
}
