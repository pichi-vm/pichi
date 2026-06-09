//! dillo entrypoint.
//!
//! Parse PMI, build the launch plan, select the host Machine, and boot it.

use std::sync::atomic::AtomicBool;

use argh::FromArgs;
use dillo_machine::Host;
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

    let launch = match launch::LaunchPlan::read(
        &pmi,
        <machine::Vm as Host>::ARCH,
        <machine::Vm as Host>::cpu_compatible(),
        memory,
        cpus,
    ) {
        Ok(plan) => plan,
        Err(e) => {
            eprintln!("dillo: {e}");
            std::process::exit(e.exit_code());
        }
    };
    let launch::LaunchPlan {
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
    let _raw_guard = <machine::Vm as Host>::enter_raw_stdio_if_tty();
    <machine::Vm as Host>::install_panic_terminal_restore();

    // Per ARCH §13.2: 1st SIGINT/SIGTERM asks for graceful guest
    // shutdown with a ~5s grace; 2nd SIGINT or SIGQUIT hard-kills.
    // Install the watcher BEFORE starting any vCPU threads so
    // signals aren't silently dropped during boot. SIGWINCH is also
    // blocked so the watcher can forward terminal-resize events to
    // the (future Phase 3) console child.
    <machine::Vm as Host>::install_signal_watchers(&SUPERVISOR_SHUTDOWN);

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

mod fdt_writer {

    #![allow(clippy::cast_possible_truncation)]

    const FDT_MAGIC: u32 = 0xD00DFEED;
    const FDT_VERSION: u32 = 17;
    const FDT_LAST_COMPATIBLE: u32 = 16;

    const FDT_BEGIN_NODE: u32 = 1;
    const FDT_END_NODE: u32 = 2;
    const FDT_PROP: u32 = 3;
    const FDT_END: u32 = 9;

    /// Builder that accumulates structure and strings blocks, then emits
    /// a complete FDT v17 blob via [`Self::finish`].
    pub(crate) struct FdtBuilder {
        structure: Vec<u8>,
        strings: Vec<u8>,
        string_offsets: Vec<(String, u32)>,
    }

    impl FdtBuilder {
        pub(crate) fn new() -> Self {
            Self {
                structure: Vec::with_capacity(1024),
                strings: Vec::with_capacity(256),
                string_offsets: Vec::new(),
            }
        }

        pub(crate) fn begin_node(&mut self, name: &str) {
            self.structure
                .extend_from_slice(&FDT_BEGIN_NODE.to_be_bytes());
            self.structure.extend_from_slice(name.as_bytes());
            self.structure.push(0);
            self.pad4();
        }

        pub(crate) fn end_node(&mut self) {
            self.structure
                .extend_from_slice(&FDT_END_NODE.to_be_bytes());
        }

        pub(crate) fn property(&mut self, name: &str, value: &[u8]) {
            let off = self.intern_string(name);
            self.structure.extend_from_slice(&FDT_PROP.to_be_bytes());
            self.structure
                .extend_from_slice(&(value.len() as u32).to_be_bytes());
            self.structure.extend_from_slice(&off.to_be_bytes());
            self.structure.extend_from_slice(value);
            self.pad4();
        }

        pub(crate) fn property_u32(&mut self, name: &str, v: u32) {
            self.property(name, &v.to_be_bytes());
        }

        pub(crate) fn property_string(&mut self, name: &str, v: &str) {
            let mut bytes = Vec::with_capacity(v.len() + 1);
            bytes.extend_from_slice(v.as_bytes());
            bytes.push(0);
            self.property(name, &bytes);
        }

        /// Emit a `reg = <hi32 lo32 hi32 lo32>` property from a single
        /// (base, size) pair, encoded as four big-endian u32 cells.
        pub(crate) fn property_reg_2cells(&mut self, name: &str, base: u64, size: u64) {
            let cells: [u32; 4] = [
                (base >> 32) as u32,
                base as u32,
                (size >> 32) as u32,
                size as u32,
            ];
            let mut bytes = Vec::with_capacity(16);
            for c in cells {
                bytes.extend_from_slice(&c.to_be_bytes());
            }
            self.property(name, &bytes);
        }

        pub(crate) fn finish(mut self) -> Vec<u8> {
            // Append FDT_END at end of structure block.
            self.structure.extend_from_slice(&FDT_END.to_be_bytes());

            // Header is 40 bytes.
            let header_len: u32 = 40;
            // Memreserve block: one terminator (16 bytes of zeros).
            let memrsv_len: u32 = 16;
            // Layout: [header][memreserve][structure][strings]
            let off_mem_rsvmap = header_len;
            let off_dt_struct = off_mem_rsvmap + memrsv_len;
            let size_dt_struct = self.structure.len() as u32;
            let off_dt_strings = off_dt_struct + size_dt_struct;
            let size_dt_strings = self.strings.len() as u32;
            let totalsize = off_dt_strings + size_dt_strings;

            let mut out = Vec::with_capacity(totalsize as usize);
            out.extend_from_slice(&FDT_MAGIC.to_be_bytes());
            out.extend_from_slice(&totalsize.to_be_bytes());
            out.extend_from_slice(&off_dt_struct.to_be_bytes());
            out.extend_from_slice(&off_dt_strings.to_be_bytes());
            out.extend_from_slice(&off_mem_rsvmap.to_be_bytes());
            out.extend_from_slice(&FDT_VERSION.to_be_bytes());
            out.extend_from_slice(&FDT_LAST_COMPATIBLE.to_be_bytes());
            out.extend_from_slice(&0u32.to_be_bytes()); // boot_cpuid_phys
            out.extend_from_slice(&size_dt_strings.to_be_bytes());
            out.extend_from_slice(&size_dt_struct.to_be_bytes());
            // Memreserve terminator.
            out.extend_from_slice(&[0u8; 16]);
            // Structure block.
            out.extend_from_slice(&self.structure);
            // Strings block.
            out.extend_from_slice(&self.strings);
            out
        }

        fn pad4(&mut self) {
            while self.structure.len() % 4 != 0 {
                self.structure.push(0);
            }
        }

        fn intern_string(&mut self, name: &str) -> u32 {
            if let Some((_, off)) = self.string_offsets.iter().find(|(n, _)| n == name) {
                return *off;
            }
            let off = self.strings.len() as u32;
            self.strings.extend_from_slice(name.as_bytes());
            self.strings.push(0);
            self.string_offsets.push((name.to_string(), off));
            off
        }
    }
}

mod placement {

    use thiserror::Error;

    const HUGE_PAGE: u64 = 2 << 20;

    /// One contiguous region for either a memslot or a `/memory@N` node.
    #[derive(Debug, Clone, Copy)]
    pub(crate) struct Region {
        pub(crate) gpa: u64,
        pub(crate) size: u64,
    }

    #[derive(Debug, Clone, Copy)]
    struct Interval {
        start: u64,
        end: u64, // exclusive
    }

    /// The full memory plan: memslots == memory_nodes by construction.
    #[derive(Debug)]
    pub(crate) struct MemoryPlan {
        pub(crate) memslots: Vec<Region>,
        pub(crate) memory_nodes: Vec<Region>,
    }

    #[derive(Debug, Error)]
    pub(crate) enum PlanError {
        #[error(
            "--memory {requested_mib} MiB is less than the {islands_mib} MiB required to cover \
             loaded sections + reset trampoline"
        )]
        TooLittleMemory {
            requested_mib: u64,
            islands_mib: u64,
        },
        #[error(
            "no contiguous {remaining_mib} MiB chunk fits below the 4 GiB identity-map ceiling \
             after carving out islands and device regions"
        )]
        NoSpaceForBigChunk { remaining_mib: u64 },
    }

    /// Identity-map ceiling on x86: 4 GiB. Big chunk must end at or below this
    /// (so its GPAs are reachable from tatu's 4 GiB identity pgtable).
    const IDENTITY_CEILING: u64 = 1u64 << 32;

    pub(crate) fn plan_around_regions<I>(
        must_cover: &[(u64, u64)],
        memory_mib: u32,
        device_regions: I,
    ) -> Result<MemoryPlan, PlanError>
    where
        I: IntoIterator<Item = (u64, u64)>,
    {
        let budget = round_up_2mib(u64::from(memory_mib) * (1 << 20));

        // ── 1. Islands ────────────────────────────────────────────────
        let mut islands: Vec<Interval> = must_cover
            .iter()
            .filter(|(_, s)| *s > 0)
            .map(|&(gpa, size)| Interval {
                start: gpa & !(HUGE_PAGE - 1),
                end: round_up_2mib(gpa.saturating_add(size)),
            })
            .collect();
        merge_intervals(&mut islands);

        let islands_total: u64 = islands.iter().map(|i| i.end - i.start).sum();

        log::info!(
            "placement: budget={} MiB, islands={} ({} MiB)",
            budget >> 20,
            islands.len(),
            islands_total >> 20,
        );
        for i in &islands {
            log::info!(
                "  island [{:#x}..{:#x}) ({} MiB)",
                i.start,
                i.end,
                (i.end - i.start) >> 20,
            );
        }

        if islands_total > budget {
            return Err(PlanError::TooLittleMemory {
                requested_mib: budget >> 20,
                islands_mib: islands_total >> 20,
            });
        }
        let remaining = budget - islands_total;

        // ── 2. Big chunk ─────────────────────────────────────────────
        let mut holes: Vec<Interval> = device_holes(device_regions);
        holes.extend(islands.iter().copied());
        merge_intervals(&mut holes);

        log::info!("placement: device+island holes ({} ranges):", holes.len());
        for h in &holes {
            log::info!("  hole [{:#x}..{:#x})", h.start, h.end);
        }

        let big_chunk = if remaining == 0 {
            None
        } else {
            Some(find_lowest_fit(remaining, &holes, IDENTITY_CEILING).ok_or(
                PlanError::NoSpaceForBigChunk {
                    remaining_mib: remaining >> 20,
                },
            )?)
        };

        if let Some(c) = big_chunk {
            log::info!(
                "placement: big_chunk [{:#x}..{:#x}) ({} MiB)",
                c.start,
                c.end,
                (c.end - c.start) >> 20,
            );
        }

        // ── 3. Assemble final region set ─────────────────────────────
        let mut regions: Vec<Region> = islands.iter().map(to_region).collect();
        if let Some(c) = big_chunk {
            regions.push(to_region(&c));
        }
        regions.sort_by_key(|r| r.gpa);

        Ok(MemoryPlan {
            memslots: regions.clone(),
            memory_nodes: regions,
        })
    }

    /// Device MMIO regions, rounded outward to 2 MiB.
    fn device_holes<I>(regions: I) -> Vec<Interval>
    where
        I: IntoIterator<Item = (u64, u64)>,
    {
        regions
            .into_iter()
            .filter(|(_, size)| *size > 0)
            .map(|(base, size)| Interval {
                start: base & !(HUGE_PAGE - 1),
                end: round_up_2mib(base.saturating_add(size)),
            })
            .collect()
    }

    fn to_region(i: &Interval) -> Region {
        Region {
            gpa: i.start,
            size: i.end - i.start,
        }
    }

    /// Merge overlapping or touching intervals in-place. Result is sorted.
    fn merge_intervals(v: &mut Vec<Interval>) {
        if v.is_empty() {
            return;
        }
        v.sort_by_key(|i| i.start);
        let mut out: Vec<Interval> = Vec::with_capacity(v.len());
        out.push(v[0]);
        for cur in v.iter().skip(1) {
            let last = out.last_mut().expect("non-empty");
            if cur.start <= last.end {
                last.end = last.end.max(cur.end);
            } else {
                out.push(*cur);
            }
        }
        *v = out;
    }

    /// Find the lowest 2 MiB-aligned start such that `[start, start + size)`
    /// is clear of every hole and ends at or below `ceiling`.
    fn find_lowest_fit(size: u64, holes: &[Interval], ceiling: u64) -> Option<Interval> {
        let mut cursor: u64 = 0;
        for h in holes {
            let candidate_end = cursor.saturating_add(size);
            if candidate_end <= h.start && candidate_end <= ceiling {
                return Some(Interval {
                    start: cursor,
                    end: candidate_end,
                });
            }
            cursor = cursor.max(h.end);
            cursor = round_up_2mib(cursor);
        }
        let candidate_end = cursor.saturating_add(size);
        if candidate_end <= ceiling {
            Some(Interval {
                start: cursor,
                end: candidate_end,
            })
        } else {
            None
        }
    }

    fn round_up_2mib(n: u64) -> u64 {
        (n + HUGE_PAGE - 1) & !(HUGE_PAGE - 1)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn merge_intervals_basic() {
            let mut v = vec![
                Interval { start: 10, end: 20 },
                Interval { start: 30, end: 40 },
                Interval { start: 15, end: 32 },
            ];
            merge_intervals(&mut v);
            assert_eq!(v.len(), 1);
            assert_eq!(v[0].start, 10);
            assert_eq!(v[0].end, 40);
        }

        #[test]
        fn find_lowest_fit_skips_holes() {
            let holes = vec![
                Interval {
                    start: 0x10_0000,
                    end: 0x20_0000,
                },
                Interval {
                    start: 0x40_0000,
                    end: 0x60_0000,
                },
            ];
            let r = find_lowest_fit(0x10_0000, &holes, u64::MAX).unwrap();
            assert_eq!(r.start, 0);
            assert_eq!(r.end, 0x10_0000);

            let r = find_lowest_fit(0x20_0000, &holes, u64::MAX).unwrap();
            assert_eq!(r.start, 0x20_0000);
            assert_eq!(r.end, 0x40_0000);

            let r = find_lowest_fit(0x40_0000, &holes, u64::MAX).unwrap();
            assert_eq!(r.start, 0x60_0000);
        }

        #[test]
        fn find_lowest_fit_respects_ceiling() {
            let holes = vec![Interval {
                start: 0x10_0000,
                end: 0x20_0000,
            }];
            // Need 0x40_0000, ceiling 0x50_0000, hole at [1M, 2M)
            // → candidate at 0 (size 4M) ends at 4M, ≤ ceiling? No — overlaps hole.
            // → candidate at 2M ends at 6M > 5M → rejected.
            assert!(find_lowest_fit(0x40_0000, &holes, 0x50_0000).is_none());
        }

        #[test]
        fn plan_around_regions_uses_declared_holes() {
            let plan = plan_around_regions(&[(0, 0x20_0000)], 8, [(0x20_0000, 0x20_0000)])
                .expect("placement");

            assert_eq!(plan.memslots.len(), 2);
            assert_eq!(plan.memslots[0].gpa, 0);
            assert_eq!(plan.memslots[0].size, 0x20_0000);
            assert_eq!(plan.memslots[1].gpa, 0x40_0000);
            assert_eq!(plan.memslots[1].size, 0x60_0000);
        }
    }
}

