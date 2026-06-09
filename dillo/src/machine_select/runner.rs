//! VM-side integration crate for dillo.
//!
//! The top-level launcher composes DTB-derived portable devices with the
//! selected machine through the common `dillo-machine` trait API.

mod error;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use super::machine as selected_machine;
use dillo_devtree::platform::Machine as PlatformMachine;
use dillo_devtree::platform::{MsiParentage, WiredInterrupt};
use dillo_machine::{
    BootVcpuState, Cpu as MachineCpu, CpuState as MachineCpuState, LaunchConfig, Machine,
    Memory as MachineMemory, RamRange, VcpuStop,
};
use dillo_mmio::syscon;
use dillo_mmio::{
    Attach, InterruptSource, MessageInterruptSource, MmioAttachment, MmioInterruptRequirement,
    MmioWindow,
};
use dillo_pci::PciRoot;
use dillo_pci_virtio::VirtioPciAdapter;

pub(crate) use error::RunError;

/// One launch-derived RAM region passed in by the top-level `dillo` launcher.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RunRegion {
    pub(crate) gpa: u64,
    pub(crate) size: u64,
}

/// One launch-time write into guest RAM, already derived by `dillo`.
#[derive(Debug)]
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

impl RunMemoryPlan {
    fn ram_ranges(&self) -> Vec<RamRange> {
        self.memslots
            .iter()
            .map(|range| RamRange {
                gpa: range.gpa,
                size: range.size,
            })
            .collect()
    }
}

/// Target-neutral launch facts already derived by `dillo`.
#[derive(Debug)]
pub(crate) struct Preflight {
    parsed: dillo::pmi_parse::ParsedPmi,
    platform: PlatformMachine,
    dtb: Vec<u8>,
    memslots: Vec<RunRegion>,
    memory_nodes: Vec<RunRegion>,
    guest_writes: Vec<RunWrite>,
}

