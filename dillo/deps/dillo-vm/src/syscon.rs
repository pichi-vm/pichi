//! x86 syscon power devices declared by the base DTB.
//!
//! These are VM-owned substrate MMIO devices: the guest writes the declared
//! value to the declared register and the run loop observes the resulting
//! structured action.

use std::sync::atomic::{AtomicU8, Ordering};

use dillo_mmio::{MmioDevice, MmioWindow};

const WINDOW_SIZE: u64 = 0x1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SysconAction {
    Poweroff,
    Reboot,
}

impl SysconAction {
    fn code(self) -> u8 {
        match self {
            Self::Poweroff => 1,
            Self::Reboot => 2,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Poweroff),
            2 => Some(Self::Reboot),
            _ => None,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct SysconState {
    action: AtomicU8,
}

impl SysconState {
    pub(crate) fn request(&self, action: SysconAction) {
        let _ = self
            .action
            .compare_exchange(0, action.code(), Ordering::AcqRel, Ordering::Acquire);
    }

    pub(crate) fn action(&self) -> Option<SysconAction> {
        SysconAction::from_code(self.action.load(Ordering::Acquire))
    }
}

#[derive(Debug)]
pub(crate) struct SysconDevice {
    window: MmioWindow,
    offset: u64,
    value: u32,
    mask: u32,
    action: SysconAction,
    state: std::sync::Arc<SysconState>,
}

impl SysconDevice {
    pub(crate) fn new(
        name: &'static str,
        syscon: dillo_platform::Syscon,
        action: SysconAction,
        state: std::sync::Arc<SysconState>,
    ) -> Self {
        Self {
            window: MmioWindow {
                name,
                base: syscon.base,
                size: WINDOW_SIZE,
            },
            offset: syscon.offset,
            value: syscon.value,
            mask: syscon.mask,
            action,
            state,
        }
    }

    fn matches(&self, offset: u64, data: &[u8]) -> bool {
        if offset != self.offset {
            return false;
        }
        let value = match data.len() {
            1 => u32::from(data[0]),
            2 => u32::from(u16::from_le_bytes([data[0], data[1]])),
            4 => u32::from_le_bytes([data[0], data[1], data[2], data[3]]),
            _ => return false,
        };
        (value & self.mask) == (self.value & self.mask)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn matches_poweroff(
        poweroff: dillo_platform::Syscon,
        addr: u64,
        data: &[u8],
    ) -> bool {
        let device = Self::new(
            "syscon-poweroff",
            poweroff,
            SysconAction::Poweroff,
            std::sync::Arc::new(SysconState::default()),
        );
        addr.checked_sub(poweroff.base)
            .is_some_and(|offset| device.matches(offset, data))
    }
}

impl MmioDevice for SysconDevice {
    fn windows(&self) -> &[MmioWindow] {
        std::slice::from_ref(&self.window)
    }

    fn read(&self, _window: MmioWindow, _offset: u64, data: &mut [u8]) -> bool {
        data.fill(0);
        true
    }

    fn write(&self, _window: MmioWindow, offset: u64, data: &[u8]) -> bool {
        if self.matches(offset, data) {
            log::info!("guest issued {:?} via {}", self.action, self.window.name);
            self.state.request(self.action);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn syscon() -> dillo_platform::Syscon {
        dillo_platform::Syscon {
            base: 0x901_0000,
            offset: 0x10,
            value: 0xCAFE,
            mask: 0xFFFF,
        }
    }

    #[test]
    fn matching_write_records_action() {
        let state = std::sync::Arc::new(SysconState::default());
        let device = SysconDevice::new(
            "syscon-poweroff",
            syscon(),
            SysconAction::Poweroff,
            std::sync::Arc::clone(&state),
        );
        let window = device.windows()[0];

        assert!(device.write(window, 0x10, &0xCAFEu32.to_le_bytes()));

        assert_eq!(state.action(), Some(SysconAction::Poweroff));
    }

    #[test]
    fn non_matching_write_is_claimed_without_action() {
        let state = std::sync::Arc::new(SysconState::default());
        let device = SysconDevice::new(
            "syscon-reboot",
            syscon(),
            SysconAction::Reboot,
            std::sync::Arc::clone(&state),
        );
        let window = device.windows()[0];

        assert!(device.write(window, 0x14, &0xCAFEu32.to_le_bytes()));

        assert_eq!(state.action(), None);
    }
}
