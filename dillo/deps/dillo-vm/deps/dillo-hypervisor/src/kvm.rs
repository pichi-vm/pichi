//! KVM-backed `Vm` and `Vcpu`.
//!
//! See `dillo/ARCHITECTURE.md` §9, §16–§17 (x86), §21–§22 (aarch64).

#![allow(clippy::cast_possible_truncation)]

use std::os::fd::AsRawFd;
use std::sync::Arc;

use kvm_bindings::{kvm_guest_debug, kvm_userspace_memory_region};
#[cfg(target_arch = "x86_64")]
use kvm_bindings::{kvm_regs, kvm_segment};
use kvm_ioctls::{Cap, Kvm, VcpuExit, VcpuFd, VmFd};
use thiserror::Error;

use crate::VmExit;
#[cfg(target_arch = "x86_64")]
use crate::cpuid_x86;

/// Hypervisor / VM errors.
#[derive(Debug, Error)]
pub enum Error {
    #[error("open /dev/kvm: {0}")]
    OpenKvm(std::io::Error),

    #[error("KVM API version mismatch: got {0}, expected 12")]
    ApiVersion(i32),

    #[error("required KVM capability {0:?} missing")]
    MissingCap(Cap),

    #[error("create VM: {0}")]
    CreateVm(std::io::Error),

    #[error("create IRQ chip: {0}")]
    CreateIrqChip(std::io::Error),

    #[error("set TSS address: {0}")]
    SetTss(std::io::Error),

    #[error("set user memory region: {0}")]
    SetMemRegion(std::io::Error),

    #[error("create vCPU {0}: {1}")]
    CreateVcpu(u32, std::io::Error),

    #[error("set vCPU {0} regs: {1}")]
    SetRegs(u32, std::io::Error),

    #[error("set vCPU {0} sregs: {1}")]
    SetSregs(u32, std::io::Error),

    #[error("get vCPU {0} sregs: {1}")]
    GetSregs(u32, std::io::Error),

    #[error("run vCPU {0}: {1}")]
    RunVcpu(u32, std::io::Error),

    #[error("cpu:profile {profile:?} not recognized by dillo")]
    UnknownCpuProfile { profile: String },

    #[error(
        "cpu:profile {profile:?} requires host CPUID feature `{feature}` which is not available"
    )]
    HostMissingCpuFeature {
        profile: String,
        feature: &'static str,
    },
}

/// Convert kvm-ioctls' `vmm_sys_util::errno::Error` into `std::io::Error`
/// so our public Error variants stay platform-neutral.
fn io(e: kvm_ioctls::Error) -> std::io::Error {
    std::io::Error::from_raw_os_error(e.errno())
}

/// VM handle. Cheaply cloned via Arc-wrapped inner state.
#[derive(Clone, Debug)]
pub struct Vm {
    inner: Arc<VmInner>,
}

#[derive(Debug)]
struct VmInner {
    _kvm: Kvm,
    vm: std::sync::Arc<VmFd>,
}

impl Vm {
    /// Borrow the underlying `Arc<VmFd>` for callers that need to wire
    /// KVM-specific facilities (ioeventfd, irqfd) directly. Cheap
    /// `Arc::clone` — both `Vm` and the returned handle share the fd.
    /// Only meaningful on Linux because `VmFd` is KVM-specific.
    pub fn vm_fd_arc(&self) -> std::sync::Arc<VmFd> {
        std::sync::Arc::clone(&self.inner.vm)
    }

    /// Open `/dev/kvm`, create a VM, and (on x86_64) set up the in-kernel
    /// LAPIC + I/O APIC.
    pub fn new() -> Result<Self, Error> {
        let kvm = Kvm::new().map_err(io).map_err(Error::OpenKvm)?;
        let api = kvm.get_api_version();
        if api != 12 {
            return Err(Error::ApiVersion(api));
        }
        let vm = kvm.create_vm().map_err(io).map_err(Error::CreateVm)?;

        #[cfg(target_arch = "x86_64")]
        {
            // In-kernel IRQchip (LAPIC + I/O APIC + PIC). Must be created
            // before any vCPU.
            vm.create_irq_chip()
                .map_err(io)
                .map_err(Error::CreateIrqChip)?;
            // KVM_SET_TSS_ADDR is required by Intel VMX (Intel CPUs that
            // don't support unrestricted guest). AMD/SVM does not need it.
            // Place at a low GPA below arma's first loaded section
            // (which on x86 starts at 0x3EE00000) to avoid any memslot
            // collision. AMD-SVM accepts and ignores the value.
            vm.set_tss_address(0x3000_0000)
                .map_err(io)
                .map_err(Error::SetTss)?;
        }

        Ok(Self {
            inner: Arc::new(VmInner {
                _kvm: kvm,
                vm: std::sync::Arc::new(vm),
            }),
        })
    }

