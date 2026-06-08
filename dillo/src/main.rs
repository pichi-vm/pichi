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

mod machine_select;

use argh::FromArgs;
use machine_select::machine;

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
    let _raw_guard = machine::RawStdio::enter_if_tty();
    machine::install_panic_terminal_restore();

    // Per ARCH §13.2: 1st SIGINT/SIGTERM asks for graceful guest
    // shutdown with a ~5s grace; 2nd SIGINT or SIGQUIT hard-kills.
    // Install the watcher BEFORE starting any vCPU threads so
    // signals aren't silently dropped during boot. SIGWINCH is also
    // blocked so the watcher can forward terminal-resize events to
    // the (future Phase 3) console child.
    machine::install_signal_watchers(&dillo_vm::SUPERVISOR_SHUTDOWN);

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
