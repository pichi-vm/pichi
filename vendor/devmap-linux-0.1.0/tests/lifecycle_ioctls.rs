// SPDX-License-Identifier: Apache-2.0

//! Real-kernel coverage for the device lifecycle ioctls beyond
//! create/load/resume/remove: `DM_TABLE_DEPS`, `DM_TABLE_CLEAR`,
//! `DM_DEV_WAIT`, `DM_DEV_RENAME`, and `DM_LIST_VERSIONS`.
//!
//! `DM_DEV_ARM_POLL` is covered in `arm_poll.rs` instead — it watches a
//! subsystem-wide counter, so it cannot share a test binary with tests
//! that create devices concurrently.

mod common;

use devmap_linux::{
    Control, DevId, Status, records::Records, targets::Target, targets::Version, targets::Zero,
};

use common::{LoopDevice, Owned};

struct Linear {
    device: DevId,
    offset: u64,
}

impl Target for Linear {
    const NAME: &'static str = "linear";

    type Table = String;
    type Info = String;

    fn encode(
        &self,
        version: Version,
        output: &mut (impl std::io::Write + ?Sized),
    ) -> std::io::Result<()> {
        if version.major != 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!(
                    "unsupported target version {}.{}.{}",
                    version.major, version.minor, version.patch
                ),
            ));
        }
        write!(output, "{} {}", self.device, self.offset)
    }
}

#[test]
#[ignore = "requires privileged device-mapper access"]
fn deps_reports_the_devices_the_table_opens() {
    let control = Control::open().expect("open /dev/mapper/control");

    let backing = LoopDevice::create("deps", 8 * 1024 * 1024);
    let backing_device = devmap_linux::DevId::try_from(std::fs::metadata(&backing.path).unwrap())
        .expect("by_node backing");

    let name = format!("devmap-test-deps-{}", std::process::id());
    let dev = Owned::create(&control, &name).expect("DM_DEV_CREATE");
    control
        .load(
            dev.id(),
            devmap_linux::Table::<4096>::default()
                .add(
                    devmap_core::Span::new(0, 8 * 1024 * 1024 / 512),
                    &Linear {
                        device: backing_device,
                        offset: 0,
                    },
                    control.version("linear").unwrap(),
                )
                .expect("add linear"),
            false,
        )
        .expect("DM_TABLE_LOAD");
    control.suspend(dev.id(), false).expect("resume");

    // A linear mapping opens exactly the one device it maps.
    let mut deps = control.deps::<4096>(dev.id()).expect("DM_TABLE_DEPS");
    assert_eq!(deps.next().unwrap().unwrap(), backing_device);
    assert!(deps.next().is_none());
}

#[test]
#[ignore = "requires privileged device-mapper access"]
fn deps_is_empty_without_an_active_table() {
    let control = Control::open().expect("open /dev/mapper/control");
    let dev = Owned::create(
        &control,
        &format!("devmap-test-deps-unloaded-{}", std::process::id()),
    )
    .unwrap();
    let mut deps = control.deps::<0>(dev.id()).unwrap();
    assert!(deps.next().is_none());
    assert!(deps.next().is_none());
}

#[test]
#[ignore = "requires privileged device-mapper access"]
fn deps_is_empty_for_a_target_that_opens_no_devices() {
    let control = Control::open().expect("open /dev/mapper/control");

    let name = format!("devmap-test-deps-zero-{}", std::process::id());
    let dev = Owned::create(&control, &name).expect("DM_DEV_CREATE");
    control
        .load(
            dev.id(),
            devmap_linux::Table::<4096>::default()
                .add(
                    devmap_core::Span::new(0, 8192),
                    &Zero,
                    control.version("zero").unwrap(),
                )
                .expect("add zero"),
            false,
        )
        .expect("DM_TABLE_LOAD");
    control.suspend(dev.id(), false).expect("resume");

    // dm-zero is purely synthetic — it opens nothing.
    assert!(
        control
            .deps::<4096>(dev.id())
            .expect("DM_TABLE_DEPS")
            .next()
            .is_none()
    );
}

