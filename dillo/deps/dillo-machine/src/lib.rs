//! Host-neutral machine boundary for dillo.
//!
//! This crate owns the narrow VM-facing traits shared by backend machine
//! implementations. Concrete backend crates implement this trait boundary, and
//! the top-level `dillo` launcher composes only through these APIs.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use dillo_mmio::{Interrupt, MessageInterruptDomain};

/// Selected-machine launch configuration derived by dillo from PMI and DTB.
#[derive(Debug)]
pub struct LaunchConfig {
    pub dtb: Vec<u8>,
    pub vcpus: u32,
    pub min_addr_space_bits: u32,
}

/// One DTB/launch-derived guest RAM range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RamRange {
    pub gpa: u64,
    pub size: u64,
}

/// Arch-erased view of the PMI boot vCPU state.
///
/// Backends pick the arm they support; dillo never constructs backend-specific
/// CPU input structs directly.
pub trait BootVcpuState {
    fn x86_64(&self) -> Option<&pmi::vm::vcpu::x86_64::CpuState> {
        None
    }

    fn aarch64(&self) -> Option<&pmi::vm::vcpu::aarch64::CpuState> {
        None
    }
}

/// Supervisor-provided lifecycle control observed by machine run loops.
pub trait RunControl: Send + Sync + 'static {
    fn stop_requested(&self) -> Option<VcpuStop>;
}

impl RunControl for std::sync::atomic::AtomicBool {
    fn stop_requested(&self) -> Option<VcpuStop> {
        self.load(std::sync::atomic::Ordering::Acquire)
            .then_some(VcpuStop::Stopped)
    }
}

/// Host architecture exposed by the selected machine backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostArchitecture {
    X86_64,

    Aarch64,
}

/// Device-host execution model used by one machine backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceModel {
    Thread,

    Process,
}

/// Host-level services exposed by the selected machine backend.
pub trait Host {
    type RawStdioGuard: 'static;

    const ARCH: HostArchitecture;

    /// Host-provided CPU `compatible` string for DT overlay CPU nodes, when the
    /// selected machine can derive one without inventing guest-visible facts.
    fn cpu_compatible() -> Option<&'static str> {
        None
    }

    fn enter_raw_stdio_if_tty() -> Self::RawStdioGuard;

    fn install_panic_terminal_restore();

    fn install_signal_watchers(supervisor_shutdown: &'static AtomicBool);
}

/// A constructed VM capable of accepting DTB-derived resources and vCPUs.
pub trait Machine: Sized + 'static {
    type Error: std::error::Error + Send + Sync + 'static;
    type Vcpu: Vcpu<Error = Self::Error>;
    type Cpu: 'static;
    type Memory: 'static;

    const DEVICE_MODEL: DeviceModel;

    /// Construct the backend VM from selected-machine launch facts.
    fn from_launch_config(config: LaunchConfig) -> Result<Self, Self::Error>;

    /// Attach all standard guest RAM ranges for this VM.
    fn attach_ram(&mut self, ranges: &[RamRange]) -> Result<(), Self::Error>;

    /// Write launch data into guest RAM through backend-owned memory access.
    fn write_guest(&mut self, gpa: u64, data: &[u8]) -> Result<(), Self::Error>;

    /// Create one backend CPU input and attach it as a runnable vCPU.
    fn create_vcpu(
        &mut self,
        index: u32,
        cpu_profile: &str,
        boot_state: Option<&dyn BootVcpuState>,
    ) -> Result<Self::Vcpu, Self::Error>;

    /// Run the VM's vCPUs until guest or supervisor lifecycle stop.
    fn run_vcpus(
        &mut self,
        count: u32,
        cpu_profile: &str,
        boot_state: &dyn BootVcpuState,
        control: Arc<dyn RunControl>,
    ) -> Result<VcpuStop, Self::Error>;

    /// Make every currently running vCPU for this machine leave `Vcpu::run`.
    fn request_vcpu_exit(&self) -> Result<(), Self::Error>;

    /// Prepare backend-owned run state after all vCPUs have been created.
    fn prepare_vcpu_run(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    /// Reset backend-owned state before replaying launch writes for a guest reboot.
    fn reset_for_reboot(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    /// Create a backend-owned wired interrupt capability.
    fn create_line_interrupt(&self, _source: u32) -> Result<Interrupt, Self::Error>;

    /// Create a backend-owned message-interrupt domain.
    fn create_message_interrupt_domain(
        &self,
        _vectors: u16,
    ) -> Result<Arc<dyn MessageInterruptDomain>, Self::Error>;
}

/// One runnable vCPU owned by a machine backend.
pub trait Vcpu: 'static {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Run until the guest or supervisor reaches a lifecycle stop point.
    fn run(&mut self) -> Result<VcpuStop, Self::Error>;
}

