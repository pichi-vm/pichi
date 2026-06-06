// SPDX-License-Identifier: Apache-2.0

//! Userspace PSCI (Power State Coordination Interface) dispatch.
//!
//! On HVF there is no in-kernel PSCI (unlike KVM), so dillo handles the
//! guest's PSCI HVC calls itself. arma's base DTB declares
//! `psci { method = "hvc"; }`, so calls arrive as `VmExit::Hvc` with the
//! 32-bit function ID in `x0` (`args[0]`) and arguments in `x1..`.
//!
//! This module is the **pure** decode: it maps a call to a [`PsciAction`]
//! the run loop then performs (wake a vCPU, shut down, or write a return
//! value into `x0`). Keeping it side-effect-free makes it unit-testable
//! without a hypervisor.

/// PSCI function IDs (SMC32 / SMC64 variants share the low 16 bits; the
/// SMC64 variants set bit 30 = `0x4000_0000`).
mod fid {
    pub(super) const VERSION: u32 = 0x8400_0000;
    pub(super) const CPU_OFF: u32 = 0x8400_0002;
    pub(super) const CPU_ON_32: u32 = 0x8400_0003;
    pub(super) const CPU_ON_64: u32 = 0xC400_0003;
    pub(super) const AFFINITY_INFO_32: u32 = 0x8400_0004;
    pub(super) const AFFINITY_INFO_64: u32 = 0xC400_0004;
    pub(super) const MIGRATE_INFO_TYPE: u32 = 0x8400_0006;
    pub(super) const SYSTEM_OFF: u32 = 0x8400_0008;
    pub(super) const SYSTEM_RESET: u32 = 0x8400_0009;
    pub(super) const FEATURES: u32 = 0x8400_000A;
}

/// PSCI return codes (per the PSCI spec; negative i32 widened to u64 in `x0`).
mod ret {
    pub(super) const SUCCESS: u64 = 0;
    pub(super) const NOT_SUPPORTED: u64 = (-1i64) as u64;
    pub(super) const INVALID_PARAMETERS: u64 = (-2i64) as u64;
    /// AFFINITY_INFO: target core is ON.
    pub(super) const AFF_ON: u64 = 0;
    /// MIGRATE_INFO_TYPE: Trusted OS migration not required / not present.
    pub(super) const MIGRATE_NOT_REQUIRED: u64 = 2;
    /// PSCI v1.1 (major 1, minor 1).
    pub(super) const VERSION_1_1: u64 = 0x0001_0001;
}

/// What the run loop should do in response to a PSCI call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PsciAction {
    /// Power on a secondary core: start it at `entry` with `x0 = context`.
    /// `target` is the requested `MPIDR_EL1` affinity.
    CpuOn {
        target: u64,
        entry: u64,
        context: u64,
    },
    /// The calling core powers itself off.
    CpuOff,
    /// Shut the whole VM down (clean guest poweroff).
    SystemOff,
    /// Reset the VM (deferred — Phase 2 in-VM restart; for now the run loop
    /// treats it as a reboot-requested exit).
    SystemReset,
    /// Complete the call by writing `x0 = value` and resuming the caller.
    Return(u64),
}

/// Decode a PSCI HVC. `args` are `x0..x7`; `args[0]` carries the function ID.
pub(crate) fn dispatch(args: &[u64; 8]) -> PsciAction {
    #[allow(clippy::cast_possible_truncation)]
    let function = args[0] as u32;
    match function {
        fid::VERSION => PsciAction::Return(ret::VERSION_1_1),
        fid::CPU_ON_32 | fid::CPU_ON_64 => PsciAction::CpuOn {
            target: args[1],
            entry: args[2],
            context: args[3],
        },
        fid::CPU_OFF => PsciAction::CpuOff,
        fid::AFFINITY_INFO_32 | fid::AFFINITY_INFO_64 => PsciAction::Return(ret::AFF_ON),
        fid::MIGRATE_INFO_TYPE => PsciAction::Return(ret::MIGRATE_NOT_REQUIRED),
        fid::SYSTEM_OFF => PsciAction::SystemOff,
        fid::SYSTEM_RESET => PsciAction::SystemReset,
        fid::FEATURES => PsciAction::Return(features(args[1] as u32)),
        _ => PsciAction::Return(ret::NOT_SUPPORTED),
    }
}

/// PSCI_FEATURES: 0 (SUCCESS) for functions we implement, NOT_SUPPORTED else.
fn features(queried: u32) -> u64 {
    match queried {
        fid::VERSION
        | fid::CPU_OFF
        | fid::CPU_ON_32
        | fid::CPU_ON_64
        | fid::AFFINITY_INFO_32
        | fid::AFFINITY_INFO_64
        | fid::MIGRATE_INFO_TYPE
        | fid::SYSTEM_OFF
        | fid::SYSTEM_RESET
        | fid::FEATURES => ret::SUCCESS,
        _ => ret::NOT_SUPPORTED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(fid: u64, a1: u64, a2: u64, a3: u64) -> PsciAction {
        dispatch(&[fid, a1, a2, a3, 0, 0, 0, 0])
    }

    #[test]
    fn version_reports_1_1() {
        assert_eq!(call(0x8400_0000, 0, 0, 0), PsciAction::Return(0x0001_0001));
    }

    #[test]
    fn cpu_on_decodes_target_entry_context() {
        // SMC64 and SMC32 forms decode identically.
        let want = PsciAction::CpuOn {
            target: 0x1,
            entry: 0x4000_0000,
            context: 0xABCD,
        };
        assert_eq!(call(0xC400_0003, 0x1, 0x4000_0000, 0xABCD), want);
        assert_eq!(call(0x8400_0003, 0x1, 0x4000_0000, 0xABCD), want);
    }

    #[test]
    fn shutdown_and_reset() {
        assert_eq!(call(0x8400_0008, 0, 0, 0), PsciAction::SystemOff);
        assert_eq!(call(0x8400_0009, 0, 0, 0), PsciAction::SystemReset);
    }

    #[test]
    fn cpu_off_and_affinity_and_migrate() {
        assert_eq!(call(0x8400_0002, 0, 0, 0), PsciAction::CpuOff);
        assert_eq!(call(0xC400_0004, 0, 0, 0), PsciAction::Return(0)); // AFF_ON
        assert_eq!(call(0x8400_0006, 0, 0, 0), PsciAction::Return(2)); // MIGRATE not required
    }

    #[test]
    fn features_known_vs_unknown() {
        assert_eq!(call(0x8400_000A, 0xC400_0003, 0, 0), PsciAction::Return(0)); // CPU_ON supported
        assert_eq!(
            call(0x8400_000A, 0xDEAD_BEEF, 0, 0),
            PsciAction::Return((-1i64) as u64)
        );
    }

    #[test]
    fn unknown_function_is_not_supported() {
        assert_eq!(
            call(0x8400_00FF, 0, 0, 0),
            PsciAction::Return((-1i64) as u64)
        );
    }
}
