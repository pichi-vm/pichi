//! dillo entrypoint.
//!
//! Two roles dispatched by argv:
//!
//! 1. **Boot mode** (default — no subcommand): act as supervisor +
//!    VM. Parse PMI, fork+exec each device backend as a separate
//!    process via `/proc/self/exe backend <kind> --fd=N`, then run
//!    KVM.
//!
//! 2. **Backend mode** (`dillo backend <kind> --fd=N`): the post-exec
//!    device-child entrypoint. Owns the inherited vhost-user
//!    socketpair fd, runs the per-device backend loop, installs its
//!    seccomp filter pre-loop.
//!
//! See `dillo/ARCHITECTURE.md` §3, §4, §13.

use argh::FromArgs;

/// VMM that boots PMI files (Linux/KVM today).
#[derive(FromArgs, Debug)]
struct Args {
    /// path to the PMI file to boot — required in boot mode, ignored
    /// in backend mode.
    #[argh(option)]
    pmi: Option<std::path::PathBuf>,

    /// guest RAM in MiB (default 1024).
    #[argh(option, default = "1024")]
    memory: u32,

    /// number of vCPUs (default 1).
    #[argh(option, default = "1")]
    cpus: u32,

    /// console endpoint (MVP: `stdio`; default: `stdio`)
    #[argh(option, default = "String::from(\"stdio\")")]
    console: String,

    /// optional subcommand — `backend <kind>` runs as a device child.
    #[argh(subcommand)]
    sub: Option<Sub>,
}

#[derive(FromArgs, Debug)]
#[argh(subcommand)]
enum Sub {
    Backend(BackendArgs),
}

/// Run as a device backend on an inherited socketpair fd.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "backend")]
struct BackendArgs {
    /// device kind — currently only `console`.
    #[argh(positional)]
    kind: String,

    /// inherited vhost-user socket fd (set by supervisor via fork+exec).
    #[argh(option)]
    fd: i32,
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Args = argh::from_env();

    if let Some(Sub::Backend(b)) = &args.sub {
        run_backend(b);
        return;
    }

    // Per ARCH §9.5 + Q11: DILLO_GDB is only valid in thread-only
    // builds. In the default process-isolation build, refuse at
    // startup with exit 2.
    #[cfg(feature = "process-isolation")]
    if std::env::var_os("DILLO_GDB").is_some() {
        eprintln!(
            "dillo: DILLO_GDB is only supported with `--no-default-features` \
             (thread-only build); process-isolation builds run gdb-incompatible \
             child processes. See ARCH §9.5."
        );
        std::process::exit(2);
    }

    // Validate BEFORE entering raw mode — otherwise a validation
    // failure hits std::process::exit(2), which bypasses the RawStdio
    // Drop guard and leaves the terminal mangled.
    if let Err(e) = validate(&args) {
        eprintln!("dillo: {e}");
        std::process::exit(2);
    }
    let pmi = args.pmi.clone().expect("validated");
    let memory = args.memory;
    let cpus = args.cpus;

    // Per ARCH §13.5: if stdin is a TTY, enter raw mode for the
    // session. A Drop guard restores cooked mode at exit; a custom
    // panic hook restores it before printing the panic message so
    // the user's terminal isn't left mangled after a crash.
    let _raw_guard = RawStdio::enter_if_tty();
    install_panic_terminal_restore();

    // Per ARCH §13.2: 1st SIGINT/SIGTERM asks for graceful guest
    // shutdown with a ~5s grace; 2nd SIGINT or SIGQUIT hard-kills.
    // Install the watcher BEFORE starting any vCPU threads so
    // signals aren't silently dropped during boot. SIGWINCH is also
    // blocked so the watcher can forward terminal-resize events to
    // the (future Phase 3) console child.
    install_signal_watchers();

    log::info!(
        "dillo starting: pmi={} memory={}MiB cpus={} console={}",
        pmi.display(),
        memory,
        cpus,
        args.console,
    );

    match dillo_vm::run(&pmi, memory, cpus) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("dillo: {e}");
            let mut src = std::error::Error::source(&e);
            while let Some(s) = src {
                eprintln!("  caused by: {s}");
                src = s.source();
            }
            std::process::exit(e.exit_code());
        }
    }
}