/// Successful vCPU lifecycle outcomes reported to the supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VcpuStop {
    /// The guest requested system poweroff.
    GuestPoweroff,

    /// The guest requested system reset.
    GuestReset,

    /// The supervisor requested that this vCPU stop running.
    Stopped,
}

#[cfg(test)]
mod tests {
    use std::fmt;
    use std::sync::Arc;

    use dillo_mmio::{Interrupt, InterruptError, MessageInterrupt, MessageInterruptDomain};

    use super::*;

    #[derive(Debug)]
    struct TestError;

    impl fmt::Display for TestError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("test machine error")
        }
    }

    impl std::error::Error for TestError {}

    struct TestCpu;

    struct TestMemory;

    struct TestVcpu {
        stop: VcpuStop,
    }

    impl Vcpu for TestVcpu {
        type Error = TestError;

        fn run(&mut self) -> Result<VcpuStop, Self::Error> {
            Ok(self.stop)
        }
    }

    struct TestMachine;

    #[derive(Debug)]
    struct TestMessageInterruptDomain;

    impl MessageInterruptDomain for TestMessageInterruptDomain {
        fn update(&self, _vector: u16, _msg: MessageInterrupt) -> Result<(), InterruptError> {
            Ok(())
        }

        fn enabled(&self, _enabled: bool) -> Result<(), InterruptError> {
            Ok(())
        }

        fn interrupt(&self, _vector: u16) -> Option<Interrupt> {
            None
        }
    }

    impl Machine for TestMachine {
        type Error = TestError;
        type Vcpu = TestVcpu;
        type Cpu = TestCpu;
        type Memory = TestMemory;

        const DEVICE_MODEL: DeviceModel = DeviceModel::Thread;

        fn from_launch_config(_config: LaunchConfig) -> Result<Self, Self::Error> {
            Ok(Self)
        }

        fn attach_ram(&mut self, _ranges: &[RamRange]) -> Result<(), Self::Error> {
            Ok(())
        }

        fn write_guest(&mut self, _gpa: u64, _data: &[u8]) -> Result<(), Self::Error> {
            Ok(())
        }

        fn create_vcpu(
            &mut self,
            _index: u32,
            _cpu_profile: &str,
            _boot_state: Option<&dyn BootVcpuState>,
        ) -> Result<Self::Vcpu, Self::Error> {
            Ok(TestVcpu {
                stop: VcpuStop::Stopped,
            })
        }

        fn run_vcpus(
            &mut self,
            _count: u32,
            _cpu_profile: &str,
            _boot_state: &dyn BootVcpuState,
            _control: Arc<dyn RunControl>,
        ) -> Result<VcpuStop, Self::Error> {
            Ok(VcpuStop::Stopped)
        }

        fn request_vcpu_exit(&self) -> Result<(), Self::Error> {
            Ok(())
        }

        fn create_line_interrupt(&self, _source: u32) -> Result<Interrupt, Self::Error> {
            Ok(Interrupt::from_fn(|| {}))
        }

        fn create_message_interrupt_domain(
            &self,
            _vectors: u16,
        ) -> Result<Arc<dyn MessageInterruptDomain>, Self::Error> {
            Ok(Arc::new(TestMessageInterruptDomain))
        }
    }

    #[test]
    fn machine_uses_common_launch_ram_and_vcpu_api() {
        let mut machine = TestMachine;
        machine
            .attach_ram(&[RamRange {
                gpa: 0x1000,
                size: 0x2000,
            }])
            .expect("RAM attached");
        machine
            .write_guest(0x1000, b"boot")
            .expect("guest write accepted");
        let mut vcpu = machine
            .create_vcpu(0, "test-profile", None)
            .expect("vCPU created");

        assert_eq!(vcpu.run().expect("vCPU run"), VcpuStop::Stopped);
        machine.request_vcpu_exit().expect("exit requested");
    }
}