    /// Register a userspace memory region.
    ///
    /// SAFETY caveat encoded by KVM's API: `host_addr` must remain valid
    /// for the lifetime of the slot. Caller (dillo-vm) holds the memfd
    /// mapping for the VM's entire lifetime.
    pub fn add_memslot(&self, slot: u32, gpa: u64, host_addr: u64, size: u64) -> Result<(), Error> {
        let region = kvm_userspace_memory_region {
            slot,
            flags: 0,
            guest_phys_addr: gpa,
            memory_size: size,
            userspace_addr: host_addr,
        };
        // SAFETY: dillo-vm keeps the underlying mmap of `host_addr`
        // alive for the VM's entire lifetime (the memfd + its mapping
        // are owned by the VM child process). KVM does not access this
        // memory after the slot is removed.
        #[allow(unsafe_code)]
        unsafe {
            self.inner
                .vm
                .set_user_memory_region(region)
                .map_err(io)
                .map_err(Error::SetMemRegion)?;
        }
        Ok(())
    }

    /// Create vCPU `idx` and apply the CPUID derived from `cpu_profile`.
    ///
    /// Per `pmi/spec/cpu.md` and ARCH §20.2: the `vm` target is
    /// non-measured, so dillo MAY pass additional host-supported leaves
    /// through unchanged. The profile is a FLOOR — every mandatory
    /// feature must be present on the host (per-feature check, host's
    /// claimed vendor/family not trusted). On x86_64 we apply via
    /// `KVM_SET_CPUID2(KVM_GET_SUPPORTED_CPUID)` and refuse with
    /// [`Error::HostMissingCpuFeature`] if the floor is unmet.
    pub fn create_vcpu(&self, idx: u32, cpu_profile: &str) -> Result<Vcpu, Error> {
        let fd = self
            .inner
            .vm
            .create_vcpu(idx.into())
            .map_err(|e| Error::CreateVcpu(idx, io(e)))?;

        #[cfg(target_arch = "x86_64")]
        {
            let level = cpuid_x86::X86Level::parse(cpu_profile).ok_or_else(|| {
                Error::UnknownCpuProfile {
                    profile: cpu_profile.to_string(),
                }
            })?;
            let supported = self
                .inner
                ._kvm
                .get_supported_cpuid(kvm_bindings::KVM_MAX_CPUID_ENTRIES)
                .map_err(|e| Error::CreateVcpu(idx, io(e)))?;
            if let Some(missing) = cpuid_x86::first_missing(level, &supported) {
                return Err(Error::HostMissingCpuFeature {
                    profile: level.as_str().to_string(),
                    feature: missing,
                });
            }
            fd.set_cpuid2(&supported)
                .map_err(|e| Error::CreateVcpu(idx, io(e)))?;
        }

        #[cfg(not(target_arch = "x86_64"))]
        {
            let _ = cpu_profile;
        }

        Ok(Vcpu { fd, idx })
    }
}

/// Per-vCPU handle. Move into the vCPU thread; call `run()` in a loop.
#[derive(Debug)]
pub struct Vcpu {
    fd: VcpuFd,
    idx: u32,
}

impl Vcpu {
    pub fn index(&self) -> u32 {
        self.idx
    }