fn validate(args: &Args) -> Result<(), &'static str> {
    if args.pmi.is_none() {
        return Err("--pmi required in boot mode");
    }
    if args.memory == 0 || !args.memory.is_multiple_of(2) {
        return Err("--memory must be a positive even number of MiB");
    }
    if args.cpus == 0 {
        return Err("--cpus must be >= 1");
    }
    if args.console != "stdio" {
        return Err("--console only supports `stdio` in MVP");
    }
    Ok(())
}

/// Spawn a thread that blocks on a `signalfd` reading SIGINT/SIGTERM/
/// SIGQUIT and enforces the §13.2 escalation policy. The vCPU threads
/// keep running until either the guest ACPI-shuts down on its own
/// (within the 5s grace) or we hard-exit.
#[cfg(target_os = "linux")]
fn install_signal_watchers() {
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::thread;
    use std::time::Duration;

    use nix::sys::signal::{SigSet, Signal};
    use nix::sys::signalfd::{SfdFlags, SignalFd};

    let mut mask = SigSet::empty();
    mask.add(Signal::SIGINT);
    mask.add(Signal::SIGTERM);
    mask.add(Signal::SIGQUIT);
    mask.add(Signal::SIGWINCH);
    // Block these in every thread so only the signalfd reader sees
    // them — must run before any other thread spawns.
    mask.thread_block().expect("block signals on main thread");

    let sfd = SignalFd::with_flags(&mask, SfdFlags::SFD_CLOEXEC).expect("signalfd creation");

    thread::Builder::new()
        .name("dillo-signals".into())
        .spawn(move || {
            static SEEN: AtomicU8 = AtomicU8::new(0);
            loop {
                match sfd.read_signal() {
                    Ok(Some(sig)) => {
                        let signo = sig.ssi_signo as i32;
                        // SIGWINCH: terminal resize. In process-mode
                        // this forwards the new winsize to the console
                        // child via its control socket; in the current
                        // thread-mode build there's no child to tell,
                        // so we just acknowledge + skip (the in-process
                        // VirtioConsole TX worker doesn't carry a
                        // resize event yet — Phase 3 dependency).
                        if signo == Signal::SIGWINCH as i32 {
                            log::trace!("SIGWINCH — no console child to forward to yet");
                            continue;
                        }
                        let count = SEEN.fetch_add(1, Ordering::SeqCst);
                        let name = match signo {
                            n if n == Signal::SIGINT as i32 => "SIGINT",
                            n if n == Signal::SIGTERM as i32 => "SIGTERM",
                            n if n == Signal::SIGQUIT as i32 => "SIGQUIT",
                            _ => "signal",
                        };
                        if signo == Signal::SIGQUIT as i32 || count >= 1 {
                            // Hard kill: second user-initiated signal,
                            // or SIGQUIT regardless of count.
                            log::warn!("{name} — hard exit");
                            std::process::exit(128 + signo);
                        }
                        log::warn!(
                            "{name} — graceful shutdown requested; \
                             waiting 5s for guest before hard exit"
                        );
                        // §13.3: tell dillo-vm the supervisor wants
                        // shutdown. Each vCPU thread checks the flag
                        // each iteration and exits 0 cleanly.
                        dillo_vm::SUPERVISOR_SHUTDOWN
                            .store(true, std::sync::atomic::Ordering::Release);
                        // Spawn a watchdog: if the guest doesn't
                        // ACPI-poweroff within 5s, hard-exit. A successful
                        // syscon-poweroff write makes the VM run loop exit
                        // cleanly before this timer fires.
                        let signo_for_timer = signo;
                        thread::Builder::new()
                            .name("dillo-shutdown-watchdog".into())
                            .spawn(move || {
                                thread::sleep(Duration::from_secs(5));
                                log::warn!("guest did not shut down within 5s — hard exit");
                                std::process::exit(128 + signo_for_timer);
                            })
                            .expect("spawn shutdown watchdog");

                        // Inject guest shutdown: synthesize a write
                        // through the syscon-poweroff MMIO path the
                        // guest itself would have used. The MMIO bus
                        // handler will see the match and exit(0).
                        //
                        // Phase 4 §13.3 follow-up: this needs a hook
                        // into dillo-vm's MMIO bus from the supervisor
                        // thread. For now we rely on the watchdog —
                        // the only path that produces a clean exit
                        // here is the guest doing its own ACPI off
                        // (which never happens since the guest is
                        // running normal workload).
                    }
                    Ok(None) => continue,
                    Err(e) => {
                        log::error!("signalfd read: {e}");
                        return;
                    }
                }
            }
        })
        .expect("spawn dillo-signals thread");
}

