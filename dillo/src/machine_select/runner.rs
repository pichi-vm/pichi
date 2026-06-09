//! VM-side integration crate for dillo.
//!
//! Orchestrates the remaining compatibility VM runner:
//! launch plan → memfd setup → backend wiring → vCPU thread launch →
//! MMIO/PIO dispatch.
//!
//! See `dillo/ARCHITECTURE.md` §7, §8, §10.1, §11, §12.

#![allow(clippy::needless_lifetimes)]

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
mod backend_select;
mod error;
// HVF guest-memory builder (KVM uses memfd, WHP uses GuestMemoryMmap).
#[cfg(target_os = "macos")]
mod hvf_devices;
// KVM/Linux-only submodules (memfd setup, gdb stub).
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod gdb;
#[cfg(target_os = "linux")]
mod memory;

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use std::sync::atomic::Ordering;
#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    target_os = "windows"
))]
use std::thread;

use anyhow::Result;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use backend_machine::Vm;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use backend_select::machine as backend_machine;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use dillo::pmi_parse::VcpuState;
#[cfg(target_os = "windows")]
use dillo::pmi_parse::VcpuState;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use dillo_machine::VcpuStop;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use dillo_mmio::Attach;
#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    target_os = "windows"
))]
use dillo_mmio::syscon;
#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    target_os = "windows"
))]
use dillo_pci::legacy_pio as pio_pci;
#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "aarch64"),
    target_os = "macos",
    target_os = "windows"
))]
use dillo_pci::{MsixInterruptAdapter, MsixNotifier};

pub(crate) use error::RunError;

/// One launch-derived RAM region passed in by the top-level `dillo` launcher.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RunRegion {
    pub(crate) gpa: u64,
    pub(crate) size: u64,
}

/// One launch-time write into guest RAM, already derived by `dillo`.
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct RunWrite {
    pub(crate) section: String,
    pub(crate) gpa: u64,
    pub(crate) data: Vec<u8>,
}

#[derive(Debug)]
struct RunMemoryPlan {
    memslots: Vec<RunRegion>,
    memory_nodes: Vec<RunRegion>,
}

/// Target-neutral launch facts already derived by `dillo`.
///
/// This is a compatibility handoff while the old runner is removed. It keeps
/// PMI parsing, DTB coverage, load cross-validation, memory placement, and
/// guest launch writes owned by the top-level launcher instead of duplicating
/// those decisions here.
#[derive(Debug)]
pub(crate) struct Preflight {
    parsed: dillo::pmi_parse::ParsedPmi,
    platform: dillo::platform::Machine,
    dtb: Vec<u8>,
    memslots: Vec<RunRegion>,
    memory_nodes: Vec<RunRegion>,
    guest_writes: Vec<RunWrite>,
}

#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    target_os = "windows"
))]
fn syscon_register(syscon: dillo::platform::Syscon) -> syscon::SysconRegister {
    syscon::SysconRegister {
        base: syscon.base,
        offset: syscon.offset,
        value: syscon.value,
        mask: syscon.mask,
    }
}

impl Preflight {
    pub(crate) fn new(
        parsed: dillo::pmi_parse::ParsedPmi,
        platform: dillo::platform::Machine,
        dtb: Vec<u8>,
        memslots: impl IntoIterator<Item = RunRegion>,
        memory_nodes: impl IntoIterator<Item = RunRegion>,
        guest_writes: impl IntoIterator<Item = RunWrite>,
    ) -> Self {
        Self {
            parsed,
            platform,
            dtb,
            memslots: memslots.into_iter().collect(),
            memory_nodes: memory_nodes.into_iter().collect(),
            guest_writes: guest_writes.into_iter().collect(),
        }
    }

    fn into_parts(
        self,
    ) -> (
        dillo::pmi_parse::ParsedPmi,
        dillo::platform::Machine,
        Vec<u8>,
        RunMemoryPlan,
        Vec<RunWrite>,
    ) {
        (
            self.parsed,
            self.platform,
            self.dtb,
            RunMemoryPlan {
                memslots: self.memslots,
                memory_nodes: self.memory_nodes,
            },
            self.guest_writes,
        )
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
#[cfg(all(test, target_os = "macos"))]
use dillo_mmio::MmioBus;
use dillo_mmio::{MappedSharedMemory, MmioWindow, SharedAccess};
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use dillo_pci::PciRoot;
#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "aarch64"),
    target_os = "macos",
    target_os = "windows"
))]
use dillo_pci_virtio::VirtioPciAdapter;
#[cfg(target_os = "windows")]
use vm_memory::{GuestAddress, GuestMemoryMmap};