mod overlay {

    use anyhow::{Result, anyhow};

    use crate::fdt_writer::FdtBuilder;
    use crate::placement::Region;

    pub(crate) fn synthesize_dtbo(
        regions: &[Region],
        vcpus: u32,
        enable_method: Option<&str>,
        cpu_compatible: Option<&str>,
        reserved_size: u64,
    ) -> Result<Vec<u8>> {
        let mut fdt = FdtBuilder::new();

        // root
        fdt.begin_node("");
        fdt.property_u32("#address-cells", 2);
        fdt.property_u32("#size-cells", 2);

        // /fragment@0 — authors the entire /cpus subtree under root. The base
        // declares nothing CPU-related (no /cpus); per merged.md §1+§2 cat 1 the
        // host overlay creates the /cpus container (with #address-cells/#size-cells)
        // and every cpu@N: device_type, a unique reg, status, and — where the
        // platform provides them — the bring-up method (`enable-method`) and the
        // MIDR-derived `compatible`. x86 cpus carry neither (no DT enable-method;
        // no consumer for a cpu compatible). The fragment targets `/` because the
        // base has no /cpus node for an overlay to extend.
        fdt.begin_node("fragment@0");
        fdt.property_string("target-path", "/");
        fdt.begin_node("__overlay__");
        fdt.begin_node("cpus");
        fdt.property_u32("#address-cells", 1);
        fdt.property_u32("#size-cells", 0);
        for n in 0..vcpus {
            let name = format!("cpu@{n}");
            fdt.begin_node(&name);
            fdt.property_string("device_type", "cpu");
            fdt.property_u32("reg", n);
            fdt.property_string("status", "okay");
            if let Some(em) = enable_method {
                fdt.property_string("enable-method", em);
            }
            if let Some(compat) = cpu_compatible {
                fdt.property_string("compatible", compat);
            }
            fdt.end_node(); // cpu@n
        }
        fdt.end_node(); // cpus
        fdt.end_node(); // __overlay__
        fdt.end_node(); // fragment@0

        // /fragment@1 — extends root `/` with /memory@<base> regions.
        // No properties on `/` here — tatu's merged-extension allowlist
        // (pmi/spec/merged.md §2) rejects any property addition on root.
        // The base DTB already declares #address-cells/#size-cells.
        fdt.begin_node("fragment@1");
        fdt.property_string("target-path", "/");
        fdt.begin_node("__overlay__");
        for r in regions {
            let name = format!("memory@{:x}", r.gpa);
            fdt.begin_node(&name);
            fdt.property_string("device_type", "memory");
            fdt.property_reg_2cells("reg", r.gpa, r.size);
            fdt.end_node(); // memory@N
        }
        fdt.end_node(); // __overlay__
        fdt.end_node(); // fragment@1

        fdt.end_node(); // root

        let bytes = fdt.finish();
        if bytes.len() as u64 > reserved_size {
            return Err(anyhow!(
                "synthesized DTBO ({} bytes) exceeds reserved .dtbo section size ({} bytes)",
                bytes.len(),
                reserved_size
            ));
        }
        Ok(bytes)
    }

