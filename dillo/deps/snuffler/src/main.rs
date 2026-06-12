//! snuffler — the guest probe.
//!
//! Runs as PID 1 inside a guest booted from a PMI. Mounts /proc + /sys + /dev,
//! snuffles out the system the kernel sees (CPU/memory/cmdline/consoles/
//! PCI/block/net), benchmarks each block device, emits a [`Report`] JSON
//! bracketed by sentinels, then poweroffs. The probe contains no test logic —
//! host harnesses assert whatever they want against the [`Report`] (the crate's
//! library half).
//!
//! Output: bracketed JSON on stdout (which is /dev/console / hvc0 for PID 1
//! given `console=hvc0`). Free-form debug lines go to stderr.
//!
//! Linux-only (it's a Linux guest probe). All syscalls go through `rustix`'s
//! safe wrappers — no `unsafe`, no `libc`.

use std::fs;
use std::path::Path;

use rustix::fd::{AsFd, BorrowedFd};
use rustix::fs::{Mode, OFlags};
use snuffler::{
    BlockDevice, ClockInfo, CpuInfo, FsResult, KernelLog, KernelLogEntry, MemoryInfo, NetIf,
    PciDevice, REPORT_BEGIN, REPORT_END, Report, SerialPort, VIRTIOFS_PROBE_CONTENT,
    VIRTIOFS_PROBE_FILE, VsockResult,
};

mod bench;

#[cfg(not(test))]
fn main() {
    run();
}

fn run() {
    setup_mounts();
    reopen_console_stdio();
    let report = observe();
    let json = serde_json::to_string(&report)
        .unwrap_or_else(|e| format!("{{\"error\":\"serialize: {e}\"}}"));
    write_report(&json);
    rustix::fs::sync();
    #[cfg(not(test))]
    poweroff();
}

/// Read the kernel ring buffer from `/dev/kmsg` (non-draining: yields the whole
/// buffer from the oldest record). Each `read` returns one record formatted as
/// `priority,seq,ts_usec,flags;message`.
fn read_kernel_log() -> KernelLog {
    let fd = match rustix::fs::open(
        "/dev/kmsg",
        OFlags::RDONLY | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(e) => {
            return KernelLog {
                entries: Vec::new(),
                error: Some(format!("open /dev/kmsg: {e}")),
            };
        }
    };
    let mut entries = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match rustix::io::read(&fd, &mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let line = String::from_utf8_lossy(&buf[..n]);
                entries.push(parse_kmsg_record(line.trim_end_matches('\n')));
            }
            // AGAIN: ring buffer drained. EPIPE: records were overwritten while
            // reading — stop either way.
            Err(rustix::io::Errno::AGAIN) => break,
            Err(rustix::io::Errno::INTR) => continue,
            Err(_) => break,
        }
    }
    KernelLog {
        entries,
        error: None,
    }
}

/// Parse one `/dev/kmsg` record: `priority,seq,ts_usec,flags;message`. The
/// `message` text matches the printk message (what harnesses grep for); the
/// level is the printk priority and the timestamp is seconds since boot.
fn parse_kmsg_record(raw: &str) -> KernelLogEntry {
    let (meta, message) = raw.split_once(';').unwrap_or(("", raw));
    let mut fields = meta.split(',');
    let level = fields
        .next()
        .and_then(|s| s.parse::<u8>().ok())
        .map(|p| p & 0x07);
    let _seq = fields.next();
    let timestamp_secs = fields
        .next()
        .and_then(|s| s.parse::<f64>().ok())
        .map(|us| us / 1_000_000.0);
    KernelLogEntry {
        raw: raw.to_owned(),
        level,
        timestamp_secs,
        message: message.to_owned(),
    }
}

fn setup_mounts() {
    // arma's cpio wrapper only stages /init; create every mountpoint we rely on
    // before mounting. devtmpfs in particular won't be auto-mounted by
    // CONFIG_DEVTMPFS_MOUNT because /dev doesn't exist on the initramfs rootfs.
    let _ = fs::create_dir("/proc");
    let _ = fs::create_dir("/sys");
    let _ = fs::create_dir("/dev");
    mount("proc", "/proc", "proc");
    mount("sysfs", "/sys", "sysfs");
    mount("devtmpfs", "/dev", "devtmpfs");
}

fn mount(src: &str, target: &str, fstype: &str) {
    use rustix::mount::{MountFlags, mount as do_mount};
    if let Err(e) = do_mount(src, target, fstype, MountFlags::empty(), None) {
        eprintln!("fixture: mount {src} -> {target} ({fstype}) failed: {e}");
    }
}