/// Top-level VM-child entry point (Windows / Windows Hypervisor Platform).
///
/// This keeps the binary and workspace build linked through the normal
/// selected-runner boundary while the WHP memory/vCPU run path is filled in.
#[cfg(target_os = "windows")]
pub(crate) fn run(
    preflight: Preflight,
    vcpus: u32,
    supervisor_shutdown: &'static AtomicBool,
) -> Result<i32, RunError> {
    let (parsed, machine, _dtb, plan, guest_writes) = preflight.into_parts();
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
        .map_err(|e| RunError::MemfdSetup(anyhow::anyhow!("GuestMemoryMmap: {e}")))?;

    let mut vm = backend_machine::Vm::new_x86_64_with_local_apic_count(vcpus)?;
    Attach::attach(&mut vm, backend_machine::Memory::new(guest_mem.clone()))?;
    vm.set_shared_memory_capabilities(vec![Arc::new(MappedSharedMemory::for_guest_memory(
        guest_mem.clone(),
        SharedAccess::ReadWrite,
    ))]);

    apply_load_sections(&mut vm, &guest_writes)?;

    let cpu_profile = parsed.cpu_profile.as_str();
    let boot_state = match &parsed.vcpu {
        VcpuState::X86_64(state) => state,
        VcpuState::Aarch64(_) => return Err(RunError::ArchMismatch),
    };
    let legacy_pci = Arc::new(pio_pci::LegacyPciState::new());

    let msix_vectors: u16 = 3;
    let notifier = Arc::new(MsixInterruptAdapter::new(
        dillo_machine::Machine::create_message_interrupt_domain(&vm, msix_vectors)?,
    ));
    let lookup_notifier = Arc::clone(&notifier);
    let console: Arc<std::sync::Mutex<Box<dyn dillo_virtio::VirtioDevice>>> = Arc::new(
        std::sync::Mutex::new(Box::new(dillo_virtio_console::VirtioConsole::new(
            Arc::new(move |vector| lookup_notifier.interrupt_for(vector)),
        ))),
    );

    let bar0_gpa = machine.pcie.mmio_base;
    let bar2_gpa = machine.pcie.mmio_base + 0x1000;
    let virtio_pci_dev = dillo_pci_virtio::VirtioPciDevice::new(
        console,
        msix_vectors,
        bar0_gpa,
        bar2_gpa,
        Arc::clone(&notifier) as Arc<dyn MsixNotifier>,
    );
    let mut pci_root = PciRoot::new(MmioWindow {
        base: machine.pcie.ecam_base,
        size: machine.pcie.ecam_size,
    });
    pci_root.register(1, Box::new(VirtioPciAdapter::new(virtio_pci_dev)));
    let pci_root = Arc::new(pci_root);
    let shutdown = Arc::new(AtomicBool::new(false));
    let ioapic_region = machine
        .ioapic
        .ok_or(RunError::MissingRequiredDevice("/intc reg[1] ioapic"))?;
    let ioapic = Arc::new(backend_machine::IoApic::new(MmioWindow {
        base: ioapic_region.base,
        size: ioapic_region.size,
    }));
    let syscon_state = Arc::new(syscon::SysconState::default());
    match &machine.uart {
        Some(uart) => {
            let serial = dillo_mmio_uart::Ns16550::new(
                MmioWindow {
                    base: uart.base,
                    size: uart.size,
                },
                uart.reg_shift,
                Some(dillo_mmio::Interrupt::new(Arc::new(
                    vm.create_ioapic_interrupt_line(Arc::clone(&ioapic), uart.irq),
                ))),
                Box::new(std::io::stderr()),
            );
            Attach::attach(&mut vm, Arc::new(serial))?;
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
    Attach::attach(
        &mut vm,
        Arc::new(syscon::SysconDevice::new(
            syscon_register(poweroff),
            syscon::SysconAction::Poweroff,
            Arc::clone(&syscon_state),
        )),
    )?;
    if let Some(reboot) = machine.reboot {
        Attach::attach(
            &mut vm,
            Arc::new(syscon::SysconDevice::new(
                syscon_register(reboot),
                syscon::SysconAction::Reboot,
                Arc::clone(&syscon_state),
            )),
        )?;
    }
    Attach::attach(&mut vm, ioapic)?;
    let attachment = Attach::attach(&mut vm, Arc::clone(&pci_root))?;
    pci_root.set_attachment(attachment);

    let mut vcpu_handles = Vec::with_capacity(vcpus as usize);
    for idx in 0..vcpus {
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
        let legacy_for_write = Arc::clone(&legacy_pci);
        let pci_for_write = Arc::clone(&pci_root);
        let pio_write = Arc::new(move |port, data: &[u8]| {
            if (pio_pci::CF8_PORT..=pio_pci::CF8_PORT_END).contains(&port)
                || (pio_pci::CFC_PORT_BASE..=pio_pci::CFC_PORT_END).contains(&port)
            {
                pio_pci::pio_write(&legacy_for_write, &pci_for_write, port, data);
            }
        });
        let vcpu = Attach::attach(
            &mut vm,
            backend_machine::Cpu {
                idx,
                cpu_profile: cpu_profile.to_string(),
                pio_read,
                pio_write,
                state: (idx == 0).then(|| boot_state.clone()),
            },
        )?;
        vcpu_handles.push(vcpu);
    }
    log::info!(
        "WHP created {} vCPU(s); boot vCPU state programmed",
        vcpu_handles.len()
    );

    let mut joins = Vec::with_capacity(vcpu_handles.len());
    for mut vcpu in vcpu_handles {
        let shutdown_c = Arc::clone(&shutdown);
        let syscon_c = Arc::clone(&syscon_state);
        let exit_requester = vm.exit_requester();
        joins.push(thread::spawn(move || -> Result<RunOutcome> {
            let result =
                run_windows_vcpu_loop(&mut vcpu, &shutdown_c, &syscon_c, supervisor_shutdown);
            shutdown_c.store(true, Ordering::Release);
            if let Err(e) = exit_requester.request_vcpu_exit() {
                log::warn!("failed to cancel WHP vCPU run: {e}");
            }
            result
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
                    if let Err(e) = vm.request_vcpu_exit() {
                        log::warn!("failed to cancel WHP vCPU run: {e}");
                    }
                }
            }
            Ok(Err(e)) => {
                let msg = format!("{e:#}");
                log::error!("Windows/WHP vCPU thread error: {msg}");
                shutdown.store(true, Ordering::Release);
                if let Err(e) = vm.request_vcpu_exit() {
                    log::warn!("failed to cancel WHP vCPU run: {e}");
                }
                err = err.or(Some(RunError::VcpuThread(msg)));
            }
            Err(_) => {
                log::error!("Windows/WHP vCPU thread panicked");
                shutdown.store(true, Ordering::Release);
                if let Err(e) = vm.request_vcpu_exit() {
                    log::warn!("failed to cancel WHP vCPU run: {e}");
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
    vcpu: &mut backend_machine::Vcpu,
    shutdown: &Arc<AtomicBool>,
    syscon_state: &Arc<syscon::SysconState>,
    supervisor_shutdown: &AtomicBool,
) -> Result<RunOutcome> {
    let index = vcpu.index();
    let stop = vcpu.run_until_stop(|| {
        if shutdown.load(Ordering::Acquire) {
            return Some(VcpuStop::Stopped);
        }
        if let Some(action) = syscon_state.action() {
            return Some(action.vcpu_stop());
        }
        if supervisor_shutdown.load(Ordering::Acquire) {
            log::info!("vCPU {index}: supervisor shutdown observed");
            shutdown.store(true, Ordering::Release);
            return Some(VcpuStop::Stopped);
        }
        None
    })?;
    Ok(vcpu_stop_outcome(stop, shutdown))
}

/// Top-level VM-child entry point (macOS / Hypervisor.framework).
///
/// Parses the PMI, surveys the Machine, computes memory placement, creates the
/// HVF VM, maps + loads guest RAM, writes the host DTBO, builds the MMIO bus
/// (ns16550a serial + PCIe ECAM + virtio-console BARs), and runs all vCPUs (thread-per-vCPU
/// with userspace PSCI). A guest reboot warm-restarts in place; SYSTEM_OFF exits.
#[cfg(target_os = "macos")]
pub(crate) fn run(
    preflight: Preflight,
    vcpus: u32,
    _supervisor_shutdown: &'static AtomicBool,
) -> Result<i32, RunError> {
    let (parsed, machine, dtb, plan, guest_writes) = preflight.into_parts();
    log::info!(
        "PMI parsed: arch={:?}, {} actions, merged_dtb={}",
        parsed.arch,
        parsed.actions.len(),
        parsed.merged_dtb_section
    );
    log::info!(
        "coverage: base DTB fully claimed — {} declared region(s), pcie={}",
        machine.plan.regions().len(),
        machine.has_pcie
    );
    if machine.has_pcie {
        log::info!(
            "machine: pcie ecam {:#x}, mmio {:#x}",
            machine.pcie.ecam_base,
            machine.pcie.mmio_base,
        );
    } else {
        log::info!("machine: no PCIe (microVM)");
    }
    let total_backed: u64 = plan.memslots.iter().map(|r| r.size).sum();
    log::info!(
        "memslots: {} region(s), {} bytes",
        plan.memslots.len(),
        total_backed
    );
    log::info!(
        "HVF DTBO /memory nodes: {} region(s)",
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

    // 6. create the HVF VM from the DTB-derived platform substrate and map
    //    guest RAM. Backend-owned platform placement and the address-space
    //    watermark X (F7) come from the machine — never hardcoded. 2^X = the
    //    BAR window's burned-buddy top when PCIe is present, else enough bits
    //    to cover the device island.
    let mut vm = backend_machine::Vm::try_from(backend_machine::Config {
        dtb,
        min_addr_space_bits: machine.min_addr_space_bits(),
    })?;
    let max_vcpus = vm.max_vcpus()?;
    if vcpus > max_vcpus {
        return Err(RunError::TooManyVcpus {
            requested: vcpus,
            max: max_vcpus,
        });
    }
    for r in &plan.memslots {
        log::info!(
            "mapping HVF RAM [{:#x}..{:#x}) ({} bytes)",
            r.gpa,
            r.gpa + r.size,
            r.size
        );
        Attach::attach(&mut vm, backend_machine::Memory::new(r.gpa, r.size))?;
    }
    let guest_mem =
        hvf_devices::build_guest_memory(&vm.region_mappings()).map_err(RunError::MemfdSetup)?;
    vm.set_shared_memory_capabilities(vec![Arc::new(MappedSharedMemory::for_guest_memory(
        guest_mem.clone(),
        SharedAccess::ReadWrite,
    ))]);

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
            Attach::attach(
                &mut vm,
                Arc::new(dillo_mmio_uart::Ns16550::new(
                    MmioWindow {
                        base: uart.base,
                        size: uart.size,
                    },
                    uart.reg_shift,
                    None,
                    Box::new(std::io::stderr()),
                )),
            )?;
        }
        None => log::warn!("no UART in Machine — guest console output will be dropped"),
    }

    // 7b. PCIe (skipped on a --pci-slots 0 microVM): one virtio-console
    //     endpoint at 00:01.0 (slot 0 = host bridge). BAR0 = virtio config;
    //     BAR2 = MSI-X table + PBA. MSI-X is injected through the backend
    //     notifier. ECAM + each BAR register on the MMIO bus.
    if machine.has_pcie {
        let msix_vectors: u16 = 3; // 2 queues (rx/tx) + config-change vector
        let notifier = Arc::new(MsixInterruptAdapter::new(
            dillo_machine::Machine::create_message_interrupt_domain(&vm, msix_vectors)?,
        ));
        let lookup_notifier = Arc::clone(&notifier);
        let console: Arc<std::sync::Mutex<Box<dyn dillo_virtio::VirtioDevice>>> = Arc::new(
            std::sync::Mutex::new(Box::new(dillo_virtio_console::VirtioConsole::new(
                Arc::new(move |vector| lookup_notifier.interrupt_for(vector)),
            ))),
        );

        let bar0_gpa = machine.pcie.mmio_base;
        let bar2_gpa = machine.pcie.mmio_base + 0x1000;
        let virtio_pci_dev = dillo_pci_virtio::VirtioPciDevice::new(
            console,
            msix_vectors,
            bar0_gpa,
            bar2_gpa,
            Arc::clone(&notifier) as Arc<dyn MsixNotifier>,
        );
        let mut pci_root = PciRoot::new(MmioWindow {
            base: machine.pcie.ecam_base,
            size: machine.pcie.ecam_size,
        });
        pci_root.register(1, Box::new(VirtioPciAdapter::new(virtio_pci_dev)));
        let pci_root = Arc::new(pci_root);
        let attachment = Attach::attach(&mut vm, Arc::clone(&pci_root))?;
        pci_root.set_attachment(attachment);
    } // end: if platform.has_pcie (microVM with --pci-slots 0 skips PCI fabric)

    // 7c. virtio-mmio (F6): bind a virtio-console to the first transport slot
    //     so a microVM (no PCIe) still gets an hvc console. Remaining slots stay
    //     empty — the guest reads DeviceID 0 (unmapped MMIO ⇒ 0) and skips them.
    //     The wired interrupt is injected through a backend-owned capability.
    if let Some(slot) = machine.virtio_mmio.first() {
        let int_status = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let irq = dillo_mmio_virtio::WiredIrq::new(
            slot.irq,
            dillo_machine::Machine::create_line_interrupt(&vm, slot.irq)?,
        );
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
        let transport = Arc::new(dillo_mmio_virtio::VirtioMmio::new(
            MmioWindow {
                base: slot.base,
                size: slot.size,
            },
            console,
            Arc::clone(&int_status),
            irq.clone(),
        ));
        let attachment = Attach::attach(&mut vm, Arc::clone(&transport))?;
        transport.set_attachment(attachment);
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
    //    CPU_ON). A guest PSCI SYSTEM_RESET resets backend-owned run state and
    //    loops to a fresh boot image (Phase 2 in-VM restart); SYSTEM_OFF exits.
    //    `vm` (and its memory mappings) lives across the whole loop on this
    //    thread.
    loop {
        apply_load_sections(&mut vm, &guest_writes)?;
        log::info!(
            "macOS/HVF: {} memslot(s) mapped, load sections + DTBO written; boot vCPU0 pc={:#x}, launching {} vCPU(s)",
            plan.memslots.len(),
            boot_state.pc,
            vcpus,
        );
        match vcpu_stop_outcome(
            backend_machine::run_smp(vcpus, boot_state.clone(), Arc::clone(&mmio_bus))?,
            &AtomicBool::new(false),
        ) {
            RunOutcome::Exit(code) => {
                drop(vm); // unmap guest RAM only after all vCPU threads joined
                return Ok(code);
            }
            RunOutcome::Reboot => {
                log::info!("guest requested reboot — warm in-VM restart");
                dillo_machine::Machine::reset_for_reboot(&mut vm)?;
            }
        }
    }
}

/// (Re-)apply the launch-time guest writes into guest RAM. Idempotent —
/// re-running it refreshes the boot image for a warm reboot without zeroing the
/// rest of RAM.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn apply_load_sections(vm: &mut Vm, guest_writes: &[RunWrite]) -> Result<(), RunError> {
    for write in guest_writes {
        vm.write_guest(write.gpa, &write.data)?;
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

#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    target_os = "windows"
))]
trait SysconActionExt {
    fn vcpu_stop(self) -> VcpuStop;
}

#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    target_os = "windows"
))]
impl SysconActionExt for syscon::SysconAction {
    fn vcpu_stop(self) -> VcpuStop {
        match self {
            syscon::SysconAction::Poweroff => VcpuStop::GuestPoweroff,
            syscon::SysconAction::Reboot => VcpuStop::GuestReset,
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn vcpu_stop_outcome(stop: VcpuStop, shutdown: &AtomicBool) -> RunOutcome {
    match stop {
        VcpuStop::GuestPoweroff => {
            log::info!("guest poweroff observed by run loop");
            dillo_virtio_console::flush_output();
            shutdown.store(true, Ordering::Release);
            RunOutcome::Exit(0)
        }
        VcpuStop::GuestReset => {
            log::warn!("guest reboot observed; x86 warm reboot is not wired yet");
            shutdown.store(true, Ordering::Release);
            RunOutcome::Reboot
        }
        VcpuStop::Stopped => RunOutcome::Exit(0),
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
        let mut vm = Vm::try_from(backend_machine::Config {
            dtb: test_gic_dtb(),
            min_addr_space_bits: 36,
        })
        .expect("vm");
        let code_base = 0x4000_0000u64;
        Attach::attach(&mut vm, backend_machine::Memory::new(code_base, 0x1_0000)).expect("mem");
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
                base: serial_base,
                size: 0x1000,
            },
            0,
            None,
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
        let outcome = backend_machine::run_smp(1, state, Arc::new(std::sync::Mutex::new(mmio_bus)))
            .expect("run loop");
        drop(vm);
        assert!(
            matches!(outcome, VcpuStop::GuestPoweroff),
            "PSCI SYSTEM_OFF -> GuestPoweroff"
        );
    }

    fn test_gic_dtb() -> Vec<u8> {
        use devtree::{OwnedNode, OwnedProperty, OwnedTree};

        fn reg2(base: u64, size: u64) -> Vec<u32> {
            vec![
                (base >> 32) as u32,
                base as u32,
                (size >> 32) as u32,
                size as u32,
            ]
        }

        let mut intc_reg = reg2(0x0800_0000, 0x1_0000);
        intc_reg.extend(reg2(0x0810_0000, 0x200_0000));

        let root = OwnedNode::new("")
            .with_property(OwnedProperty::new("#address-cells").with_u32(2))
            .with_property(OwnedProperty::new("#size-cells").with_u32(2))
            .with_child(
                OwnedNode::new("interrupt-controller@8000000")
                    .with_property(OwnedProperty::new("compatible").with_str("arm,gic-v3"))
                    .with_property(OwnedProperty::new("#interrupt-cells").with_u32(3))
                    .with_property(OwnedProperty::new("interrupt-controller"))
                    .with_property(OwnedProperty::new("reg").with_u32s(&intc_reg))
                    .with_property(OwnedProperty::new("phandle").with_u32(1)),
            )
            .with_child(
                OwnedNode::new("msi-controller@a100000")
                    .with_property(OwnedProperty::new("compatible").with_str("arm,gic-v2m-frame"))
                    .with_property(OwnedProperty::new("msi-controller"))
                    .with_property(
                        OwnedProperty::new("reg").with_u32s(&reg2(0x0a10_0000, 0x1_0000)),
                    )
                    .with_property(OwnedProperty::new("arm,msi-base-spi").with_u32(64))
                    .with_property(OwnedProperty::new("arm,msi-num-spis").with_u32(32))
                    .with_property(OwnedProperty::new("phandle").with_u32(3)),
            );

        OwnedTree::new(root).encode().expect("test DTB")
    }
}

/// Top-level VM-child entry point (Linux / KVM).
///
/// Parses the PMI at `pmi_path`, sets up KVM, allocates memory, copies
/// load sections, synthesizes + writes the DTBO, spawns `vcpus`
/// vCPU threads, and runs until guest shutdown.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) fn run(
    preflight: Preflight,
    vcpus: u32,
    supervisor_shutdown: &'static AtomicBool,
) -> Result<i32, RunError> {
    let (parsed, machine, _dtb, plan, guest_writes) = preflight.into_parts();
    log::info!(
        "PMI parsed: arch={:?}, {} actions, merged_dtb={}",
        parsed.arch,
        parsed.actions.len(),
        parsed.merged_dtb_section
    );
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

    // ── 7. copy launch-time guest writes ───────────────────────────
    for write in &guest_writes {
        gpa_map
            .write(write.gpa, &write.data)
            .map_err(|source| RunError::SectionWrite {
                section: write.section.clone(),
                gpa: write.gpa,
                source,
            })?;
    }

    // ── 8. create KVM VM + memslots ────────────────────────────────
    let mut vm = backend_machine::Vm::new()?;
    for (slot_idx, r) in plan.memslots.iter().enumerate() {
        let host_addr = gpa_map
            .lookup(r.gpa)
            .ok_or_else(|| RunError::SectionWrite {
                section: format!("memslot[{slot_idx}]"),
                gpa: r.gpa,
                source: anyhow::anyhow!("no host mapping for GPA {:#x}", r.gpa),
            })?;
        log::info!(
            "registering KVM memslot {}: [{:#x}..{:#x}) host={:#x}",
            slot_idx,
            r.gpa,
            r.gpa + r.size,
            host_addr
        );
        Attach::attach(
            &mut vm,
            backend_machine::Memory::new(slot_idx as u32, r.gpa, host_addr, r.size),
        )?;
    }
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
    vm.set_shared_memory_capabilities(vec![Arc::new(MappedSharedMemory::for_guest_memory(
        guest_mem.clone(),
        SharedAccess::ReadWrite,
    ))]);

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
    Attach::attach(
        &mut vm,
        Arc::new(syscon::SysconDevice::new(
            syscon_register(poweroff),
            syscon::SysconAction::Poweroff,
            Arc::clone(&syscon_state),
        )),
    )?;
    if let Some(reboot) = machine.reboot {
        Attach::attach(
            &mut vm,
            Arc::new(syscon::SysconDevice::new(
                syscon_register(reboot),
                syscon::SysconAction::Reboot,
                Arc::clone(&syscon_state),
            )),
        )?;
    }

    // Build the device → adapter → bus chain.
    let irq_mgr = Arc::new(Mutex::new(vm.create_irq_manager().map_err(|e| {
        RunError::Kvm(backend_machine::Error::RunVcpu(
            0,
            std::io::Error::other(format!("irq manager: {e}")),
        ))
    })?));

    // Machine-driven UART attach (device-model §"Serial port"): the serial
    // port is an MMIO ns16550a. If the Machine declares one, attach it with a
    // KVM irqfd at the declared GSI and map its register window on the MMIO
    // bus. Absent → no UART emulation at all.
    match machine.uart {
        Some(uart) => {
            let eventfd = {
                let mut manager = irq_mgr.lock().expect("IRQ manager lock poisoned");
                manager
                    .register_irqfd_at_gsi(uart.irq)
                    .map_err(|e| RunError::SerialInit {
                        source: anyhow::anyhow!("irqfd for serial GSI {}: {e}", uart.irq),
                    })?
            };
            let serial = dillo_mmio_uart::Ns16550::new(
                MmioWindow {
                    base: uart.base,
                    size: uart.size,
                },
                uart.reg_shift,
                Some(dillo_mmio::Interrupt::new(Arc::new(
                    backend_machine::EventFdInterruptLine::new(eventfd),
                ))),
                Box::new(std::io::stdout()),
            );
            Attach::attach(&mut vm, Arc::new(serial))?;
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
    let irqfd_domain: Arc<dyn dillo_mmio::MessageInterruptDomain> = Arc::new(
        backend_machine::IrqfdNotifier::new(Arc::clone(&irq_mgr), msix_vectors),
    );
    let irqfd_notifier = Arc::new(MsixInterruptAdapter::new(Arc::clone(&irqfd_domain)));

    let console: Arc<std::sync::Mutex<Box<dyn dillo_virtio::VirtioDevice>>> = {
        let call_lookup_notifier = Arc::clone(&irqfd_notifier);
        Arc::new(std::sync::Mutex::new(Box::new(
            dillo_virtio_console::VirtioConsole::new(Arc::new(move |vector| {
                call_lookup_notifier.interrupt_for(vector)
            })),
        )))
    };

    // Pick a BAR layout inside the DTB-declared MMIO window. Two 4 KiB
    // BARs per device (BAR0 + BAR2); slot 1 is the first endpoint.
    let bar_window_base = machine.pcie.mmio_base;
    let bar0_gpa = bar_window_base + 0x0000;
    let bar2_gpa = bar_window_base + 0x1000;
    let virtio_pci_dev = dillo_pci_virtio::VirtioPciDevice::new(
        console,
        msix_vectors,
        bar0_gpa,
        bar2_gpa,
        irqfd_notifier as Arc<dyn MsixNotifier>,
    );

    let mut pci_root = PciRoot::new(MmioWindow {
        base: machine.pcie.ecam_base,
        size: machine.pcie.ecam_size,
    });
    pci_root.register(1, Box::new(VirtioPciAdapter::new(virtio_pci_dev)));
    let pci_root = Arc::new(pci_root);
    let attachment = Attach::attach(&mut vm, Arc::clone(&pci_root))?;
    pci_root.set_attachment(attachment);
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
        let legacy_for_write = Arc::clone(&legacy_pci);
        let pci_for_write = Arc::clone(&pci_root);
        let pio_write = Arc::new(move |port, data: &[u8]| {
            if (pio_pci::CF8_PORT..=pio_pci::CF8_PORT_END).contains(&port)
                || (pio_pci::CFC_PORT_BASE..=pio_pci::CFC_PORT_END).contains(&port)
            {
                pio_pci::pio_write(&legacy_for_write, &pci_for_write, port, data);
            }
        });
        let vcpu = Attach::attach(
            &mut vm,
            backend_machine::Cpu {
                idx,
                cpu_profile: cpu_profile.to_string(),
                pio_read,
                pio_write,
                state: (idx == 0).then(|| boot_state.clone()),
            },
        )?;
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
        let target = gdb::GdbTarget::new(
            vcpu0,
            gpa_arc,
            syscon_register(poweroff),
            Arc::clone(&shutdown),
        );
        gdb::run_loop(target, stream);
        return Ok(0);
    }

    let mut joins = Vec::with_capacity(vcpus as usize);
    for mut vcpu in vcpu_handles {
        let shutdown_c = Arc::clone(&shutdown);
        let syscon_c = Arc::clone(&syscon_state);
        let exit_requester = vm.exit_requester();
        let join = thread::spawn(move || -> Result<RunOutcome> {
            let result = run_vcpu_loop(&mut vcpu, &shutdown_c, &syscon_c, supervisor_shutdown);
            shutdown_c.store(true, Ordering::Release);
            exit_requester.request_vcpu_exit();
            result
        });
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
                    vm.request_vcpu_exit();
                }
            }
            Ok(Err(e)) => {
                let msg = format!("{e:#}");
                log::error!("vCPU thread error: {msg}");
                shutdown.store(true, Ordering::Release);
                vm.request_vcpu_exit();
                err = err.or(Some(RunError::VcpuThread(msg)));
            }
            Err(_panic) => {
                log::error!("vCPU thread panicked");
                shutdown.store(true, Ordering::Release);
                vm.request_vcpu_exit();
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

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
pub(crate) fn run(
    preflight: Preflight,
    vcpus: u32,
    supervisor_shutdown: &'static AtomicBool,
) -> Result<i32, RunError> {
    let (parsed, machine, dtb, plan, guest_writes) = preflight.into_parts();
    log::info!(
        "PMI parsed: arch={:?}, {} actions, merged_dtb={}",
        parsed.arch,
        parsed.actions.len(),
        parsed.merged_dtb_section
    );
    log::info!(
        "coverage: base DTB fully claimed — {} declared region(s), pcie={}",
        machine.plan.regions().len(),
        machine.has_pcie
    );
    let total_backed: u64 = plan.memslots.iter().map(|r| r.size).sum();
    log::info!("/memory@N nodes: {} region(s)", plan.memory_nodes.len());
    for r in &plan.memory_nodes {
        log::info!("  [{:#x}..{:#x}) ({} bytes)", r.gpa, r.gpa + r.size, r.size);
    }
    let host_ram = host_total_ram_bytes().unwrap_or(u64::MAX);
    let overhead = 256u64 << 20;
    if total_backed.saturating_add(overhead) > host_ram {
        return Err(RunError::HostRam {
            requested_mib: total_backed >> 20,
            overhead_mib: overhead >> 20,
            available_mib: host_ram >> 20,
        });
    }

    let memfd = memory::create_and_size(total_backed).map_err(RunError::MemfdSetup)?;
    let mut gpa_map = memory::GpaMap::new();
    let mut host_base: u64 = 0;
    for r in &plan.memslots {
        let host = memory::mmap_range(&memfd, host_base, r.size).map_err(RunError::Mmap)?;
        gpa_map.add(r.gpa, host, r.size);
        host_base += r.size;
    }
    for write in &guest_writes {
        gpa_map
            .write(write.gpa, &write.data)
            .map_err(|source| RunError::SectionWrite {
                section: write.section.clone(),
                gpa: write.gpa,
                source,
            })?;
    }

    let mut vm = backend_machine::Vm::try_from(backend_machine::Config { dtb })?;
    for (slot_idx, r) in plan.memslots.iter().enumerate() {
        let host_addr = gpa_map
            .lookup(r.gpa)
            .ok_or_else(|| RunError::SectionWrite {
                section: format!("memslot[{slot_idx}]"),
                gpa: r.gpa,
                source: anyhow::anyhow!("no host mapping for GPA {:#x}", r.gpa),
            })?;
        Attach::attach(
            &mut vm,
            backend_machine::Memory::new(slot_idx as u32, r.gpa, host_addr, r.size),
        )?;
    }
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
    vm.set_shared_memory_capabilities(vec![Arc::new(MappedSharedMemory::for_guest_memory(
        guest_mem.clone(),
        SharedAccess::ReadWrite,
    ))]);

    if let Some(uart) = machine.uart {
        let interrupt = dillo_machine::Machine::create_line_interrupt(&vm, uart.irq)?;
        Attach::attach(
            &mut vm,
            Arc::new(dillo_mmio_uart::Ns16550::new(
                MmioWindow {
                    base: uart.base,
                    size: uart.size,
                },
                uart.reg_shift,
                Some(interrupt),
                Box::new(std::io::stderr()),
            )),
        )?;
    }

    if machine.has_pcie {
        let msix_vectors: u16 = 3;
        let notifier = Arc::new(MsixInterruptAdapter::new(
            dillo_machine::Machine::create_message_interrupt_domain(&vm, msix_vectors)?,
        ));
        let lookup_notifier = Arc::clone(&notifier);
        let console: Arc<Mutex<Box<dyn dillo_virtio::VirtioDevice>>> = Arc::new(Mutex::new(
            Box::new(dillo_virtio_console::VirtioConsole::new(Arc::new(
                move |vector| lookup_notifier.interrupt_for(vector),
            ))),
        ));
        let virtio_pci_dev = dillo_pci_virtio::VirtioPciDevice::new(
            console,
            msix_vectors,
            machine.pcie.mmio_base,
            machine.pcie.mmio_base + 0x1000,
            Arc::clone(&notifier) as Arc<dyn MsixNotifier>,
        );
        let mut pci_root = PciRoot::new(MmioWindow {
            base: machine.pcie.ecam_base,
            size: machine.pcie.ecam_size,
        });
        pci_root.register(1, Box::new(VirtioPciAdapter::new(virtio_pci_dev)));
        let pci_root = Arc::new(pci_root);
        let attachment = Attach::attach(&mut vm, Arc::clone(&pci_root))?;
        pci_root.set_attachment(attachment);
    }

    if let Some(slot) = machine.virtio_mmio.first() {
        let int_status = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let irq = dillo_mmio_virtio::WiredIrq::new(
            slot.irq,
            dillo_machine::Machine::create_line_interrupt(&vm, slot.irq)?,
        );
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
        let transport = Arc::new(dillo_mmio_virtio::VirtioMmio::new(
            MmioWindow {
                base: slot.base,
                size: slot.size,
            },
            console,
            Arc::clone(&int_status),
            irq,
        ));
        let attachment = Attach::attach(&mut vm, Arc::clone(&transport))?;
        transport.set_attachment(attachment);
    }

    let boot_state = match &parsed.vcpu {
        VcpuState::Aarch64(state) => state.clone(),
        VcpuState::X86_64(_) => return Err(RunError::ArchMismatch),
    };
    let cpu_profile = parsed.cpu_profile.as_str();
    let mut created_vcpus = Vec::with_capacity(vcpus as usize);
    for idx in 0..vcpus {
        let vcpu = Attach::attach(
            &mut vm,
            backend_machine::Cpu {
                idx,
                cpu_profile: cpu_profile.to_string(),
                state: (idx == 0).then(|| boot_state.clone()),
            },
        )?;
        created_vcpus.push(vcpu);
    }
    dillo_machine::Machine::prepare_vcpu_run(&mut vm)?;

    let mut joins = Vec::with_capacity(vcpus as usize);
    let shutdown = Arc::new(AtomicBool::new(false));
    for mut vcpu in created_vcpus {
        let shutdown_c = Arc::clone(&shutdown);
        let exit_requester = vm.exit_requester();
        let join = std::thread::spawn(move || -> Result<RunOutcome> {
            let result = run_vcpu_loop_aarch64(&mut vcpu, &shutdown_c, supervisor_shutdown);
            shutdown_c.store(true, Ordering::Release);
            exit_requester.request_vcpu_exit();
            result
        });
        joins.push(join);
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
                    vm.request_vcpu_exit();
                }
            }
            Ok(Err(e)) => {
                let msg = format!("{e:#}");
                shutdown.store(true, Ordering::Release);
                vm.request_vcpu_exit();
                err = err.or(Some(RunError::VcpuThread(msg)));
            }
            Err(_) => {
                shutdown.store(true, Ordering::Release);
                vm.request_vcpu_exit();
                err = err.or(Some(RunError::VcpuPanic));
            }
        }
    }
    if let Some(e) = err {
        return Err(e);
    }
    match outcome {
        RunOutcome::Exit(code) => Ok(code),
        RunOutcome::Reboot => Ok(0),
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn run_vcpu_loop(
    vcpu: &mut backend_machine::Vcpu,
    shutdown: &Arc<AtomicBool>,
    syscon_state: &Arc<syscon::SysconState>,
    supervisor_shutdown: &AtomicBool,
) -> Result<RunOutcome> {
    let index = vcpu.index();
    let stop = vcpu.run_until_stop(|| {
        if shutdown.load(Ordering::Acquire) {
            return Some(VcpuStop::Stopped);
        }
        if let Some(action) = syscon_state.action() {
            return Some(action.vcpu_stop());
        }
        // §13.3: supervisor requested orderly shutdown.
        if supervisor_shutdown.load(Ordering::Acquire) {
            log::info!("vCPU {index}: supervisor shutdown observed");
            shutdown.store(true, Ordering::Release);
            return Some(VcpuStop::Stopped);
        }
        None
    })?;
    Ok(vcpu_stop_outcome(stop, shutdown))
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
fn run_vcpu_loop_aarch64(
    vcpu: &mut backend_machine::Vcpu,
    shutdown: &Arc<AtomicBool>,
    supervisor_shutdown: &AtomicBool,
) -> Result<RunOutcome> {
    let index = vcpu.index();
    let stop = vcpu.run_until_stop(|| {
        if shutdown.load(Ordering::Acquire) {
            return Some(VcpuStop::Stopped);
        }
        if supervisor_shutdown.load(Ordering::Acquire) {
            log::info!("vCPU {index}: supervisor shutdown observed");
            shutdown.store(true, Ordering::Release);
            return Some(VcpuStop::Stopped);
        }
        None
    })?;
    Ok(vcpu_stop_outcome(stop, shutdown))
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
