//! VM-side integration crate for dillo.
//!
//! Orchestrates the VM child's work: PMI loader → Machine survey →
//! DTBO synthesis → memfd setup → KVM wiring → vCPU thread launch →
//! MMIO/PIO dispatch.
//!
//! See `dillo/ARCHITECTURE.md` §7, §8, §10.1, §11, §12.

#![allow(clippy::needless_lifetimes)]

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
mod backend;
mod cpu_id;
mod error;
mod fdt_writer;
mod overlay;
#[cfg(target_os = "linux")]
mod pci_notify;
mod placement;
#[allow(dead_code)]
mod syscon;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
#[cfg(target_os = "windows")]
mod uart;

// Userspace PSCI handling is the HVF/aarch64 path (KVM handles PSCI in-kernel).
#[cfg(target_os = "macos")]
mod psci;
// HVF MSI-X notifier + guest-memory builder (KVM uses memfd + irqfd instead).
#[cfg(target_os = "macos")]
mod hvf_devices;
// KVM/Linux-only submodules (memfd, irqfd, vhost-user, gdb stub).
#[cfg(target_os = "linux")]
mod gdb;
#[cfg(target_os = "windows")]
mod ioapic;
#[cfg(target_os = "linux")]
mod irq;
#[cfg(target_os = "linux")]
mod memory;
#[cfg(target_os = "linux")]
mod pci_irq;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod pio_pci;
#[cfg(target_os = "linux")]
mod vhost_frontend;
#[cfg(target_os = "windows")]
mod whp_devices;

#[cfg(target_os = "linux")]
pub use vhost_frontend::{VhostUserFrontend, spawn_backend};

use std::fs::File;
use std::io::Read;
#[cfg(target_os = "linux")]
use std::os::unix::thread::JoinHandleExt;
use std::path::Path;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use std::sync::atomic::Ordering;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use std::thread;

use anyhow::{Result, anyhow};
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use backend::{BackendVm, VcpuSeed};
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use dillo_machine_backend::Vm;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use dillo_machine_backend::VmExit;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use dillo_pci::MsixNotifier;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use dillo_pmi::{Action as PmiAction, FillKind, HostArch, ParseOptions, VcpuState};
#[cfg(target_os = "windows")]
use dillo_pmi::{Action as PmiAction, FillKind, HostArch, ParseOptions, VcpuState};

pub use error::RunError;

#[cfg(target_os = "linux")]
const VCPU_KICK_SIGNAL: nix::sys::signal::Signal = nix::sys::signal::Signal::SIGUSR1;

#[cfg(target_os = "linux")]
extern "C" fn vcpu_kick_signal_handler(_: libc::c_int) {}

#[cfg(target_os = "linux")]
struct VcpuKicker {
    threads: Vec<libc::pthread_t>,
}

#[cfg(target_os = "linux")]
impl VcpuKicker {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            threads: Vec::with_capacity(capacity),
        }
    }

    fn install_handler() {
        use std::sync::OnceLock;

        static INSTALLED: OnceLock<()> = OnceLock::new();
        INSTALLED.get_or_init(|| {
            let action = nix::sys::signal::SigAction::new(
                nix::sys::signal::SigHandler::Handler(vcpu_kick_signal_handler),
                nix::sys::signal::SaFlags::empty(),
                nix::sys::signal::SigSet::empty(),
            );
            // SAFETY: installs a trivial async-signal-safe handler for a process-local
            // wake signal used only to interrupt KVM_RUN.
            #[allow(unsafe_code)]
            unsafe {
                nix::sys::signal::sigaction(VCPU_KICK_SIGNAL, &action)
                    .expect("install vCPU kick signal handler");
            }
        });
    }

    fn push(&mut self, thread: libc::pthread_t) {
        self.threads.push(thread);
    }

    fn kick_all(&self) {
        for thread in &self.threads {
            // SAFETY: pthread_t values come from live JoinHandles for vCPU worker
            // threads. If a thread has already exited, pthread_kill returns an error
            // and there is nothing left to wake.
            #[allow(unsafe_code)]
            let rc = unsafe { libc::pthread_kill(*thread, VCPU_KICK_SIGNAL as libc::c_int) };
            if rc != 0 && rc != libc::ESRCH {
                log::warn!("failed to kick vCPU thread with signal: errno {rc}");
            }
        }
    }
}

/// Process-wide "supervisor wants us to shut down" flag — set by the
/// supervisor's signal watcher on 1st SIGINT/SIGTERM, polled by every
/// vCPU loop iteration. Implements ARCH §13.3: the supervisor asks,
/// dillo-vm exits 0 cleanly within the §13.2 grace period.
pub static SUPERVISOR_SHUTDOWN: AtomicBool = AtomicBool::new(false);

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
#[cfg(target_os = "macos")]
use dillo_mmio::MmioBus;
use dillo_mmio::MmioWindow;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use dillo_pci::PciRoot;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use dillo_pci_virtio::VirtioPciAdapter;
#[cfg(target_os = "windows")]
use vm_memory::{GuestAddress, GuestMemoryMmap};

