// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Real-kernel coverage for `DM_DEV_ARM_POLL` and the `AsFd` impl that
//! makes it usable — the readiness path an async reactor would drive.
//!
//! Deliberately a test binary of its own containing a single test. The
//! kernel arms against one subsystem-wide counter, bumped by *any* dm
//! change, so a test that asserts a freshly armed fd is quiet races with
//! any concurrent device creation. Cargo runs test binaries one at a time
//! but threads the tests within a binary, so isolation here means one
//! test in one file. Anything else on the system touching dm during the
//! run could still perturb it; nothing in this suite will.

mod common;

use std::os::fd::AsFd as _;

use devmap_linux::Control;
use rustix::event::{PollFd, PollFlags, Timespec, poll};

use common::Owned;

/// Poll the control fd for readability with a bounded timeout, so the
/// test can never hang waiting on an event that will not arrive.
fn poll_readable(control: &Control, millis: i32) -> bool {
    let fd = control.as_fd();
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    let timeout = Timespec {
        tv_sec: i64::from(millis) / 1000,
        tv_nsec: (i64::from(millis) % 1000) * 1_000_000,
    };
    let n = poll(&mut fds, Some(&timeout)).expect("poll");
    n > 0 && fds[0].revents().contains(PollFlags::IN)
}

#[test]
#[ignore = "requires privileged device-mapper access"]
fn arm_poll_drives_readiness_on_the_control_fd() {
    let control = Control::open().expect("open /dev/mapper/control");

    // Arming clears readiness left over from earlier dm activity, so the
    // fd starts quiet.
    control.arm_poll().expect("DM_DEV_ARM_POLL");
    assert!(
        !poll_readable(&control, 0),
        "a freshly armed control fd must not already be readable"
    );

    // A clone shares the underlying file, so it shares the armed state
    // rather than getting a private copy of it.
    let clone = control.clone();
    assert!(
        !poll_readable(&clone, 0),
        "a clone shares the armed fd, so it is quiet too"
    );

    // DM_DEV_CREATE bumps the subsystem-wide counter.
    let name = format!("devmap-test-armpoll-{}", std::process::id());
    let dev = Owned::create(&control, &name).expect("DM_DEV_CREATE");

    assert!(
        poll_readable(&control, 1000),
        "creating a device must make the armed control fd readable"
    );
    assert!(
        poll_readable(&clone, 0),
        "the clone observes the same readiness"
    );

    // Level-triggered: readiness persists until the fd is re-armed, which
    // is what lets a reactor decide when to acknowledge it.
    assert!(
        poll_readable(&control, 0),
        "readiness must persist until re-armed"
    );
    control.arm_poll().expect("re-arm");
    assert!(
        !poll_readable(&control, 0),
        "re-arming must clear the pending readiness"
    );

    // Removal is another global event, so the re-armed fd wakes again.
    drop(dev);
    assert!(
        poll_readable(&control, 1000),
        "removing a device must wake the re-armed fd"
    );
}
