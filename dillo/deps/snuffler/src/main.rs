//! snuffler — the guest probe.
//!
//! Runs as PID 1 inside a guest booted from a PMI. Mounts /proc + /sys,
//! snuffles out the system the kernel sees (CPU/memory/cmdline/consoles/
//! PCI/block/net), emits a [`Report`] JSON bracketed by sentinels, then
//! poweroffs. The probe contains no test logic — host harnesses assert
//! whatever they want against the [`Report`] (the crate's library half).
//!
//! Output: bracketed JSON on stdout (which is /dev/console / hvc0 for
//! PID 1 given `console=hvc0`). Free-form debug lines go to stderr.

#![no_main]

use std::ffi::CStr;
use std::fs;
use std::path::Path;
use std::ptr;

use snuffler::{
    BlockDevice, ClockInfo, CpuInfo, KernelLog, KernelLogEntry, MemoryInfo, NetIf, PciDevice,
    REPORT_BEGIN, REPORT_END, Report, SCHEMA_VERSION, SerialPort,
};

const RB_POWER_OFF: libc::c_int = 0x4321_FEDC_u32 as i32;

#[unsafe(no_mangle)]
pub extern "C" fn main(
    _: libc::c_int,
    _: *const *const libc::c_char,
    _: *const *const libc::c_char,
) -> libc::c_int {
    run();
    0
}

fn run() {
    setup_mounts();
    reopen_console_stdio();
    let report = observe();
    let json = serde_json::to_string(&report)
        .unwrap_or_else(|e| format!("{{\"error\":\"serialize: {e}\"}}"));
    write_report(&json);
    // SAFETY: sync() is parameterless and infallible.
    unsafe {
        libc::sync();
    }
    poweroff();
}

fn read_kernel_log() -> KernelLog {
    let size = unsafe { libc::syscall(libc::SYS_syslog, 10, ptr::null_mut::<u8>(), 0) };
    if size <= 0 {
        return KernelLog {
            entries: Vec::new(),
            error: Some(format!("syslog size unavailable rc={size}")),
        };
    }
    let mut buf = vec![0u8; size as usize + 1];
    let n = unsafe { libc::syscall(libc::SYS_syslog, 3, buf.as_mut_ptr(), buf.len()) };
    if n <= 0 {
        return KernelLog {
            entries: Vec::new(),
            error: Some(format!("syslog read unavailable rc={n}")),
        };
    }
    let text = String::from_utf8_lossy(&buf[..n as usize]);
    KernelLog {
        entries: text.lines().map(parse_kernel_log_entry).collect(),
        error: None,
    }
}

fn parse_kernel_log_entry(raw: &str) -> KernelLogEntry {
    let Some(rest) = raw.strip_prefix('<') else {
        return KernelLogEntry {
            raw: raw.to_owned(),
            level: None,
            timestamp_secs: None,
            message: raw.to_owned(),
        };
    };
    let Some((level_text, rest)) = rest.split_once('>') else {
        return KernelLogEntry {
            raw: raw.to_owned(),
            level: None,
            timestamp_secs: None,
            message: raw.to_owned(),
        };
    };
    let level = level_text.parse().ok();
    let rest = rest.trim_start();
    let Some(rest) = rest.strip_prefix('[') else {
        return KernelLogEntry {
            raw: raw.to_owned(),
            level,
            timestamp_secs: None,
            message: rest.to_owned(),
        };
    };
    let Some((timestamp_text, message)) = rest.split_once(']') else {
        return KernelLogEntry {
            raw: raw.to_owned(),
            level,
            timestamp_secs: None,
            message: rest.to_owned(),
        };
    };
    KernelLogEntry {
        raw: raw.to_owned(),
        level,
        timestamp_secs: timestamp_text.trim().parse().ok(),
        message: message.trim_start().to_owned(),
    }
}

fn setup_mounts() {
    // arma's cpio wrapper only stages /init; create every mountpoint
    // we rely on before calling mount. devtmpfs in particular won't be
    // auto-mounted by CONFIG_DEVTMPFS_MOUNT because /dev doesn't exist
    // on the initramfs rootfs.
    let _ = fs::create_dir("/proc");
    let _ = fs::create_dir("/sys");
    let _ = fs::create_dir("/dev");
    mount(c"proc", c"/proc", c"proc");
    mount(c"sysfs", c"/sys", c"sysfs");
    mount(c"devtmpfs", c"/dev", c"devtmpfs");
}

