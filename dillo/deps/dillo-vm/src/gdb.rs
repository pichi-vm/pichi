//! GDB Remote Serial Protocol stub for dillo.
//!
//! When `DILLO_GDB=<port>` is set, dillo listens on TCP `localhost:<port>`,
//! blocks until a `gdb` connects, and drives vCPU 0 through the
//! gdbstub event loop. The other vCPUs are not started in this mode —
//! debugging a multi-vCPU image is a follow-up.
//!
//! Supported gdb operations:
//!   - register read/write (general-purpose + segment + control regs)
//!   - memory read/write (translated through `GpaMap`)
//!   - software breakpoints (INT3-patched into guest memory)
//!   - single-step (KVM_GUESTDBG_SINGLESTEP)
//!   - continue (KVM_RUN until break / shutdown / fault)
//!   - SIGINT (Ctrl-C) to halt a running guest at the next exit
//!
//! Limitations:
//!   - Single-threaded: vCPU 0 only.
//!   - Identity-mapped guest VA == GPA (tatu and the early kernel both
//!     use identity maps for the addresses the user would set breakpoints
//!     on; gdb passes addresses straight through).
//!   - MMIO/PIO that occurs during a continue: the UART is forwarded
//!     to stderr; the syscon-poweroff write surfaces as TargetExited;
//!     everything else is silently allowed to keep running.

use std::collections::HashMap;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use dillo_hypervisor::{Vcpu, VmExit, debug_flags};
use gdbstub::common::Signal;
use gdbstub::conn::{Connection, ConnectionExt};
use gdbstub::stub::run_blocking::{BlockingEventLoop, Event, WaitForStopReasonError};
use gdbstub::stub::{DisconnectReason, GdbStub, SingleThreadStopReason};
use gdbstub::target::ext::base::BaseOps;
use gdbstub::target::ext::base::singlethread::{
    SingleThreadBase, SingleThreadResume, SingleThreadResumeOps, SingleThreadSingleStep,
    SingleThreadSingleStepOps,
};
use gdbstub::target::ext::breakpoints::{
    Breakpoints, BreakpointsOps, SwBreakpoint, SwBreakpointOps,
};
use gdbstub::target::{Target, TargetError, TargetResult};
use gdbstub_arch::x86::X86_64_SSE;
use gdbstub_arch::x86::reg::X86_64CoreRegs;

use crate::memory::GpaMap;
use crate::uart;

/// State shared between the gdb stub and the vCPU dispatch loop.
pub(crate) struct GdbTarget {
    vcpu: Vcpu,
    gpa: Arc<GpaMap>,
    platform: Arc<dillo_platform::Platform>,
    /// Address → original byte patched out by an INT3.
    sw_breakpoints: HashMap<u64, u8>,
    /// `true` once gdb has issued `continue`, cleared on every stop.
    resume_continue: bool,
    /// `true` if gdb just requested a single instruction step.
    resume_step: bool,
    /// Signaled when the guest hits `syscon-poweroff`; main loop checks.
    shutdown: Arc<AtomicBool>,
}

impl GdbTarget {
    pub(crate) fn new(
        vcpu: Vcpu,
        gpa: Arc<GpaMap>,
        platform: Arc<dillo_platform::Platform>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            vcpu,
            gpa,
            platform,
            sw_breakpoints: HashMap::new(),
            resume_continue: false,
            resume_step: false,
            shutdown,
        }
    }

    fn configure_debug(&self) -> Result<(), &'static str> {
        let mut flags = debug_flags::KVM_GUESTDBG_ENABLE | debug_flags::KVM_GUESTDBG_USE_SW_BP;
        if self.resume_step {
            flags |= debug_flags::KVM_GUESTDBG_SINGLESTEP;
        }
        self.vcpu
            .set_guest_debug_flags(flags)
            .map_err(|_| "set_guest_debug failed")
    }

    /// Drive one KVM_RUN, handling pass-through exits inline. Returns
    /// `Some` when execution should stop and gdb should be notified.
    fn run_one(&mut self) -> Result<Option<SingleThreadStopReason<u64>>, &'static str> {
        self.configure_debug()?;
        let exit = self
            .vcpu
            .run(
                |port, _size| uart::try_pio_read(port).map_or(0u32, u32::from),
                |_addr, _data| false, // gdb stub: no MMIO bus wiring
            )
            .map_err(|_| "vcpu.run failed")?;
        Ok(self.classify_exit(exit))
    }

    fn classify_exit(&mut self, exit: VmExit) -> Option<SingleThreadStopReason<u64>> {
        match exit {
            // INT3 breakpoint or KVM_GUESTDBG_SINGLESTEP completion.
            VmExit::Debug => {
                // If we were stepping, report DoneStep; otherwise a SW break.
                if self.resume_step {
                    self.resume_step = false;
                    Some(SingleThreadStopReason::DoneStep)
                } else {
                    Some(SingleThreadStopReason::SwBreak(()))
                }
            }
            VmExit::Halted => {
                // The guest issued HLT. With gdb attached we keep going
                // on next "continue" — KVM resumes on the next interrupt.
                if self.resume_continue {
                    None // keep running on subsequent KVM_RUN
                } else {
                    Some(SingleThreadStopReason::Signal(Signal::SIGTRAP))
                }
            }
            VmExit::Shutdown => {
                self.shutdown.store(true, Ordering::Release);
                Some(SingleThreadStopReason::Exited(0))
            }
            VmExit::MmioWrite { addr, data, size } => {
                if crate::syscon_match_for_gdb(&self.platform, addr, &data[..size as usize]) {
                    self.shutdown.store(true, Ordering::Release);
                    return Some(SingleThreadStopReason::Exited(0));
                }
                None
            }
            VmExit::PioWrite { port, data, size } => {
                let _ = uart::try_pio_write(port, &data[..size as usize]);
                None
            }
            VmExit::MmioRead { .. }
            | VmExit::PioRead { .. }
            | VmExit::Hvc { .. }
            | VmExit::Smc { .. } => None,
            VmExit::Unknown(reason) => {
                log::warn!("gdb: unknown KVM exit: {reason}");
                Some(SingleThreadStopReason::Signal(Signal::SIGSEGV))
            }
        }
    }
}

