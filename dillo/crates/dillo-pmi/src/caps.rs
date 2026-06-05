//! Defensive resource caps. See `dillo/ARCHITECTURE.md` §5.2.

/// Maximum PMI file size: 4 GiB. Refused before any read.
pub const MAX_FILE_SIZE: u64 = 4 << 30;

/// Maximum `.pmi.<target>` manifest size: 4 KiB.
pub const MAX_MANIFEST_SIZE: usize = 4 << 10;

/// Maximum PE section count.
pub const MAX_SECTION_COUNT: usize = 64;

/// Maximum PE section name length (bytes).
pub const MAX_SECTION_NAME_LEN: usize = 64;

/// Absolute cap on sum of `load` action `VirtualSize` bytes.
///
/// The effective cap is `min(memory_mib * MiB, MAX_TOTAL_LOADED_HARD)`.
pub const MAX_TOTAL_LOADED_HARD: u64 = 16 << 30;

/// `.dtbo` reservation: minimum size.
pub const DTBO_MIN_SIZE: u64 = 4 << 10;

/// `.dtbo` reservation: maximum size.
pub const DTBO_MAX_SIZE: u64 = 64 << 10;

/// CBOR maximum nesting depth.
pub const CBOR_MAX_DEPTH: usize = 8;

/// CBOR maximum entries in any array (e.g., `actions`).
///
/// Enforced indirectly via [`MAX_MANIFEST_SIZE`] (a 4 KiB CBOR map cannot
/// contain more than ~64 actions worth of payload). Also re-checked at
/// the typed-Spec level.
pub const CBOR_MAX_ARRAY_LEN: usize = 64;

/// Canonical address bound for x86-64 / aarch64 (`< 2^48`).
pub const CANONICAL_ADDR_BOUND: u128 = 1u128 << 48;

/// 2 MiB huge-page granularity (large-section alignment + backing).
pub const HUGE_PAGE: u64 = 2 << 20;

/// 4 KiB small-section alignment.
pub const SMALL_PAGE: u64 = 4 << 10;

/// Inflation-ratio multiplier for the pathological-spread refusal.
///
/// `footprint_2mib * HUGE_PAGE > N * sum(load sizes)` triggers refusal.
pub const SPREAD_INFLATION_RATIO: u64 = 4;