    #[cfg(test)]
    mod tests {
        use dillo_devtree::devtree::{NodeView, PropertyView, Tree, TreeView};

        use super::*;

        fn synth(vcpus: u32, enable_method: Option<&str>, compatible: Option<&str>) -> Vec<u8> {
            synthesize_dtbo(&[], vcpus, enable_method, compatible, 1 << 20).expect("synth")
        }

        fn pstr<N: NodeView>(node: &N, name: &str) -> Option<String> {
            node.property(name)
                .and_then(|p| p.as_str().map(str::to_owned))
        }
        fn pu32<N: NodeView>(node: &N, name: &str) -> Option<u32> {
            node.property(name).and_then(|p| p.as_u32())
        }

        /// O1: the overlay authors the whole /cpus subtree — the container (with
        /// its cell properties) and every cpu instance (incl. cpu@0), unique reg,
        /// no phandle.
        #[test]
        fn authors_whole_cpus_subtree_with_unique_reg() {
            let dtbo = synth(4, None, None);
            let tree: Tree<'_> = Tree::parse(&dtbo).unwrap();
            let cpus = tree
                .find_path("/fragment@0/__overlay__/cpus")
                .expect("overlay authors /cpus container");
            assert_eq!(pu32(&cpus, "#address-cells"), Some(1));
            assert_eq!(pu32(&cpus, "#size-cells"), Some(0));
            for n in 0..4u32 {
                let cpu = tree
                    .find_path(&format!("/fragment@0/__overlay__/cpus/cpu@{n}"))
                    .unwrap_or_else(|| panic!("cpu@{n} present"));
                assert_eq!(pstr(&cpu, "device_type").as_deref(), Some("cpu"));
                assert_eq!(pu32(&cpu, "reg"), Some(n));
                assert_eq!(pstr(&cpu, "status").as_deref(), Some("okay"));
                assert!(cpu.property("phandle").is_none());
                assert!(cpu.property("linux,phandle").is_none());
            }
        }