/// Top-level VM-child entry point (Windows / Windows Hypervisor Platform).
///
/// This keeps the binary and workspace build linked through the normal
/// `dillo-vm` boundary while the WHP memory/vCPU run path is filled in.
#[cfg(target_os = "windows")]
pub fn run(pmi_path: &Path, memory_mib: u32, vcpus: u32) -> Result<i32, RunError> {
    let mut bytes = Vec::new();
    let mut f = File::open(pmi_path).map_err(|source| RunError::ReadPmi {
        path: pmi_path.display().to_string(),
        source,
    })?;
    f.read_to_end(&mut bytes)
        .map_err(|source| RunError::ReadPmi {
            path: pmi_path.display().to_string(),
            source,
        })?;

    let arch = host_arch();
    let parsed = dillo_pmi::parse(
        &bytes,
        &ParseOptions {
            host_arch: arch,
            memory_mib,
        },
    )?;
    validate_cpu_profile(parsed.cpu_profile.as_str(), arch)?;

    let dtb_info = parsed
        .sections
        .get(&parsed.merged_dtb_section)
        .ok_or_else(|| {
            RunError::DtboSynth(anyhow!("merged_dtb section missing from parsed.sections"))
        })?;
    let dtb_bytes = read_section(&bytes, dtb_info.file_offset, dtb_info.file_size);
    let platform_arch = match arch {
        HostArch::X86_64 => dillo_platform::Arch::X86_64,
        HostArch::Aarch64 => dillo_platform::Arch::Aarch64,
    };
    let machine =
        dillo_platform::Machine::survey(dtb_bytes, platform_arch).map_err(RunError::Coverage)?;
    log::info!(
        "WHP coverage: base DTB fully claimed — {} declared region(s), pcie={}",
        machine.plan.regions().len(),
        machine.has_pcie
    );
    if !machine.has_pcie {
        return Err(RunError::MissingRequiredDevice("/pcie"));
    }
    let poweroff = machine
        .poweroff
        .ok_or(RunError::MissingRequiredDevice("/syscon-poweroff"))?;
    log::info!(
        "WHP machine from DTB: pcie mmio {:#x}..{:#x}, ecam {:#x}..{:#x}, ioapic={:?}, poweroff @ {:#x}+{:#x} = {:#x} & {:#x}",
        machine.pcie.mmio_base,
        machine.pcie.mmio_base + machine.pcie.mmio_size,
        machine.pcie.ecam_base,
        machine.pcie.ecam_base + machine.pcie.ecam_size,
        machine.ioapic,
        poweroff.base,
        poweroff.offset,
        poweroff.value,
        poweroff.mask,
    );

    let load_ranges: Vec<(String, u64, u64)> = parsed
        .sections
        .iter()
        .map(|(n, s)| (n.clone(), s.gpa, s.virtual_size))
        .collect();
    machine
        .plan
        .cross_validate_loads(&load_ranges)
        .map_err(RunError::Coverage)?;

    let must_cover: Vec<(u64, u64)> = parsed
        .sections
        .values()
        .map(|s| (s.gpa, s.virtual_size))
        .collect();
    let plan = placement::plan_around_regions(&must_cover, memory_mib, machine.placement_regions())
        .map_err(|source| RunError::Placement {
            source: source.into(),
        })?;
    log::info!("WHP memory placement: {} memslot(s)", plan.memslots.len());
    for r in &plan.memslots {
        log::info!(
            "  RAM memslot [{:#x}..{:#x}) ({} bytes)",
            r.gpa,
            r.gpa + r.size,
            r.size
        );
    }
    log::info!(
        "WHP DTBO /memory nodes: {} region(s)",
        plan.memory_nodes.len()
    );
    for r in &plan.memory_nodes {
        log::info!(
            "  DTB memory node [{:#x}..{:#x}) ({} bytes)",
            r.gpa,
            r.gpa + r.size,
            r.size
        );
    }

    let ranges: Vec<(GuestAddress, usize)> = plan
        .memslots
        .iter()
        .map(|r| (GuestAddress(r.gpa), r.size as usize))
        .collect();
    let guest_mem: GuestMemoryMmap = GuestMemoryMmap::from_ranges(&ranges)
        .map_err(|e| RunError::MemfdSetup(anyhow!("GuestMemoryMmap: {e}")))?;

    let mut vm = <dillo_machine_backend::Vm as BackendVm>::new(backend::VmOptions {
        vcpus,
        guest_memory: guest_mem.clone(),
    })?;

    apply_load_sections(
        &mut vm,
        &parsed,
        &bytes,
        machine.arch,
        machine.psci.is_some(),
        &plan,
        vcpus,
    )?;

    let cpu_profile = parsed.cpu_profile.as_str();
    let boot_state = match &parsed.vcpu {
        VcpuState::X86_64(state) => state,
        VcpuState::Aarch64(_) => return Err(RunError::ArchMismatch),
    };
    let legacy_pci = Arc::new(pio_pci::LegacyPciState::new());

    let msix_vectors: u16 = 3;
    let notifier = vm.msix_notifier((), msix_vectors);
    let lookup_notifier = Arc::clone(&notifier);
    let console: Arc<std::sync::Mutex<Box<dyn dillo_virtio::VirtioDevice>>> = Arc::new(
        std::sync::Mutex::new(Box::new(dillo_virtio_console::VirtioConsole::new(
            Arc::new(move |vector| lookup_notifier.interrupt_for(vector)),
        ))),
    );

    let bar0_gpa = machine.pcie.mmio_base;
    let bar2_gpa = machine.pcie.mmio_base + 0x1000;
    let mut virtio_pci_dev = dillo_pci_virtio::VirtioPciDevice::new(
        console,
        msix_vectors,
        bar0_gpa,
        bar2_gpa,
        Arc::clone(&notifier) as Arc<dyn MsixNotifier>,
    );
    virtio_pci_dev.set_mem(guest_mem.clone());

    let mut pci_root = PciRoot::new(MmioWindow {
        name: "pcie-ecam",
        base: machine.pcie.ecam_base,
        size: machine.pcie.ecam_size,
    });
    pci_root.register(1, Box::new(VirtioPciAdapter::new(virtio_pci_dev)));
    let pci_root = Arc::new(pci_root);
    let shutdown = Arc::new(AtomicBool::new(false));
    let ioapic_region = machine
        .ioapic
        .ok_or(RunError::MissingRequiredDevice("/intc reg[1] ioapic"))?;
    let ioapic = Arc::new(ioapic::IoApic::new(MmioWindow {
        name: "ioapic",
        base: ioapic_region.base,
        size: ioapic_region.size,
    }));
    let syscon_state = Arc::new(syscon::SysconState::default());
    match &machine.uart {
        Some(uart) => {
            let serial = vm.ns16550(
                MmioWindow {
                    name: "ns16550a",
                    base: uart.base,
                    size: uart.size,
                },
                uart.reg_shift,
                (Arc::clone(&ioapic), uart.irq),
                Box::new(std::io::stderr()),
            )?;
            vm.attach_mmio(Arc::new(serial))?;
            log::info!(
                "serial: ns16550a @ {:#x} (size {:#x}, reg-shift {}, GSI {})",
                uart.base,
                uart.size,
                uart.reg_shift,
                uart.irq
            );
        }
        None => log::warn!("no UART in Machine — guest console output will be dropped"),
    }
    vm.attach_x86_syscon_devices(poweroff, machine.reboot, Arc::clone(&syscon_state))?;
    vm.attach_mmio(ioapic)?;
    vm.attach_mmio(Arc::clone(&pci_root))?;

    let mut vcpu_handles = Vec::with_capacity(vcpus as usize);
    for idx in 0..vcpus {
        let seed = if idx == 0 {
            VcpuSeed::X86_64Boot(boot_state)
        } else {
            VcpuSeed::X86_64Secondary
        };
        let legacy_for_read = Arc::clone(&legacy_pci);
        let pci_for_read = Arc::clone(&pci_root);
        let pio_read = Arc::new(move |port, size| {
            if (pio_pci::CF8_PORT..=pio_pci::CF8_PORT_END).contains(&port)
                || (pio_pci::CFC_PORT_BASE..=pio_pci::CFC_PORT_END).contains(&port)
            {
                pio_pci::pio_read(&legacy_for_read, &pci_for_read, port, size)
            } else {
                0
            }
        });
        vcpu_handles.push(BackendVm::create_vcpu(
            &vm,
            idx,
            cpu_profile,
            seed,
            pio_read,
        )?);
    }
    log::info!(
        "WHP created {} vCPU(s); boot vCPU state programmed",
        vcpu_handles.len()
    );

    let mut joins = Vec::with_capacity(vcpu_handles.len());
    let mut vcpu_cancels = Vec::with_capacity(vcpu_handles.len());
    for mut vcpu in vcpu_handles {
        vcpu_cancels.push(vcpu.cancel_handle());
        let shutdown_c = Arc::clone(&shutdown);
        let legacy_c = Arc::clone(&legacy_pci);
        let pci_c = Arc::clone(&pci_root);
        let syscon_c = Arc::clone(&syscon_state);
        joins.push(thread::spawn(move || -> Result<RunOutcome> {
            run_windows_vcpu_loop(&mut vcpu, &shutdown_c, &legacy_c, &pci_c, &syscon_c)
        }));
    }

    let mut err: Option<RunError> = None;
    let mut outcome = RunOutcome::Exit(0);
    for join in joins {
        match join.join() {
            Ok(Ok(thread_outcome)) => {
                if matches!(thread_outcome, RunOutcome::Reboot) {
                    outcome = RunOutcome::Reboot;
                }
                if shutdown.load(Ordering::Acquire) {
                    for cancel in &vcpu_cancels {
                        if let Err(e) = cancel.cancel() {
                            log::warn!("failed to cancel WHP vCPU run: {e}");
                        }
                    }
                }
            }
            Ok(Err(e)) => {
                let msg = format!("{e:#}");
                log::error!("Windows/WHP vCPU thread error: {msg}");
                shutdown.store(true, Ordering::Release);
                for cancel in &vcpu_cancels {
                    if let Err(e) = cancel.cancel() {
                        log::warn!("failed to cancel WHP vCPU run: {e}");
                    }
                }
                err = err.or(Some(RunError::VcpuThread(msg)));
            }
            Err(_) => {
                log::error!("Windows/WHP vCPU thread panicked");
                shutdown.store(true, Ordering::Release);
                for cancel in &vcpu_cancels {
                    if let Err(e) = cancel.cancel() {
                        log::warn!("failed to cancel WHP vCPU run: {e}");
                    }
                }
                err = err.or(Some(RunError::VcpuPanic));
            }
        }
    }
    if let Some(err) = err {
        return Err(err);
    }

    let _guest_mem = guest_mem;
    match outcome {
        RunOutcome::Exit(code) => Ok(code),
        RunOutcome::Reboot => {
            log::warn!("WHP guest reboot requested; exiting until x86 warm reboot is implemented");
            Ok(0)
        }
    }
}

#[cfg(target_os = "windows")]
fn run_windows_vcpu_loop(
    vcpu: &mut dillo_machine_backend::Vcpu,
    shutdown: &Arc<AtomicBool>,
    legacy_pci: &Arc<pio_pci::LegacyPciState>,
    pci_bus: &Arc<PciRoot>,
    syscon_state: &Arc<syscon::SysconState>,
) -> Result<RunOutcome> {
    let mut exit_count = 0u64;
    loop {
        if shutdown.load(Ordering::Acquire) {
            return Ok(RunOutcome::Exit(0));
        }
        if let Some(action) = syscon_state.action() {
            return Ok(action.run_outcome(shutdown));
        }
        if SUPERVISOR_SHUTDOWN.load(Ordering::Acquire) {
            log::info!("vCPU {}: supervisor shutdown observed", vcpu.index());
            shutdown.store(true, Ordering::Release);
            return Ok(RunOutcome::Exit(0));
        }

        let exit = vcpu.run()?;

        exit_count += 1;
        if exit_count <= 20 || exit_count % 1000 == 0 {
            log::debug!("Windows/WHP vCPU exit #{}: {:?}", exit_count, exit);
        }

        match exit {
            VmExit::PioWrite { port, data, size } => {
                // x86 serial is MMIO (ns16550a); only PCI config ports are PIO.
                if (pio_pci::CF8_PORT..=pio_pci::CF8_PORT_END).contains(&port)
                    || (pio_pci::CFC_PORT_BASE..=pio_pci::CFC_PORT_END).contains(&port)
                {
                    pio_pci::pio_write(legacy_pci, pci_bus, port, &data[..size as usize]);
                } else {
                    log::debug!(
                        "WHP PIO write to unmapped {:#x} (size {}, data {:02x?})",
                        port,
                        size,
                        &data[..size as usize],
                    );
                }
            }
            VmExit::PioRead { .. } => {}
            VmExit::MmioWrite { .. } => {}
            VmExit::MmioRead { .. } => {}
            VmExit::Interrupted => {}
            VmExit::Halted => {}
            VmExit::Shutdown => {
                log::warn!("guest shutdown via WHP shutdown exit");
                shutdown.store(true, Ordering::Release);
                return Ok(RunOutcome::Exit(0));
            }
            VmExit::Debug => {}
            VmExit::Hvc { args } | VmExit::Smc { args } => {
                log::warn!("unexpected HVC/SMC on WHP: args={args:?}");
            }
            VmExit::Unknown(reason) => {
                log::warn!("unknown WHP exit: {reason}");
                return Err(anyhow!("unknown WHP exit: {reason}"));
            }
        }
    }
}

