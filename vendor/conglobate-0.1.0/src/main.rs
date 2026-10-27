// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! conglobate — the pichi build driver, PID 1 (`/init`) in the PMI-only
//! build VM.
//!
//! Phase 0 skeleton: do the init plumbing (mount `/proc`, `/sys`, `/dev`,
//! `/tmp`), announce readiness on the console, then power off cleanly. The
//! actual build — serialize the virtiofs context, attach + verify source
//! carapaces, snapshot → chroot → run per `carapace.yaml` directive, emit
//! scutes to the output sink, and (for `pmi.yaml`) seal the `.pmi` — is
//! filled in by later phases.
//!
//! Linux-only; every syscall goes through rustix's safe wrappers (no
//! `unsafe`, no libc), matching the snuffler guest probe.

#[cfg(target_os = "linux")]
mod build;

// In-process network bring-up / teardown for build stages. Not yet wired
// into the build flow; the public API is exercised by its own unit tests.
#[cfg(target_os = "linux")]
#[allow(dead_code)]
mod net;

#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::path::Path;

/// Printed to the console once conglobate (PID 1) has done its init
/// plumbing and is about to drive the build. Host boot tests match this to
/// confirm the build image booted and reached userspace.
#[cfg(target_os = "linux")]
const CONGLOBATE_READY: &str = "CONGLOBATE-READY";

/// Printed once the build completed and `output/build.yaml` was written.
/// Host boot tests match this to confirm the in-guest build ran to
/// completion (distinct from [`CONGLOBATE_READY`], which only marks
/// reaching userspace).
#[cfg(target_os = "linux")]
const CONGLOBATE_BUILD_DONE: &str = "CONGLOBATE-BUILD-DONE";

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("conglobate runs only on Linux");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn main() {
    // Re-exec path: `conglobate __exec-in-root <root> <cmd>` chroots into a
    // directive's working filesystem and runs the command. Handled before any
    // init plumbing — this invocation is a throwaway child, not PID 1.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some(build::EXEC_IN_ROOT_ARG) {
        let root = args.get(2).map_or("/", String::as_str);
        let cmd = args.get(3).map_or("", String::as_str);
        if let Err(e) = build::exec_in_root(root, cmd) {
            eprintln!("conglobate: exec-in-root failed: {e}");
            std::process::exit(1);
        }
        return;
    }

    setup_mounts();
    load_modules();
    // PID 1's stdout is the console (given `console=...` on the cmdline).
    // Announce readiness so host boot tests can confirm we got here.
    println!("{CONGLOBATE_READY}");
    match build::run() {
        Ok(()) => println!("{CONGLOBATE_BUILD_DONE}"),
        Err(e) => eprintln!("conglobate: build failed: {e}"),
    }
    rustix::fs::sync();
    poweroff();
}

/// Load the kernel modules bundled in the initramfs at `/modules`, in
/// sorted filename order. The bootstrap names them with a numeric prefix
/// (`00-dm-mod.ko`, `01-dm-bufio.ko`, …) so lexical order is dependency
/// order. A missing or empty `/modules` is fine — a build image may use a
/// kernel with everything built in (the Phase 0 boot test does).
#[cfg(target_os = "linux")]
fn load_modules() {
    let Ok(entries) = fs::read_dir("/modules") else {
        return;
    };
    let mut kos: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "ko"))
        .collect();
    kos.sort();
    let mut loaded: Vec<String> = Vec::new();
    for ko in kos {
        let name = ko
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        match load_one_module(&ko) {
            Ok(()) => loaded.push(name),
            // Already-loaded / built-in modules return EEXIST; log and
            // continue rather than abort the build.
            Err(e) => eprintln!("conglobate: load module {} failed: {e}", ko.display()),
        }
    }
    if !loaded.is_empty() {
        // Reported on the console so host boot tests can confirm the
        // initramfs module set loaded.
        println!("CONGLOBATE-MODULES: {}", loaded.join(","));
    }
}

#[cfg(target_os = "linux")]
fn load_one_module(path: &Path) -> Result<(), String> {
    let f = fs::File::open(path).map_err(|e| e.to_string())?;
    // rustix wraps finit_module(2) safely; empty params, no flags.
    rustix::system::finit_module(&f, c"", 0).map_err(|e| e.to_string())
}

/// Create the standard mountpoints and mount the pseudo-filesystems. arma's
/// cpio wrapper stages only `/init`, so every mountpoint must be created
/// before it can be mounted (devtmpfs in particular is not auto-mounted).
#[cfg(target_os = "linux")]
fn setup_mounts() {
    for dir in ["/proc", "/sys", "/dev", "/tmp"] {
        let _ = fs::create_dir(dir);
    }
    mount("proc", "/proc", "proc");
    mount("sysfs", "/sys", "sysfs");
    mount("devtmpfs", "/dev", "devtmpfs");
    mount("tmpfs", "/tmp", "tmpfs");
}

#[cfg(target_os = "linux")]
fn mount(src: &str, target: &str, fstype: &str) {
    use rustix::mount::{MountFlags, mount as do_mount};
    if let Err(e) = do_mount(src, target, fstype, MountFlags::empty(), None) {
        eprintln!("conglobate: mount {src} -> {target} ({fstype}) failed: {e}");
    }
}

/// Power off the VM. PID 1 must never return; loop defensively if the
/// syscall somehow returns (the kernel panics on PID 1 exit otherwise).
#[cfg(target_os = "linux")]
fn poweroff() -> ! {
    let _ = rustix::system::reboot(rustix::system::RebootCommand::PowerOff);
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
