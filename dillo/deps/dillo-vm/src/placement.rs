//! Compatibility memory-plan types consumed by the remaining `dillo-vm` runner.
//!
//! Memory placement is now computed by `dillo::launch`; this module only keeps
//! the concrete region shape needed until the old runner is removed.

/// One contiguous region for either a memslot or a `/memory@N` node.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Region {
    pub gpa: u64,
    pub size: u64,
}

/// The full memory plan: memslots == memory_nodes by construction.
#[derive(Debug)]
pub(crate) struct MemoryPlan {
    pub memslots: Vec<Region>,
    pub memory_nodes: Vec<Region>,
}
