//! `attach` — assemble a carapace chain into `/dev/mapper/<name>`.

use std::path::PathBuf;

use crate::assemble::assemble_read_stack;
use crate::chain::walk_chain;
use crate::name::validate_dm_name;
use crate::partition::PartitionMap;
use crate::util::decode_hex;
use crate::CarapaceError;

/// Assemble the carapace chain rooted at `root_hex` into `/dev/mapper/<name>`,
/// returning the operator-visible device path (`/dev/dm-<minor>`).
///
/// Walks the chain backward from the trusted root, validates every scute's
/// parameters against the RDP whitelist, builds the dm-verity + dm-snapshot
/// stack, and returns its path. Requires `CAP_SYS_ADMIN` — the first dm ioctl
/// (`open /dev/mapper/control`) returns `EACCES` with a clear message
/// otherwise.
///
/// `name` is validated here (so non-CLI callers can't skip it); see
/// [`validate_dm_name`].
pub fn attach(name: &str, root_hex: &str) -> Result<PathBuf, CarapaceError> {
    validate_dm_name(name)?;
    // trusted_root + PartitionMap (~30 KiB on busy hosts) drop at the
    // semicolon, narrowing the live-state surface before any dm ioctl.
    let chain = {
        let trusted_root = decode_hex(root_hex)?;
        let partitions = PartitionMap::scan()?;
        walk_chain(&trusted_root, &partitions)?
    };
    assemble_read_stack(name, chain)
}
