//! Typed errors for the selected dillo runner.
//!
//! Each variant maps to one of the ARCHITECTURE.md §13.4 exit codes
//! via [`RunError::exit_code`]. `main.rs` is responsible for the
//! `eprintln!` + `process::exit` call.

use thiserror::Error;

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
    #[error("machine: {0}")]
    Machine(String),

    #[error("write load section `{section}` to GPA {gpa:#x}: {source}")]
    SectionWrite {
        section: String,
        gpa: u64,
        #[source]
        source: anyhow::Error,
    },

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
}

impl RunError {
    pub(crate) fn machine(source: impl std::error::Error) -> Self {
        Self::Machine(source.to_string())
    }

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
            Self::Machine(_) | Self::SectionWrite { .. } => 12,
            Self::HostRam { .. } => 13,
            Self::Placement { .. } => 13,
        }
    }
}