        /// O3: x86-style cpus carry no enable-method and no compatible.
        #[test]
        fn x86_cpus_have_no_enable_method_or_compatible() {
            let dtbo = synth(2, None, None);
            let tree: Tree<'_> = Tree::parse(&dtbo).unwrap();
            let cpu0 = tree
                .find_path("/fragment@0/__overlay__/cpus/cpu@0")
                .unwrap();
            assert!(cpu0.property("enable-method").is_none());
            assert!(cpu0.property("compatible").is_none());
        }

        /// O2: aarch64-style cpus carry psci + the registered compatible.
        #[test]
        fn aarch64_cpus_carry_psci_and_compatible_when_known() {
            let dtbo = synth(2, Some("psci"), Some("arm,neoverse-v2"));
            let tree: Tree<'_> = Tree::parse(&dtbo).unwrap();
            for n in 0..2u32 {
                let cpu = tree
                    .find_path(&format!("/fragment@0/__overlay__/cpus/cpu@{n}"))
                    .unwrap();
                assert_eq!(pstr(&cpu, "enable-method").as_deref(), Some("psci"));
                assert_eq!(pstr(&cpu, "compatible").as_deref(), Some("arm,neoverse-v2"));
            }
        }

        /// O2 (unknown core) + single-cpu: psci kept, compatible omitted, cpu@0 authored.
        #[test]
        fn aarch64_unknown_core_omits_compatible_but_keeps_psci() {
            let dtbo = synth(1, Some("psci"), None);
            let tree: Tree<'_> = Tree::parse(&dtbo).unwrap();
            let cpu0 = tree
                .find_path("/fragment@0/__overlay__/cpus/cpu@0")
                .unwrap();
            assert_eq!(pstr(&cpu0, "enable-method").as_deref(), Some("psci"));
            assert!(cpu0.property("compatible").is_none());
        }
    }
}

