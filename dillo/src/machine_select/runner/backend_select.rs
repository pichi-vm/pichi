#[cfg(target_os = "linux")]
pub(crate) use dillo_machine_kvm as machine;

#[cfg(target_os = "macos")]
pub(crate) use dillo_machine_hvf as machine;

#[cfg(target_os = "windows")]
pub(crate) use dillo_machine_whp as machine;