fn reopen_console_stdio() {
    // The kernel can start PID 1 with fd 0/1/2 closed if /dev/console was not
    // available before devtmpfs was mounted. Rebind them after setup_mounts().
    for path in ["/dev/hvc0", "/dev/console", "/dev/ttyS0"] {
        if let Ok(fd) = rustix::fs::open(path, OFlags::RDWR | OFlags::NONBLOCK, Mode::empty()) {
            let _ = rustix::stdio::dup2_stdin(&fd);
            let _ = rustix::stdio::dup2_stdout(&fd);
            let _ = rustix::stdio::dup2_stderr(&fd);
            return;
        }
    }
}

fn write_report(json: &str) {
    // Keep report emission independent of Rust stdio startup. This fixture runs
    // as PID 1 in a minimal initramfs.
    write_report_fd(rustix::stdio::stdout(), json);
    for path in ["/dev/hvc0", "/dev/console"] {
        if let Ok(fd) = rustix::fs::open(path, OFlags::WRONLY | OFlags::NONBLOCK, Mode::empty()) {
            write_report_fd(fd.as_fd(), json);
        }
    }
}

fn write_report_fd(fd: BorrowedFd<'_>, json: &str) {
    for bytes in [
        REPORT_BEGIN.as_bytes(),
        json.as_bytes(),
        REPORT_END.as_bytes(),
        b"\n",
    ] {
        write_all_fd(fd, bytes);
    }
}

fn write_all_fd(fd: BorrowedFd<'_>, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        match rustix::io::write(fd, bytes) {
            Ok(0) | Err(_) => return,
            Ok(n) => bytes = &bytes[n..],
        }
    }
}

fn observe() -> Report {
    let cmdline = read_trim("/proc/cmdline");
    let vsock = probe_vsock(&cmdline);
    let (virtiofs, virtiofs_ro) = probe_virtiofs(&cmdline);
    Report {
        arch: uname_machine(),
        uptime_secs: parse_uptime(),
        cpu: read_cpu(),
        memory: read_memory(),
        consoles: read_consoles(),
        pci: walk_pci(),
        block: walk_block(),
        net: walk_net(),
        serial: walk_serial(),
        clock: read_clock(),
        kernel_log: read_kernel_log(),
        kaslr_seed: read_kaslr_seed(),
        vsock,
        virtiofs,
        virtiofs_ro,
        cmdline,
    }
}

/// Probe virtio-fs when the cmdline requests it. `dillo.virtiofs_tag=TAG` drives
/// the read-write probe (mount, list, read `dillo.virtiofs_file=NAME`, then write
/// a probe file); `dillo.virtiofs_ro_tag=TAG` drives a read-only probe (mount,
/// attempt a write that must be rejected). Either is absent → `None`, so
/// ordinary boots never touch virtio-fs. Returns `(read_write, read_only)`.
fn probe_virtiofs(cmdline: &str) -> (Option<FsResult>, Option<FsResult>) {
    let token = |key: &str| {
        cmdline
            .split_whitespace()
            .find_map(|tok| tok.strip_prefix(key))
            .map(str::to_owned)
    };
    let file = token("dillo.virtiofs_file=");
    let rw = token("dillo.virtiofs_tag=")
        .map(|tag| run_virtiofs_probe("/mnt-virtiofs", tag, file.clone()));
    let ro = token("dillo.virtiofs_ro_tag=")
        .map(|tag| run_virtiofs_probe("/mnt-virtiofs-ro", tag, None));
    (rw, ro)
}

fn run_virtiofs_probe(mountpoint: &str, tag: String, file: Option<String>) -> FsResult {
    use rustix::mount::{MountFlags, mount as do_mount};

    let mut res = FsResult {
        tag: tag.clone(),
        mounted: false,
        entries: Vec::new(),
        file: file.clone(),
        content: None,
        wrote: false,
        write_error: None,
        error: None,
    };

    let _ = fs::create_dir(mountpoint);
    // Mount read-write (no MountFlags::RDONLY): write rejection is enforced by
    // the *device* on a read-only share, which is what we want to exercise — not
    // the guest kernel's own mount flag.
    if let Err(e) = do_mount(
        tag.as_str(),
        mountpoint,
        "virtiofs",
        MountFlags::empty(),
        None,
    ) {
        res.error = Some(format!("mount: {e}"));
        return res;
    }
    res.mounted = true;

    if let Ok(rd) = fs::read_dir(mountpoint) {
        let mut entries: Vec<String> = rd
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        entries.sort();
        res.entries = entries;
    }

    if let Some(name) = file {
        match fs::read_to_string(format!("{mountpoint}/{name}")) {
            Ok(s) => res.content = Some(s),
            Err(e) => res.error = Some(format!("read {name}: {e}")),
        }
    }

    // Exercise the guest→host write path (create + write a probe file). On a
    // read-only share this is expected to fail with EROFS.
    match fs::write(
        format!("{mountpoint}/{VIRTIOFS_PROBE_FILE}"),
        VIRTIOFS_PROBE_CONTENT,
    ) {
        Ok(()) => res.wrote = true,
        Err(e) => res.write_error = Some(format!("write: {e}")),
    }

    res
}