mod launch {

    use std::fs::File;
    use std::io::Read;
    use std::path::Path;

    use dillo_devtree::platform::{Arch, Machine as PlatformMachine, SurveyError};
    use dillo_machine::HostArchitecture;
    use thiserror::Error;

    use crate::placement::{self, MemoryPlan};
    use dillo::pmi_parse::{Action as PmiAction, FillKind, HostArch, ParseOptions};

    /// Target-neutral launch facts derived before backend construction.
    #[derive(Debug)]
    pub(crate) struct LaunchPlan {
        pub(crate) parsed: dillo::pmi_parse::ParsedPmi,
        pub(crate) merged_dtb: Vec<u8>,
        pub(crate) platform: PlatformMachine,
        pub(crate) memory: MemoryPlan,
        pub(crate) guest_writes: Vec<GuestWrite>,
    }

    /// One launch-time write into guest RAM.
    #[derive(Debug)]
    pub(crate) struct GuestWrite {
        pub(crate) section: String,
        pub(crate) gpa: u64,
        pub(crate) data: Vec<u8>,
    }

    impl LaunchPlan {
        /// Read a PMI file, validate target-neutral launch facts, and compute RAM
        /// placement from the DTB-declared platform.
        pub(crate) fn read(
            pmi_path: &Path,
            host_arch: HostArchitecture,
            cpu_compatible: Option<&str>,
            memory_mib: u32,
            vcpus: u32,
        ) -> Result<Self, LaunchError> {
            let mut bytes = Vec::new();
            File::open(pmi_path)
                .map_err(|source| LaunchError::ReadPmi {
                    path: pmi_path.display().to_string(),
                    source,
                })?
                .read_to_end(&mut bytes)
                .map_err(|source| LaunchError::ReadPmi {
                    path: pmi_path.display().to_string(),
                    source,
                })?;

            let pmi_arch = pmi_arch(host_arch);
            let parsed = dillo::pmi_parse::parse(
                &bytes,
                &ParseOptions {
                    host_arch: pmi_arch,
                    memory_mib,
                },
            )?;
            validate_cpu_profile(parsed.cpu_profile.as_str(), pmi_arch)?;

            let dtb = merged_dtb(&bytes, &parsed)?.to_vec();
            let platform = PlatformMachine::survey(&dtb, platform_arch(host_arch))
                .map_err(LaunchError::Coverage)?;

            let load_ranges: Vec<(String, u64, u64)> = parsed
                .sections
                .iter()
                .map(|(name, section)| (name.clone(), section.gpa, section.virtual_size))
                .collect();
            platform
                .plan
                .cross_validate_loads(&load_ranges)
                .map_err(LaunchError::DtbCrossValidate)?;

            let must_cover: Vec<(u64, u64)> = parsed
                .sections
                .values()
                .map(|section| (section.gpa, section.virtual_size))
                .collect();
            let memory = placement::plan_around_regions(
                &must_cover,
                memory_mib,
                platform.placement_regions(),
            )?;
            let guest_writes = guest_writes(
                &bytes,
                &parsed,
                &memory,
                platform.arch,
                cpu_compatible,
                vcpus,
            )?;

            Ok(Self {
                parsed,
                merged_dtb: dtb,
                platform,
                memory,
                guest_writes,
            })
        }
    }

