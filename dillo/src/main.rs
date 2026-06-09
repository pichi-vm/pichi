//! dillo entrypoint.
//!
//! Parse PMI, build the launch plan, select the host Machine, and boot it.

mod machine_select;

use std::sync::atomic::AtomicBool;

use argh::FromArgs;
use machine_select::machine;
use machine_select::runner;

static SUPERVISOR_SHUTDOWN: AtomicBool = AtomicBool::new(false);

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
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Args = argh::from_env();

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

    let launch = match dillo::launch::LaunchPlan::read(
        &pmi,
        machine::HOST_ARCH,
        machine::platform,
        memory,
        cpus,
    ) {
        Ok(plan) => plan,
        Err(e) => {
            eprintln!("dillo: {e}");
            std::process::exit(e.exit_code());
        }
    };
    let dillo::launch::LaunchPlan {
        parsed,
        platform,
        merged_dtb,
        memory: memory_plan,
        guest_writes,
        ..
    } = launch;
    let preflight = runner::Preflight::new(
        parsed,
        platform,
        merged_dtb,
        memory_plan.memslots.iter().map(|r| runner::RunRegion {
            gpa: r.gpa,
            size: r.size,
        }),
        memory_plan.memory_nodes.iter().map(|r| runner::RunRegion {
            gpa: r.gpa,
            size: r.size,
        }),
        guest_writes.into_iter().map(|w| runner::RunWrite {
            section: w.section,
            gpa: w.gpa,
            data: w.data,
        }),
    );

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
    machine::install_signal_watchers(&SUPERVISOR_SHUTDOWN);

    log::info!(
        "dillo starting: pmi={} memory={}MiB cpus={} console={}",
        pmi.display(),
        memory,
        cpus,
        args.console,
    );

    match runner::run(preflight, cpus, &SUPERVISOR_SHUTDOWN) {
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
        return Err("--pmi required");
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
