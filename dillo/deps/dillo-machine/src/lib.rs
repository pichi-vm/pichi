//! Host-neutral machine boundary for dillo.
//!
//! This crate owns the narrow VM-facing traits shared by backend machine
//! implementations. Concrete backend crates provide inherent constructors and
//! implement the attachment set that the top-level `dillo` launcher uses.

pub use dillo_mmio::DeviceModel;

/// A constructed VM capable of accepting DTB-derived resources and vCPUs.
pub trait Machine: Sized + Send + Sync + 'static {
    type Error: std::error::Error + Send + Sync + 'static;
    type Vcpu: Vcpu<Error = Self::Error>;
    type Cpu: Send + 'static;
    type Memory: Send + 'static;

    const DEVICE_MODEL: DeviceModel;

    /// Make every currently running vCPU for this machine leave `Vcpu::run`.
    fn request_vcpu_exit(&self) -> Result<(), Self::Error>;
}

/// One runnable vCPU owned by a machine backend.
pub trait Vcpu: Send + 'static {
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

    use dillo_mmio::Attach;

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

    impl Machine for TestMachine {
        type Error = TestError;
        type Vcpu = TestVcpu;
        type Cpu = TestCpu;
        type Memory = TestMemory;

        const DEVICE_MODEL: DeviceModel = DeviceModel::Thread;

        fn request_vcpu_exit(&self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    impl Attach<TestMemory> for TestMachine {
        type Error = TestError;
        type Output = ();

        fn attach(&mut self, _item: TestMemory) -> Result<Self::Output, Self::Error> {
            Ok(())
        }
    }

    impl Attach<TestCpu> for TestMachine {
        type Error = TestError;
        type Output = TestVcpu;

        fn attach(&mut self, _item: TestCpu) -> Result<Self::Output, Self::Error> {
            Ok(TestVcpu {
                stop: VcpuStop::Stopped,
            })
        }
    }

    fn build_one_vcpu<M>(machine: &mut M) -> Result<<M as Machine>::Vcpu, <M as Machine>::Error>
    where
        M: Machine,
        M: Attach<<M as Machine>::Memory, Error = <M as Machine>::Error, Output = ()>,
        M: Attach<<M as Machine>::Cpu, Error = <M as Machine>::Error, Output = M::Vcpu>,
        <M as Machine>::Memory: Default,
        <M as Machine>::Cpu: Default,
    {
        <M as Attach<M::Memory>>::attach(machine, M::Memory::default())?;
        <M as Attach<M::Cpu>>::attach(machine, M::Cpu::default())
    }

    impl Default for TestCpu {
        fn default() -> Self {
            Self
        }
    }

    impl Default for TestMemory {
        fn default() -> Self {
            Self
        }
    }

    #[test]
    fn machine_uses_associated_input_types_and_attach() {
        let mut machine = TestMachine;
        let mut vcpu = build_one_vcpu(&mut machine).expect("vCPU created");

        assert_eq!(TestMachine::DEVICE_MODEL, DeviceModel::Thread);
        assert_eq!(vcpu.run().expect("vCPU run"), VcpuStop::Stopped);
        machine.request_vcpu_exit().expect("exit requested");
    }
}