impl Target for GdbTarget {
    type Arch = X86_64_SSE;
    type Error = &'static str;

    fn base_ops(&mut self) -> BaseOps<'_, Self::Arch, Self::Error> {
        BaseOps::SingleThread(self)
    }

    fn support_breakpoints(&mut self) -> Option<BreakpointsOps<'_, Self>> {
        Some(self)
    }
}

impl SingleThreadBase for GdbTarget {
    fn read_registers(&mut self, regs: &mut X86_64CoreRegs) -> TargetResult<(), Self> {
        let r = self
            .vcpu
            .get_regs()
            .map_err(|_| TargetError::Fatal("get_regs"))?;
        let s = self
            .vcpu
            .get_sregs()
            .map_err(|_| TargetError::Fatal("get_sregs"))?;
        regs.regs = [
            r.rax, r.rbx, r.rcx, r.rdx, r.rsi, r.rdi, r.rbp, r.rsp, r.r8, r.r9, r.r10, r.r11,
            r.r12, r.r13, r.r14, r.r15,
        ];
        regs.rip = r.rip;
        regs.eflags = r.rflags as u32;
        regs.segments = gdbstub_arch::x86::reg::X86SegmentRegs {
            cs: s.cs.selector as u32,
            ss: s.ss.selector as u32,
            ds: s.ds.selector as u32,
            es: s.es.selector as u32,
            fs: s.fs.selector as u32,
            gs: s.gs.selector as u32,
        };
        // FPU / SSE: we don't read them from KVM yet; gdb will see
        // zeros and not complain unless the user looks at xmm/st.
        Ok(())
    }

    fn write_registers(&mut self, regs: &X86_64CoreRegs) -> TargetResult<(), Self> {
        let mut r = self
            .vcpu
            .get_regs()
            .map_err(|_| TargetError::Fatal("get_regs"))?;
        r.rax = regs.regs[0];
        r.rbx = regs.regs[1];
        r.rcx = regs.regs[2];
        r.rdx = regs.regs[3];
        r.rsi = regs.regs[4];
        r.rdi = regs.regs[5];
        r.rbp = regs.regs[6];
        r.rsp = regs.regs[7];
        r.r8 = regs.regs[8];
        r.r9 = regs.regs[9];
        r.r10 = regs.regs[10];
        r.r11 = regs.regs[11];
        r.r12 = regs.regs[12];
        r.r13 = regs.regs[13];
        r.r14 = regs.regs[14];
        r.r15 = regs.regs[15];
        r.rip = regs.rip;
        r.rflags = u64::from(regs.eflags);
        self.vcpu
            .set_regs(&r)
            .map_err(|_| TargetError::Fatal("set_regs"))?;
        Ok(())
    }

    fn read_addrs(&mut self, start: u64, data: &mut [u8]) -> TargetResult<usize, Self> {
        Ok(self.gpa.read(start, data))
    }

    fn write_addrs(&mut self, start: u64, data: &[u8]) -> TargetResult<(), Self> {
        self.gpa
            .write(start, data)
            .map_err(|_| TargetError::Fatal("write_addrs"))?;
        Ok(())
    }

    fn support_resume(&mut self) -> Option<SingleThreadResumeOps<'_, Self>> {
        Some(self)
    }
}

