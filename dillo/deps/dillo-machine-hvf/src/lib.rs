//! HVF machine backend crate for dillo.
//!
//! This is the target-selected backend crate for macOS/HVF. It currently
//! exposes the existing lower hypervisor wrapper while the machine-owned
//! routing and vCPU lifecycle model is migrated in later stages.

#[cfg(target_os = "macos")]
pub use dillo_hypervisor::{
    Error, GicParams, Vcpu, VcpuHandle, Vm, VmExit, create_vcpu_current_thread, force_vcpus_exit,
    send_msi, set_spi,
};
