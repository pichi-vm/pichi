//! KVM machine backend crate for dillo.
//!
//! This is the target-selected backend crate for Linux/KVM. It currently
//! exposes the existing lower hypervisor wrapper while the machine-owned
//! routing and vCPU lifecycle model is migrated in later stages.

#[cfg(target_os = "linux")]
pub use dillo_hypervisor::{Error, Vcpu, Vm, VmExit, debug_flags};