/// Probe virtio-vsock when the cmdline requests it (`dillo.vsock_port=N`):
/// open an `AF_VSOCK` stream to host CID 2 on port N and round-trip a message.
/// `None` when the token is absent, so ordinary boots never touch vsock.
fn probe_vsock(cmdline: &str) -> Option<VsockResult> {
    let port = cmdline
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("dillo.vsock_port="))
        .and_then(|v| v.parse::<u32>().ok())?;
    Some(run_vsock_probe(port))
}

fn run_vsock_probe(port: u32) -> VsockResult {
    use std::io::{Read, Write};

    /// Well-known host CID (`VMADDR_CID_HOST`).
    const VMADDR_CID_HOST: u32 = 2;
    const MSG: &[u8] = b"dillo-vsock-ping";

    let mut res = VsockResult {
        port,
        connected: false,
        echo_ok: false,
        error: None,
    };

    let mut stream =
        match vsock::VsockStream::connect(&vsock::VsockAddr::new(VMADDR_CID_HOST, port)) {
            Ok(s) => s,
            Err(e) => {
                res.error = Some(format!("connect cid={VMADDR_CID_HOST} port={port}: {e}"));
                return res;
            }
        };
    res.connected = true;

    // Bound the round-trip so a missing host peer can't hang PID 1 until the
    // harness's boot timeout.
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));

    if let Err(e) = stream.write_all(MSG) {
        res.error = Some(format!("write: {e}"));
        return res;
    }

    let mut buf = [0u8; 64];
    match stream.read(&mut buf) {
        Ok(n) => {
            res.echo_ok = &buf[..n] == MSG;
            if !res.echo_ok {
                res.error = Some(format!("echo mismatch ({n} bytes)"));
            }
        }
        Err(e) => res.error = Some(format!("read: {e}")),
    }
    res
}

/// Read the 8-byte `/chosen/kaslr-seed` the kernel received, as lowercase hex
/// (FDT big-endian order). `None` if absent (x86 / no device tree) or malformed.
fn read_kaslr_seed() -> Option<String> {
    let bytes = fs::read("/proc/device-tree/chosen/kaslr-seed").ok()?;
    if bytes.len() != 8 {
        return None;
    }
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn read_trim(p: &str) -> String {
    fs::read_to_string(p).unwrap_or_default().trim().to_owned()
}

fn uname_machine() -> String {
    rustix::system::uname()
        .machine()
        .to_string_lossy()
        .into_owned()
}

fn parse_uptime() -> f64 {
    read_trim("/proc/uptime")
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0)
}

fn read_cpu() -> CpuInfo {
    let online_list = read_trim("/sys/devices/system/cpu/online");
    let online_count = count_cpu_range(&online_list);
    let (model_name, flags) = parse_cpuinfo();
    CpuInfo {
        online_count,
        online_list,
        model_name,
        flags,
    }
}

/// Parse a Linux cpulist (e.g. "0-3,5,7-8") into a count.
fn count_cpu_range(s: &str) -> usize {
    let mut n = 0usize;
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((a, b)) = part.split_once('-') {
            if let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) {
                if b >= a {
                    n += b - a + 1;
                }
            }
        } else if part.parse::<usize>().is_ok() {
            n += 1;
        }
    }
    n
}

fn parse_cpuinfo() -> (Option<String>, Vec<String>) {
    let s = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let mut model = None;
    let mut flags = Vec::new();
    for line in s.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if k == "model name" && model.is_none() {
            model = Some(v.to_owned());
        } else if (k == "flags" || k == "Features") && flags.is_empty() {
            // x86 uses "flags"; aarch64 uses "Features".
            flags = v.split_whitespace().map(str::to_owned).collect();
        }
    }
    (model, flags)
}

fn read_memory() -> MemoryInfo {
    let s = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let kv = |key: &str| -> u64 {
        for line in s.lines() {
            let Some((k, v)) = line.split_once(':') else {
                continue;
            };
            if k.trim() != key {
                continue;
            }
            return v
                .split_whitespace()
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
        }
        0
    };
    MemoryInfo {
        total_kib: kv("MemTotal"),
        free_kib: kv("MemFree"),
        available_kib: kv("MemAvailable"),
    }
}

fn read_clock() -> ClockInfo {
    let base = "/sys/devices/system/clocksource/clocksource0";
    let current_source = read_opt(&Path::new(base).join("current_clocksource"));
    let available_sources = read_trim(&format!("{base}/available_clocksource"))
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    let mut event_devices = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/devices/system/clockevents") {
        for e in entries.flatten() {
            let path = e.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.starts_with("clockevent") {
                continue;
            }
            if let Some(device) = read_opt(&path.join("current_device")) {
                event_devices.push(format!("{name}:{device}"));
            }
        }
    }
    event_devices.sort();
    ClockInfo {
        current_source,
        available_sources,
        event_devices,
    }
}

