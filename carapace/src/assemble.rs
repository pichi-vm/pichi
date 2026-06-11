//! Top-level orchestrator that bridges the chain walker
//! (`crate::chain`) and the dm primitives (`crate::dm`). Builds the
//! read stack: dm-zero + per-scute (verity, snapshot) + top alias.
//! `RollbackOnDrop` ensures partial-failure cleanup runs in
//! dependency-correct (reverse-push) order.
//!
//! This module exists outside `dm/` so the dm submodule stays
//! chain-agnostic — `dm/` knows kernel ABI, dm-table rendering, and
//! per-device RAII; it does NOT know what a "scute" is. The bridge
//! between the chain walker's output and the dm activation primitives
//! lives here.

use crate::chain::ValidatedChain;
use crate::dm::{open_dm_control, DmCreateMode, DmDevice, DmTable, TableLine, TargetSpec};
use crate::snapshot::{ValidatedSnapshotHeader, SNAPSHOT_HEADER_SIZE};
use crate::CarapaceError;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

/// dm-zero virtual length. Snapshot length lock: every per-scute
/// snapshot inherits this value (kernel rejects snapshot.length >
/// origin.length, and every snapshot's origin chains to dm-zero).
pub(crate) const ZERO_COUNT_SECTORS: u64 = 64u64 * 1024 * 1024 * 1024 / 512;

/// dm-snapshot chunk_size for read-stack snapshots. Spec RDP = 8 sectors.
pub(crate) const SNAPSHOT_CHUNK_SIZE_SECTORS: u64 = 8;

const VERITY_BLOCK_SIZE_BYTES: u32 = 4096;

/// Create a dm device, load a single-line table, resume.
///
/// All four target shapes the read stack uses (zero, linear, verity,
/// snapshot) follow this exact ioctl sequence — only the per-target
/// `TargetSpec` and the create-time `mode` vary. One helper instead of
/// four removes ~40 LOC of repetition and lets each call site read as
/// "what dm device am I making" rather than "which of four nearly-
/// identical wrappers to call."
///
/// `control` is the orchestrator's shared `/dev/mapper/control` fd —
/// reused across all three ioctls (create, load_table, resume) for
/// every device in the stack.
fn activate_one_line(
    control: &mut File,
    name: &str,
    mode: DmCreateMode,
    length_sectors: u64,
    target: TargetSpec,
) -> Result<DmDevice, CarapaceError> {
    let dev = DmDevice::create(control, name, mode)?;
    let t = DmTable {
        lines: vec![TableLine {
            start: 0,
            length: length_sectors,
            target,
        }],
    };
    dev.load_table(control, &t)?;
    dev.resume(control)?;
    Ok(dev)
}

/// Read the dm-snapshot persistent header from offset 0 of an
/// activated dm-verity device and validate it against spec §136 +
/// §281 (magic / valid / version / chunk_size == 8). Reading THROUGH
/// the verity device — not directly from the cow partition — means
/// the kernel has integrity-checked the bytes before we parse them
/// (CRITICAL-1's spirit applied to the snapshot header).
///
/// Returns `()` because every accepted header is identical (per the
/// RDP whitelist). Cross-scute equality is automatic if every scute
/// passes this check, so no comparison loop is needed in the caller.
fn validate_snapshot_header_through_verity(
    verity_dev_path: &Path,
    scute_index: usize,
) -> Result<(), CarapaceError> {
    let mut f = File::open(verity_dev_path)?;
    let mut buf = [0u8; SNAPSHOT_HEADER_SIZE];
    f.read_exact(&mut buf)?;
    ValidatedSnapshotHeader::parse(&buf, scute_index)?;
    Ok(())
}