    /// Apply boot-vCPU register state from a `pmi::vm::vcpu::x86_64::CpuState`.
    #[cfg(target_arch = "x86_64")]
    pub fn set_x86_64_state(
        &mut self,
        state: &pmi::vm::vcpu::x86_64::CpuState,
    ) -> Result<(), Error> {
        let regs = kvm_regs {
            rip: state.rip,
            rsp: state.rsp,
            rflags: if state.rflags == 0 { 0x2 } else { state.rflags },
            rax: state.rax,
            rbx: state.rbx,
            rcx: state.rcx,
            rdx: state.rdx,
            rsi: state.rsi,
            rdi: state.rdi,
            rbp: state.rbp,
            r8: state.r8,
            r9: state.r9,
            r10: state.r10,
            r11: state.r11,
            r12: state.r12,
            r13: state.r13,
            r14: state.r14,
            r15: state.r15,
        };
        self.fd
            .set_regs(&regs)
            .map_err(|e| Error::SetRegs(self.idx, io(e)))?;

        let mut sregs = self
            .fd
            .get_sregs()
            .map_err(|e| Error::SetSregs(self.idx, io(e)))?;
        sregs.cr0 = state.cr0;
        sregs.cr3 = state.cr3;
        sregs.cr4 = state.cr4;
        sregs.efer = state.efer;
        sregs.cs = seg_from_pmi(&state.cs);
        sregs.ds = seg_from_pmi(&state.ds);
        sregs.es = seg_from_pmi(&state.es);
        sregs.fs = seg_from_pmi(&state.fs);
        sregs.gs = seg_from_pmi(&state.gs);
        sregs.ss = seg_from_pmi(&state.ss);
        sregs.gdt.base = state.gdtr.base;
        sregs.gdt.limit = state.gdtr.limit;
        sregs.idt.base = state.idtr.base;
        sregs.idt.limit = state.idtr.limit;

        // TR + LDTR: arma's vm:vcpu doesn't set these. VMX entry
        // requires TR to be a present 64-bit TSS in long mode; an
        // all-zero TR fails the VM-entry control check. Initialize
        // both with default-valid descriptors. See task P12.
        sregs.tr = kvm_segment {
            base: 0,
            limit: 0xFFFF,
            selector: 0,
            type_: 0xB,
            present: 1,
            dpl: 0,
            db: 0,
            s: 0,
            l: 0,
            g: 0,
            avl: 0,
            unusable: 0,
            padding: 0,
        };
        sregs.ldt = kvm_segment {
            base: 0,
            limit: 0xFFFF,
            selector: 0,
            type_: 0x2,
            present: 1,
            dpl: 0,
            db: 0,
            s: 0,
            l: 0,
            g: 0,
            avl: 0,
            unusable: 0,
            padding: 0,
        };

        self.fd
            .set_sregs(&sregs)
            .map_err(|e| Error::SetSregs(self.idx, io(e)))?;
        Ok(())
    }

    /// Configure KVM_GUESTDBG flags directly. Used by the gdb stub to
    /// toggle between "run free", "single-step", and "report INT3/HW
    /// breakpoint" modes between guest runs.
    pub fn set_guest_debug_flags(&self, flags: u32) -> Result<(), Error> {
        let dbg = kvm_guest_debug {
            control: flags,
            pad: 0,
            arch: kvm_bindings::kvm_guest_debug_arch::default(),
        };
        self.fd
            .set_guest_debug(&dbg)
            .map_err(|e| Error::RunVcpu(self.idx, io(e)))?;
        Ok(())
    }

    /// Read the vCPU's general-purpose registers (for debug snapshots).
    pub fn get_regs(&self) -> Result<kvm_regs, Error> {
        self.fd
            .get_regs()
            .map_err(|e| Error::SetRegs(self.idx, io(e)))
    }

    /// Write the vCPU's general-purpose registers.
    pub fn set_regs(&self, regs: &kvm_regs) -> Result<(), Error> {
        self.fd
            .set_regs(regs)
            .map_err(|e| Error::SetRegs(self.idx, io(e)))
    }

    /// Read the vCPU's segment / system registers.
    #[cfg(target_arch = "x86_64")]
    pub fn get_sregs(&self) -> Result<kvm_bindings::kvm_sregs, Error> {
        self.fd
            .get_sregs()
            .map_err(|e| Error::GetSregs(self.idx, io(e)))
    }

    /// Write the vCPU's segment / system registers.
    #[cfg(target_arch = "x86_64")]
    pub fn set_sregs(&self, sregs: &kvm_bindings::kvm_sregs) -> Result<(), Error> {
        self.fd
            .set_sregs(sregs)
            .map_err(|e| Error::SetSregs(self.idx, io(e)))
    }