#[test]
#[ignore = "requires privileged device-mapper access"]
fn clear_discards_the_staged_table() {
    let control = Control::open().expect("open /dev/mapper/control");

    let name = format!("devmap-test-clear-{}", std::process::id());
    let dev = Owned::create(&control, &name).expect("DM_DEV_CREATE");

    // Stage and activate a first table so the device has an active one.
    control
        .load(
            dev.id(),
            devmap_linux::Table::<4096>::default()
                .add(
                    devmap_core::Span::new(0, 8192),
                    &Zero,
                    control.version("zero").unwrap(),
                )
                .expect("add zero"),
            false,
        )
        .expect("DM_TABLE_LOAD");
    control.suspend(dev.id(), false).expect("resume");
    assert!(
        control.status(dev.id()).expect("status").flags & Status::ACTIVE_TABLE != 0,
        "the resumed table is active"
    );

    // Stage a second, larger table but do NOT resume it.
    control
        .load(
            dev.id(),
            devmap_linux::Table::<4096>::default()
                .add(
                    devmap_core::Span::new(0, 16384),
                    &Zero,
                    control.version("zero").unwrap(),
                )
                .expect("add zero"),
            false,
        )
        .expect("DM_TABLE_LOAD (staged)");
    assert!(
        control.status(dev.id()).expect("status").flags & Status::INACTIVE_TABLE != 0,
        "the staged table is inactive"
    );

    control
        .clear(dev.id())
        .expect("DM_TABLE_CLEAR must discard the staged table");

    let status = control.status(dev.id()).expect("status");
    assert!(
        status.flags & Status::INACTIVE_TABLE == 0,
        "the staged table must be gone after a clear"
    );
    assert!(
        status.flags & Status::ACTIVE_TABLE != 0,
        "clearing the inactive table must leave the active one alone"
    );
}

#[test]
#[ignore = "requires privileged device-mapper access"]
fn clear_is_a_no_op_with_nothing_staged() {
    let control = Control::open().expect("open /dev/mapper/control");

    let name = format!("devmap-test-clear-noop-{}", std::process::id());
    let dev = Owned::create(&control, &name).expect("DM_DEV_CREATE");
    control
        .load(
            dev.id(),
            devmap_linux::Table::<4096>::default()
                .add(
                    devmap_core::Span::new(0, 8192),
                    &Zero,
                    control.version("zero").unwrap(),
                )
                .expect("add zero"),
            false,
        )
        .expect("DM_TABLE_LOAD");
    control.suspend(dev.id(), false).expect("resume");

    // Nothing is staged; clearing must succeed rather than error.
    control
        .clear(dev.id())
        .expect("clearing with nothing staged is not an error");
    assert_eq!(
        control.status(dev.id()).expect("status").flags & Status::INACTIVE_TABLE,
        0
    );
}

#[test]
#[ignore = "requires privileged device-mapper access"]
fn rename_moves_the_device_to_the_new_name() {
    let control = Control::open().expect("open /dev/mapper/control");

    let old = format!("devmap-test-rename-old-{}", std::process::id());
    let new = format!("devmap-test-rename-new-{}", std::process::id());
    let dev = Owned::create(&control, &old).expect("DM_DEV_CREATE");
    control
        .load(
            dev.id(),
            devmap_linux::Table::<4096>::default()
                .add(
                    devmap_core::Span::new(0, 8192),
                    &Zero,
                    control.version("zero").unwrap(),
                )
                .expect("add zero"),
            false,
        )
        .expect("DM_TABLE_LOAD");
    control.suspend(dev.id(), false).expect("resume");
    let id = dev.id();

    let renamed = control.rename(&old, &new).expect("DM_DEV_RENAME");
    assert_eq!(renamed, id, "rename keeps the same dev_t");

    // The new name resolves and the old one is gone.
    let (found, _) = control.by_name(&new).expect("by_name after rename");
    assert_eq!(found, id);
    assert!(
        control.by_name(&old).is_err(),
        "the old name must no longer resolve"
    );
}

#[test]
#[ignore = "requires privileged device-mapper access"]
fn rename_rejects_a_name_already_in_use() {
    let control = Control::open().expect("open /dev/mapper/control");

    let first = format!("devmap-test-rename-clash-a-{}", std::process::id());
    let second = format!("devmap-test-rename-clash-b-{}", std::process::id());
    let a = Owned::create(&control, &first).expect("create first");
    control
        .load(
            a.id(),
            devmap_linux::Table::<4096>::default()
                .add(
                    devmap_core::Span::new(0, 8192),
                    &Zero,
                    control.version("zero").unwrap(),
                )
                .expect("add"),
            false,
        )
        .expect("load");
    control.suspend(a.id(), false).expect("resume");
    let b = Owned::create(&control, &second).expect("create second");
    control
        .load(
            b.id(),
            devmap_linux::Table::<4096>::default()
                .add(
                    devmap_core::Span::new(0, 8192),
                    &Zero,
                    control.version("zero").unwrap(),
                )
                .expect("add"),
            false,
        )
        .expect("load");
    control.suspend(b.id(), false).expect("resume");

    assert!(
        control.rename(&first, &second).is_err(),
        "renaming onto a live name must fail rather than clobber it"
    );
}

