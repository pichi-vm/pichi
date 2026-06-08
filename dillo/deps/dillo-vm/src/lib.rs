//! VM-side integration crate for dillo.
//!
//! Orchestrates the remaining compatibility VM runner:
//! launch plan → memfd setup → backend wiring → vCPU thread launch →
//! MMIO/PIO dispatch.
//!
//! See `dillo/ARCHITECTURE.md` §7, §8, §10.1, §11, §12.

#![allow(clippy::needless_lifetimes)]

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
mod backend;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
mod backend_select;
mod cpu_id;
mod error;
mod fdt_writer;
mod overlay;
mod placement;
#[allow(dead_code)]
mod syscon;
// HVF MSI-X notifier + guest-memory builder (KVM uses memfd + irqfd instead).
#[cfg(target_os = "macos")]
mod hvf_devices;
// KVM/Linux-only submodules (memfd, vhost-user, gdb stub).
#[cfg(target_os = "linux")]
mod gdb;
#[cfg(target_os = "linux")]
mod memory;
#[cfg(target_os = "linux")]
mod vhost_frontend;
#[cfg(target_os = "windows")]
mod whp_devices;

#[cfg(target_os = "linux")]
pub use vhost_frontend::spawn_backend;

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use std::sync::atomic::Ordering;
#[cfg(any(target_os = "linux", target_os = "windows"))]
use std::thread;

use anyhow::Result;
#[cfg(target_os = "macos")]
use backend::BackendVm;
#[cfg(any(target_os = "linux", target_os = "windows"))]
use backend::{BackendVm, VcpuSeed};
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use backend_machine::Vm;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use backend_select::machine as backend_machine;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use dillo_machine::VcpuStop;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use dillo_pci::MsixNotifier;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use dillo_pmi::{Action as PmiAction, FillKind, VcpuState};
#[cfg(target_os = "windows")]
use dillo_pmi::{Action as PmiAction, FillKind, VcpuState};
#[cfg(any(target_os = "linux", target_os = "windows"))]
use dillo_x86::pio_pci;

pub use error::RunError;

/// One launch-derived RAM region passed in by the top-level `dillo` launcher.
#[derive(Debug, Clone, Copy)]
pub struct RunRegion {
    pub gpa: u64,
    pub size: u64,
}

/// Target-neutral launch facts already derived by `dillo`.
///
/// This is a compatibility handoff while the old runner is removed. It keeps
/// PMI parsing, DTB coverage, load cross-validation, and memory placement owned
/// by the top-level launcher instead of duplicating those decisions here.
#[derive(Debug)]
pub struct Preflight {
    bytes: Vec<u8>,
    parsed: dillo_pmi::ParsedPmi,
    platform: dillo_platform::Machine,
    memslots: Vec<RunRegion>,
    memory_nodes: Vec<RunRegion>,
}

impl Preflight {
    pub fn new(
        bytes: Vec<u8>,
        parsed: dillo_pmi::ParsedPmi,
        platform: dillo_platform::Machine,
        memslots: impl IntoIterator<Item = RunRegion>,
        memory_nodes: impl IntoIterator<Item = RunRegion>,
    ) -> Self {
        Self {
            bytes,
            parsed,
            platform,
            memslots: memslots.into_iter().collect(),
            memory_nodes: memory_nodes.into_iter().collect(),
        }
    }

