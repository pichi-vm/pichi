//! Platform extraction from PMI base DTBs.
//!
//! Walks a parsed base DTB once into a typed [`Platform`] struct
//! (PCIe topology, interrupt controller, syscon-poweroff register,
//! UART, PSCI method). The base must declare nothing CPU-related — a
//! base carrying a `/cpus` node is rejected; the host overlay authors
//! the entire `/cpus` subtree (PMI merged.md §1). All downstream code
//! consumes this typed struct; the raw DTB is dropped after extraction.
//!
//! See `dillo/ARCHITECTURE.md` §6.

use devtree::{NodeView, PropertyView, Tree, TreeView};
use thiserror::Error;

pub mod machine;
pub use machine::{
    DeclaredRegion, GicConfig, Machine, Origin, RegionKind, ResourcePlan, SurveyError,
};

/// Extracted platform definition.
#[derive(Debug, Clone)]
pub struct Platform {
    pub arch: Arch,
    /// The PCIe host bridge. Valid only when [`has_pcie`](Self::has_pcie) is
    /// true; a `--pci-slots 0` microVM declares no bridge, in which case this
    /// is a zeroed sentinel the VMM must not install.
    pub pcie: Pcie,
    /// Whether the image declares a PCIe host bridge (false ⇒ virtio-mmio-only
    /// microVM; skip all PCI fabric).
    pub has_pcie: bool,
    pub intc: Intc,
    /// Full GICv3 + v2m configuration derived from `/intc`+`/v2m` (aarch64
    /// only). The hypervisor programs the in-kernel GIC from THIS, not from
    /// hardcoded constants — addresses are Arma's to assign (device-model §4).
    pub gic: Option<GicConfig>,
    pub ioapic: Option<MmioRegion>,
    pub poweroff: Syscon,
    pub reboot: Option<Syscon>,
    pub uart: Option<Uart>,
    pub psci: Option<Psci>,
    /// virtio-mmio transport slots (device-model §4 virtio-mmio). Each is a
    /// fixed MMIO window + a wired IRQ; the VMM may back the first N with
    /// devices, the rest read DeviceID 0 (empty).
    pub virtio_mmio: Vec<VirtioMmio>,
    /// Every device MMIO region `(base, size)`, arch-specifically enumerated
    /// but generically consumed: guest RAM placement MUST NOT overlap any of
    /// these, and `/memory@N` MUST exclude them. aarch64: GIC distributor +
    /// redistributor + MSI frame, serial UART, PCIe ECAM, PCIe MMIO. x86: LAPIC,
    /// I/O APIC, syscon(s), PCIe ECAM, PCIe MMIO.
    pub device_regions: Vec<(u64, u64)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    pub fn cpu_enable_method(self) -> Option<&'static str> {
        match self {
            Arch::Aarch64 => Some("psci"),
            Arch::X86_64 => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Pcie {
    pub ecam_base: u64,
    pub ecam_size: u64,
    pub bus_min: u8,
    pub bus_max: u8,
    pub mmio_base: u64,
    pub mmio_size: u64,
}

impl Pcie {
    /// Sentinel for a microVM with no PCIe bridge (`has_pcie == false`).
    pub const ZEROED: Pcie = Pcie {
        ecam_base: 0,
        ecam_size: 0,
        bus_min: 0,
        bus_max: 0,
        mmio_base: 0,
        mmio_size: 0,
    };
}

#[derive(Debug, Clone, Copy)]
pub struct Intc {
    pub kind: IntcKind,
    pub base: u64,
    pub size: u64,
}

/// A virtio-mmio transport slot: a fixed MMIO window + its wired IRQ.
#[derive(Debug, Clone, Copy)]
pub struct VirtioMmio {
    pub base: u64,
    pub size: u64,
    /// GIC SPI number from `interrupts = <0 spi flags>` (aarch64), or the
    /// IO-APIC pin (x86). The VMM injects this when the slot's device signals.
    pub irq: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct MmioRegion {
    pub base: u64,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntcKind {
    /// x86 LAPIC (e.g., `intel,ce4100-lapic`).
    Lapic,
    /// `arm,gic-v3`.
    GicV3,
}

#[derive(Debug, Clone, Copy)]
pub struct Syscon {
    pub base: u64,
    pub offset: u64,
    pub value: u32,
    pub mask: u32,
}

/// The device-model serial: an `ns16550a` (16550) over MMIO, both arches.
#[derive(Debug, Clone, Copy)]
pub struct Uart {
    pub base: u64,
    pub size: u64,
    /// `reg-shift`: the register stride is `1 << reg_shift` bytes (ns16550a
    /// over MMIO uses `reg-shift = 2`, so register N lives at offset `N << 2`).
    pub reg_shift: u32,
    /// `interrupts` cell 0: the IRQ line the VMM wires to an irqfd. On x86
    /// this is the IO-APIC pin (GSI); on aarch64 it's the GIC interrupt's
    /// first cell. Used to route the serial IRQ on the Linux/KVM path.
    pub irq: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct Psci {
    pub method: PsciMethod,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PsciMethod {
    Hvc,
    Smc,
}

/// Failures during platform extraction.
#[derive(Debug, Error)]
pub enum Error {
    #[error("base DTB parse failed: {0:?}")]
    DtbParse(devtree::Error),

    #[error("root #address-cells = {0}; expected 2")]
    BadRootAddressCells(u32),
    #[error("root #size-cells = {0}; expected 2")]
    BadRootSizeCells(u32),

    #[error("required node `{0}` not found")]
    MissingNode(&'static str),
    #[error(
        "base DTB declares a `/cpus` node; the base must declare nothing \
         CPU-related — the host overlay authors the entire `/cpus` subtree \
         (PMI merged.md §1)"
    )]
    BaseHasCpus,
    #[error("required property `{prop}` not found on `{node}`")]
    MissingProperty { node: String, prop: &'static str },
    #[error("property `{prop}` on `{node}` has wrong encoding ({reason})")]
    BadPropertyEncoding {
        node: String,
        prop: &'static str,
        reason: &'static str,
    },

    #[error("intc compatible `{0}` is not in allowlist")]
    UnsupportedIntc(String),
    #[error("PCIe compatible `{0}` is not `pci-host-ecam-generic`")]
    UnsupportedPcie(String),
    #[error("PSCI method `{0}` is not `hvc` or `smc`")]
    UnsupportedPsciMethod(String),
    #[error("UART compatible `{0}` is not in allowlist")]
    UnsupportedUart(String),
    #[error("PCIe ECAM and MMIO ranges overlap")]
    PcieEcamMmioOverlap,
    #[error("base-DTB address {addr:#x} exceeds canonical 2^48 bound")]
    AddressOutOfCanonicalBound { addr: u64 },
    #[error("syscon-poweroff regmap phandle {0} does not match any /syscon@*")]
    PoweroffPhandleMismatch(u32),
    #[error(
        "loaded section `{section}` at [{start:#x}..{end:#x}) overlaps base-DTB MMIO region [{mmio_start:#x}..{mmio_end:#x})"
    )]
    SectionOverlapsMmio {
        section: String,
        start: u64,
        end: u64,
        mmio_start: u64,
        mmio_end: u64,
    },
}

/// Parse a base-DTB blob and extract the typed [`Platform`].
///
/// `arch` MUST match the host architecture; it is used to pick the
/// expected compatible allowlist for arch-specific nodes (PSCI, serial).
pub fn extract(dtb: &[u8], arch: Arch) -> Result<Platform, Error> {
    let tree: Tree<'_> = Tree::parse(dtb).map_err(Error::DtbParse)?;
    let root = tree.root();

    // Validate root #address-cells / #size-cells == 2.
    if let Some(p) = root.property("#address-cells") {
        let v = p.as_u32().ok_or(Error::BadPropertyEncoding {
            node: "/".to_string(),
            prop: "#address-cells",
            reason: "expected single u32",
        })?;
        if v != 2 {
            return Err(Error::BadRootAddressCells(v));
        }
    } else {
        return Err(Error::MissingProperty {
            node: "/".to_string(),
            prop: "#address-cells",
        });
    }
    if let Some(p) = root.property("#size-cells") {
        let v = p.as_u32().ok_or(Error::BadPropertyEncoding {
            node: "/".to_string(),
            prop: "#size-cells",
            reason: "expected single u32",
        })?;
        if v != 2 {
            return Err(Error::BadRootSizeCells(v));
        }
    } else {
        return Err(Error::MissingProperty {
            node: "/".to_string(),
            prop: "#size-cells",
        });
    }

    reject_base_cpus(&root)?;
    let (pcie, has_pcie) = match extract_pcie_opt(&root)? {
        Some(p) => (p, true),
        None => (Pcie::ZEROED, false),
    };
    let intc = extract_intc(&root, arch)?;
    let gic = if matches!(arch, Arch::Aarch64) {
        Some(extract_gic(&root)?)
    } else {
        None
    };
    let virtio_mmio = extract_virtio_mmio(&root)?;
    // x86 shuts down via a syscon-poweroff MMIO device; aarch64 uses PSCI
    // SYSTEM_OFF and declares no syscon, so `poweroff` is a zeroed sentinel
    // there (never read by the aarch64/HVF run loop, which handles the PSCI
    // HVC directly).
    let (poweroff, reboot) = if matches!(arch, Arch::Aarch64) {
        (
            Syscon {
                base: 0,
                offset: 0,
                value: 0,
                mask: 0,
            },
            None,
        )
    } else {
        extract_syscon(&root)?
    };
    let psci = if matches!(arch, Arch::Aarch64) {
        Some(extract_psci(&root)?)
    } else {
        None
    };
    // ns16550a serial is optional (arma emits it only with `--serial`) and
    // is MMIO on both arches per the device model. Absent → None;
    // present-but-malformed propagates as an error.
    let uart = extract_serial_opt(&root)?;
    let intc_node = root
        .children()
        .find(|c| has_compatible(c, intc_compatible(arch)))
        .ok_or(Error::MissingNode("interrupt controller"))?;
    // F2: the IO-APIC is now its OWN node (intel,ce4100-ioapic), not reg[1] of
    // the LAPIC. Find it by compatible.
    let ioapic = if arch == Arch::X86_64 {
        root.children()
            .find(|c| has_compatible(c, "intel,ce4100-ioapic"))
            .and_then(|n| read_reg_pair(&n, 0))
            .map(|(base, size)| MmioRegion { base, size })
    } else {
        None
    };

    // Enumerate every device MMIO region (arch-specific; generic output).
    let mut device_regions: Vec<(u64, u64)> = Vec::new();
    if has_pcie {
        device_regions.push((pcie.ecam_base, pcie.ecam_size));
        device_regions.push((pcie.mmio_base, pcie.mmio_size));
    }
    for v in &virtio_mmio {
        device_regions.push((v.base, v.size));
    }
    match arch {
        Arch::Aarch64 => {
            // GIC distributor (reg pair 0 = intc.base/size) + redistributor
            // (reg pair 1), the GICv2m MSI frame, then the serial UART.
            device_regions.push((intc.base, intc.size));
            if let Some(gicr) = read_reg_pair(&intc_node, 1) {
                device_regions.push(gicr);
            }
            // GICv2m MSI frame: a root node (compatible arm,gic-v2m-frame),
            // reg pair 0 = the MSI doorbell frame.
            if let Some(v2m) = root
                .children()
                .find(|c| has_compatible(c, "arm,gic-v2m-frame"))
                && let Some(frame) = read_reg_pair(&v2m, 0)
            {
                device_regions.push(frame);
            }
            if let Some(u) = uart {
                device_regions.push((u.base, u.size));
            }
        }
        Arch::X86_64 => {
            device_regions.push((intc.base, intc.size)); // LAPIC
            if let Some(ioapic) = ioapic {
                device_regions.push((ioapic.base, ioapic.size));
            }
            device_regions.push((poweroff.base, 0x1000));
            if let Some(r) = reboot {
                device_regions.push((r.base, 0x1000));
            }
            if let Some(u) = uart {
                device_regions.push((u.base, u.size));
            }
        }
    }

    Ok(Platform {
        arch,
        pcie,
        has_pcie,
        intc,
        gic,
        ioapic,
        poweroff,
        reboot,
        uart,
        psci,
        virtio_mmio,
        device_regions,
    })
}

/// Enumerate every `virtio,mmio` transport slot (by compatible), reading each
/// node's `reg` window and `interrupts` SPI/pin (cell 1 of the 3-cell SPI form
/// on aarch64, the pin on x86).
fn extract_virtio_mmio(root: &impl NodeView) -> Result<Vec<VirtioMmio>, Error> {
    let mut out = Vec::new();
    for c in root.children() {
        if !has_compatible(&c, "virtio,mmio") {
            continue;
        }
        let (base, size) = read_reg2(&c, "reg")?;
        let cells: Vec<u32> = match c.property("interrupts") {
            Some(prop) => prop.as_u32s().map(Iterator::collect).unwrap_or_default(),
            None => Vec::new(),
        };
        // aarch64 GIC SPI: <type number flags> ⇒ cell 1; x86 IO-APIC: <pin sense> ⇒ cell 0.
        let irq = cells.get(1).or_else(|| cells.first()).copied().unwrap_or(0);
        out.push(VirtioMmio { base, size, irq });
    }
    Ok(out)
}

/// Cross-validate the platform's MMIO regions against PMI load GPAs.
///
/// `loaded` is a list of `(section_name, gpa, size)` triples derived
/// from `ParsedPmi.sections`. Returns the first overlap or canonical-
/// bound violation as an error.
pub fn cross_validate_loads(
    platform: &Platform,
    loaded: &[(String, u64, u64)],
) -> Result<(), Error> {
    let mmio_regions: Vec<(&'static str, u64, u64)> = {
        let mut v = vec![("intc", platform.intc.base, platform.intc.size)];
        // poweroff is an x86 syscon MMIO register; aarch64 powers off via PSCI,
        // leaving poweroff.base = 0 (unused). A phantom [0x0..0x4) region would
        // otherwise collide with tatu's sections (which start at GPA 0 on arm).
        if platform.poweroff.base != 0 {
            v.push((
                "poweroff",
                platform.poweroff.base,
                platform.poweroff.offset + 4,
            ));
        }
        if platform.has_pcie {
            v.push((
                "pcie-ecam",
                platform.pcie.ecam_base,
                platform.pcie.ecam_size,
            ));
            v.push((
                "pcie-mmio",
                platform.pcie.mmio_base,
                platform.pcie.mmio_size,
            ));
        }
        if let Some(u) = &platform.uart {
            v.push(("uart", u.base, u.size));
        }
        v
    };

    // ECAM and MMIO must be non-overlapping with each other (when present).
    if platform.has_pcie
        && ranges_overlap(
            platform.pcie.ecam_base,
            platform.pcie.ecam_size,
            platform.pcie.mmio_base,
            platform.pcie.mmio_size,
        )
    {
        return Err(Error::PcieEcamMmioOverlap);
    }

    for (name, gpa, size) in loaded {
        if *size == 0 {
            continue;
        }
        let end = gpa
            .checked_add(*size)
            .ok_or(Error::AddressOutOfCanonicalBound { addr: *gpa })?;
        if u128::from(end) > (1u128 << 48) {
            return Err(Error::AddressOutOfCanonicalBound { addr: *gpa });
        }
        for (region_name, base, region_size) in &mmio_regions {
            if ranges_overlap(*gpa, *size, *base, *region_size) {
                return Err(Error::SectionOverlapsMmio {
                    section: name.clone(),
                    start: *gpa,
                    end,
                    mmio_start: *base,
                    mmio_end: base + region_size,
                });
            }
            let _ = region_name; // reserved for richer diagnostics later
        }
    }
    Ok(())
}

fn ranges_overlap(a: u64, asize: u64, b: u64, bsize: u64) -> bool {
    let a_end = a.saturating_add(asize);
    let b_end = b.saturating_add(bsize);
    a < b_end && b < a_end
}

// ─── per-node extraction ──────────────────────────────────────

/// Reject any base that declares a `/cpus` node. Per merged.md §1 the base
/// declares nothing CPU-related; the host overlay authors the entire `/cpus`
/// subtree (the container, its cell properties, and every `cpu@N`).
fn reject_base_cpus(root: &impl NodeView) -> Result<(), Error> {
    if root.child("cpus").is_some() {
        return Err(Error::BaseHasCpus);
    }
    Ok(())
}

fn extract_pcie_opt(root: &impl NodeView) -> Result<Option<Pcie>, Error> {
    // Find the host bridge by compatible (the node is named `pcie@…`, but
    // consumers match by binding, not node name). Absent ⇒ a `--pci-slots 0`
    // microVM (virtio-mmio only); the bridge is optional.
    let Some(node) = root
        .children()
        .find(|c| has_compatible(c, "pci-host-ecam-generic"))
    else {
        return Ok(None);
    };

    let compat_prop = node
        .property("compatible")
        .ok_or_else(|| Error::MissingProperty {
            node: node.name().to_string(),
            prop: "compatible",
        })?;
    let compat = compat_prop
        .as_str()
        .ok_or_else(|| Error::BadPropertyEncoding {
            node: node.name().to_string(),
            prop: "compatible",
            reason: "expected single string",
        })?;
    if compat != "pci-host-ecam-generic" {
        return Err(Error::UnsupportedPcie(compat.to_string()));
    }

    let reg = read_reg2(&node, "reg")?;
    let bus_range = node
        .property("bus-range")
        .ok_or_else(|| Error::MissingProperty {
            node: node.name().to_string(),
            prop: "bus-range",
        })?;
    let bus_cells: Vec<u32> = bus_range
        .as_u32s()
        .ok_or_else(|| Error::BadPropertyEncoding {
            node: node.name().to_string(),
            prop: "bus-range",
            reason: "expected two u32 cells",
        })?
        .collect();
    if bus_cells.len() != 2 {
        return Err(Error::BadPropertyEncoding {
            node: node.name().to_string(),
            prop: "bus-range",
            reason: "expected exactly two u32 cells",
        });
    }

    let (ranges_base, ranges_size) = parse_pci_ranges_first_window(&node)?;

    Ok(Some(Pcie {
        ecam_base: reg.0,
        ecam_size: reg.1,
        bus_min: bus_cells[0] as u8,
        bus_max: bus_cells[1] as u8,
        mmio_base: ranges_base,
        mmio_size: ranges_size,
    }))
}

/// Read a `reg = <hi32 lo32 hi32 lo32>` property as `(base_u64, size_u64)`.
fn read_reg2(node: &impl NodeView, prop_name: &'static str) -> Result<(u64, u64), Error> {
    let prop = node
        .property(prop_name)
        .ok_or_else(|| Error::MissingProperty {
            node: node.name().to_string(),
            prop: prop_name,
        })?;
    let cells: Vec<u32> = prop
        .as_u32s()
        .ok_or_else(|| Error::BadPropertyEncoding {
            node: node.name().to_string(),
            prop: prop_name,
            reason: "not a multiple-of-4 byte slice",
        })?
        .collect();
    if cells.len() < 4 {
        return Err(Error::BadPropertyEncoding {
            node: node.name().to_string(),
            prop: prop_name,
            reason: "expected at least 4 u32 cells (2 for address, 2 for size)",
        });
    }
    let base = (u64::from(cells[0]) << 32) | u64::from(cells[1]);
    let size = (u64::from(cells[2]) << 32) | u64::from(cells[3]);
    Ok((base, size))
}

/// Read the `idx`-th `(base, size)` pair from a `reg` (2 address + 2 size
/// cells per entry). `None` if absent or too short. Used to pick the GIC
/// redistributor (pair 1) out of the GICv3 `reg = <GICD…, GICR…>`.
fn read_reg_pair(node: &impl NodeView, idx: usize) -> Option<(u64, u64)> {
    let prop = node.property("reg")?;
    let cells: Vec<u32> = prop.as_u32s()?.collect();
    let i = idx * 4;
    if cells.len() < i + 4 {
        return None;
    }
    let base = (u64::from(cells[i]) << 32) | u64::from(cells[i + 1]);
    let size = (u64::from(cells[i + 2]) << 32) | u64::from(cells[i + 3]);
    Some((base, size))
}

/// First `ranges` window: `<phys.hi phys.mid phys.lo cpu.hi cpu.lo size.hi size.lo>`.
fn parse_pci_ranges_first_window(node: &impl NodeView) -> Result<(u64, u64), Error> {
    let prop = node
        .property("ranges")
        .ok_or_else(|| Error::MissingProperty {
            node: node.name().to_string(),
            prop: "ranges",
        })?;
    let cells: Vec<u32> = prop
        .as_u32s()
        .ok_or_else(|| Error::BadPropertyEncoding {
            node: node.name().to_string(),
            prop: "ranges",
            reason: "not a multiple-of-4 byte slice",
        })?
        .collect();
    // First 7-cell tuple. phys.hi (cells[0]) carries the space code; we
    // accept any (caller validated compatible above).
    if cells.len() < 7 {
        return Err(Error::BadPropertyEncoding {
            node: node.name().to_string(),
            prop: "ranges",
            reason: "expected at least 7 u32 cells for one window",
        });
    }
    let cpu_base = (u64::from(cells[3]) << 32) | u64::from(cells[4]);
    let size = (u64::from(cells[5]) << 32) | u64::from(cells[6]);
    Ok((cpu_base, size))
}

/// True iff the node's `compatible` (single string or stringlist) advertises
/// `want`. Consumers match devices by binding, never by node name (the
/// device-model addresses/names are illustrative; the binding is the contract).
fn has_compatible(node: &impl NodeView, want: &str) -> bool {
    let Some(prop) = node.property("compatible") else {
        return false;
    };
    let Some(mut strs) = prop.as_strs() else {
        return false;
    };
    strs.any(|s| s == want)
}

/// The interrupt-controller binding Arma emits for this arch.
fn intc_compatible(arch: Arch) -> &'static str {
    match arch {
        Arch::Aarch64 => "arm,gic-v3",
        Arch::X86_64 => "intel,ce4100-lapic",
    }
}

/// Derive the full GICv3 + v2m configuration from `/intc` (dist = reg pair 0,
/// redist = reg pair 1) and `/v2m` (MSI frame = reg pair 0, `arm,msi-base-spi`
/// / `arm,msi-num-spis`). The hypervisor programs the in-kernel GIC from these
/// DTB-derived values (F7a) — never from constants.
fn extract_gic(root: &impl NodeView) -> Result<GicConfig, Error> {
    let intc = root
        .children()
        .find(|c| has_compatible(c, "arm,gic-v3"))
        .ok_or(Error::MissingNode("arm,gic-v3 interrupt controller"))?;
    let (dist_base, dist_size) =
        read_reg_pair(&intc, 0).ok_or(Error::MissingNode("GIC distributor reg"))?;
    let (redist_base, redist_size) =
        read_reg_pair(&intc, 1).ok_or(Error::MissingNode("GIC redistributor reg"))?;

    let v2m = root
        .children()
        .find(|c| has_compatible(c, "arm,gic-v2m-frame"))
        .ok_or(Error::MissingNode("arm,gic-v2m-frame MSI controller"))?;
    let (msi_frame_base, msi_frame_size) =
        read_reg_pair(&v2m, 0).ok_or(Error::MissingNode("GICv2m frame reg"))?;
    let spi_base = read_u32_prop(&v2m, "arm,msi-base-spi")?;
    let spi_count = read_u32_prop(&v2m, "arm,msi-num-spis")?;

    Ok(GicConfig {
        dist_base,
        dist_size,
        redist_base,
        redist_size,
        msi_frame_base,
        msi_frame_size,
        spi_base,
        spi_count,
    })
}

fn extract_intc(root: &impl NodeView, arch: Arch) -> Result<Intc, Error> {
    let want = intc_compatible(arch);
    let node = root
        .children()
        .find(|c| has_compatible(c, want))
        .ok_or(Error::MissingNode("interrupt controller"))?;

    let kind = match arch {
        Arch::X86_64 => IntcKind::Lapic,
        Arch::Aarch64 => IntcKind::GicV3,
    };

    let (base, size) = read_reg2(&node, "reg")?;
    Ok(Intc { kind, base, size })
}

fn extract_syscon(root: &impl NodeView) -> Result<(Syscon, Option<Syscon>), Error> {
    // F4: poweroff/reset are standalone nodes, each with its OWN `reg` + a
    // trigger `value` (no `/syscon`, no `regmap`/`offset`). Matched by binding.
    let po_node = root
        .children()
        .find(|c| has_compatible(c, "syscon-poweroff"))
        .ok_or(Error::MissingNode("syscon-poweroff"))?;
    let po = read_syscon_action(&po_node)?;

    let reboot = root
        .children()
        .find(|c| has_compatible(c, "syscon-reboot"))
        .map(|n| read_syscon_action(&n))
        .transpose()?;

    Ok((po, reboot))
}

fn read_syscon_action(node: &impl NodeView) -> Result<Syscon, Error> {
    let (base, _size) = read_reg2(node, "reg")?;
    let value = read_u32_prop(node, "value")?;
    // Trigger is a write of `value` at the node's own reg (offset 0); match the
    // low byte (the guest's SLEEP/RESET register write is a single byte).
    Ok(Syscon {
        base,
        offset: 0,
        value,
        mask: 0xFF,
    })
}

fn read_u32_prop(node: &impl NodeView, prop_name: &'static str) -> Result<u32, Error> {
    node.property(prop_name)
        .ok_or_else(|| Error::MissingProperty {
            node: node.name().to_string(),
            prop: prop_name,
        })?
        .as_u32()
        .ok_or_else(|| Error::BadPropertyEncoding {
            node: node.name().to_string(),
            prop: prop_name,
            reason: "expected single u32",
        })
}

fn extract_psci(root: &impl NodeView) -> Result<Psci, Error> {
    // F1: PSCI is a root-level /psci node (arm,psci.yaml), not under /firmware.
    let psci = root.child("psci").ok_or(Error::MissingNode("/psci"))?;

    // Require an arm,psci-*.* compatible.
    let compat_prop = psci.property("compatible").ok_or(Error::MissingProperty {
        node: "/psci".to_string(),
        prop: "compatible",
    })?;
    let has_psci_compat = match compat_prop.as_strs() {
        Some(it) => it
            .collect::<Vec<_>>()
            .iter()
            .any(|s| s.starts_with("arm,psci-")),
        None => false,
    };
    if !has_psci_compat {
        return Err(Error::MissingProperty {
            node: "/psci".to_string(),
            prop: "compatible",
        });
    }

    let method_prop = psci.property("method").ok_or(Error::MissingProperty {
        node: "/psci".to_string(),
        prop: "method",
    })?;
    let method_str = method_prop.as_str().ok_or(Error::BadPropertyEncoding {
        node: "/psci".to_string(),
        prop: "method",
        reason: "expected single string",
    })?;
    let method = match method_str {
        "hvc" => PsciMethod::Hvc,
        "smc" => PsciMethod::Smc,
        other => return Err(Error::UnsupportedPsciMethod(other.to_string())),
    };
    Ok(Psci { method })
}

/// F3: find the serial port by compatible `ns16550a` (the node is named
/// `serial@…`, but consumers match by binding). Reads `reg` and `reg-shift`
/// (default 0 if absent). Absent node ⇒ `None` (serial is `--serial`-gated).
fn extract_serial_opt(root: &impl NodeView) -> Result<Option<Uart>, Error> {
    let Some(node) = root.children().find(|c| has_compatible(c, "ns16550a")) else {
        return Ok(None);
    };
    let (base, size) = read_reg2(&node, "reg")?;
    let reg_shift = node
        .property("reg-shift")
        .and_then(|p| p.as_u32())
        .unwrap_or(0);
    // `interrupts` cell 0: IO-APIC pin (x86) / GIC interrupt first cell.
    let irq = node
        .property("interrupts")
        .and_then(|p| p.as_u32s().and_then(|mut it| it.next()))
        .unwrap_or(0);
    Ok(Some(Uart {
        base,
        size,
        reg_shift,
        irq,
    }))
}