    /// Run the vCPU until the next exit. PIO and MMIO reads are
    /// handled inline via the supplied callbacks — the response is
    /// written into `kvm_run.io.data` / `kvm_run.mmio.data` before
    /// this call returns so the guest sees the value at its next
    /// instruction.
    ///
    /// `mmio_read(addr, &mut data)` returns `true` if the read was
    /// handled (`data` filled). `false` leaves the KVM-supplied
    /// zero-fill in place — equivalent to "device not present" /
    /// real-hardware all-zeros for unmapped MMIO.
    ///
    /// `KVM_RUN` returning `EAGAIN` is not a real failure — per
    /// `Documentation/virt/kvm/api.rst`, "the call may be retried." In
    /// particular, APs created with an in-kernel IRQchip start in
    /// `MP_STATE_UNINITIALIZED`; their `KVM_RUN` returns `EAGAIN` until the BSP
    /// delivers INIT+SIPI. Retry transparently.
    ///
    /// `EINTR` is returned to the caller so the supervisor can use a
    /// thread-directed signal to make blocked vCPUs observe shutdown.
    pub fn run(
        &mut self,
        pio_read: impl Fn(u16, u8) -> u32,
        mmio_read: impl Fn(u64, &mut [u8]) -> bool,
    ) -> Result<VmExit, Error> {
        let exit = loop {
            match self.fd.run() {
                Ok(e) => break e,
                Err(e) if e.errno() == nix::libc::EAGAIN => continue,
                Err(e) if e.errno() == nix::libc::EINTR => return Ok(VmExit::Interrupted),
                Err(e) => return Err(Error::RunVcpu(self.idx, io(e))),
            }
        };
        if matches!(exit, kvm_ioctls::VcpuExit::InternalError) {
            let run = self.fd.get_kvm_run();
            // SAFETY: kvm_run is kernel-mediated; union fields valid for the active variant.
            #[allow(unsafe_code)]
            let internal = unsafe { &run.__bindgen_anon_1.internal };
            return Ok(VmExit::Unknown(format!(
                "InternalError(suberror={:#x}, ndata={}, data={:x?})",
                internal.suberror,
                internal.ndata,
                &internal.data[..(internal.ndata as usize).min(internal.data.len())]
            )));
        }
        // Handle PIO read in-place: write the value the device returns
        // into kvm_run's io.data buffer before returning.
        if let kvm_ioctls::VcpuExit::IoIn(port, data) = exit {
            let size = data.len() as u8;
            let value = pio_read(port, size);
            for (i, slot) in data.iter_mut().enumerate() {
                *slot = ((value >> (8 * i)) & 0xFF) as u8;
            }
            return Ok(VmExit::PioRead { port, size });
        }
        // Handle MMIO read in-place.
        if let kvm_ioctls::VcpuExit::MmioRead(addr, data) = exit {
            let size = data.len() as u8;
            if !mmio_read(addr, data) {
                // Leave KVM's pre-zeroed buffer — matches real hardware
                // "no device" all-zeros on unmapped MMIO.
                for slot in data.iter_mut() {
                    *slot = 0;
                }
            }
            return Ok(VmExit::MmioRead { addr, size });
        }
        Ok(translate_exit(exit))
    }
}

impl AsRawFd for Vcpu {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.fd.as_raw_fd()
    }
}

#[cfg(target_arch = "x86_64")]
fn seg_from_pmi(s: &pmi::vm::vcpu::x86_64::SegReg) -> kvm_segment {
    // TEMPORARY: this translation matches what arma + tatu actually emit
    // (Intel VMCS access-rights layout, bits 12-15 for AVL/L/DB/G),
    // which diverges from `pmi/spec/vm.md` §2 (bits 8-11). See dillo
    // task P11: the spec MUST be the source of truth and either the
    // spec or the implementations need to change to agree. Revert this
    // translation to follow the spec text directly once that's resolved.
    //   bits 3:0  = type
    //   bit  4    = s
    //   bits 6:5  = dpl
    //   bit  7    = present
    //   bit  12   = avl
    //   bit  13   = l   (64-bit code segment)
    //   bit  14   = db
    //   bit  15   = g   (4 KiB granularity)
    let attr = s.attributes;
    kvm_segment {
        base: s.base,
        limit: s.limit,
        selector: s.selector,
        type_: (attr & 0xF) as u8,
        s: ((attr >> 4) & 0x1) as u8,
        dpl: ((attr >> 5) & 0x3) as u8,
        present: ((attr >> 7) & 0x1) as u8,
        avl: ((attr >> 12) & 0x1) as u8,
        l: ((attr >> 13) & 0x1) as u8,
        db: ((attr >> 14) & 0x1) as u8,
        g: ((attr >> 15) & 0x1) as u8,
        unusable: 0,
        padding: 0,
    }
}

fn translate_exit(exit: VcpuExit<'_>) -> VmExit {
    match exit {
        VcpuExit::IoIn(port, data) => VmExit::PioRead {
            port,
            size: data.len() as u8,
        },
        VcpuExit::IoOut(port, data) => {
            let mut buf = [0u8; 4];
            let n = data.len().min(4);
            buf[..n].copy_from_slice(&data[..n]);
            VmExit::PioWrite {
                port,
                data: buf,
                size: n as u8,
            }
        }
        VcpuExit::MmioRead(addr, data) => VmExit::MmioRead {
            addr,
            size: data.len() as u8,
        },
        VcpuExit::MmioWrite(addr, data) => {
            let mut buf = [0u8; 8];
            let n = data.len().min(8);
            buf[..n].copy_from_slice(&data[..n]);
            VmExit::MmioWrite {
                addr,
                data: buf,
                size: n as u8,
            }
        }
        VcpuExit::Hlt => VmExit::Halted,
        VcpuExit::Shutdown => VmExit::Shutdown,
        VcpuExit::Debug(_) => VmExit::Debug,
        VcpuExit::SystemEvent(_ty, _data) => VmExit::Shutdown,
        other => VmExit::Unknown(format!("{other:?}")),
    }
}