    fn pmi_arch(host_arch: HostArchitecture) -> HostArch {
        match host_arch {
            HostArchitecture::X86_64 => HostArch::X86_64,
            HostArchitecture::Aarch64 => HostArch::Aarch64,
        }
    }

    fn platform_arch(host_arch: HostArchitecture) -> Arch {
        match host_arch {
            HostArchitecture::X86_64 => Arch::X86_64,
            HostArchitecture::Aarch64 => Arch::Aarch64,
        }
    }

    /// Error produced by target-neutral launch preflight.
    #[derive(Debug, Error)]
    pub(crate) enum LaunchError {
        #[error("read PMI {path}: {source}")]
        ReadPmi {
            path: String,
            #[source]
            source: std::io::Error,
        },

        #[error("PMI parse: {0}")]
        PmiParse(#[from] dillo::pmi_parse::Error),

        #[error("merged_dtb section missing from parsed.sections")]
        MissingMergedDtb,

        #[error("base DTB coverage: {0}")]
        Coverage(SurveyError),

        #[error("base DTB / PE cross-validation: {0}")]
        DtbCrossValidate(SurveyError),

        #[error("merged_dtb section lies outside the PMI file")]
        MalformedMergedDtb,

        #[error("memory placement: {0}")]
        Placement(#[from] placement::PlanError),

        #[error("unrecognized cpu:profile {0:?} for {1:?}")]
        UnknownCpuProfile(String, HostArch),

        #[error("load section `{0}` lies outside the PMI file")]
        MalformedLoadSection(String),

        #[error("synthesize host DTBO: {0}")]
        DtboSynth(#[source] anyhow::Error),
    }

    impl LaunchError {
        /// Map to the documented dillo launch exit code categories.
        #[must_use]
        pub(crate) fn exit_code(&self) -> i32 {
            match self {
                Self::ReadPmi { .. } | Self::PmiParse(_) => 10,
                Self::MissingMergedDtb
                | Self::Coverage(_)
                | Self::DtbCrossValidate(_)
                | Self::MalformedMergedDtb
                | Self::MalformedLoadSection(_)
                | Self::DtboSynth(_) => 11,
                Self::Placement(_) => 13,
                Self::UnknownCpuProfile(_, _) => 12,
            }
        }
    }

    fn guest_writes(
        bytes: &[u8],
        parsed: &dillo::pmi_parse::ParsedPmi,
        memory: &MemoryPlan,
        arch: Arch,
        cpu_compatible: Option<&str>,
        vcpus: u32,
    ) -> Result<Vec<GuestWrite>, LaunchError> {
        let mut writes = Vec::new();
        for action in &parsed.actions {
            match action {
                PmiAction::Load { section } => {
                    let s = &parsed.sections[section];
                    if s.file_size == 0 {
                        continue;
                    }
                    let data = read_section(bytes, s.file_offset, s.file_size)
                        .ok_or_else(|| LaunchError::MalformedLoadSection(section.clone()))?
                        .to_vec();
                    writes.push(GuestWrite {
                        section: section.clone(),
                        gpa: s.gpa,
                        data,
                    });
                }
                PmiAction::Fill {
                    section,
                    kind: FillKind::MergedDtbo,
                } => {
                    let s = &parsed.sections[section];
                    let data = crate::overlay::synthesize_dtbo(
                        &memory.memory_nodes,
                        vcpus,
                        arch.cpu_enable_method(),
                        cpu_compatible,
                        s.virtual_size,
                    )
                    .map_err(LaunchError::DtboSynth)?;
                    writes.push(GuestWrite {
                        section: section.clone(),
                        gpa: s.gpa,
                        data,
                    });
                }
            }
        }
        Ok(writes)
    }

    fn merged_dtb<'a>(
        bytes: &'a [u8],
        parsed: &dillo::pmi_parse::ParsedPmi,
    ) -> Result<&'a [u8], LaunchError> {
        let dtb_info = parsed
            .sections
            .get(&parsed.merged_dtb_section)
            .ok_or(LaunchError::MissingMergedDtb)?;
        read_section(bytes, dtb_info.file_offset, dtb_info.file_size)
            .ok_or(LaunchError::MalformedMergedDtb)
    }

    fn read_section(bytes: &[u8], offset: u64, size: u64) -> Option<&[u8]> {
        let start = usize::try_from(offset).ok()?;
        let size = usize::try_from(size).ok()?;
        let end = start.checked_add(size)?;
        bytes.get(start..end)
    }

    /// Validate the `cpu:profile` name against the PMI machine architecture.
    pub(crate) fn validate_cpu_profile(profile: &str, arch: HostArch) -> Result<(), LaunchError> {
        let recognized = match arch {
            HostArch::Aarch64 => parse_armv_profile(profile).is_some(),
            HostArch::X86_64 => matches!(
                profile,
                "x86-64-v1" | "x86-64-v2" | "x86-64-v3" | "x86-64-v4"
            ),
        };
        if recognized {
            Ok(())
        } else {
            Err(LaunchError::UnknownCpuProfile(profile.to_string(), arch))
        }
    }

