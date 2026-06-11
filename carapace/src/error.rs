//! Single flat error type. Variants exist for stderr clarity (each
//! `Display` impl carries enough operator-relevant context to localize
//! the failure) and to drive a two-bucket exit code split so initrd
//! scripts can distinguish chain-rejection from operational failure
//! without grepping stderr — see [`CarapaceError::is_adversary_rejection`]
//! and `cli::run`.
//!
//! Exit codes:
//!   * 0 — success
//!   * 1 — operational failure (kernel ioctl, I/O, name conflict, CLI usage)
//!   * 2 — chain rejected (malformed/forged superblock, depth/cycle,
//!         non-whitelisted algorithm or block size)

use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum CarapaceError {
    #[error("usage: {0}")]
    Usage(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("dm ioctl {op} failed: {source}{}", table_line.as_deref().map(|s| format!(" (table: {s})")).unwrap_or_default())]
    DmIoctl {
        op: &'static str,
        #[source]
        source: std::io::Error,
        /// Operator-facing table line when the failure point can attribute
        /// it to a specific dm target (e.g. DM_TABLE_LOAD failures). None
        /// for ops that don't produce a table line (DM_DEV_CREATE, etc.).
        table_line: Option<String>,
    },

    #[error("dm device name conflict: /dev/mapper/{name} already exists")]
    NameConflict { name: String },

    #[error("partition lookup failed: PARTUUID {partuuid} not present in /sys/class/block (no GPT-partscanned device exposes it)")]
    PartitionNotFound { partuuid: String },

    #[error("verity superblock invalid at scute {scute_index}: {reason}")]
    SuperblockInvalid { scute_index: usize, reason: String },

    #[error("snapshot header invalid at scute {scute_index}: {reason}")]
    SnapshotHeaderInvalid { scute_index: usize, reason: String },

    /// A scute parameter does not match the whitelist (RDP-locked).
    #[error("whitelist violation at scute {scute_index}: {field} = {value}")]
    WhitelistViolation {
        scute_index: usize,
        field: &'static str,
        value: String,
    },

    /// Chain walk failed structurally (missing scute, depth exceeded, cycle).
    #[error("chain walk failed at depth {depth}: {reason}")]
    ChainWalkFailed { depth: usize, reason: String },

    #[error("unsupported algorithm: {0}")]
    UnsupportedAlgorithm(String),
}

impl CarapaceError {
    /// True iff the error is the result of validating adversary-influenced
    /// input (the chain itself: superblocks, salt prefixes, snapshot header,
    /// declared algorithm, walk structure). False for operational failures
    /// (kernel ioctl, I/O, CLI usage, dm name conflict). Drives the
    /// exit-code split in `cli::run`.
    pub(crate) fn is_adversary_rejection(&self) -> bool {
        matches!(
            self,
            Self::SuperblockInvalid { .. }
                | Self::SnapshotHeaderInvalid { .. }
                | Self::WhitelistViolation { .. }
                | Self::ChainWalkFailed { .. }
                | Self::UnsupportedAlgorithm(_)
        )
    }
}