#[test]
#[ignore = "requires privileged device-mapper access"]
fn set_uuid_attaches_a_uuid_that_by_uuid_then_finds() {
    let control = Control::open().expect("open /dev/mapper/control");

    // `create` never assigns a uuid, so DM_DEV_RENAME with DM_UUID_FLAG is
    // the only way to give a device one — and the only way `by_uuid` can
    // be exercised against a real device at all.
    let name = format!("devmap-test-uuid-{}", std::process::id());
    let uuid = format!("devmap-test-uuid-value-{}", std::process::id());
    let dev = Owned::create(&control, &name).expect("DM_DEV_CREATE");
    control
        .load(
            dev.id(),
            devmap_linux::Table::<4096>::default()
                .add(
                    devmap_core::Span::new(0, 8192),
                    &Zero,
                    control.version("zero").unwrap(),
                )
                .expect("add zero"),
            false,
        )
        .expect("DM_TABLE_LOAD");
    control.suspend(dev.id(), false).expect("resume");
    let id = dev.id();

    control
        .set_uuid(&name, &uuid)
        .expect("DM_DEV_RENAME (uuid)");

    let (found, _) = control.by_uuid(&uuid).expect("by_uuid after set_uuid");
    assert_eq!(found, id);

    // The kernel allows a uuid to be set once, never changed.
    let other = format!("{uuid}-again");
    assert!(
        control.set_uuid(&name, &other).is_err(),
        "a device's uuid must not be replaceable once set"
    );
}

#[test]
#[ignore = "requires privileged device-mapper access"]
fn list_targets_reports_the_targets_the_kernel_has_registered() {
    let control = Control::open().expect("open /dev/mapper/control");

    let mut reply = control.versions::<4096>().expect("DM_LIST_VERSIONS");
    let mut versions = Vec::new();
    while let Some(record) = reply.next() {
        let (name, version) = record.expect("decode version");
        versions.push((name.to_owned(), version));
    }
    assert!(
        !versions.is_empty(),
        "a kernel with device-mapper has at least one target registered"
    );
    // dm-zero and dm-linear are built into any dm-capable kernel that can
    // run the rest of this suite.
    for expected in ["zero", "linear"] {
        let found = versions.iter().find(|(name, _)| name == expected);
        let target = found.unwrap_or_else(|| panic!("{expected} must be registered: {versions:?}"));
        assert!(
            target.1.major >= 1,
            "{expected} version looks unset: {:?}",
            target.1
        );
    }
}

#[test]
#[ignore = "requires privileged device-mapper access"]
fn a_custom_target_loads_a_table_line() {
    let control = Control::open().expect("open /dev/mapper/control");

    let backing = LoopDevice::create("addraw", 8 * 1024 * 1024);
    let backing_device = devmap_linux::DevId::try_from(std::fs::metadata(&backing.path).unwrap())
        .expect("by_node backing");

    let params = format!("{backing_device} 0");
    let name = format!("devmap-test-addraw-{}", std::process::id());
    let dev = Owned::create(&control, &name).expect("DM_DEV_CREATE");
    control
        .load(
            dev.id(),
            devmap_linux::Table::<4096>::default()
                .add(
                    devmap_core::Span::new(0, 8 * 1024 * 1024 / 512),
                    &Linear {
                        device: backing_device,
                        offset: 0,
                    },
                    control.version("linear").unwrap(),
                )
                .expect("add linear"),
            false,
        )
        .expect("DM_TABLE_LOAD");
    control.suspend(dev.id(), false).expect("resume");

    // The kernel accepts it and reads the same line back.
    let mut table = control.table::<4096>(dev.id()).expect("DM_TABLE_STATUS");
    let rows = table.next().unwrap().unwrap();
    assert_eq!(
        rows.parse::<Linear>(control.version("linear").unwrap())
            .unwrap(),
        params.clone()
    );
}

#[test]
#[ignore = "requires privileged device-mapper access"]
fn wait_returns_at_once_when_the_counter_already_differs() {
    let control = Control::open().expect("open /dev/mapper/control");

    let name = format!("devmap-test-wait-{}", std::process::id());
    let dev = Owned::create(&control, &name).expect("DM_DEV_CREATE");
    control
        .load(
            dev.id(),
            devmap_linux::Table::<4096>::default()
                .add(
                    devmap_core::Span::new(0, 8192),
                    &Zero,
                    control.version("zero").unwrap(),
                )
                .expect("add zero"),
            false,
        )
        .expect("DM_TABLE_LOAD");
    control.suspend(dev.id(), false).expect("resume");

    // The kernel blocks only while the passed value EQUALS the device's
    // current counter, so a value that differs returns immediately. This
    // is the same path a caller hits when an event fired between reading
    // the status and waiting on it, and it keeps the test from hanging.
    let event_nr = control.status(dev.id()).expect("status").event_nr;
    let status = control
        .wait(dev.id(), event_nr.wrapping_add(1))
        .expect("DM_DEV_WAIT");
    assert_eq!(
        status.event_nr, event_nr,
        "wait returns the device's real counter, not the value waited on"
    );
    assert_ne!(status.flags & Status::ACTIVE_TABLE, 0);
}