/// Build the full read stack — `<name>-z0`, then per-scute pair
/// `<name>-v<i>` and `<name>-s<i>`, then top alias `<name>` — and
/// return the operator-visible `/dev/dm-<minor>` path of the top
/// alias. On any error the partial stack is torn down in reverse-push
/// order via [`RollbackOnDrop`]; on success the devices are forgotten
/// so the kernel state outlives this process.
///
/// dm-verity references the cow/verity partition devices by `<maj>:<min>`
/// (sysfs-published at GPT-partscan time; no udev sync). The chunk_size
/// is read through each freshly-activated dm-verity device so dm-verity
/// integrity-checks the bytes the consumer trusts.
///
/// **Panic discipline.** Cargo.toml release profile sets
/// `panic = "abort"` (smaller binary, no unwinding code). A consequence
/// is that *if* this function panics mid-build, `RollbackOnDrop`'s
/// `Drop` does NOT run and the partial dm stack leaks until an
/// explicit `carapace detach` clears it. Therefore every `unwrap` /
/// `expect` reachable from this function is backed by an invariant
/// the function itself maintains:
///
///   * `stack.devices.last().unwrap()` after a successful push.
///   * `expect("DmTableBuf invariant: …")` is guarded by zerocopy's
///     `mut_from_prefix` against a buffer we constructed at known size.
///   * `from_utf8(kernel_type_name())` operates on hardcoded ASCII.
///
/// New panic sources (especially in helpers) MUST preserve this
/// property. Recover-via-Result, not panic, is the rule for any
/// caller-influenced state in this code path.
pub(crate) fn assemble_read_stack(
    name: &str,
    chain: ValidatedChain,
) -> Result<PathBuf, CarapaceError> {
    // ONE /dev/mapper/control fd shared across every dm ioctl in this
    // activation. Replaces the prior shape's per-device open (3N+1
    // syscalls per attach with a per-DmDevice RefCell<File>).
    let mut control = open_dm_control()?;
    let mut stack = RollbackOnDrop {
        devices: Vec::new(),
    };

    // Bottom layer: dm-zero of the spec-mandated apparent length.
    let z = activate_one_line(
        &mut control,
        &format!("{name}-z0"),
        DmCreateMode::ReadWrite,
        ZERO_COUNT_SECTORS,
        TargetSpec::Zero,
    )?;
    stack.devices.push(z);

    let mut last_top_length: u64 = 0;

    for (i, scute) in chain.scutes.into_iter().enumerate() {
        let data_sectors = scute.superblock.data_blocks * (VERITY_BLOCK_SIZE_BYTES as u64) / 512;
        let v = activate_one_line(
            &mut control,
            &format!("{name}-v{i}"),
            DmCreateMode::ReadOnly,
            data_sectors,
            TargetSpec::verity(
                scute.cow,
                scute.verity,
                scute.superblock.algorithm,
                scute.superblock.data_blocks,
                scute.superblock.full_salt(),
                &scute.root,
            ),
        )?;

        // Snapshot-header sanity through the activated dm-verity device.
        // Per-scute literal whitelist makes cross-scute equality
        // automatic (every accepted header is identical), so no
        // comparison loop is needed.
        validate_snapshot_header_through_verity(&v.dev_node(), i)?;
        stack.devices.push(v);

        // After this branch the layout is z, v0, s0, v1, s1, …
        // Snapshot origin = z0 for i==0 else previous snapshot. Length
        // is locked to ZERO_COUNT_SECTORS (kernel rejects snapshot.length
        // > origin.length).
        let origin = if i == 0 {
            stack.devices[0].dev_ref()
        } else {
            // Previous snapshot at vec index 1 + 2(i-1) + 1 = 2*i.
            stack.devices[2 * i].dev_ref()
        };
        let cow = stack.devices.last().unwrap().dev_ref();
        let s = activate_one_line(
            &mut control,
            &format!("{name}-s{i}"),
            DmCreateMode::ReadOnly,
            ZERO_COUNT_SECTORS,
            TargetSpec::Snapshot {
                origin,
                cow,
                chunk_size_sectors: SNAPSHOT_CHUNK_SIZE_SECTORS,
            },
        )?;
        stack.devices.push(s);

        last_top_length = scute.superblock.data_blocks * (VERITY_BLOCK_SIZE_BYTES as u64) / 512;
    }

    // Top alias: dm-linear over the top snapshot, sized to the top
    // scute's actual data length (NOT ZERO_COUNT_SECTORS — that would
    // expose a 64 GiB device whose tail blocks all-zero from dm-zero).
    let alias = activate_one_line(
        &mut control,
        name,
        DmCreateMode::ReadOnly,
        last_top_length,
        TargetSpec::Linear {
            device: stack.devices.last().unwrap().dev_ref(),
            offset_sectors: 0,
        },
    )?;
    // /dev/dm-<minor> is kernel-synchronous via devtmpfs at
    // DM_DEV_CREATE time. The `/dev/mapper/<name>` symlink would
    // arrive when udev catches up; we don't block on it.
    let path = alias.dev_node();
    stack.devices.push(alias);

    stack.commit();
    Ok(path)
}

/// RAII rollback for partially-built dm stacks. Pushed devices are
/// removed in reverse-push order on drop unless `commit()` is called,
/// which forgets them so the kernel state outlives this scope. The
/// reverse order is load-bearing: tearing down dependents before their
/// dependencies (e.g. snapshot before the verity below it) avoids the
/// EBUSY noise DmDevice::Drop would otherwise log on a panic-driven
/// teardown.
struct RollbackOnDrop {
    devices: Vec<DmDevice>,
}

impl RollbackOnDrop {
    fn commit(mut self) {
        for dev in self.devices.drain(..) {
            dev.forget();
        }
    }
}

impl Drop for RollbackOnDrop {
    fn drop(&mut self) {
        while let Some(dev) = self.devices.pop() {
            drop(dev);
        }
    }
}
