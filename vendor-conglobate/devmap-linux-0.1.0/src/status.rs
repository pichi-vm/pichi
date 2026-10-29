// SPDX-License-Identifier: Apache-2.0

use super::abi::Header;

/// Whole-device state, including open references and active or staged tables.
///
/// Returned by [`Control::status`](crate::Control::status),
/// [`Control::wait`](crate::Control::wait), and name/UUID lookups.
/// Target-specific runtime information is available through
/// [`Control::info`](crate::Control::info).
///
/// # Examples
///
/// Test individual flags without assuming that all returned bits are known.
///
/// ```no_run
/// use devmap_linux::{Control, Status};
///
/// # fn example() -> std::io::Result<()> {
/// let control = Control::open()?;
/// let (_, status) = control.by_name("data")?;
/// if status.flags & Status::SUSPENDED != 0 {
///     println!("data is suspended");
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Status {
    /// Number of open references to the device. Signed to match the kernel.
    pub open_count: i32,
    /// Number of targets in the active table.
    pub target_count: u32,
    /// Event counter to pass to [`Control::wait`](crate::Control::wait).
    pub event_nr: u32,
    /// Device-state and operation-result flags; test individual bits with the
    /// associated constants. Unknown bits are preserved, including the buffer-full
    /// flag that [`Control::wait`](crate::Control::wait) may return.
    pub flags: u32,
}

impl Status {
    /// The device is read-only.
    pub const READ_ONLY: u32 = 1 << 0;
    /// The device is suspended.
    pub const SUSPENDED: u32 = 1 << 1;
    /// An active table is present.
    pub const ACTIVE_TABLE: u32 = 1 << 5;
    /// An inactive (staged) table is present.
    pub const INACTIVE_TABLE: u32 = 1 << 6;
    /// The operation returning this status generated a uevent.
    pub const UEVENT_GENERATED: u32 = 1 << 13;
}

impl From<&Header> for Status {
    fn from(header: &Header) -> Self {
        Self {
            open_count: header.open_count(),
            target_count: header.target_count(),
            event_nr: header.event_nr(),
            flags: header.flags(),
        }
    }
}