    fn parse_armv_profile(s: &str) -> Option<(u32, u32)> {
        let body = s.strip_prefix("armv")?.strip_suffix("-a")?;
        let (major, minor) = body.split_once('.')?;
        Some((major.parse().ok()?, minor.parse().ok()?))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn cpu_profile_validation_accepts_known_x86_levels() {
            for profile in ["x86-64-v1", "x86-64-v2", "x86-64-v3", "x86-64-v4"] {
                validate_cpu_profile(profile, HostArch::X86_64).expect(profile);
            }
        }

        #[test]
        fn cpu_profile_validation_accepts_armv_profiles() {
            validate_cpu_profile("armv8.0-a", HostArch::Aarch64).expect("armv8.0-a");
            validate_cpu_profile("armv9.2-a", HostArch::Aarch64).expect("armv9.2-a");
        }

        #[test]
        fn cpu_profile_validation_rejects_arch_mismatch() {
            assert!(validate_cpu_profile("armv8.0-a", HostArch::X86_64).is_err());
            assert!(validate_cpu_profile("x86-64-v2", HostArch::Aarch64).is_err());
        }

        #[test]
        fn read_section_checks_bounds() {
            assert_eq!(read_section(b"abcdef", 1, 3), Some(&b"bcd"[..]));
            assert!(read_section(b"abcdef", 4, 3).is_none());
            assert!(read_section(b"abcdef", u64::MAX, 1).is_none());
        }
    }
}

mod machine_select {
    #[cfg(target_os = "linux")]
    pub(crate) use dillo_machine_kvm as machine;

    #[cfg(target_os = "macos")]
    pub(crate) use dillo_machine_hvf as machine;

    #[cfg(target_os = "windows")]
    pub(crate) use dillo_machine_whp as machine;

    pub(crate) mod runner {
        mod error {

            use thiserror::Error;

            /// Exit-code-bearing error for the VM-side run loop. Each variant
            /// corresponds to one of ARCH §13.4's documented categories.
            #[derive(Debug, Error)]
            #[allow(dead_code)]
            pub(crate) enum RunError {
                // ── exit 10 — PMI parse / validation ───────────────────────────
                #[error("read PMI {path}: {source}")]
                ReadPmi {
                    path: String,
                    #[source]
                    source: std::io::Error,
                },

                #[error("PMI parse: {0}")]
                PmiParse(#[from] dillo::pmi_parse::Error),

                // ── exit 11 — DTB parse / validation ───────────────────────────
                #[error("base DTB extraction: {0}")]
                DtbExtract(dillo_devtree::platform::SurveyError),

                #[error("base DTB coverage (undeclared hardware / unclaimed node): {0}")]
                Coverage(dillo_devtree::platform::SurveyError),

                #[error("base DTB ↔ PE cross-validation: {0}")]
                DtbCrossValidate(dillo_devtree::platform::SurveyError),

                #[error("base DTB is missing required device `{0}`")]
                MissingRequiredDevice(&'static str),

                #[error("synthesize host DTBO: {0}")]
                DtboSynth(#[source] anyhow::Error),

                #[error("write DTBO section `{section}` to GPA {gpa:#x}: {source}")]
                DtboWrite {
                    section: String,
                    gpa: u64,
                    #[source]
                    source: anyhow::Error,
                },

                // ── exit 12 — Hypervisor init failed ───────────────────────────
                #[error("machine: {0}")]
                Machine(String),

                #[error("write load section `{section}` to GPA {gpa:#x}: {source}")]
                SectionWrite {
                    section: String,
                    gpa: u64,
                    #[source]
                    source: anyhow::Error,
                },

                // ── exit 13 — Host RAM check ───────────────────────────────────
                #[error(
                    "host RAM ({available_mib} MiB) insufficient for guest ({requested_mib} MiB) + \
                     {overhead_mib} MiB overhead"
                )]
                HostRam {
                    requested_mib: u64,
                    overhead_mib: u64,
                    available_mib: u64,
                },

                #[error("memory placement: {source}")]
                Placement {
                    #[source]
                    source: anyhow::Error,
                },
                // ── exit 20 — Guest crash ──────────────────────────────────────
            }

            impl RunError {
                pub(crate) fn machine(source: impl std::error::Error) -> Self {
                    Self::Machine(source.to_string())
                }

                /// Map to the documented exit code from ARCH §13.4.
                #[must_use]
                pub(crate) fn exit_code(&self) -> i32 {
                    match self {
                        Self::ReadPmi { .. } | Self::PmiParse(_) => 10,
                        Self::DtbExtract(_)
                        | Self::Coverage(_)
                        | Self::DtbCrossValidate(_)
                        | Self::MissingRequiredDevice(_)
                        | Self::DtboSynth(_)
                        | Self::DtboWrite { .. } => 11,
                        Self::Machine(_) | Self::SectionWrite { .. } => 12,
                        Self::HostRam { .. } => 13,
                        Self::Placement { .. } => 13,
                    }
                }
            }
        }

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
            Attach, InterruptSource, MessageInterruptSource, MmioAttachment,
            MmioInterruptRequirement, MmioWindow,
        };
        use dillo_pci::PciRoot;
        use dillo_pci_virtio::VirtioPciAdapter;