fn read_consoles() -> Vec<String> {
    fs::read_to_string("/proc/consoles")
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn walk_pci() -> Vec<PciDevice> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/bus/pci/devices") else {
        return out;
    };
    for e in entries.flatten() {
        let path = e.path();
        let bdf = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_owned();
        let driver = fs::read_link(path.join("driver"))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
        out.push(PciDevice {
            bdf,
            vendor: read_hex(&path.join("vendor")) as u16,
            device: read_hex(&path.join("device")) as u16,
            class: read_hex(&path.join("class")),
            driver,
        });
    }
    out.sort_by(|a, b| a.bdf.cmp(&b.bdf));
    out
}

fn read_hex(p: &Path) -> u32 {
    let s = fs::read_to_string(p).unwrap_or_default();
    let s = s.trim();
    let s = s.strip_prefix("0x").unwrap_or(s);
    u32::from_str_radix(s, 16).unwrap_or(0)
}

fn walk_block() -> Vec<BlockDevice> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/block") else {
        return out;
    };
    for e in entries.flatten() {
        let path = e.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_owned();
        let size_sectors: u64 = fs::read_to_string(path.join("size"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let vendor = read_opt(&path.join("device/vendor"));
        let model = read_opt(&path.join("device/model"));
        let ro = read_opt(&path.join("ro")).as_deref() == Some("1");
        let size_bytes = size_sectors.saturating_mul(512);
        let bench = bench::benchmark_device(&name, size_bytes, ro);
        out.push(BlockDevice {
            name,
            size_bytes,
            vendor,
            model,
            ro,
            bench,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn walk_net() -> Vec<NetIf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/net") else {
        return out;
    };
    for e in entries.flatten() {
        let path = e.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_owned();
        let mac = read_opt(&path.join("address"));
        let mtu = fs::read_to_string(path.join("mtu"))
            .ok()
            .and_then(|s| s.trim().parse().ok());
        let operstate = read_opt(&path.join("operstate"));
        out.push(NetIf {
            name,
            mac,
            mtu,
            operstate,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn read_opt(p: &Path) -> Option<String> {
    let s = fs::read_to_string(p).ok()?.trim().to_owned();
    (!s.is_empty()).then_some(s)
}

fn walk_serial() -> Vec<SerialPort> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/tty") else {
        return out;
    };
    for e in entries.flatten() {
        let path = e.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_owned();
        // Only enumerate UART-shaped tty entries — ttyS* (8250). Skip ttyN (vt),
        // tty, pts/*, etc.
        if !name.starts_with("ttyS") {
            continue;
        }
        // /sys/class/tty/<n>/type is the kernel's `tmp.type` value — 0 means
        // PORT_UNKNOWN, what the kernel reports for every pre-allocated 8250 slot
        // the driver never bound. Skip those; only emit ports the driver owns.
        let uart_type_id: Option<u32> = read_opt(&path.join("type")).and_then(|s| s.parse().ok());
        if uart_type_id.is_none_or(|t| t == 0) {
            continue;
        }
        // io_type is the numeric UPIO_* constant: 0 = PORT (I/O port), 2 = MEM
        // (8-bit MMIO), 3 = MEM32 (32-bit MMIO).
        let io_type_raw = read_opt(&path.join("io_type"));
        let (io_type, address) = match io_type_raw.as_deref() {
            Some("0") => (
                "port".to_string(),
                read_opt(&path.join("port")).and_then(parse_hex_or_dec_u64),
            ),
            Some("2") | Some("3") => (
                "mem".to_string(),
                read_opt(&path.join("iomem_base")).and_then(parse_hex_or_dec_u64),
            ),
            _ => ("unknown".to_string(), None),
        };
        let irq = read_opt(&path.join("irq")).and_then(|s| s.parse().ok());
        let uartclk_hz = read_opt(&path.join("uartclk")).and_then(|s| s.parse().ok());
        out.push(SerialPort {
            name,
            io_type,
            address,
            irq,
            uart_type_id,
            uartclk_hz,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn parse_hex_or_dec_u64(s: String) -> Option<u64> {
    let trimmed = s.trim();
    if let Some(rest) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        u64::from_str_radix(rest, 16).ok()
    } else {
        trimmed.parse().ok()
    }
}

#[cfg(not(test))]
fn poweroff() -> ! {
    let _ = rustix::system::reboot(rustix::system::RebootCommand::PowerOff);
    // Defensive: if reboot somehow returns, never let PID 1 exit (the kernel
    // would panic).
    loop {
        std::thread::park();
    }
}