// ─── Raw TTY mode (§13.5) ───────────────────────────────────────

/// Stashed cooked-mode termios for the atexit restorer. Captured by
/// [`RawStdio::enter_if_tty`] before flipping stdin to raw, used by
/// both the [`RawStdio`] Drop guard (normal return) and the atexit
/// handler (any `std::process::exit` path, including the syscon
/// shutdown inside dillo-vm and the §13.2 signal hard-exit).
///
/// On macOS the supervisor uses the thread model (no fork / signalfd);
/// signal handling for that path is wired in a later increment (§F).
#[cfg(not(target_os = "linux"))]
fn install_signal_watchers() {}

/// Stored as the raw `libc::termios` (POD) so it's both `Send + Sync`
/// and safe to read from the atexit handler.
#[cfg(not(target_os = "windows"))]
static ORIGINAL_TERMIOS: std::sync::OnceLock<libc::termios> = std::sync::OnceLock::new();

/// RAII guard that puts stdin in raw mode at construction and
/// restores cooked mode on Drop. No-op if stdin isn't a TTY. Also
/// installs an atexit handler that restores cooked mode on
/// `process::exit()` paths that bypass Drop.
struct RawStdio {
    armed: bool,
}

#[cfg(not(target_os = "windows"))]
impl RawStdio {
    fn enter_if_tty() -> Self {
        use std::os::fd::{AsFd, AsRawFd};
        let stdin = std::io::stdin();
        let fd = stdin.as_fd().as_raw_fd();
        // SAFETY: isatty inspects the fd-table entry; no aliasing.
        #[allow(unsafe_code)]
        let is_tty = unsafe { libc::isatty(fd) } == 1;
        if !is_tty {
            return Self { armed: false };
        }
        // Use libc termios directly: the value is POD, so we can park
        // a copy in a `OnceLock<libc::termios>` for the atexit handler
        // without fighting nix's `RefCell` wrapping.
        // SAFETY: termios is a POD that `tcgetattr` initializes
        // completely on success.
        #[allow(unsafe_code)]
        let original = unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut t) != 0 {
                return Self { armed: false };
            }
            t
        };
        let mut raw = original;
        raw.c_lflag &= !(libc::ECHO | libc::ICANON | libc::ISIG | libc::IEXTEN);
        // SAFETY: tcsetattr writes the termios struct we just built.
        #[allow(unsafe_code)]
        unsafe {
            if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
                return Self { armed: false };
            }
        }
        // Stash the cooked-mode termios once and register an atexit
        // restorer. SAFETY: libc::atexit takes `extern "C" fn()`;
        // ours only touches the OnceLock + tcsetattr.
        if ORIGINAL_TERMIOS.set(original).is_ok() {
            #[allow(unsafe_code)]
            unsafe {
                libc::atexit(restore_termios_atexit);
            }
        }
        Self { armed: true }
    }
}

#[cfg(target_os = "windows")]
impl RawStdio {
    fn enter_if_tty() -> Self {
        Self { armed: false }
    }
}

impl Drop for RawStdio {
    fn drop(&mut self) {
        if self.armed {
            restore_termios();
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn restore_termios() {
    use std::os::fd::{AsFd, AsRawFd};
    if let Some(orig) = ORIGINAL_TERMIOS.get() {
        let stdin = std::io::stdin();
        let fd = stdin.as_fd().as_raw_fd();
        // SAFETY: orig is a fully-initialized libc::termios; tcsetattr
        // just writes it back to the kernel.
        #[allow(unsafe_code)]
        unsafe {
            let _ = libc::tcsetattr(fd, libc::TCSANOW, orig);
        }
    }
}

#[cfg(target_os = "windows")]
fn restore_termios() {}

#[cfg(not(target_os = "windows"))]
extern "C" fn restore_termios_atexit() {
    restore_termios();
}

/// Custom panic hook that restores the terminal before printing the
/// panic message. Without this, a panic in raw mode leaves the
/// terminal mangled (no echo, no line discipline).
fn install_panic_terminal_restore() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_termios();
        prev(info);
    }));
}

fn run_backend(args: &BackendArgs) {
    log::info!("dillo backend: kind={} fd={}", args.kind, args.fd);
    let code = match args.kind.as_str() {
        "console" => dillo_virtio_console::run_backend(args.fd),
        other => {
            eprintln!("dillo: unknown backend kind {other:?}");
            2
        }
    };
    std::process::exit(code);
}