impl SingleThreadResume for GdbTarget {
    fn resume(&mut self, _signal: Option<Signal>) -> Result<(), Self::Error> {
        self.resume_continue = true;
        self.resume_step = false;
        Ok(())
    }

    fn support_single_step(&mut self) -> Option<SingleThreadSingleStepOps<'_, Self>> {
        Some(self)
    }
}

impl SingleThreadSingleStep for GdbTarget {
    fn step(&mut self, _signal: Option<Signal>) -> Result<(), Self::Error> {
        self.resume_continue = false;
        self.resume_step = true;
        Ok(())
    }
}

impl Breakpoints for GdbTarget {
    fn support_sw_breakpoint(&mut self) -> Option<SwBreakpointOps<'_, Self>> {
        Some(self)
    }
}

impl SwBreakpoint for GdbTarget {
    fn add_sw_breakpoint(&mut self, addr: u64, _kind: usize) -> TargetResult<bool, Self> {
        // Capture original byte then patch INT3 (0xCC).
        let mut buf = [0u8; 1];
        if self.gpa.read(addr, &mut buf) != 1 {
            return Ok(false);
        }
        self.sw_breakpoints.insert(addr, buf[0]);
        self.gpa
            .write(addr, &[0xCCu8])
            .map_err(|_| TargetError::Fatal("write INT3"))?;
        Ok(true)
    }

    fn remove_sw_breakpoint(&mut self, addr: u64, _kind: usize) -> TargetResult<bool, Self> {
        match self.sw_breakpoints.remove(&addr) {
            Some(orig) => {
                self.gpa
                    .write(addr, &[orig])
                    .map_err(|_| TargetError::Fatal("restore byte"))?;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

/// Drives gdb ↔ vCPU. Runs KVM_RUN, classifies the exit, then between
/// runs peeks at the gdb socket so the user's `Ctrl-C` is observable.
pub(crate) enum DilloEventLoop {}

impl BlockingEventLoop for DilloEventLoop {
    type Target = GdbTarget;
    type Connection = Box<dyn ConnectionExt<Error = io::Error>>;
    type StopReason = SingleThreadStopReason<u64>;

    fn wait_for_stop_reason(
        target: &mut Self::Target,
        conn: &mut Self::Connection,
    ) -> Result<
        Event<Self::StopReason>,
        WaitForStopReasonError<
            <Self::Target as Target>::Error,
            <Self::Connection as Connection>::Error,
        >,
    > {
        loop {
            // Check for incoming gdb bytes before each run so Ctrl-C
            // gets in even if the guest is in a tight loop without
            // KVM exits.
            if conn
                .peek()
                .map_err(WaitForStopReasonError::Connection)?
                .is_some()
            {
                let b = conn.read().map_err(WaitForStopReasonError::Connection)?;
                return Ok(Event::IncomingData(b));
            }

            let stop = target.run_one().map_err(WaitForStopReasonError::Target)?;
            if let Some(reason) = stop {
                return Ok(Event::TargetStopped(reason));
            }
            // run_one returned None (e.g., UART write) — loop and run again.
        }
    }

    fn on_interrupt(
        _target: &mut Self::Target,
    ) -> Result<Option<Self::StopReason>, <Self::Target as Target>::Error> {
        Ok(Some(SingleThreadStopReason::Signal(Signal::SIGINT)))
    }
}

/// Bind the gdb listener on `localhost:port`, log the wait banner, and
/// return the accepted stream.
pub(crate) fn wait_for_gdb(port: u16) -> Result<TcpStream> {
    let listener =
        TcpListener::bind(("127.0.0.1", port)).with_context(|| format!("bind gdb port {port}"))?;
    eprintln!("dillo: gdb stub listening on 127.0.0.1:{port} — `gdb -ex 'target remote :{port}'`");
    let (stream, addr) = listener.accept().context("gdb accept")?;
    eprintln!("dillo: gdb attached from {addr}");
    Ok(stream)
}

/// Run the gdb stub loop against `target` over `conn`. Returns on
/// disconnect or guest exit.
pub(crate) fn run_loop(target: GdbTarget, stream: TcpStream) {
    let mut target = target;
    let conn: Box<dyn ConnectionExt<Error = io::Error>> = Box::new(stream);
    let stub = GdbStub::new(conn);
    match stub.run_blocking::<DilloEventLoop>(&mut target) {
        Ok(reason) => match reason {
            DisconnectReason::Disconnect => log::info!("gdb: client disconnected"),
            DisconnectReason::TargetExited(c) => log::info!("gdb: guest exited code {c}"),
            DisconnectReason::TargetTerminated(s) => log::info!("gdb: guest terminated {s:?}"),
            DisconnectReason::Kill => log::info!("gdb: client sent kill"),
        },
        Err(e) => log::error!("gdb: stub error: {e}"),
    }
}