/// Top-level VM-child entry point (macOS / Hypervisor.framework).
///
/// Parses the PMI, surveys the Machine, computes memory placement, creates the
/// HVF VM, maps + loads guest RAM, writes the host DTBO, builds the MMIO bus
/// (ns16550a serial + PCIe ECAM + virtio-console BARs), and runs all vCPUs (thread-per-vCPU
/// with userspace PSCI). A guest reboot warm-restarts in place; SYSTEM_OFF exits.
#[cfg(target_os = "macos")]
pub fn run(pmi_path: &Path, memory_mib: u32, vcpus: u32) -> Result<i32, RunError> {
    // 1. read PMI bytes.
    let mut bytes = Vec::new();
    let mut f = File::open(pmi_path).map_err(|source| RunError::ReadPmi {
        path: pmi_path.display().to_string(),
        source,
    })?;
    f.read_to_end(&mut bytes)
        .map_err(|source| RunError::ReadPmi {
            path: pmi_path.display().to_string(),
            source,
        })?;

    // 2. parse PMI with defensive caps.
    let arch = host_arch();
    let parsed = dillo_pmi::parse(
        &bytes,
        &ParseOptions {
            host_arch: arch,
            memory_mib,
        },
    )?;
    log::info!(
        "PMI parsed: arch={:?}, {} actions, merged_dtb={}",
        parsed.arch,
        parsed.actions.len(),
        parsed.merged_dtb_section
    );

    // 2b. Validate the cpu:profile shape against the machine architecture.
    //     Per spec/cpu.md a VMM MUST refuse a profile it does not recognize or
    //     that doesn't match the machine. (The per-mandatory-feature host floor
    //     check is tracked separately — see §J.)
    validate_cpu_profile(parsed.cpu_profile.as_str(), arch)?;

    // 3. survey Machine from base DTB + cross-validate loads vs MMIO.
    let dtb_info = parsed
        .sections
        .get(&parsed.merged_dtb_section)
        .ok_or_else(|| {
            RunError::DtboSynth(anyhow!("merged_dtb section missing from parsed.sections"))
        })?;
    let dtb_bytes = read_section(&bytes, dtb_info.file_offset, dtb_info.file_size);
    let platform_arch = match arch {
        HostArch::X86_64 => dillo_platform::Arch::X86_64,
        HostArch::Aarch64 => dillo_platform::Arch::Aarch64,
    };
    // Coverage gate (no undeclared hardware): the survey must claim EVERY node
    // and property in the base DTB, failing closed on any leftover — proving
    // the image declares nothing the VMM silently ignores, before realization.
    let machine =
        dillo_platform::Machine::survey(dtb_bytes, platform_arch).map_err(RunError::Coverage)?;
    log::info!(
        "coverage: base DTB fully claimed — {} declared region(s), pcie={}",
        machine.plan.regions().len(),
        machine.has_pcie
    );
    if machine.has_pcie {
        log::info!(
            "machine: pcie ecam {:#x}, mmio {:#x}, gic={}",
            machine.pcie.ecam_base,
            machine.pcie.mmio_base,
            machine.gic.is_some(),
        );
    } else {
        log::info!("machine: no PCIe (microVM), gic={}", machine.gic.is_some());
    }
    let load_ranges: Vec<(String, u64, u64)> = parsed
        .sections
        .iter()
        .map(|(n, s)| (n.clone(), s.gpa, s.virtual_size))
        .collect();
    machine
        .plan
        .cross_validate_loads(&load_ranges)
        .map_err(RunError::Coverage)?;

    // 4. compute memory placement.
    let must_cover: Vec<(u64, u64)> = parsed
        .sections
        .values()
        .map(|s| (s.gpa, s.virtual_size))
        .collect();
    let plan = placement::plan_around_regions(&must_cover, memory_mib, machine.placement_regions())
        .map_err(|source| RunError::Placement {
            source: source.into(),
        })?;
    let total_backed: u64 = plan.memslots.iter().map(|r| r.size).sum();
    log::info!(
        "memslots: {} region(s), {} bytes",
        plan.memslots.len(),
        total_backed
    );

    // 5. host-RAM pre-flight.
    let host_ram = host_total_ram_bytes().unwrap_or(u64::MAX);
    let overhead = 256u64 << 20;
    if total_backed.saturating_add(overhead) > host_ram {
        return Err(RunError::HostRam {
            requested_mib: total_backed >> 20,
            overhead_mib: overhead >> 20,
            available_mib: host_ram >> 20,
        });
    }

    // 6. create the HVF VM (in-kernel GICv3 from the DTB) and map guest RAM.
    //    GIC placement (F7a) and the address-space watermark X (F7) come from
    //    the machine — never hardcoded. 2^X = the BAR window's burned-buddy
    //    top when PCIe is present, else enough bits to cover the device island.
    let gic = machine
        .gic
        .as_ref()
        .ok_or_else(|| RunError::DtbExtract(dillo_platform::Error::MissingNode("GIC config")))?;
    let gic_params = dillo_machine_backend::GicParams {
        dist_base: gic.dist_base,
        redist_base: gic.redist_base,
        msi_base: gic.msi_frame_base,
        msi_intid_base: gic.spi_base,
        msi_intid_count: gic.spi_count,
    };
    let memory_regions = plan
        .memslots
        .iter()
        .map(|r| backend::MemoryRegion {
            gpa: r.gpa,
            size: r.size,
        })
        .collect();
    let mut vm = <Vm as BackendVm>::new(backend::VmOptions {
        gic_params,
        min_addr_space_bits: machine.min_addr_space_bits(),
        vcpus,
        memory_regions,
    })?;

    // 7. Attach MMIO devices once (reused across warm reboots): the ns16550a
    //    serial console (TX → stderr) here, then the PCIe ECAM + virtio-console BARs
    //    in 7b. aarch64 shutdown/reboot is PSCI (handled in the run loop), so
    //    there is no syscon device.
    match &machine.uart {
        Some(uart) => {
            log::info!(
                "registering ns16550a at {:#x} (size {:#x}, reg-shift {})",
                uart.base,
                uart.size,
                uart.reg_shift
            );
            vm.attach_mmio(Arc::new(dillo_mmio_uart::Ns16550::new(
                MmioWindow {
                    name: "ns16550a",
                    base: uart.base,
                    size: uart.size,
                },
                uart.reg_shift,
                dillo_mmio_uart::NoopTrigger,
                Box::new(std::io::stderr()),
            )))?;
        }
        None => log::warn!("no UART in Machine — guest console output will be dropped"),
    }

    // 7b. PCIe (skipped on a --pci-slots 0 microVM): one virtio-console
    //     endpoint at 00:01.0 (slot 0 = host bridge). BAR0 = virtio config;
    //     BAR2 = MSI-X table + PBA. MSI-X is injected through the backend
    //     notifier. ECAM + each BAR register on the MMIO bus.
    if machine.has_pcie {
        let msix_vectors: u16 = 3; // 2 queues (rx/tx) + config-change vector
        let notifier = vm.msix_notifier((), msix_vectors);
        let lookup_notifier = Arc::clone(&notifier);
        let console: Arc<std::sync::Mutex<Box<dyn dillo_virtio::VirtioDevice>>> = Arc::new(
            std::sync::Mutex::new(Box::new(dillo_virtio_console::VirtioConsole::new(
                Arc::new(move |vector| lookup_notifier.interrupt_for(vector)),
            ))),
        );

        let bar0_gpa = machine.pcie.mmio_base;
        let bar2_gpa = machine.pcie.mmio_base + 0x1000;
        let mut virtio_pci_dev = dillo_pci_virtio::VirtioPciDevice::new(
            console,
            msix_vectors,
            bar0_gpa,
            bar2_gpa,
            Arc::clone(&notifier) as Arc<dyn MsixNotifier>,
        );
        // No backend queue notifier on macOS; queue notifies kick directly.
        let guest_mem = vm.guest_memory()?;
        virtio_pci_dev.set_mem(guest_mem);

        let mut pci_root = PciRoot::new(MmioWindow {
            name: "pcie-ecam",
            base: machine.pcie.ecam_base,
            size: machine.pcie.ecam_size,
        });
        pci_root.register(1, Box::new(VirtioPciAdapter::new(virtio_pci_dev)));
        let pci_root = Arc::new(pci_root);
        vm.attach_mmio(Arc::clone(&pci_root))?;
    } // end: if platform.has_pcie (microVM with --pci-slots 0 skips PCI fabric)

    // 7c. virtio-mmio (F6): bind a virtio-console to the first transport slot
    //     so a microVM (no PCIe) still gets an hvc console. Remaining slots stay
    //     empty — the guest reads DeviceID 0 (unmapped MMIO ⇒ 0) and skips them.
    //     The wired GIC SPI is injected through a backend-owned IRQ capability.
    if let Some(slot) = machine.virtio_mmio.first() {
        let int_status = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let irq = vm.wired_irq(slot.irq);
        let interrupt_irq = irq.clone();
        let is = Arc::clone(&int_status);
        let console: Box<dyn dillo_virtio::VirtioDevice> = Box::new(
            dillo_virtio_console::VirtioConsole::new(Arc::new(move |_vector| {
                Some(dillo_mmio_virtio::VirtioMmio::interrupt(
                    Arc::clone(&is),
                    interrupt_irq.clone(),
                ))
            })),
        );
        let guest_mem = vm.guest_memory()?;
        let transport = Arc::new(dillo_mmio_virtio::VirtioMmio::new(
            MmioWindow {
                name: "virtio-mmio-console",
                base: slot.base,
                size: slot.size,
            },
            console,
            Arc::clone(&int_status),
            irq.clone(),
            guest_mem,
        ));
        vm.attach_mmio(transport)?;
        log::info!(
            "virtio-mmio console at {:#x} (SPI {}); {} slot(s) total",
            slot.base,
            irq.intid(),
            machine.virtio_mmio.len()
        );
    }

    let mmio_bus = vm.mmio_bus();

    let boot_state = match &parsed.vcpu {
        VcpuState::Aarch64(state) => state.clone(),
        VcpuState::X86_64(_) => return Err(RunError::ArchMismatch),
    };

    // 8. Warm-reboot loop. Each iteration (re-)applies the PMI load plan + DTBO
    //    into the persistent guest RAM and runs all vCPUs (each on its own
    //    thread; vCPU0 boots from the PMI state, secondaries park for PSCI
    //    CPU_ON). A guest PSCI SYSTEM_RESET resets the GIC and loops to a fresh
    //    boot image (Phase 2 in-VM restart); SYSTEM_OFF exits. `vm` (and its
    //    memory mappings) lives across the whole loop on this thread.
    loop {
        apply_load_sections(
            &mut vm,
            &parsed,
            &bytes,
            machine.arch,
            machine.psci.is_some(),
            &plan,
            vcpus,
        )?;
        log::info!(
            "macOS/HVF: {} memslot(s) mapped, load sections + DTBO written; boot vCPU0 pc={:#x}, launching {} vCPU(s)",
            plan.memslots.len(),
            boot_state.pc,
            vcpus,
        );
        match run_smp(vcpus, boot_state.clone(), Arc::clone(&mmio_bus))? {
            RunOutcome::Exit(code) => {
                drop(vm); // unmap guest RAM only after all vCPU threads joined
                return Ok(code);
            }
            RunOutcome::Reboot => {
                log::info!("guest requested reboot — warm in-VM restart");
                vm.reset_gic()?;
            }
        }
    }
}

