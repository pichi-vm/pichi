//! `carapace attach` — assemble a chain into /dev/mapper/<name>.

use crate::assemble::assemble_read_stack;
use crate::chain::walk_chain;
use crate::partition::PartitionMap;
use crate::util::decode_hex;
use crate::CarapaceError;

pub(crate) fn run(name: &str, root_hex: &str) -> Result<(), CarapaceError> {
    // --name validity is enforced by cli::validate_dm_name at parse
    // time; trust the caller here. No explicit root-privilege check —
    // the next syscall (open /dev/mapper/control) returns EACCES with
    // a clear message if we lack CAP_SYS_ADMIN.
    //
    // trusted_root + PartitionMap (~30 KiB on busy hosts) drop at the
    // semicolon, narrowing the live-state surface before any dm ioctl.
    let chain = {
        let trusted_root = decode_hex(root_hex)?;
        let partitions = PartitionMap::scan()?;
        walk_chain(&trusted_root, &partitions)?
    };
    let mapper_path = assemble_read_stack(name, chain)?;
    println!("{}", mapper_path.display());
    Ok(())
}
