//! WHP machine backend crate for dillo.
//!
//! This is the target-selected backend crate for Windows/WHP. It currently
//! exposes the existing lower hypervisor wrapper while the machine-owned
//! routing and vCPU lifecycle model is migrated in later stages.

#[cfg(target_os = "windows")]
pub use dillo_hypervisor::{Error, InterruptController, Vcpu, Vm, VmExit};