impl Preflight {
    pub(crate) fn new(
        parsed: dillo::pmi_parse::ParsedPmi,
        platform: PlatformMachine,
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
        PlatformMachine,
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

#[derive(Debug)]
struct SupervisorControl {
    supervisor_shutdown: &'static AtomicBool,
    syscon_state: Option<Arc<syscon::SysconState>>,
}

impl SupervisorControl {
    fn stop_requested(&self) -> Option<VcpuStop> {
        if let Some(state) = &self.syscon_state {
            match state.action() {
                Some(syscon::SysconAction::Poweroff) => return Some(VcpuStop::GuestPoweroff),
                Some(syscon::SysconAction::Reboot) => return Some(VcpuStop::GuestReset),
                None => {}
            }
        }
        self.supervisor_shutdown
            .load(Ordering::Acquire)
            .then_some(VcpuStop::Stopped)
    }
}

fn syscon_register(syscon: dillo_devtree::platform::Syscon) -> syscon::SysconRegister {
    syscon::SysconRegister {
        base: syscon.base,
        offset: syscon.offset,
        value: syscon.value,
        mask: syscon.mask,
    }
}

fn interrupt_source(interrupt: &WiredInterrupt) -> InterruptSource {
    let mut cells = [0u32; 4];
    for (dst, src) in cells.iter_mut().zip(interrupt.cells.iter().copied()) {
        *dst = src;
    }
    InterruptSource {
        controller: interrupt.controller.phandle,
        cells,
        cell_count: interrupt.cells.len().min(cells.len()) as u8,
    }
}

fn line_requirement(interrupt: &WiredInterrupt) -> MmioInterruptRequirement {
    MmioInterruptRequirement::Line {
        source: interrupt_source(interrupt),
    }
}

fn message_requirement(msi: &MsiParentage, vectors: u16) -> MmioInterruptRequirement {
    MmioInterruptRequirement::MessageDomain {
        source: Some(MessageInterruptSource {
            controller: msi.controller.phandle,
        }),
        vectors,
    }
}

fn optional_message_requirement(
    msi: Option<&MsiParentage>,
    vectors: u16,
) -> MmioInterruptRequirement {
    match msi {
        Some(msi) => message_requirement(msi, vectors),
        None => MmioInterruptRequirement::MessageDomain {
            source: None,
            vectors,
        },
    }
}

fn attach_pci_console<M, E>(
    vm: &mut M,
    platform: &PlatformMachine,
) -> Result<Option<Arc<PciRoot>>, RunError>
where
    E: std::error::Error + Send + Sync + 'static,
    M: Machine<Error = E>,
    M: Attach<Arc<PciRoot>, Error = E, Output = Arc<dyn MmioAttachment>>,
{
    if !platform.has_pcie {
        return Ok(None);
    }

    let vectors: u16 = 3;
    let interrupt_lookup = Arc::new(dillo_pci_virtio::MsixInterruptLookup::new());
    let console: Arc<std::sync::Mutex<Box<dyn dillo_virtio::VirtioDevice>>> =
        Arc::new(std::sync::Mutex::new(Box::new(
            dillo_virtio_console::VirtioConsole::new(interrupt_lookup.lookup_fn()),
        )));

    let mut virtio_pci_dev = dillo_pci_virtio::VirtioPciDevice::new(
        console,
        vectors,
        platform.pcie.mmio_base,
        platform.pcie.mmio_base + 0x1000,
    );
    virtio_pci_dev.set_interrupt_lookup(interrupt_lookup);
    let ecam = MmioWindow {
        base: platform.pcie.ecam_base,
        size: platform.pcie.ecam_size,
    };
    let mut pci_root = PciRoot::with_interrupt_requirement(
        ecam,
        optional_message_requirement(platform.pcie.msi.as_ref(), vectors),
    );
    pci_root.register(1, Box::new(VirtioPciAdapter::new(virtio_pci_dev)));
    let pci_root = Arc::new(pci_root);
    let attachment = Attach::attach(vm, Arc::clone(&pci_root)).map_err(RunError::machine)?;
    pci_root.set_attachment(attachment);
    Ok(Some(pci_root))
}

fn attach_first_virtio_mmio_console<M, E>(
    vm: &mut M,
    platform: &PlatformMachine,
) -> Result<(), RunError>
where
    E: std::error::Error + Send + Sync + 'static,
    M: Machine<Error = E>,
    M: Attach<Arc<dillo_mmio_virtio::VirtioMmio>, Error = E, Output = Arc<dyn MmioAttachment>>,
{
    let Some(slot) = platform.virtio_mmio.first() else {
        return Ok(());
    };

    let int_status = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let irq = dillo_mmio_virtio::WiredIrq::unresolved(slot.irq);
    let interrupt_irq = irq.clone();
    let interrupt_status = Arc::clone(&int_status);
    let transport = Arc::new(dillo_mmio_virtio::VirtioMmio::with_interrupt_requirement(
        MmioWindow {
            base: slot.base,
            size: slot.size,
        },
        Box::new(dillo_virtio_console::VirtioConsole::new(Arc::new(
            move |_vector| {
                Some(dillo_mmio_virtio::VirtioMmio::interrupt(
                    Arc::clone(&interrupt_status),
                    interrupt_irq.clone(),
                ))
            },
        ))),
        Arc::clone(&int_status),
        irq.clone(),
        line_requirement(&slot.interrupt),
    ));
    let attachment = Attach::attach(vm, Arc::clone(&transport)).map_err(RunError::machine)?;
    transport.set_attachment(attachment);
    log::info!(
        "virtio-mmio console at {:#x} (SPI {}); {} slot(s) total",
        slot.base,
        irq.intid(),
        platform.virtio_mmio.len()
    );
    Ok(())
}

fn attach_uart<M, E>(vm: &mut M, platform: &PlatformMachine) -> Result<(), RunError>
where
    E: std::error::Error + Send + Sync + 'static,
    M: Machine<Error = E>,
    M: Attach<Arc<dillo_mmio_uart::Ns16550>, Error = E, Output = Arc<dyn MmioAttachment>>,
{
    let Some(uart) = platform.uart.as_ref() else {
        log::warn!("no UART in Machine - guest console output will be dropped");
        return Ok(());
    };
    let serial = Arc::new(dillo_mmio_uart::Ns16550::with_interrupt_requirement(
        MmioWindow {
            base: uart.base,
            size: uart.size,
        },
        uart.reg_shift,
        line_requirement(&uart.interrupt),
        Box::new(std::io::stderr()),
    ));
    let attachment = Attach::attach(vm, Arc::clone(&serial)).map_err(RunError::machine)?;
    serial.set_attachment(attachment.as_ref());
    log::info!(
        "serial: ns16550a @ {:#x} (size {:#x}, reg-shift {}, IRQ {})",
        uart.base,
        uart.size,
        uart.reg_shift,
        uart.irq
    );
    Ok(())
}

fn attach_syscon<M, E>(
    vm: &mut M,
    platform: &PlatformMachine,
) -> Result<Option<Arc<syscon::SysconState>>, RunError>
where
    E: std::error::Error + Send + Sync + 'static,
    M: Machine<Error = E>,
    M: Attach<Arc<syscon::SysconDevice>, Error = E>,
{
    let Some(poweroff) = platform.poweroff else {
        return Ok(None);
    };
    let state = Arc::new(syscon::SysconState::default());
    Attach::attach(
        vm,
        Arc::new(syscon::SysconDevice::new(
            syscon_register(poweroff),
            syscon::SysconAction::Poweroff,
            Arc::clone(&state),
        )),
    )
    .map_err(RunError::machine)?;
    if let Some(reboot) = platform.reboot {
        Attach::attach(
            vm,
            Arc::new(syscon::SysconDevice::new(
                syscon_register(reboot),
                syscon::SysconAction::Reboot,
                Arc::clone(&state),
            )),
        )
        .map_err(RunError::machine)?;
    }
    Ok(Some(state))
}

fn apply_load_sections<M: Machine>(vm: &mut M, guest_writes: &[RunWrite]) -> Result<(), RunError> {
    for write in guest_writes {
        log::debug!(
            "writing launch section `{}` to GPA {:#x} ({} bytes)",
            write.section,
            write.gpa,
            write.data.len()
        );
        vm.write_guest(write.gpa, &write.data)
            .map_err(RunError::machine)?;
    }
    Ok(())
}

fn run_vcpus<M, E>(
    vm: &mut M,
    count: u32,
    cpu_profile: &str,
    boot_state: &dyn BootVcpuState,
    control: Arc<SupervisorControl>,
) -> Result<VcpuStop, RunError>
where
    E: std::error::Error + Send + Sync + 'static,
    M: Machine<Error = E>,
    M: Attach<M::CpuState, Error = E, Output = Arc<M::Cpu>>,
{
    let mut cpus = Vec::with_capacity(count as usize);
    for index in 0..count {
        let state =
            M::CpuState::new(index, cpu_profile, Some(boot_state)).map_err(RunError::machine)?;
        cpus.push(Attach::attach(vm, state).map_err(RunError::machine)?);
    }
    vm.prepare_vcpu_run().map_err(RunError::machine)?;

    let shutdown = Arc::new(AtomicBool::new(false));
    let mut first_stop = VcpuStop::Stopped;
    let mut first_error = None;

    thread::scope(|scope| {
        let mut joins = Vec::with_capacity(cpus.len());
        for cpu in &cpus {
            let cpu = Arc::clone(cpu);
            let shutdown = Arc::clone(&shutdown);
            joins.push(scope.spawn(move || -> Result<VcpuStop, String> {
                if shutdown.load(Ordering::Acquire) {
                    return Ok(VcpuStop::Stopped);
                }
                let result = cpu.run().map_err(|e| e.to_string());
                shutdown.store(true, Ordering::Release);
                let _ = cpu.stop();
                result
            }));
        }

        let monitor = {
            let control = Arc::clone(&control);
            let cpus = cpus.clone();
            let shutdown = Arc::clone(&shutdown);
            scope.spawn(move || {
                while !shutdown.load(Ordering::Acquire) {
                    if control.stop_requested().is_some() {
                        shutdown.store(true, Ordering::Release);
                        for cpu in &cpus {
                            let _ = cpu.stop();
                        }
                        return;
                    }
                    thread::sleep(std::time::Duration::from_millis(10));
                }
            })
        };

        for join in joins {
            match join.join() {
                Ok(Ok(stop)) => {
                    if matches!(stop, VcpuStop::GuestReset | VcpuStop::GuestPoweroff) {
                        first_stop = stop;
                    }
                    for cpu in &cpus {
                        let _ = cpu.stop();
                    }
                }
                Ok(Err(error)) => {
                    first_error.get_or_insert(error);
                    for cpu in &cpus {
                        let _ = cpu.stop();
                    }
                }
                Err(_) => {
                    first_error.get_or_insert_with(|| "vCPU thread panicked".to_string());
                    for cpu in &cpus {
                        let _ = cpu.stop();
                    }
                }
            }
        }
        shutdown.store(true, Ordering::Release);
        monitor.join().expect("vCPU stop monitor panicked");
        if let Some(stop) = control.stop_requested() {
            first_stop = stop;
        }
        Ok::<(), RunError>(())
    })?;

    if let Some(error) = first_error {
        return Err(RunError::Machine(error));
    }
    Ok(first_stop)
}

fn run_selected<M, E>(
    preflight: Preflight,
    vcpus: u32,
    supervisor_shutdown: &'static AtomicBool,
) -> Result<i32, RunError>
where
    E: std::error::Error + Send + Sync + 'static,
    M: Machine<Error = E>,
    M: Attach<M::Memory, Error = E, Output = ()>,
    M: Attach<M::CpuState, Error = E, Output = Arc<M::Cpu>>,
    M: Attach<Arc<PciRoot>, Error = E, Output = Arc<dyn MmioAttachment>>,
    M: Attach<Arc<dillo_mmio_virtio::VirtioMmio>, Error = E, Output = Arc<dyn MmioAttachment>>,
    M: Attach<Arc<dillo_mmio_uart::Ns16550>, Error = E, Output = Arc<dyn MmioAttachment>>,
    M: Attach<Arc<syscon::SysconDevice>, Error = E>,
{
    let (parsed, platform, dtb, plan, guest_writes) = preflight.into_parts();
    log::info!(
        "PMI parsed: arch={:?}, {} actions, merged_dtb={}",
        parsed.arch,
        parsed.actions.len(),
        parsed.merged_dtb_section
    );
    log::info!(
        "coverage: base DTB fully claimed - {} declared region(s), pcie={}",
        platform.plan.regions().len(),
        platform.has_pcie
    );
    let total_backed: u64 = plan.memslots.iter().map(|r| r.size).sum();
    log::info!(
        "memslots: {} region(s), {} bytes",
        plan.memslots.len(),
        total_backed
    );
    log::info!("/memory@N nodes: {} region(s)", plan.memory_nodes.len());
    for r in &plan.memory_nodes {
        log::info!("  [{:#x}..{:#x}) ({} bytes)", r.gpa, r.gpa + r.size, r.size);
    }

    let mut vm = M::from_launch_config(LaunchConfig {
        dtb,
        vcpus,
        min_addr_space_bits: platform.min_addr_space_bits(),
    })
    .map_err(RunError::machine)?;
    let memory = M::Memory::from_ranges(&plan.ram_ranges()).map_err(RunError::machine)?;
    Attach::attach(&mut vm, memory).map_err(RunError::machine)?;
    apply_load_sections(&mut vm, &guest_writes)?;

    attach_uart(&mut vm, &platform)?;
    let syscon_state = attach_syscon(&mut vm, &platform)?;
    attach_pci_console(&mut vm, &platform)?;
    attach_first_virtio_mmio_console(&mut vm, &platform)?;

    let control = Arc::new(SupervisorControl {
        supervisor_shutdown,
        syscon_state: syscon_state.clone(),
    });
    let cpu_profile = parsed.cpu_profile.as_str();
    let mut outcome = run_vcpus::<M, E>(
        &mut vm,
        vcpus,
        cpu_profile,
        &parsed.vcpu as &dyn BootVcpuState,
        control,
    )?;
    while matches!(outcome, VcpuStop::GuestReset) {
        if syscon_state.is_some() {
            log::warn!(
                "guest reboot requested through syscon; exiting until warm reboot is implemented for this machine"
            );
            return Ok(0);
        }
        log::info!("guest requested reboot - replaying launch writes");
        vm.reset_for_reboot().map_err(RunError::machine)?;
        apply_load_sections(&mut vm, &guest_writes)?;
        let control = Arc::new(SupervisorControl {
            supervisor_shutdown,
            syscon_state: None,
        });
        outcome = run_vcpus::<M, E>(
            &mut vm,
            vcpus,
            cpu_profile,
            &parsed.vcpu as &dyn BootVcpuState,
            control,
        )?;
    }

    if matches!(outcome, VcpuStop::GuestPoweroff) {
        dillo_virtio_console::flush_output();
    }
    Ok(0)
}

/// Top-level VM-child entry point for the selected host machine.
pub(crate) fn run(
    preflight: Preflight,
    vcpus: u32,
    supervisor_shutdown: &'static AtomicBool,
) -> Result<i32, RunError> {
    run_selected::<selected_machine::Vm, selected_machine::Error>(
        preflight,
        vcpus,
        supervisor_shutdown,
    )
}