fn reopen_console_stdio() {
    // The kernel can start PID 1 with fd 0/1/2 closed if /dev/console was not
    // available before devtmpfs was mounted. Rebind them after setup_mounts()
    // so the report has a real console sink.
    for path in [c"/dev/hvc0", c"/dev/console", c"/dev/ttyS0"] {
        // SAFETY: path is NUL-terminated; open/dup2/close follow libc contracts.
        unsafe {
            let fd = libc::open(path.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK);
            if fd < 0 {
                continue;
            }
            for target in 0..=2 {
                let _ = libc::dup2(fd, target);
            }
            if fd > 2 {
                let _ = libc::close(fd);
            }
            return;
        }
    }
}

fn write_report(json: &str) {
    // Keep report emission independent of Rust stdio startup. This fixture runs
    // as PID 1 in a minimal initramfs and enters through the C ABI on aarch64.
    write_report_fd(1, json);
    for path in [c"/dev/hvc0", c"/dev/console", c"/dev/ttyS0"] {
        // SAFETY: path is NUL-terminated; open/close follow libc contracts.
        unsafe {
            let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK);
            if fd < 0 {
                continue;
            }
            write_report_fd(fd, json);
            let _ = libc::close(fd);
        }
    }
}

fn write_report_fd(fd: libc::c_int, json: &str) {
    for bytes in [
        REPORT_BEGIN.as_bytes(),
        json.as_bytes(),
        REPORT_END.as_bytes(),
        b"\n",
    ] {
        let _ = write_all_fd(fd, bytes);
    }
}

fn write_all_fd(fd: libc::c_int, mut bytes: &[u8]) -> bool {
    let mut wrote = false;
    while !bytes.is_empty() {
        // SAFETY: bytes points to a live buffer of the supplied length.
        let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if n <= 0 {
            return wrote;
        }
        wrote = true;
        bytes = &bytes[n as usize..];
    }
    true
}

fn mount(src: &CStr, target: &CStr, fstype: &CStr) {
    // SAFETY: PID 1 has CAP_SYS_ADMIN; arguments are NUL-terminated.
    let rc = unsafe {
        libc::mount(
            src.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            ptr::null(),
        )
    };
    if rc != 0 {
        // SAFETY: __errno_location returns a thread-local int* the kernel
        // populated on the failing syscall.
        let err = unsafe { *libc::__errno_location() };
        eprintln!(
            "fixture: mount {} -> {} ({}) failed: errno={err}",
            src.to_string_lossy(),
            target.to_string_lossy(),
            fstype.to_string_lossy(),
        );
    }
}

fn observe() -> Report {
    Report {
        schema_version: SCHEMA_VERSION,
        arch: uname_machine(),
        cmdline: read_trim("/proc/cmdline"),
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
    }
}

fn read_trim(p: &str) -> String {
    fs::read_to_string(p).unwrap_or_default().trim().to_owned()
}

fn uname_machine() -> String {
    // SAFETY: uname writes a fully-initialized utsname on success; machine
    // is a NUL-terminated C string within it.
    unsafe {
        let mut u: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut u) == 0 {
            CStr::from_ptr(u.machine.as_ptr())
                .to_string_lossy()
                .into_owned()
        } else {
            String::new()
        }
    }
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
                .trim()
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
        out.push(BlockDevice {
            name,
            size_bytes: size_sectors.saturating_mul(512),
            vendor,
            model,
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
        // Only enumerate UART-shaped tty entries — ttyS* (8250).
        // Skip ttyN (vt), tty, pts/*, etc.
        let is_uart = name.starts_with("ttyS");
        if !is_uart {
            continue;
        }
        // /sys/class/tty/<n>/type is the kernel's `tmp.type` value —
        // 0 means PORT_UNKNOWN, which is what the kernel reports for
        // every pre-allocated 8250 slot the driver never bound. Skip
        // those; only emit ports the driver actually owns.
        let uart_type_id: Option<u32> = read_opt(&path.join("type")).and_then(|s| s.parse().ok());
        if uart_type_id.is_none_or(|t| t == 0) {
            continue;
        }
        // io_type is the numeric UPIO_* constant from
        // include/linux/serial_core.h: 0 = PORT (I/O port), 2 = MEM
        // (8-bit MMIO), 3 = MEM32 (32-bit MMIO), etc. Map the values
        // we care about; treat anything else as "unknown" rather
        // than guessing.
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

fn poweroff() -> ! {
    // SAFETY: PID 1 invoking the documented poweroff syscall.
    unsafe {
        libc::reboot(RB_POWER_OFF);
    }
    // Defensive: if the syscall somehow returns, loop so the kernel
    // doesn't see PID 1 exit (which would panic).
    loop {
        // SAFETY: pause blocks until a signal is delivered.
        unsafe {
            libc::pause();
        }
    }
}