/// (Re-)apply the PMI load plan into guest RAM: copy every `Load` section and
/// synthesize + write the merged DTBO. Idempotent — re-running it refreshes the
/// boot image for a warm reboot without zeroing the rest of RAM.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn apply_load_sections(
    vm: &mut Vm,
    parsed: &dillo_pmi::ParsedPmi,
    bytes: &[u8],
    arch: dillo_platform::Arch,
    psci_present: bool,
    plan: &placement::MemoryPlan,
    vcpus: u32,
) -> Result<(), RunError> {
    for action in &parsed.actions {
        match action {
            PmiAction::Load { section } => {
                let s = &parsed.sections[section];
                if s.file_size == 0 {
                    continue;
                }
                let src = read_section(bytes, s.file_offset, s.file_size);
                vm.write_guest(s.gpa, src)?;
            }
            PmiAction::Fill {
                section,
                kind: FillKind::MergedDtbo,
            } => {
                let s = &parsed.sections[section];
                let overlay_bytes = overlay::synthesize_dtbo(
                    &plan.memory_nodes,
                    vcpus,
                    psci_present.then_some("psci"),
                    cpu_id::host_cpu_compatible(arch),
                    s.virtual_size,
                )
                .map_err(RunError::DtboSynth)?;
                vm.write_guest(s.gpa, &overlay_bytes)?;
            }
        }
    }
    Ok(())
}

/// Outcome of a full `run_smp` invocation: the guest powered off (process exit
/// code) or requested a reboot (warm in-VM restart).
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
enum RunOutcome {
    Exit(i32),
    Reboot,
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
impl syscon::SysconAction {
    fn run_outcome(self, shutdown: &AtomicBool) -> RunOutcome {
        match self {
            syscon::SysconAction::Poweroff => {
                log::info!("guest syscon poweroff observed by run loop");
                dillo_virtio_console::flush_output();
                shutdown.store(true, Ordering::Release);
                RunOutcome::Exit(0)
            }
            syscon::SysconAction::Reboot => {
                log::warn!("guest syscon reboot observed; x86 warm reboot is not wired yet");
                shutdown.store(true, Ordering::Release);
                RunOutcome::Reboot
            }
        }
    }
}

/// Validate the `cpu:profile` *name* against the machine architecture
/// (spec/cpu.md): aarch64 profiles are `armvN.M-a`, x86-64 profiles are
/// the psABI levels `x86-64-v1` through `x86-64-v4`.
/// This enforces two of the spec's three MUST-refuse clauses — machine
/// mismatch and unrecognized profile. The third (each mandatory feature is
/// individually present on the host, per the Arm ARM / psABI) is a deeper
/// host-capability check tracked in §J: it requires the authoritative feature
/// tables, and the spec explicitly warns that a host's self-claimed revision
/// is not authoritative (e.g. Apple M4 claims Armv9 but omits SVE2).
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn validate_cpu_profile(profile: &str, arch: HostArch) -> Result<(), RunError> {
    let recognized = match arch {
        HostArch::Aarch64 => parse_armv_profile(profile).is_some(),
        HostArch::X86_64 => matches!(
            profile,
            "x86-64-v1" | "x86-64-v2" | "x86-64-v3" | "x86-64-v4"
        ),
    };
    if recognized {
        log::info!("cpu:profile {profile:?} recognized for {arch:?}");
        Ok(())
    } else {
        Err(RunError::UnknownCpuProfile(profile.to_string()))
    }
}

/// Parse an aarch64 `armvN.M-a` profile name into `(major, minor)`.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn parse_armv_profile(s: &str) -> Option<(u32, u32)> {
    let body = s.strip_prefix("armv")?.strip_suffix("-a")?;
    let (major, minor) = body.split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// PSCI return codes the SMP launcher writes into `x0`.
#[cfg(target_os = "macos")]
mod psci_ret {
    pub(crate) const SUCCESS: u64 = 0;
    pub(crate) const INVALID_PARAMETERS: u64 = (-2i64) as u64;
    pub(crate) const ALREADY_ON: u64 = (-4i64) as u64;
}

/// Per-vCPU power-on mailbox. A core parks on the condvar until another core's
/// PSCI `CPU_ON` deposits a `(entry, context)` request and notifies it.
#[cfg(target_os = "macos")]
struct CpuSlot {
    /// Started flag (for `ALREADY_ON`); set by the core that powers a target on.
    started: std::sync::atomic::AtomicBool,
    /// Pending power-on request: `Some((entry, context))`.
    request: std::sync::Mutex<Option<(u64, u64)>>,
    cv: std::sync::Condvar,
}

#[cfg(target_os = "macos")]
impl CpuSlot {
    fn new() -> Self {
        Self {
            started: std::sync::atomic::AtomicBool::new(false),
            request: std::sync::Mutex::new(None),
            cv: std::sync::Condvar::new(),
        }
    }

    /// Deposit a power-on request and wake the target's thread.
    fn deposit(&self, entry: u64, context: u64) {
        *self.request.lock().expect("cpu-slot poisoned") = Some((entry, context));
        self.cv.notify_all();
    }

