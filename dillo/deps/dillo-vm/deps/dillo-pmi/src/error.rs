//! Error type for the PMI loader.

use thiserror::Error;

/// Every failure mode dillo-pmi can produce. All errors map to exit
/// code 10 (PMI parse / validation error) at the binary level.
#[derive(Debug, Error)]
pub enum Error {
    // ─── Resource caps (§5.2) ────────────────────────────────────
    #[error("PMI file size {actual} bytes exceeds cap of {cap} bytes")]
    FileTooLarge { actual: u64, cap: u64 },

    #[error("manifest size {actual} bytes exceeds cap of {cap} bytes for section `{section}`")]
    ManifestTooLarge {
        section: String,
        actual: usize,
        cap: usize,
    },

    #[error("PE has {actual} sections; cap is {cap}")]
    TooManySections { actual: usize, cap: usize },

    #[error("PE section name `{name}` ({len} bytes) exceeds cap of {cap} bytes")]
    SectionNameTooLong {
        name: String,
        len: usize,
        cap: usize,
    },

    #[error(
        "sum of loaded VirtualSize ({actual} bytes) exceeds cap of {cap} bytes \
         (effective cap = min(--memory, hard cap))"
    )]
    LoadedBytesExceedMemory { actual: u64, cap: u64 },

    #[error(
        ".dtbo section size {actual} bytes is outside the accepted range \
         [{min}, {max}]"
    )]
    DtboSizeOutOfRange { actual: u64, min: u64, max: u64 },

    // ─── PE structural ──────────────────────────────────────────
    #[error("PE parse failed: {0}")]
    PeParse(String),

    #[error("PE FileHeader.Machine {actual:#06x} does not match host arch {expected:#06x}")]
    HostArchMismatch { actual: u16, expected: u16 },

    #[error("section `{name}` raw data range [{offset}..{end}) extends past file size {file_size}")]
    SectionDataPastEof {
        name: String,
        offset: u64,
        end: u64,
        file_size: u64,
    },

    #[error("section `{name}`: VirtualAddress + VirtualSize overflows u64")]
    VirtualAddressOverflow { name: String },

    #[error(
        "section `{name}` GPA range [{start:#x}..{end:#x}) exceeds canonical address bound 2^48"
    )]
    GpaOutOfCanonicalBound { name: String, start: u64, end: u64 },

    #[error("sections `{a}` and `{b}` overlap in [VirtualAddress, VirtualAddress + VirtualSize)")]
    SectionsOverlap { a: String, b: String },

    #[error(
        "section `{name}` VirtualSize {virtual_size} fails alignment requirement \
         ({rule})"
    )]
    AlignmentViolation {
        name: String,
        virtual_size: u64,
        rule: &'static str,
    },

    #[error("multiple `{section}` PE sections found; expected exactly one")]
    DuplicatePmiTargetSection { section: String },

    // ─── Manifest semantic ──────────────────────────────────────
    #[error("`.pmi.<target>` section not found for target `{target}`")]
    TargetSectionMissing { target: String },

    #[error("manifest references PE section `{section}` which is not present")]
    ManifestReferencesMissingSection { section: String },

    #[error("CBOR decode failed: {0}")]
    CborDecode(String),

    #[error("vm:vcpu variant ({variant}) does not match PE FileHeader.Machine ({machine:#06x})")]
    VcpuVariantMismatch { variant: &'static str, machine: u16 },

    #[error("merged:dtbo fill present but merged:dtb attribute missing (or vice versa)")]
    MergedExtensionPartial,

    #[error("merged:dtb attribute names section `{section}` which is not present")]
    MergedDtbSectionMissing { section: String },

    // ─── Pathological-spread refusal (§5.5) ─────────────────────
    #[error(
        "loaded layout is pathologically spread: 2 MiB footprint {footprint} bytes \
         exceeds {ratio}× sum of load sizes ({sum})"
    )]
    SpreadRatioExceeded {
        footprint: u64,
        sum: u64,
        ratio: u64,
    },

    #[error("loaded layout's 2 MiB footprint {footprint} bytes exceeds memory cap {cap}")]
    SpreadAbsoluteExceeded { footprint: u64, cap: u64 },
}
