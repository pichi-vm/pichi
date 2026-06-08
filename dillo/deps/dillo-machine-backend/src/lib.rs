//! Target-selected machine backend facade for dillo.
//!
//! This crate gives the launcher one backend crate name. Cargo target
//! dependencies select the concrete backend package for the host platform.

#[cfg(target_os = "linux")]
pub use dillo_machine_kvm::*;

#[cfg(target_os = "macos")]
pub use dillo_machine_hvf::*;

#[cfg(target_os = "windows")]
pub use dillo_machine_whp::*;