    /// Block until a power-on request arrives, or `shutdown` is set. Returns the
    /// `(entry, context)`, or `None` if woken for shutdown. Uses a timed wait so
    /// a shutdown notify can never be lost.
    fn wait(&self, shutdown: &AtomicBool) -> Option<(u64, u64)> {
        let mut g = self.request.lock().expect("cpu-slot poisoned");
        loop {
            if let Some(req) = g.take() {
                return Some(req);
            }
            if shutdown.load(Ordering::SeqCst) {
                return None;
            }
            let (ng, _) = self
                .cv
                .wait_timeout(g, std::time::Duration::from_millis(100))
                .expect("cpu-slot poisoned");
            g = ng;
        }
    }
}

/// Initial register state for a secondary brought up via PSCI `CPU_ON`: it
/// enters at `entry` with `x0 = context`, MMU off (sctlr=0), at EL1h with
/// interrupts masked. `pstate`/`cpacr` mirror the boot vCPU so FP/SIMD doesn't
/// trap before the kernel configures it.
#[cfg(target_os = "macos")]
fn secondary_state(
    entry: u64,
    context: u64,
    boot: &pmi::vm::vcpu::aarch64::CpuState,
) -> pmi::vm::vcpu::aarch64::CpuState {
    pmi::vm::vcpu::aarch64::CpuState {
        pc: entry,
        x0: context,
        pstate: boot.pstate,
        cpacr_el1: boot.cpacr_el1,
        ..Default::default()
    }
}

/// MPIDR for vCPU `idx`: bit31 RES1, affinity Aff0 = `idx` (matches the host
/// overlay's `cpu@N { reg = N; }`, so a PSCI `CPU_ON` target resolves to `idx`).
#[cfg(target_os = "macos")]
fn mpidr_for(idx: usize) -> u64 {
    0x8000_0000 | (idx as u64)
}

/// Launch one thread per vCPU and run until guest shutdown. Returns the process
/// exit code (0 on PSCI `SYSTEM_OFF`). vCPU0 boots from `boot_state`;
/// secondaries park until a PSCI `CPU_ON` (issued by a running core) wakes them.
#[cfg(target_os = "macos")]
fn run_smp(
    vcpus: u32,
    boot_state: pmi::vm::vcpu::aarch64::CpuState,
    mmio_bus: Arc<std::sync::Mutex<MmioBus>>,
) -> Result<RunOutcome, RunError> {
    use dillo_machine_backend::VcpuHandle;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicI32;

    let n = vcpus.max(1) as usize;
    let shutdown = AtomicBool::new(false);
    let reboot = AtomicBool::new(false);
    let exit_code = AtomicI32::new(0);
    let slots: Vec<CpuSlot> = (0..n).map(|_| CpuSlot::new()).collect();
    let handles: Vec<Mutex<Option<VcpuHandle>>> = (0..n).map(|_| Mutex::new(None)).collect();

    thread::scope(|scope| {
        for idx in 0..n {
            let mmio = Arc::clone(&mmio_bus);
            let boot = boot_state.clone();
            let slots = &slots;
            let handles = &handles;
            let shutdown = &shutdown;
            let reboot = &reboot;
            let exit_code = &exit_code;
            scope.spawn(move || {
                if let Err(e) = vcpu_thread(
                    idx, n, boot, mmio, slots, handles, shutdown, reboot, exit_code,
                ) {
                    log::error!("vCPU{idx} thread error: {e}");
                }
                // First thread to return triggers shutdown for the rest: set the
                // flag, wake parked secondaries, and force running ones out of
                // run() so every thread observes it and the scope can join.
                shutdown.store(true, Ordering::SeqCst);
                for s in slots {
                    s.cv.notify_all();
                }
                let live: Vec<VcpuHandle> = handles
                    .iter()
                    .filter_map(|h| h.lock().expect("handle poisoned").clone())
                    .collect();
                let _ = dillo_machine_backend::force_vcpus_exit(&live);
            });
        }
    });

    if reboot.load(Ordering::SeqCst) {
        Ok(RunOutcome::Reboot)
    } else {
        Ok(RunOutcome::Exit(exit_code.load(Ordering::SeqCst)))
    }
}

/// One vCPU thread: create the (thread-bound) vCPU, bring it up, and run the
/// MMIO/PSCI loop until shutdown.
#[cfg(target_os = "macos")]
#[allow(clippy::too_many_arguments)]
fn vcpu_thread(
    idx: usize,
    n: usize,
    boot_state: pmi::vm::vcpu::aarch64::CpuState,
    mmio_bus: Arc<std::sync::Mutex<MmioBus>>,
    slots: &[CpuSlot],
    handles: &[std::sync::Mutex<Option<dillo_machine_backend::VcpuHandle>>],
    shutdown: &AtomicBool,
    reboot: &AtomicBool,
    exit_code: &std::sync::atomic::AtomicI32,
) -> Result<(), RunError> {
    // vCPU0 boots immediately; secondaries park until powered on.
    let init = if idx == 0 {
        slots[0].started.store(true, Ordering::SeqCst);
        boot_state.clone()
    } else {
        match slots[idx].wait(shutdown) {
            Some((entry, context)) => secondary_state(entry, context, &boot_state),
            None => return Ok(()), // shutdown before this core was ever powered on
        }
    };
    let vcpu = <Vm as BackendVm>::current_thread_vcpu(
        VcpuSeed::Aarch64 {
            mpidr: mpidr_for(idx),
            state: &init,
        },
        mmio_bus,
    )?;
    *handles[idx].lock().expect("handle poisoned") = Some(vcpu.handle());
    if idx != 0 {
        log::info!("vCPU{idx} powered on: pc={:#x}", init.pc);
    }

    loop {
        if shutdown.load(Ordering::SeqCst) {
            return Ok(());
        }
        match vcpu.run()? {
            VmExit::MmioRead { .. } | VmExit::MmioWrite { .. } => {}
            VmExit::Hvc { args } => match psci::dispatch(&args) {
                psci::PsciAction::SystemOff => {
                    log::info!("guest issued PSCI SYSTEM_OFF (vCPU{idx})");
                    // Drain the virtio-console output before teardown — a guest
                    // that writes its last line to hvc0 then immediately powers
                    // off would otherwise lose it to the async TX worker.
                    dillo_virtio_console::flush_output();
                    exit_code.store(0, Ordering::SeqCst);
                    return Ok(());
                }
                psci::PsciAction::SystemReset => {
                    // Warm in-VM restart: signal the launcher to re-apply the
                    // load plan + reset the GIC and run again (Phase 2).
                    log::info!("guest issued PSCI SYSTEM_RESET (vCPU{idx}) — warm reboot");
                    reboot.store(true, Ordering::SeqCst);
                    return Ok(());
                }
                psci::PsciAction::CpuOff => {
                    // This core powers off and re-parks; a later CPU_ON can
                    // bring it back. (Linux keeps cores online until shutdown,
                    // so this is rare in practice.)
                    log::info!("vCPU{idx} PSCI CPU_OFF — parking");
                    slots[idx].started.store(false, Ordering::SeqCst);
                    match slots[idx].wait(shutdown) {
                        Some((entry, context)) => {
                            let st = secondary_state(entry, context, &boot_state);
                            vcpu.set_aarch64_state(&st)?;
                        }
                        None => return Ok(()),
                    }
                }
                psci::PsciAction::CpuOn {
                    target,
                    entry,
                    context,
                } => {
                    // cpu@N { reg = N } ⇒ target affinity == N == thread index.
                    let tgt = (target & 0x00ff_ffff) as usize;
                    let code = if tgt >= n {
                        log::warn!("vCPU{idx} CPU_ON target={target:#x} out of range (n={n})");
                        psci_ret::INVALID_PARAMETERS
                    } else if slots[tgt].started.swap(true, Ordering::SeqCst) {
                        psci_ret::ALREADY_ON
                    } else {
                        log::info!("vCPU{idx} powers on vCPU{tgt} at pc={entry:#x}");
                        slots[tgt].deposit(entry, context);
                        psci_ret::SUCCESS
                    };
                    vcpu.set_gpr(0, code)?;
                }
                psci::PsciAction::Return(value) => {
                    vcpu.set_gpr(0, value)?;
                }
            },
            // A forced exit (vcpus_exit, from the shutdown broadcast) surfaces
            // as Unknown; if we're shutting down, just stop.
            VmExit::Unknown(_) if shutdown.load(Ordering::SeqCst) => return Ok(()),
            other => {
                let (esr, elr, far) = vcpu.el1_exception_state();
                log::warn!(
                    "vCPU{idx} unhandled exit: {other:?}; guest EL1 state at first exception: \
                     ESR_EL1={esr:#x} (EC={:#x}) ELR_EL1={elr:#x} FAR_EL1={far:#x}",
                    esr >> 26
                );
                return Err(RunError::UnknownKvmExit(format!("{other:?}")));
            }
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::*;

    /// Empirically verify the macOS run loop end-to-end against a tiny stub
    /// guest: `str w2,[x1]` writes a byte to the ns16550a data register (→ MMIO
    /// bus → stderr), then `hvc #0` with x0 = PSCI SYSTEM_OFF makes the loop
    /// return exit code 0. Requires the codesigned harness (hypervisor
    /// entitlement). Exercises the real MMIO-bus dispatch + PSCI handling.
    #[test]
    #[ignore = "requires a codesigned binary with com.apple.security.hypervisor; run via the codesigned harness with --ignored"]
    fn run_loop_ns16550_write_then_psci_off() {
        let gic = dillo_machine_backend::GicParams {
            dist_base: 0x8000000,
            redist_base: 0x8100000,
            msi_base: 0xa100000,
            msi_intid_base: 64,
            msi_intid_count: 32,
        };
        let mut vm = Vm::new(&gic, 36).expect("vm");
        let code_base = 0x4000_0000u64;
        vm.add_memory(code_base, 0x1_0000).expect("mem");
        // str w2,[x1] ; hvc #0
        let code = [0xB900_0022u32, 0xD400_0002u32];
        let mut bytes = Vec::new();
        for w in code {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        vm.write_guest(code_base, &bytes).expect("write");

        let serial_base = 0x0A11_0000u64;
        let mut mmio_bus = MmioBus::new();
        mmio_bus.register_device(Arc::new(dillo_mmio_uart::Ns16550::new(
            MmioWindow {
                name: "ns16550a",
                base: serial_base,
                size: 0x1000,
            },
            0,
            dillo_mmio_uart::NoopTrigger,
            Box::new(std::io::stderr()),
        )));

        // GPRs are carried in the boot state (set_aarch64_state programs
        // x0..x30): x1=serial THR, x2='h', x0=PSCI SYSTEM_OFF.
        let state = pmi::vm::vcpu::aarch64::CpuState {
            pc: code_base,
            pstate: 0x3c5,
            sctlr_el1: 0,
            x0: 0x8400_0008,
            x1: serial_base,
            x2: u64::from(b'h'),
            ..Default::default()
        };

        // Run via the production single-/multi-vCPU launcher (vcpus = 1). It
        // creates the vCPU on its own thread, so `vm` must outlive the call.
        let outcome =
            run_smp(1, state, Arc::new(std::sync::Mutex::new(mmio_bus))).expect("run loop");
        drop(vm);
        assert!(
            matches!(outcome, RunOutcome::Exit(0)),
            "PSCI SYSTEM_OFF → Exit(0)"
        );
    }

    /// The PSCI bring-up bookkeeping is HVF-independent, so test it under plain
    /// `cargo test`: a deposited request wakes a waiting core; `started` gates
    /// `ALREADY_ON`; affinity maps to the thread index.
    #[test]
    fn cpu_slot_deposit_wakes_waiter() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        assert_eq!(mpidr_for(0), 0x8000_0000);
        assert_eq!(mpidr_for(3), 0x8000_0003);
        // CPU_ON target affinity resolves to the thread index.
        assert_eq!((mpidr_for(2) & 0x00ff_ffff) as usize, 2);

        let slot = Arc::new(CpuSlot::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        let s2 = Arc::clone(&slot);
        let sd2 = Arc::clone(&shutdown);
        let waiter = thread::spawn(move || s2.wait(&sd2));
        // Deposit a request; the waiter must observe exactly it.
        slot.deposit(0x4000_0000, 0xABCD);
        assert_eq!(waiter.join().unwrap(), Some((0x4000_0000, 0xABCD)));

        // A shutdown wakes a parked core with None.
        let slot = Arc::new(CpuSlot::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        let s2 = Arc::clone(&slot);
        let sd2 = Arc::clone(&shutdown);
        let waiter = thread::spawn(move || s2.wait(&sd2));
        sd2_store_and_wake(&shutdown, &slot);
        assert_eq!(waiter.join().unwrap(), None);
    }

    fn sd2_store_and_wake(shutdown: &std::sync::atomic::AtomicBool, slot: &CpuSlot) {
        shutdown.store(true, Ordering::SeqCst);
        slot.cv.notify_all();
    }

    #[test]
    fn cpu_profile_name_validation() {
        use dillo_pmi::HostArch;
        // Recognized aarch64 / x86 profile names.
        assert!(validate_cpu_profile("armv8.2-a", HostArch::Aarch64).is_ok());
        assert!(validate_cpu_profile("armv9.0-a", HostArch::Aarch64).is_ok());
        assert!(validate_cpu_profile("x86-64-v3", HostArch::X86_64).is_ok());
        assert!(validate_cpu_profile("x86-64-v4", HostArch::X86_64).is_ok());
        // Machine mismatch (x86 name on aarch64 machine, and vice versa).
        assert!(validate_cpu_profile("x86-64-v3", HostArch::Aarch64).is_err());
        assert!(validate_cpu_profile("armv8.2-a", HostArch::X86_64).is_err());
        // Unrecognized forms.
        assert!(validate_cpu_profile("armv8-a", HostArch::Aarch64).is_err());
        assert!(validate_cpu_profile("x86-64-v5", HostArch::X86_64).is_err());
        assert!(validate_cpu_profile("nonsense", HostArch::Aarch64).is_err());
        assert_eq!(parse_armv_profile("armv8.2-a"), Some((8, 2)));
        assert_eq!(parse_armv_profile("armv9.4-a"), Some((9, 4)));
        assert_eq!(parse_armv_profile("armv8-a"), None);
    }
}

/// Top-level VM-child entry point (Linux / KVM).
///
/// Parses the PMI at `pmi_path`, sets up KVM, allocates memory, copies
/// load sections, synthesizes + writes the DTBO, spawns `vcpus`
/// vCPU threads, and runs until guest shutdown.
#[cfg(target_os = "linux")]
pub fn run(pmi_path: &Path, memory_mib: u32, vcpus: u32) -> Result<i32, RunError> {
    // ── 1. read PMI bytes ──────────────────────────────────────────
    let mut bytes = Vec::new();
    let mut f = File::open(pmi_path).map_err(|source| RunError::ReadPmi {
        path: pmi_path.display().to_string(),
        source,
    })?;
    f.read_to_end(&mut bytes)
        .map_err(|source| RunError::ReadPmi {
            path: pmi_path.display().to_string(),
            source,
        })?;

    // ── 2. parse PMI with defensive caps ───────────────────────────
    let arch = host_arch();
    let parsed = dillo_pmi::parse(
        &bytes,
        &ParseOptions {
            host_arch: arch,
            memory_mib,
        },
    )?;
    log::info!(
        "PMI parsed: arch={:?}, {} actions, merged_dtb={}",
        parsed.arch,
        parsed.actions.len(),
        parsed.merged_dtb_section
    );

    // ── 3. survey Machine from base DTB ───────────────────────────
    let dtb_info = parsed
        .sections
        .get(&parsed.merged_dtb_section)
        .ok_or_else(|| {
            RunError::DtboSynth(anyhow!("merged_dtb section missing from parsed.sections"))
        })?;
    let dtb_bytes = read_section(&bytes, dtb_info.file_offset, dtb_info.file_size);
    let platform_arch = match arch {
        HostArch::X86_64 => dillo_platform::Arch::X86_64,
        HostArch::Aarch64 => dillo_platform::Arch::Aarch64,
    };
    let machine =
        dillo_platform::Machine::survey(dtb_bytes, platform_arch).map_err(RunError::Coverage)?;
    log::info!(
        "coverage: base DTB fully claimed — {} declared region(s), pcie={}",
        machine.plan.regions().len(),
        machine.has_pcie
    );
    if !machine.has_pcie {
        return Err(RunError::MissingRequiredDevice("/pcie"));
    }
    let poweroff = machine
        .poweroff
        .ok_or(RunError::MissingRequiredDevice("/syscon-poweroff"))?;
    log::info!(
        "machine: pcie@{:#x} (ecam {:#x}), ioapic={:?}, poweroff @ {:#x}+{:#x} = {:#x} & {:#x}",
        machine.pcie.mmio_base,
        machine.pcie.ecam_base,
        machine.ioapic,
        poweroff.base,
        poweroff.offset,
        poweroff.value,
        poweroff.mask,
    );

    // Cross-validate load GPAs vs MMIO declared by DTB.
    let load_ranges: Vec<(String, u64, u64)> = parsed
        .sections
        .iter()
        .map(|(n, s)| (n.clone(), s.gpa, s.virtual_size))
        .collect();
    machine
        .plan
        .cross_validate_loads(&load_ranges)
        .map_err(RunError::Coverage)?;

    // ── 4. compute memory placement ────────────────────────────────
    let must_cover: Vec<(u64, u64)> = parsed
        .sections
        .values()
        .map(|s| (s.gpa, s.virtual_size))
        .collect();
    let plan = placement::plan_around_regions(&must_cover, memory_mib, machine.placement_regions())
        .map_err(|source| RunError::Placement {
            source: source.into(),
        })?;
    log::info!("memslots: {} region(s)", plan.memslots.len());
    for r in &plan.memslots {
        log::info!("  [{:#x}..{:#x}) ({} bytes)", r.gpa, r.gpa + r.size, r.size);
    }
    log::info!("/memory@N nodes: {} region(s)", plan.memory_nodes.len());
    for r in &plan.memory_nodes {
        log::info!("  [{:#x}..{:#x}) ({} bytes)", r.gpa, r.gpa + r.size, r.size);
    }

    // ── 5. host-RAM pre-flight ─────────────────────────────────────
    let host_ram = host_total_ram_bytes().unwrap_or(u64::MAX);
    let total_backed: u64 = plan.memslots.iter().map(|r| r.size).sum();
    let overhead = 256u64 << 20;
    if total_backed.saturating_add(overhead) > host_ram {
        return Err(RunError::HostRam {
            requested_mib: total_backed >> 20,
            overhead_mib: overhead >> 20,
            available_mib: host_ram >> 20,
        });
    }

    // ── 6. memfd + mmap ────────────────────────────────────────────
    let memfd = memory::create_and_size(total_backed).map_err(RunError::MemfdSetup)?;
    let mut gpa_map = memory::GpaMap::new();
    let mut host_base: u64 = 0;
    for r in &plan.memslots {
        let host = memory::mmap_range(&memfd, host_base, r.size).map_err(RunError::Mmap)?;
        gpa_map.add(r.gpa, host, r.size);
        host_base += r.size;
    }

    // ── 7. copy load sections + write DTBO ─────────────────────────
    for action in &parsed.actions {
        match action {
            PmiAction::Load { section } => {
                let s = &parsed.sections[section];
                if s.file_size == 0 {
                    continue; // Zero shape — already zeroed by memfd ftruncate
                }
                let src = read_section(&bytes, s.file_offset, s.file_size);
                gpa_map
                    .write(s.gpa, src)
                    .map_err(|source| RunError::SectionWrite {
                        section: section.clone(),
                        gpa: s.gpa,
                        source,
                    })?;
            }
            PmiAction::Fill {
                section,
                kind: FillKind::MergedDtbo,
            } => {
                let s = &parsed.sections[section];
                let overlay_bytes = overlay::synthesize_dtbo(
                    &plan.memory_nodes,
                    vcpus,
                    machine.psci.is_some().then_some("psci"),
                    cpu_id::host_cpu_compatible(machine.arch),
                    s.virtual_size,
                )
                .map_err(RunError::DtboSynth)?;
                gpa_map
                    .write(s.gpa, &overlay_bytes)
                    .map_err(|source| RunError::DtboWrite {
                        section: section.clone(),
                        gpa: s.gpa,
                        source,
                    })?;
            }
        }
    }

    // ── 8. create KVM VM + memslots ────────────────────────────────
    let mut memslots = Vec::with_capacity(plan.memslots.len());
    for (slot_idx, r) in plan.memslots.iter().enumerate() {
        let host_addr = gpa_map
            .lookup(r.gpa)
            .ok_or_else(|| RunError::SectionWrite {
                section: format!("memslot[{slot_idx}]"),
                gpa: r.gpa,
                source: anyhow!("no host mapping for GPA {:#x}", r.gpa),
            })?;
        memslots.push(backend::Memslot {
            index: slot_idx as u32,
            gpa: r.gpa,
            host_addr,
            size: r.size,
        });
    }
    let mut vm = <Vm as BackendVm>::new(backend::VmOptions { memslots })?;

    // ── 8.5. build PCI bus + virtio-console + MMIO dispatch ────────
    //
    // The kernel's PCIe enumeration walks the ECAM range declared by
    // the base DTB; we register a single virtio-console device at
    // 00:01.0 (slot 0 is the host bridge). Its BAR0 (virtio config /
    // notify / ISR / device-config) and BAR2 (MSI-X table + PBA) get
    // independently registered with the MMIO bus so guest accesses
    // route directly to the transport.
    //
    // MSI-X uses a backend notifier: on each MSI-X table write the guest does,
    // a fresh backend interrupt route is allocated. Queue completions are then
    // backend-direct — no VMM relay.
    let syscon_state = Arc::new(syscon::SysconState::default());
    vm.attach_x86_syscon_devices(poweroff, machine.reboot, Arc::clone(&syscon_state))?;

    // Build the device → adapter → bus chain.
    let irq_mgr = vm.interrupt_state()?;

    // Machine-driven UART attach (device-model §"Serial port"): the serial
    // port is an MMIO ns16550a. If the Machine declares one, attach it with a
    // KVM irqfd at the declared GSI and map its register window on the MMIO
    // bus. Absent → no UART emulation at all.
    match machine.uart {
        Some(uart) => {
            let serial = vm.ns16550(
                MmioWindow {
                    name: "ns16550a",
                    base: uart.base,
                    size: uart.size,
                },
                uart.reg_shift,
                (Arc::clone(&irq_mgr), uart.irq),
                Box::new(std::io::stdout()),
            )?;
            vm.attach_mmio(Arc::new(serial))?;
            log::info!(
                "serial: ns16550a @ {:#x} (size {:#x}, reg-shift {}, GSI {})",
                uart.base,
                uart.size,
                uart.reg_shift,
                uart.irq
            );
        }
        None => log::warn!("no UART in Machine — guest console output will be dropped"),
    }

    // num_queues + 1 vector for config-change. Console has 2 queues.
    let msix_vectors: u16 = 3;
    let irqfd_notifier = vm.msix_notifier(Arc::clone(&irq_mgr), msix_vectors);

    let call_lookup_notifier = Arc::clone(&irqfd_notifier);

    // Process-isolation: fork+exec the console backend as a separate
    // child and use the vhost-user proxy as the PCI device. The proxy
    // runs the full vhost-user handshake (set_owner/get_features) in
    // its constructor; the data plane (descriptor walking, stdout
    // writes) lives in the child after `activate()` shares memory and
    // queue events. Falls back to the in-process device if the spawn
    // fails so a missing/unreadable /proc/self/exe doesn't crash boot.
    #[cfg(feature = "process-isolation-spawn")]
    let console: Arc<std::sync::Mutex<Box<dyn dillo_virtio::VirtioDevice>>> = {
        let notifier_for_frontend = Arc::clone(&irqfd_notifier);
        match spawn_backend("console") {
            Ok((stream, child)) => {
                match VhostUserFrontend::new(stream, child, notifier_for_frontend) {
                    Ok(frontend) => {
                        log::info!("process-isolation: vhost-user console backend wired");
                        Arc::new(std::sync::Mutex::new(Box::new(frontend)))
                    }
                    Err(e) => {
                        log::warn!(
                            "process-isolation: vhost-user handshake failed ({e}); \
                         falling back to in-process console"
                        );
                        Arc::new(std::sync::Mutex::new(Box::new(
                            dillo_virtio_console::VirtioConsole::new(Arc::new(move |vector| {
                                call_lookup_notifier.get_irqfd_for_vector(vector)
                            })),
                        )))
                    }
                }
            }
            Err(e) => {
                log::warn!(
                    "process-isolation: spawn_backend failed ({e}); \
                     falling back to in-process console"
                );
                Arc::new(std::sync::Mutex::new(Box::new(
                    dillo_virtio_console::VirtioConsole::new(Arc::new(move |vector| {
                        call_lookup_notifier.get_irqfd_for_vector(vector)
                    })),
                )))
            }
        }
    };

    #[cfg(not(feature = "process-isolation-spawn"))]
    let console: Arc<std::sync::Mutex<Box<dyn dillo_virtio::VirtioDevice>>> = Arc::new(
        std::sync::Mutex::new(Box::new(dillo_virtio_console::VirtioConsole::new(
            Arc::new(move |vector| call_lookup_notifier.get_irqfd_for_vector(vector)),
        ))),
    );

    // Pick a BAR layout inside the DTB-declared MMIO window. Two 4 KiB
    // BARs per device (BAR0 + BAR2); slot 1 is the first endpoint.
    let bar_window_base = machine.pcie.mmio_base;
    let bar0_gpa = bar_window_base + 0x0000;
    let bar2_gpa = bar_window_base + 0x1000;
    let mut virtio_pci_dev = dillo_pci_virtio::VirtioPciDevice::new(
        console,
        msix_vectors,
        bar0_gpa,
        bar2_gpa,
        irqfd_notifier as Arc<dyn MsixNotifier>,
    );
    virtio_pci_dev.set_queue_notifier(vm.queue_notifier());
    // Build a vm-memory view over our memfd regions so virtio-pci can
    // access queues / descriptors when the guest activates the device.
    let region_tuples: Vec<(u64, u64, u64)> = plan
        .memslots
        .iter()
        .map(|r| {
            let host = gpa_map.lookup(r.gpa).expect("memslot has host mapping");
            (r.gpa, host, r.size)
        })
        .collect();
    let guest_mem =
        memory::build_guest_memory(&memfd, &region_tuples).map_err(RunError::MemfdSetup)?;
    virtio_pci_dev.set_mem(guest_mem);

    let mut pci_root = PciRoot::new(MmioWindow {
        name: "pcie-ecam",
        base: machine.pcie.ecam_base,
        size: machine.pcie.ecam_size,
    });
    pci_root.register(1, Box::new(VirtioPciAdapter::new(virtio_pci_dev)));
    let pci_root = Arc::new(pci_root);
    vm.attach_mmio(Arc::clone(&pci_root))?;
    let legacy_pci = Arc::new(pio_pci::LegacyPciState::new());

    // ── 9. create vCPUs + set boot vCPU state ──────────────────────
    let mut vcpu_handles = Vec::with_capacity(vcpus as usize);
    let cpu_profile = parsed.cpu_profile.as_str();
    let boot_state = match &parsed.vcpu {
        VcpuState::X86_64(state) => state,
        VcpuState::Aarch64(_) => {
            return Err(RunError::ArchMismatch);
        }
    };
    for idx in 0..vcpus {
        let seed = if idx == 0 {
            VcpuSeed::X86_64Boot(boot_state)
        } else {
            VcpuSeed::X86_64Secondary
        };
        let legacy_for_read = Arc::clone(&legacy_pci);
        let pci_for_read = Arc::clone(&pci_root);
        let pio_read = Arc::new(move |port, size| {
            if (pio_pci::CF8_PORT..=pio_pci::CF8_PORT_END).contains(&port)
                || (pio_pci::CFC_PORT_BASE..=pio_pci::CFC_PORT_END).contains(&port)
            {
                pio_pci::pio_read(&legacy_for_read, &pci_for_read, port, size)
            } else {
                0
            }
        });
        let vcpu = BackendVm::create_vcpu(&vm, idx, cpu_profile, seed, pio_read)?;
        vcpu_handles.push(vcpu);
    }

    // ── 10. spawn vCPU threads + dispatch loop ─────────────────────
    let shutdown = Arc::new(AtomicBool::new(false));

    // gdb mode: take vCPU 0, ignore the rest, hand it to the gdb stub.
    // Useful for debugging tatu / early-boot Linux without rebuilding
    // dillo. See `gdb` module for the protocol details.
    if let Ok(port_str) = std::env::var("DILLO_GDB") {
        let port: u16 = port_str.parse().map_err(|source| RunError::GdbPort {
            value: port_str.clone(),
            source,
        })?;
        let mut handles = vcpu_handles.into_iter();
        let vcpu0 = handles.next().expect("at least one vCPU was created above");
        let n_skipped = handles.count();
        if n_skipped > 0 {
            log::warn!(
                "DILLO_GDB set: only vCPU 0 will run; the other {n_skipped} vCPU(s) are inert"
            );
        }
        let stream = gdb::wait_for_gdb(port).map_err(|e| RunError::VcpuThread(e.to_string()))?;
        let gpa_arc = Arc::new(gpa_map);
        let target = gdb::GdbTarget::new(vcpu0, gpa_arc, poweroff, Arc::clone(&shutdown));
        gdb::run_loop(target, stream);
        return Ok(0);
    }

    VcpuKicker::install_handler();

    let mut joins = Vec::with_capacity(vcpus as usize);
    let mut vcpu_kicker = VcpuKicker::with_capacity(vcpus as usize);
    for mut vcpu in vcpu_handles {
        let shutdown_c = Arc::clone(&shutdown);
        let legacy_c = Arc::clone(&legacy_pci);
        let pci_c = Arc::clone(&pci_root);
        let syscon_c = Arc::clone(&syscon_state);
        let join = thread::spawn(move || -> Result<RunOutcome> {
            run_vcpu_loop(&mut vcpu, &shutdown_c, &legacy_c, &pci_c, &syscon_c)
        });
        vcpu_kicker.push(join.as_pthread_t());
        joins.push(join);
    }

    // ── 11. wait for shutdown ──────────────────────────────────────
    let mut err: Option<RunError> = None;
    let mut outcome = RunOutcome::Exit(0);
    for j in joins {
        match j.join() {
            Ok(Ok(thread_outcome)) => {
                if matches!(thread_outcome, RunOutcome::Reboot) {
                    outcome = RunOutcome::Reboot;
                }
                if shutdown.load(Ordering::Acquire) {
                    vcpu_kicker.kick_all();
                }
            }
            Ok(Err(e)) => {
                let msg = format!("{e:#}");
                log::error!("vCPU thread error: {msg}");
                shutdown.store(true, Ordering::Release);
                vcpu_kicker.kick_all();
                err = err.or(Some(RunError::VcpuThread(msg)));
            }
            Err(_panic) => {
                log::error!("vCPU thread panicked");
                shutdown.store(true, Ordering::Release);
                vcpu_kicker.kick_all();
                err = err.or(Some(RunError::VcpuPanic));
            }
        }
    }
    if let Some(e) = err {
        return Err(e);
    }
    match outcome {
        RunOutcome::Exit(code) => Ok(code),
        RunOutcome::Reboot => {
            log::warn!("KVM guest reboot requested; exiting until x86 warm reboot is implemented");
            Ok(0)
        }
    }
}

#[cfg(target_os = "linux")]
fn run_vcpu_loop(
    vcpu: &mut dillo_machine_backend::Vcpu,
    shutdown: &Arc<AtomicBool>,
    legacy_pci: &Arc<pio_pci::LegacyPciState>,
    pci_bus: &Arc<PciRoot>,
    syscon_state: &Arc<syscon::SysconState>,
) -> Result<RunOutcome> {
    let mut exit_count = 0u64;
    loop {
        if shutdown.load(Ordering::Acquire) {
            return Ok(RunOutcome::Exit(0));
        }
        if let Some(action) = syscon_state.action() {
            return Ok(action.run_outcome(shutdown));
        }
        // §13.3: supervisor requested orderly shutdown.
        if SUPERVISOR_SHUTDOWN.load(Ordering::Acquire) {
            log::info!("vCPU {}: supervisor shutdown observed", vcpu.index());
            shutdown.store(true, Ordering::Release);
            return Ok(RunOutcome::Exit(0));
        }
        let exit = vcpu.run()?;
        exit_count += 1;
        if exit_count <= 20 || exit_count % 1000 == 0 {
            log::debug!("vCPU exit #{}: {:?}", exit_count, exit);
        }
        match exit {
            VmExit::Debug => {
                // Single-step / breakpoint exit. The non-gdb run loop
                // never enables guest_debug, so reaching this branch
                // means stale state; ignore.
            }
            VmExit::PioWrite { port, data, size } => {
                // x86 serial is MMIO (ns16550a); only PCI config ports are PIO.
                if (pio_pci::CF8_PORT..=pio_pci::CF8_PORT_END).contains(&port)
                    || (pio_pci::CFC_PORT_BASE..=pio_pci::CFC_PORT_END).contains(&port)
                {
                    pio_pci::pio_write(legacy_pci, pci_bus, port, &data[..size as usize]);
                }
            }
            VmExit::PioRead { .. } => {
                // Handled inline in vcpu.run via pio_read callback above.
            }
            VmExit::MmioWrite { .. } => {}
            VmExit::MmioRead { .. } => {
                // Handled inline in vcpu.run via mmio_read callback above.
            }
            VmExit::Halted => {}
            VmExit::Shutdown => {
                log::warn!(
                    "guest shutdown via KVM_EXIT_SHUTDOWN (triple fault on x86, PSCI SYSTEM_OFF on aarch64) — \
                     not a syscon-poweroff write. Attach gdb (DILLO_GDB=<port>) to inspect."
                );
                shutdown.store(true, Ordering::Release);
                return Ok(RunOutcome::Exit(0));
            }
            VmExit::Interrupted => {}
            VmExit::Hvc { args } | VmExit::Smc { args } => {
                log::warn!("unhandled HVC/SMC: args={args:?}");
            }
            VmExit::Unknown(reason) => {
                log::warn!("unknown KVM exit: {reason}");
                return Err(anyhow!("unknown KVM exit: {reason}"));
            }
        }
    }
}

fn read_section<'a>(bytes: &'a [u8], offset: u64, size: u64) -> &'a [u8] {
    let s = offset as usize;
    let e = s + size as usize;
    &bytes[s..e]
}

fn host_arch() -> HostArch {
    #[cfg(target_arch = "x86_64")]
    {
        HostArch::X86_64
    }
    #[cfg(target_arch = "aarch64")]
    {
        HostArch::Aarch64
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        compile_error!("dillo only supports x86_64 and aarch64 hosts");
    }
}

#[cfg(target_os = "linux")]
fn host_total_ram_bytes() -> Option<u64> {
    // sysconf is a pure FFI query of kernel-resident counters; no aliasing,
    // no allocation, no callbacks. The unsafe block exists because libc
    // declares all syscalls as unsafe.
    #[allow(unsafe_code)]
    let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
    #[allow(unsafe_code)]
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if pages <= 0 || page_size <= 0 {
        return None;
    }
    Some((pages as u64) * (page_size as u64))
}

// macOS host-RAM query (sysctl HW_MEMSIZE) is a TODO (§F); returning None
// skips the pre-flight check rather than blocking boot. Safe: the check is
// a gross-misuse guard, not a correctness requirement.
#[cfg(target_os = "macos")]
fn host_total_ram_bytes() -> Option<u64> {
    None
}