        pub(crate) use self::error::RunError;

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
        }

        impl SupervisorControl {
            fn stop_requested(&self) -> Option<VcpuStop> {
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
            let console: Arc<std::sync::Mutex<Box<dyn dillo_virtio::VirtioDevice>>> = Arc::new(
                std::sync::Mutex::new(Box::new(dillo_virtio_console::VirtioConsole::new())),
            );

            let virtio_pci_dev = dillo_pci_virtio::VirtioPciDevice::new(
                console,
                vectors,
                platform.pcie.mmio_base,
                platform.pcie.mmio_base + 0x1000,
            );
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
            let attachment =
                Attach::attach(vm, Arc::clone(&pci_root)).map_err(RunError::machine)?;
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
            M: Attach<
                    Arc<dillo_mmio_virtio::VirtioMmio>,
                    Error = E,
                    Output = Arc<dyn MmioAttachment>,
                >,
        {
            let Some(slot) = platform.virtio_mmio.first() else {
                return Ok(());
            };

            let int_status = Arc::new(std::sync::atomic::AtomicU32::new(0));
            let irq = dillo_mmio_virtio::WiredIrq::unresolved(slot.irq);
            let transport = Arc::new(dillo_mmio_virtio::VirtioMmio::with_interrupt_requirement(
                MmioWindow {
                    base: slot.base,
                    size: slot.size,
                },
                Box::new(dillo_virtio_console::VirtioConsole::new()),
                Arc::clone(&int_status),
                irq.clone(),
                line_requirement(&slot.interrupt),
            ));
            let attachment =
                Attach::attach(vm, Arc::clone(&transport)).map_err(RunError::machine)?;
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

        fn attach_syscon<M, E>(vm: &mut M, platform: &PlatformMachine) -> Result<(), RunError>
        where
            E: std::error::Error + Send + Sync + 'static,
            M: Machine<Error = E>,
            M: Attach<Arc<syscon::SysconDevice>, Error = E>,
        {
            let Some(poweroff) = platform.poweroff else {
                return Ok(());
            };
            Attach::attach(
                vm,
                Arc::new(syscon::SysconDevice::new(
                    syscon_register(poweroff),
                    syscon::SysconAction::Poweroff,
                )),
            )
            .map_err(RunError::machine)?;
            if let Some(reboot) = platform.reboot {
                Attach::attach(
                    vm,
                    Arc::new(syscon::SysconDevice::new(
                        syscon_register(reboot),
                        syscon::SysconAction::Reboot,
                    )),
                )
                .map_err(RunError::machine)?;
            }
            Ok(())
        }

        fn apply_load_sections<M: Machine>(
            vm: &mut M,
            guest_writes: &[RunWrite],
        ) -> Result<(), RunError> {
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
                let state = M::CpuState::new(index, cpu_profile, Some(boot_state))
                    .map_err(RunError::machine)?;
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
                    let all_cpus = cpus.clone();
                    let shutdown = Arc::clone(&shutdown);
                    joins.push(scope.spawn(move || -> Result<VcpuStop, String> {
                        if shutdown.load(Ordering::Acquire) {
                            return Ok(VcpuStop::Stopped);
                        }
                        let result = cpu.run().map_err(|e| e.to_string());
                        shutdown.store(true, Ordering::Release);
                        for cpu in &all_cpus {
                            let _ = cpu.stop();
                        }
                        result
                    }));
                }

                let monitor = {
                    let control = Arc::clone(&control);
                    let cpus = cpus.clone();
                    let shutdown = Arc::clone(&shutdown);
                    scope.spawn(move || {
                        let mut stop_requested = false;
                        while !shutdown.load(Ordering::Acquire) {
                            if stop_requested || control.stop_requested().is_some() {
                                stop_requested = true;
                                for cpu in &cpus {
                                    let _ = cpu.stop();
                                }
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
            M: Attach<
                    Arc<dillo_mmio_virtio::VirtioMmio>,
                    Error = E,
                    Output = Arc<dyn MmioAttachment>,
                >,
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
            attach_syscon(&mut vm, &platform)?;
            attach_pci_console(&mut vm, &platform)?;
            attach_first_virtio_mmio_console(&mut vm, &platform)?;

            let control = Arc::new(SupervisorControl {
                supervisor_shutdown,
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
                log::info!("guest requested reboot - replaying launch writes");
                vm.reset_for_reboot().map_err(RunError::machine)?;
                apply_load_sections(&mut vm, &guest_writes)?;
                let control = Arc::new(SupervisorControl {
                    supervisor_shutdown,
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
    }
}
