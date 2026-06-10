// SPDX-License-Identifier: Apache-2.0

//! Wire schema shared between the guest-side e2e fixture and the host-side
//! test harness. The fixture observes the system it boots into (mounts
//! /proc + /sys, walks /sys/{block,bus/pci,class/net} and /proc) and emits
//! a [`Report`] as JSON. The harness deserialises the same [`Report`] and
//! asserts whatever the test cares about — the fixture itself contains no
//! test logic; it only describes what it sees.

use serde::{Deserialize, Serialize};

/// Sentinel pair the fixture brackets the JSON report with on stdout. The
/// kernel also writes its own printks to hvc0 when `console=hvc0` is in
/// cmdline, so the host harness needs unambiguous markers to extract the
/// report from interleaved kernel log noise.
pub const REPORT_BEGIN: &str = "<<<DILLO-E2E-REPORT>>>";
pub const REPORT_END: &str = "<<<END>>>";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub arch: String,
    pub cmdline: String,
    pub uptime_secs: f64,
    pub cpu: CpuInfo,
    pub memory: MemoryInfo,
    pub consoles: Vec<String>,
    pub pci: Vec<PciDevice>,
    pub block: Vec<BlockDevice>,
    pub net: Vec<NetIf>,
    /// Enumerated serial / UART ports the kernel sees, walked from
    /// `/sys/class/tty/ttyS*/`. Empty when arma built the
    /// PMI without `--serial`. Each entry carries the bound IRQ
    /// (0 = polled-mode fallback, nonzero = interrupt-driven), the
    /// I/O port or MMIO address, and the auto-detected UART type.
    #[serde(default)]
    pub serial: Vec<SerialPort>,
    /// Guest-selected timekeeping surfaces. This helps distinguish "host
    /// spent time in the VMM" from "guest selected a slow/poor clock path".
    #[serde(default)]
    pub clock: ClockInfo,
    /// Full guest kernel ring buffer captured immediately before the fixture
    /// emits the report.
    #[serde(default)]
    pub kernel_log: KernelLog,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuInfo {
    /// Number of CPUs the guest sees online.
    pub online_count: usize,
    /// Raw `/sys/devices/system/cpu/online` string (e.g. "0-1").
    pub online_list: String,
    /// First `model name` from `/proc/cpuinfo`, if present.
    pub model_name: Option<String>,
    /// `flags` line from `/proc/cpuinfo`, split on whitespace.
    pub flags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryInfo {
    pub total_kib: u64,
    pub free_kib: u64,
    pub available_kib: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PciDevice {
    /// Bus:device.function (e.g. "0000:00:01.0").
    pub bdf: String,
    pub vendor: u16,
    pub device: u16,
    pub class: u32,
    /// Driver name from `/sys/bus/pci/devices/<bdf>/driver` symlink, if bound.
    pub driver: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockDevice {
    /// `/sys/block/<name>` (e.g. "vda").
    pub name: String,
    /// `size` sysfs entry * 512.
    pub size_bytes: u64,
    pub vendor: Option<String>,
    pub model: Option<String>,
    /// `/sys/block/<name>/ro` (read-only). Default false for older reports.
    #[serde(default)]
    pub ro: bool,
    /// Raw-device I/O benchmark results, when the probe could open and exercise
    /// the device. `None` if benchmarking was skipped or failed (see
    /// [`BlkBench::error`] when present).
    #[serde(default)]
    pub bench: Option<BlkBench>,
}

/// Per-device raw I/O benchmark. snuffler measures throughput and verifies the
/// data path; it asserts nothing — host harnesses decide what matters. Numbers
/// are useful for finding bottlenecks outside CI; in CI only the correctness
/// invariants (bytes moved, zero errors, writes verified, RO rejected) are
/// stable enough to assert.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlkBench {
    /// `"o_direct"` when the device was opened `O_DIRECT` (I/O bypasses the page
    /// cache and traverses virtio-blk), else `"buffered"`.
    pub mode: String,
    /// Sequential read of up to ~16 MiB in 64 KiB blocks from offset 0.
    pub seq_read: BlkOp,
    /// Random 4 KiB reads at aligned offsets.
    pub rand_read: BlkOp,
    /// Sequential write (read-write devices only).
    #[serde(default)]
    pub seq_write: Option<BlkOp>,
    /// Random 4 KiB writes at aligned offsets (read-write devices only).
    #[serde(default)]
    pub rand_write: Option<BlkOp>,
    /// Read-only devices only: whether opening the device `O_RDWR` was correctly
    /// rejected by the kernel (proves `VIRTIO_BLK_F_RO` enforcement).
    #[serde(default)]
    pub ro_write_rejected: Option<bool>,
    /// Set when the device could not be opened / benchmarked.
    #[serde(default)]
    pub error: Option<String>,
}

/// One benchmark operation's results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlkOp {
    /// Total bytes transferred.
    pub bytes: u64,
    /// Number of individual read/write calls issued.
    pub ops: u64,
    /// Wall-clock duration in microseconds.
    pub duration_us: u64,
    /// `bytes / duration` in MiB/s (telemetry — noisy in a VM; do not gate CI).
    pub throughput_mibps: f64,
    /// Count of failed read/write calls.
    pub errors: u64,
    /// For write ops: whether a full read-back matched the written pattern.
    /// `None` for read ops.
    #[serde(default)]
    pub verified: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetIf {
    pub name: String,
    pub mac: Option<String>,
    pub mtu: Option<u32>,
    pub operstate: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerialPort {
    /// `/sys/class/tty` entry name (e.g. `"ttyS0"`).
    pub name: String,
    /// `"port"` for I/O-port-mapped UARTs (8250 family), `"mem"` for
    /// MMIO UARTs (MMIO 8250), `"unknown"` otherwise.
    pub io_type: String,
    /// I/O port number (for `io_type == "port"`) or MMIO base address
    /// (for `io_type == "mem"`).
    pub address: Option<u64>,
    /// IRQ assigned to this port. `Some(0)` means polled-mode fallback
    /// (the driver couldn't find an IRQ in firmware tables); nonzero
    /// means interrupt-driven. `None` means the `irq` sysfs attribute
    /// wasn't readable (some drivers don't expose it).
    pub irq: Option<u32>,
    /// 8250 driver type code from `/sys/class/tty/<name>/type`. 5 =
    /// 16550A; absent for non-8250 drivers.
    pub uart_type_id: Option<u32>,
    /// Clock frequency (Hz) from `/sys/class/tty/<name>/uartclk` —
    /// `baud_base × 16` for the 8250 family.
    pub uartclk_hz: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClockInfo {
    /// `/sys/devices/system/clocksource/clocksource0/current_clocksource`.
    pub current_source: Option<String>,
    /// Split `/sys/devices/system/clocksource/clocksource0/available_clocksource`.
    pub available_sources: Vec<String>,
    /// `/sys/devices/system/clockevents/clockevent*/current_device`, sorted by CPU/index.
    pub event_devices: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KernelLog {
    /// Parsed kernel log records in ring-buffer order.
    pub entries: Vec<KernelLogEntry>,
    /// Error text if the fixture could not read `/proc/kmsg` via syslog(2).
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelLogEntry {
    /// Original kmsg line, without the trailing newline.
    pub raw: String,
    /// printk priority from the leading `<N>` prefix when present.
    pub level: Option<u8>,
    /// Seconds since boot from the leading `[ seconds ]` timestamp when present.
    pub timestamp_secs: Option<f64>,
    /// Message text after the printk prefix and timestamp when both parse.
    pub message: String,
}
