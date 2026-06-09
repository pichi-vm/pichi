//! Typed errors for the selected dillo runner.
//!
//! Each variant maps to one of the ARCHITECTURE.md §13.4 exit codes
//! via [`RunError::exit_code`]. `main.rs` is responsible for the
//! `eprintln!` + `process::exit` call.

use thiserror::Error;

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use super::backend_select::machine as backend_machine;

/// Exit-code-bearing error for the VM-side run loop. Each variant
/// corresponds to one of ARCH §13.4's documented categories.
#[derive(Debug, Error)]
#[allow(dead_code)]
pub(crate) enum RunError {
    // ── exit 10 — PMI parse / validation ───────────────────────────
    #[error("read PMI {path}: {source}")]
    ReadPmi {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("PMI parse: {0}")]
    PmiParse(#[from] dillo::pmi_parse::Error),

    // ── exit 11 — DTB parse / validation ───────────────────────────
    #[error("base DTB extraction: {0}")]
    DtbExtract(dillo_devtree::platform::SurveyError),

    #[error("base DTB coverage (undeclared hardware / unclaimed node): {0}")]
    Coverage(dillo_devtree::platform::SurveyError),

    #[error("base DTB ↔ PE cross-validation: {0}")]
    DtbCrossValidate(dillo_devtree::platform::SurveyError),

    #[error("base DTB is missing required device `{0}`")]
    MissingRequiredDevice(&'static str),

    #[error("synthesize host DTBO: {0}")]
    DtboSynth(#[source] anyhow::Error),

    #[error("write DTBO section `{section}` to GPA {gpa:#x}: {source}")]
    DtboWrite {
        section: String,
        gpa: u64,
        #[source]
        source: anyhow::Error,
    },

    // ── exit 12 — Hypervisor init failed ───────────────────────────
    #[error("memfd setup: {0}")]
    MemfdSetup(#[source] anyhow::Error),

    #[error("mmap memfd range: {0}")]
    Mmap(#[source] anyhow::Error),

    #[error("KVM: {0}")]
    Kvm(#[from] backend_machine::Error),

    #[error("write load section `{section}` to GPA {gpa:#x}: {source}")]
    SectionWrite {
        section: String,
        gpa: u64,
        #[source]
        source: anyhow::Error,
    },

    #[error("vm:vcpu variant does not match host architecture")]
    ArchMismatch,

    /// macOS/HVF run path: a stage past memory setup is not yet wired.
    #[error("macOS/HVF run path: {0} not yet implemented")]
    Unimplemented(&'static str),

    #[error("requested {requested} vCPUs but the host hypervisor supports at most {max}")]
    TooManyVcpus { requested: u32, max: u32 },

    #[error("unrecognized cpu:profile {0:?} for this architecture")]
    UnknownCpuProfile(String),

    // ── exit 13 — Host RAM check ───────────────────────────────────
    #[error(
        "host RAM ({available_mib} MiB) insufficient for guest ({requested_mib} MiB) + \
         {overhead_mib} MiB overhead"
    )]
    HostRam {
        requested_mib: u64,
        overhead_mib: u64,
        available_mib: u64,
    },

    #[error("memory placement: {source}")]
    Placement {
        #[source]
        source: anyhow::Error,
    },

    // ── exit 20 — Guest crash ──────────────────────────────────────
    #[error("vCPU thread error: {0}")]
    VcpuThread(String),

    #[error("vCPU thread panicked")]
    VcpuPanic,

    #[error("unknown KVM exit: {0}")]
    UnknownKvmExit(String),

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    #[error("serial init from DTB: {source}")]
    SerialInit {
        #[source]
        source: anyhow::Error,
    },

    // ── exit 2 — Invocation / env error ────────────────────────────
    // DILLO_GDB is set via env; an unparseable port is a usability
    // error of the same shape as bad argv.
    #[error("DILLO_GDB port must be u16, got {value:?}: {source}")]
    GdbPort {
        value: String,
        #[source]
        source: std::num::ParseIntError,
    },
}

impl RunError {
    /// Map to the documented exit code from ARCH §13.4.
    #[must_use]
    pub(crate) fn exit_code(&self) -> i32 {
        match self {
            Self::ReadPmi { .. } | Self::PmiParse(_) => 10,
            Self::DtbExtract(_)
            | Self::Coverage(_)
            | Self::DtbCrossValidate(_)
            | Self::MissingRequiredDevice(_)
            | Self::DtboSynth(_)
            | Self::DtboWrite { .. } => 11,
            Self::MemfdSetup(_)
            | Self::Mmap(_)
            | Self::Kvm(_)
            | Self::SectionWrite { .. }
            | Self::ArchMismatch
            | Self::Unimplemented(_)
            | Self::TooManyVcpus { .. }
            | Self::UnknownCpuProfile(_) => 12,
            Self::HostRam { .. } => 13,
            Self::Placement { .. } => 13,
            Self::VcpuThread(_) | Self::VcpuPanic | Self::UnknownKvmExit(_) => 20,
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            Self::SerialInit { .. } => 11,
            Self::GdbPort { .. } => 2,
        }
    }
}
