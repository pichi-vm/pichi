// SPDX-License-Identifier: Apache-2.0
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Real-kernel coverage for snapshot-merge handover and data preservation.

mod common;

use std::io::Write;

use devmap_core::BlockSize;
use devmap_linux::{Control, records::Records};
use devmap_snapshot as snapshot;
use devmap_snapshot::{Merge, Persistent, Snapshot};

use common::{LoopDevice, Owned, ensure_module_loaded};

fn parameters<P>(
    origin: devmap_linux::DevId,
    cow: devmap_linux::DevId,
    persistence: P,
) -> snapshot::Parameters<P> {
    snapshot::Parameters {
        origin,
        cow,
        persistence,
        chunk_size: BlockSize::default(),
        discard: snapshot::Discard::Disabled,
    }
}

/// Exercises `snapshot::Merge`'s real handover procedure end to
/// end: a `snapshot-origin` device, a `snapshot` device sharing its COW
/// device, a write that creates a real COW exception, then the handover
/// itself (suspend origin, reload it as `snapshot-merge`, suspend the old
/// snapshot, resume) — followed by reading the written data back through
/// the now-merged origin to prove the merge preserved it.
///
/// The exact sequencing was verified directly against `dm-snap.c`
/// (`snapshot_preresume`/`snapshot_resume`): resuming a `snapshot-merge`
/// target refuses with `EINVAL` unless the old `snapshot` target sharing
/// its COW device is already suspended — that's what makes step 5 below
/// load-bearing, not optional.
#[test]
#[ignore = "requires privileged device-mapper access"]
fn snapshot_merge_takes_over_from_snapshot_and_merges() {
    let control = Control::open().expect("open /dev/mapper/control");
    ensure_module_loaded("dm-snapshot");

    let origin_backing = LoopDevice::create("snapmerge-origin", 16 * 1024 * 1024);
    let cow_backing = LoopDevice::create("snapmerge-cow", 16 * 1024 * 1024);
    let origin_device =
        devmap_linux::DevId::try_from(std::fs::metadata(&origin_backing.path).unwrap())
            .expect("by_node origin backing");
    let cow_device = devmap_linux::DevId::try_from(std::fs::metadata(&cow_backing.path).unwrap())
        .expect("by_node cow");
    let origin_len_sectors = 16 * 1024 * 1024 / 512;

    // 1. Origin device: dm-linear-equivalent passthrough (snapshot-origin
    //    with no snapshot yet just forwards I/O) to the backing device.
    let origin_name = format!("devmap-test-snapmerge-origin-{}", std::process::id());
    let origin = Owned::create(&control, &origin_name).expect("DM_DEV_CREATE origin");
    control
        .load(
            origin.id(),
            devmap_linux::Table::<4096>::default()
                .add(
                    devmap_core::Span::new(0, origin_len_sectors),
                    &snapshot::Origin {
                        origin: origin_device,
                    },
                    control.version("snapshot-origin").unwrap(),
                )
                .expect("add snapshot-origin"),
            false,
        )
        .expect("DM_TABLE_LOAD origin");
    control.suspend(origin.id(), false).expect("resume origin");

    // 2. Write a first pattern before any snapshot exists.
    let origin_path = origin.node_path();
    write_block(&origin_path, 0, 0xAA);

    // 3. A persistent snapshot of that origin, sharing the same COW
    //    device the eventual snapshot-merge target will take over.
    let snap_name = format!("devmap-test-snapmerge-snap-{}", std::process::id());
    let snap = Owned::create(&control, &snap_name).expect("DM_DEV_CREATE snapshot");
    control
        .load(
            snap.id(),
            devmap_linux::Table::<4096>::default()
                .add(
                    devmap_core::Span::new(0, origin_len_sectors),
                    &Snapshot::from(parameters(
                        origin_device,
                        cow_device,
                        snapshot::Persistence::Persistent(snapshot::Persistent::Overflow),
                    )),
                    control.version("snapshot").unwrap(),
                )
                .expect("add snapshot"),
            false,
        )
        .expect("DM_TABLE_LOAD snapshot");
    control.suspend(snap.id(), false).expect("resume snapshot");

    // 4. Write a second pattern now that the snapshot is active. This is
    //    the divergence a merge undoes: dm-snapshot preserves the *old*
    //    (pre-write) contents of this chunk in the COW device before
    //    letting the write through to the origin, so the snapshot's view
    //    of this chunk is now the all-zero data that was here before —
    //    merging will restore exactly that, overwriting this 0xBB write.
    write_block(&origin_path, 1, 0xBB);

    // 5. Handover: suspend the origin, stage snapshot-merge on it,
    //    suspend the old snapshot device (required precondition per
    //    dm-snap.c's snapshot_preresume), then resume the origin —
    //    activating the merge, which runs in the background.
    control.suspend(origin.id(), true).expect("suspend origin");
    control
        .load(
            origin.id(),
            devmap_linux::Table::<4096>::default()
                .add(
                    devmap_core::Span::new(0, origin_len_sectors),
                    &Merge::from(parameters(origin_device, cow_device, Persistent::Overflow)),
                    control.version("snapshot-merge").unwrap(),
                )
                .expect("add snapshot-merge"),
            false,
        )
        .expect("DM_TABLE_LOAD snapshot-merge");
    control
        .suspend(snap.id(), true)
        .expect("suspend old snapshot before handover");
    control
        .suspend(origin.id(), false)
        .expect("resume as snapshot-merge");

    // 6. Wait for the background merge to finish: sectors_allocated drops
    //    to exactly metadata_sectors once nothing is left to fold in.
    let version = control.version("snapshot-merge").unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let status = control.status(origin.id()).expect("DM_DEV_STATUS");
        assert_eq!(status.target_count, 1);
        let mut info = control.info::<4096>(origin.id()).expect("DM_TABLE_STATUS");
        let reported = info.next().unwrap().unwrap();
        // The merge is done once nothing but metadata is left allocated.
        let complete = matches!(
            reported.parse::<snapshot::Merge>(version),
            Ok(snapshot::Info::Usage {
                allocated_sectors,
                metadata_sectors,
                ..
            }) if allocated_sectors == metadata_sectors
        );
        if complete {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "snapshot-merge never finished"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // 7. Merging folds the snapshot's preserved view back into the
    //    origin: block 0 (never touched after the snapshot was taken, so
    //    no exception exists for it) is unaffected; block 1's post-write
    //    0xBB is overwritten by the pre-write zeros the snapshot
    //    captured — proving the merge actually moved real data, not just
    //    that the ioctl sequence didn't error.
    assert_block(&origin_path, 0, 0xAA);
    assert_block(&origin_path, 1, 0x00);

    // The old snapshot device is now a dead end (kernel returns -EIO on
    // access to it once merging has started) — remove it explicitly
    // rather than relying on `Owned`'s best-effort drop, so a failure
    // here is visible instead of silently swallowed.
    snap.remove().expect("remove handed-over snapshot device");
}

fn write_block(path: &std::path::Path, block_index: u64, byte: u8) {
    use std::io::{Seek, SeekFrom};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open for write_block");
    file.seek(SeekFrom::Start(block_index * 4096))
        .expect("seek");
    file.write_all(&[byte; 4096]).expect("write");
    file.sync_all().expect("fsync");
}

fn assert_block(path: &std::path::Path, block_index: u64, expected_byte: u8) {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).expect("open for assert_block");
    file.seek(SeekFrom::Start(block_index * 4096))
        .expect("seek");
    let mut buf = [0u8; 4096];
    file.read_exact(&mut buf).expect("read");
    assert!(
        buf.iter().all(|&b| b == expected_byte),
        "block {block_index} does not match expected pattern"
    );
}