    fn into_parts(
        self,
    ) -> (
        Vec<u8>,
        dillo_pmi::ParsedPmi,
        dillo_platform::Machine,
        placement::MemoryPlan,
    ) {
        let memslots = self
            .memslots
            .into_iter()
            .map(|r| placement::Region {
                gpa: r.gpa,
                size: r.size,
            })
            .collect();
        let memory_nodes = self
            .memory_nodes
            .into_iter()
            .map(|r| placement::Region {
                gpa: r.gpa,
                size: r.size,
            })
            .collect();
        (
            self.bytes,
            self.parsed,
            self.platform,
            placement::MemoryPlan {
                memslots,
                memory_nodes,
            },
        )
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
#[cfg(all(test, target_os = "macos"))]
use dillo_mmio::MmioBus;
use dillo_mmio::{MappedSharedMemory, MmioWindow, SharedAccess};
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
pub fn run(
    preflight: Preflight,
    vcpus: u32,
    supervisor_shutdown: &'static AtomicBool,
) -> Result<i32, RunError> {
    let (bytes, parsed, machine, plan) = preflight.into_parts();
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

    let mut vm = <backend_machine::Vm as BackendVm>::new(backend::VmOptions {
        vcpus,
        guest_memory: guest_mem.clone(),
    })?;
    vm.set_shared_memory_capabilities(vec![Arc::new(MappedSharedMemory::for_guest_memory(
        guest_mem.clone(),
        SharedAccess::ReadWrite,
    ))]);

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
    let virtio_pci_dev = dillo_pci_virtio::VirtioPciDevice::new(
        console,
        msix_vectors,
        bar0_gpa,
        bar2_gpa,
        Arc::clone(&notifier) as Arc<dyn MsixNotifier>,
    );
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
    let ioapic = Arc::new(dillo_x86::IoApic::new(MmioWindow {
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
    let attachment = vm.attach_mmio(Arc::clone(&pci_root))?;
    pci_root.set_attachment(attachment);

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
        let legacy_for_write = Arc::clone(&legacy_pci);
        let pci_for_write = Arc::clone(&pci_root);
        let pio_write = Arc::new(move |port, data: &[u8]| {
            if (pio_pci::CF8_PORT..=pio_pci::CF8_PORT_END).contains(&port)
                || (pio_pci::CFC_PORT_BASE..=pio_pci::CFC_PORT_END).contains(&port)
            {
                pio_pci::pio_write(&legacy_for_write, &pci_for_write, port, data);
            }
        });
        vcpu_handles.push(BackendVm::create_vcpu(
            &vm,
            idx,
            cpu_profile,
            seed,
            pio_read,
            pio_write,
        )?);
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
pub fn run(
    preflight: Preflight,
    vcpus: u32,
    _supervisor_shutdown: &'static AtomicBool,
) -> Result<i32, RunError> {
    let (bytes, parsed, machine, plan) = preflight.into_parts();
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
            "machine: pcie ecam {:#x}, mmio {:#x}, gic={}",
            machine.pcie.ecam_base,
            machine.pcie.mmio_base,
            machine.gic.is_some(),
        );
    } else {
        log::info!("machine: no PCIe (microVM), gic={}", machine.gic.is_some());
    }
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
    let gic_params = backend_machine::GicParams {
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
    let guest_mem = vm.guest_memory()?;
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
            vm.attach_mmio(Arc::new(dillo_mmio_uart::Ns16550::new(
                MmioWindow {
                    name: "ns16550a",
                    base: uart.base,
                    size: uart.size,
                },
                uart.reg_shift,
                None,
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
        let virtio_pci_dev = dillo_pci_virtio::VirtioPciDevice::new(
            console,
            msix_vectors,
            bar0_gpa,
            bar2_gpa,
            Arc::clone(&notifier) as Arc<dyn MsixNotifier>,
        );
        let mut pci_root = PciRoot::new(MmioWindow {
            name: "pcie-ecam",
            base: machine.pcie.ecam_base,
            size: machine.pcie.ecam_size,
        });
        pci_root.register(1, Box::new(VirtioPciAdapter::new(virtio_pci_dev)));
        let pci_root = Arc::new(pci_root);
        let attachment = vm.attach_mmio(Arc::clone(&pci_root))?;
        pci_root.set_attachment(attachment);
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
        let transport = Arc::new(dillo_mmio_virtio::VirtioMmio::new(
            MmioWindow {
                name: "virtio-mmio-console",
                base: slot.base,
                size: slot.size,
            },
            console,
            Arc::clone(&int_status),
            irq.clone(),
        ));
        let attachment = vm.attach_mmio(Arc::clone(&transport))?;
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
        let gic = backend_machine::GicParams {
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
}

/// Top-level VM-child entry point (Linux / KVM).
///
/// Parses the PMI at `pmi_path`, sets up KVM, allocates memory, copies
/// load sections, synthesizes + writes the DTBO, spawns `vcpus`
/// vCPU threads, and runs until guest shutdown.
#[cfg(target_os = "linux")]
pub fn run(
    preflight: Preflight,
    vcpus: u32,
    supervisor_shutdown: &'static AtomicBool,
) -> Result<i32, RunError> {
    let (bytes, parsed, machine, plan) = preflight.into_parts();
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
                source: anyhow::anyhow!("no host mapping for GPA {:#x}", r.gpa),
            })?;
        memslots.push(backend::Memslot {
            index: slot_idx as u32,
            gpa: r.gpa,
            host_addr,
            size: r.size,
        });
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
    let mut vm = <Vm as BackendVm>::new(backend::VmOptions { memslots })?;
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

    // Process-isolation: fork+exec the console backend as a separate
    // child and use the vhost-user proxy as the PCI device. The proxy
    // runs the full vhost-user handshake (set_owner/get_features) in
    // its constructor; the data plane (descriptor walking, stdout
    // writes) lives in the child after `activate()` shares memory and
    // queue events. Process-isolation must fail closed if the child
    // cannot be started or negotiated; substituting an in-process
    // console would change the configured device model.
    #[cfg(feature = "process-isolation-spawn")]
    let console: Arc<std::sync::Mutex<Box<dyn dillo_virtio::VirtioDevice>>> = {
        let notifier_for_frontend = Arc::clone(&irqfd_notifier);
        let (stream, child) =
            spawn_backend("console").map_err(|source| RunError::DeviceBackend {
                kind: "console",
                source: source.into(),
            })?;
        let frontend = vhost_frontend::VhostUserFrontend::new(
            stream,
            child,
            notifier_for_frontend,
            guest_mem.clone(),
        )
        .map_err(|source| RunError::DeviceBackend {
            kind: "console",
            source,
        })?;
        log::info!("process-isolation: vhost-user console backend wired");
        Arc::new(std::sync::Mutex::new(Box::new(frontend)))
    };

    #[cfg(not(feature = "process-isolation-spawn"))]
    let console: Arc<std::sync::Mutex<Box<dyn dillo_virtio::VirtioDevice>>> = {
        let call_lookup_notifier = Arc::clone(&irqfd_notifier);
        Arc::new(std::sync::Mutex::new(Box::new(
            dillo_virtio_console::VirtioConsole::new(Arc::new(move |vector| {
                call_lookup_notifier.interrupt_for_vector(vector)
            })),
        )))
    };

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

    let mut pci_root = PciRoot::new(MmioWindow {
        name: "pcie-ecam",
        base: machine.pcie.ecam_base,
        size: machine.pcie.ecam_size,
    });
    pci_root.register(1, Box::new(VirtioPciAdapter::new(virtio_pci_dev)));
    let pci_root = Arc::new(pci_root);
    let attachment = vm.attach_mmio(Arc::clone(&pci_root))?;
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
        let legacy_for_write = Arc::clone(&legacy_pci);
        let pci_for_write = Arc::clone(&pci_root);
        let pio_write = Arc::new(move |port, data: &[u8]| {
            if (pio_pci::CF8_PORT..=pio_pci::CF8_PORT_END).contains(&port)
                || (pio_pci::CFC_PORT_BASE..=pio_pci::CFC_PORT_END).contains(&port)
            {
                pio_pci::pio_write(&legacy_for_write, &pci_for_write, port, data);
            }
        });
        let vcpu = BackendVm::create_vcpu(&vm, idx, cpu_profile, seed, pio_read, pio_write)?;
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

#[cfg(target_os = "linux")]
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

fn read_section<'a>(bytes: &'a [u8], offset: u64, size: u64) -> &'a [u8] {
    let s = offset as usize;
    let e = s + size as usize;
    &bytes[s..e]
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
