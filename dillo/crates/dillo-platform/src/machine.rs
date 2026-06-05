//! Coverage-framework survey of the base DTB (the `merged` model).
//!
//! Where [`crate::extract`] reads the nodes it knows and ignores the rest,
//! `Machine::survey` proves **total coverage**: it materializes the base DTB
//! into an owned, drainable tree and runs a fixed **specific → general**
//! sequence of self-routing device constructors. Each `from_tree` finds its
//! own node(s), removes the properties it models, and removes the node once it
//! is empty. There is no router handing nodes out — all routing knowledge lives
//! in the device. When every constructor has run, the tree must be empty; a
//! non-empty residual is an uncovered node/property and fails the launch
//! ([`SurveyError::Uncovered`]). Every guest-visible region is declared *from* a
//! claimed property ([`ResourcePlan::declare_from`]), so no constant can reach
//! the resource plan.
//!
//! aarch64 only for now; x86 still goes through [`crate::extract`] until it is
//! ported (see `TODO.md`). This module is additive — `extract`/`Platform` stay
//! until dillo-vm migrates onto `Machine` (Stage 4).

use devtree::{OwnedNode, OwnedProperty, OwnedTree, Tree};
use thiserror::Error;

use crate::{Arch, MmioRegion, Pcie, Psci, PsciMethod, Syscon, Uart};

/// Failures surveying the base DTB into a [`Machine`].
#[derive(Debug, Error)]
pub enum SurveyError {
    #[error("base DTB parse failed: {0:?}")]
    Parse(devtree::Error),

    #[error(
        "base DTB declares a `/cpus` node; the base must declare nothing \
         CPU-related — the host overlay authors the entire `/cpus` subtree \
         (PMI merged.md §1)"
    )]
    BaseHasCpus,

    #[error("required node `{0}` not found")]
    MissingNode(&'static str),

    #[error("required property `{prop}` not found on `{node}`")]
    MissingProperty { node: String, prop: &'static str },

    #[error("property `{prop}` on `{node}` is malformed ({reason})")]
    BadProperty {
        node: &'static str,
        prop: &'static str,
        reason: &'static str,
    },

    #[error("property `{prop}` on `{node}` has unsupported value `{value}`")]
    Unsupported {
        node: &'static str,
        prop: &'static str,
        value: String,
    },

    #[error("node `{node}` has properties/children no device claimed: {props:?} {children:?}")]
    Leftover {
        node: String,
        props: Vec<String>,
        children: Vec<String>,
    },

    #[error("base DTB has uncovered nodes/properties after survey: {0}")]
    Uncovered(String),

    #[error("declared regions overlap: `{a}` and `{b}`")]
    Overlap { a: String, b: String },
}

/// What a declared region is, so the realize step can map RAM, install MMIO
/// handlers, and treat windows (ECAM, BAR) appropriately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionKind {
    /// Guest RAM (authored by the host into the overlay; not seen here).
    Ram,
    /// A device MMIO register block dillo emulates.
    Mmio,
    /// The interrupt-controller substrate (GIC dist/redist/MSI frame).
    SubstrateMmio,
    /// A PCIe ECAM config window.
    EcamWindow,
    /// A PCIe BAR (MMIO) window.
    BarWindow,
}

/// Proof that a [`DeclaredRegion`] came from a claimed DTB property — no region
/// can be created without a property in hand (see [`ResourcePlan::declare_from`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub node: String,
    pub prop: String,
}

impl core::fmt::Display for Origin {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}:{}", self.node, self.prop)
    }
}

/// One guest-physical region, tagged by kind and traced to its origin property.
#[derive(Debug, Clone)]
pub struct DeclaredRegion {
    pub gpa: u64,
    pub size: u64,
    pub kind: RegionKind,
    pub origin: Origin,
}

/// The typed, origin-tracked replacement for `Platform.device_regions`.
///
/// Every region is created through [`Self::declare_from`], which requires a
/// reference to the claimed [`OwnedProperty`]. That is the only constructor, so
/// a region cannot originate from a constant.
#[derive(Debug, Default, Clone)]
pub struct ResourcePlan {
    regions: Vec<DeclaredRegion>,
}

impl ResourcePlan {
    /// Declare a region parsed from a claimed property. The `prop` reference is
    /// the proof of provenance; its name is recorded in the region's [`Origin`].
    pub fn declare_from(
        &mut self,
        prop: &OwnedProperty,
        node: &str,
        gpa: u64,
        size: u64,
        kind: RegionKind,
    ) {
        self.regions.push(DeclaredRegion {
            gpa,
            size,
            kind,
            origin: Origin {
                node: node.to_string(),
                prop: prop.name().to_string(),
            },
        });
    }

    /// The declared regions.
    pub fn regions(&self) -> &[DeclaredRegion] {
        &self.regions
    }

    /// Reject any pair of declared regions that overlap in guest-physical space.
    pub fn check_disjoint(&self) -> Result<(), SurveyError> {
        for (i, a) in self.regions.iter().enumerate() {
            for b in &self.regions[i + 1..] {
                if overlaps(a.gpa, a.size, b.gpa, b.size) {
                    return Err(SurveyError::Overlap {
                        a: a.origin.to_string(),
                        b: b.origin.to_string(),
                    });
                }
            }
        }
        Ok(())
    }
}

fn overlaps(a: u64, a_size: u64, b: u64, b_size: u64) -> bool {
    let a_end = a.saturating_add(a_size);
    let b_end = b.saturating_add(b_size);
    a_size != 0 && b_size != 0 && a < b_end && b < a_end
}

/// The aarch64 interrupt-controller configuration, lifted from the GICv3
/// (`interrupt-controller@…`) and its GICv2m MSI frame (`msi-controller@…`) —
/// the values the hypervisor binds the in-kernel GIC to (replacing the
/// hardcoded `hvf.rs` constants in Stage 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GicConfig {
    pub dist_base: u64,
    pub dist_size: u64,
    pub redist_base: u64,
    pub redist_size: u64,
    pub msi_frame_base: u64,
    pub msi_frame_size: u64,
    pub spi_base: u32,
    pub spi_count: u32,
}

/// An x86 16550 serial port claimed for coverage. The device model makes this
/// an `ns16550a` over MMIO (see [`Serial8250::from_tree`]); the `io_base: u16`
/// field is a frozen pre-device-model shape and holds only the MMIO base's low
/// 16 bits, while the full window is declared in the [`ResourcePlan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Serial8250 {
    pub io_base: u16,
    pub irq: u32,
}

/// A fully surveyed machine: every base-DTB node was claimed, every region
/// traces to a property, and the regions are pairwise disjoint. Fields populate
/// per architecture (aarch64: `gic`/`psci`/`uart`; x86: `lapic`/`ioapic`/
/// `poweroff`/`reboot`/`serial8250`); `pcie`/`plan` are common.
#[derive(Debug, Clone)]
pub struct Machine {
    pub arch: Arch,
    /// The PCIe host bridge. Valid only when [`has_pcie`](Self::has_pcie) is
    /// true; a `--pci-slots 0` microVM declares no bridge, in which case this
    /// is a zeroed sentinel ([`Pcie::ZEROED`]) the VMM must not install. This
    /// mirrors [`crate::Platform`]'s `pcie`/`has_pcie` pair so the types stay
    /// consistent across both paths.
    pub pcie: Pcie,
    /// Whether the base declares a PCIe host bridge (false ⇒ virtio-mmio-only
    /// microVM; skip all PCI fabric).
    pub has_pcie: bool,
    pub plan: ResourcePlan,
    // aarch64
    pub gic: Option<GicConfig>,
    pub psci: Option<Psci>,
    pub uart: Option<Uart>,
    // x86-64
    pub lapic: Option<MmioRegion>,
    pub ioapic: Option<MmioRegion>,
    pub poweroff: Option<Syscon>,
    pub reboot: Option<Syscon>,
    pub serial8250: Option<Serial8250>,
}

impl Machine {
    /// Survey a base DTB into a `Machine`, proving total coverage.
    pub fn survey(dtb: &[u8], arch: Arch) -> Result<Machine, SurveyError> {
        let tree: Tree<'_> = Tree::parse(dtb).map_err(SurveyError::Parse)?;
        let mut t = OwnedTree::materialize(&tree);
        let mut plan = ResourcePlan::default();

        let mut gic = None;
        let mut psci = None;
        let mut uart = None;
        let mut lapic = None;
        let mut ioapic = None;
        let mut poweroff = None;
        let mut reboot = None;
        let mut serial8250 = None;

        // arch-specific substrate (specific → general within the arch).
        match arch {
            Arch::Aarch64 => {
                gic = Some(GicConfig::from_tree(&mut t, &mut plan)?);
                Timer::from_tree(&mut t, &mut plan)?;
                psci = Some(Psci::from_tree(&mut t, &mut plan)?);
                uart = Uart::from_tree(&mut t, &mut plan)?;
            }
            Arch::X86_64 => {
                let (l, io) = X86Intc::from_tree(&mut t, &mut plan)?;
                lapic = Some(l);
                ioapic = Some(io);
                let (po, rb) = X86Syscon::from_tree(&mut t, &mut plan)?;
                poweroff = Some(po);
                reboot = rb;
                serial8250 = Serial8250::from_tree(&mut t, &mut plan)?;
            }
        }

        // Shared devices, then the general device last.
        VirtioMmioSlots::from_tree(&mut t, &mut plan)?;
        let (pcie, has_pcie) = match Pcie::from_tree(&mut t, &mut plan)? {
            Some(p) => (p, true),
            None => (Pcie::ZEROED, false),
        };
        CoreVm::from_tree(&mut t, &mut plan)?;

        // (a) total coverage: nothing left.
        let root = t.root();
        if root.properties().next().is_some() || root.children().next().is_some() {
            return Err(SurveyError::Uncovered(residual_report(root)));
        }
        // (b) declared regions are disjoint.
        plan.check_disjoint()?;

        Ok(Machine {
            arch,
            pcie,
            has_pcie,
            plan,
            gic,
            psci,
            uart,
            lapic,
            ioapic,
            poweroff,
            reboot,
            serial8250,
        })
    }
}

// ── self-routing device constructors ──────────────────────────────────

impl GicConfig {
    /// Claim the GICv3 (`interrupt-controller@…`, `arm,gic-v3`) and the GICv2m
    /// MSI frame (`msi-controller@…`, `arm,gic-v2m-frame`). Nodes are
    /// unit-addressed; matched by name-prefix, then verified by compatible.
    fn from_tree(t: &mut OwnedTree, plan: &mut ResourcePlan) -> Result<GicConfig, SurveyError> {
        let root = t.root_mut();

        let intc_name = child_name_prefixed(root, "interrupt-controller@")
            .ok_or(SurveyError::MissingNode("/interrupt-controller@*"))?;
        let mut intc = root.remove_child(&intc_name).expect("just located");
        require_compatible(&mut intc, "/interrupt-controller", "arm,gic-v3")?;
        let reg = intc.require("reg", "/interrupt-controller")?;
        let (dist_base, dist_size) = reg.reg_pair(0).ok_or(SurveyError::BadProperty {
            node: "/interrupt-controller",
            prop: "reg",
            reason: "missing GICD pair",
        })?;
        let (redist_base, redist_size) = reg.reg_pair(1).ok_or(SurveyError::BadProperty {
            node: "/interrupt-controller",
            prop: "reg",
            reason: "missing GICR pair",
        })?;
        plan.declare_from(
            &reg,
            "/interrupt-controller",
            dist_base,
            dist_size,
            RegionKind::SubstrateMmio,
        );
        plan.declare_from(
            &reg,
            "/interrupt-controller",
            redist_base,
            redist_size,
            RegionKind::SubstrateMmio,
        );
        intc.ack("#interrupt-cells");
        intc.ack("interrupt-controller");
        intc.ack("phandle");
        intc.ensure_drained()?;

        let v2m_name = child_name_prefixed(root, "msi-controller@")
            .ok_or(SurveyError::MissingNode("/msi-controller@*"))?;
        let mut v2m = root.remove_child(&v2m_name).expect("just located");
        require_compatible(&mut v2m, "/msi-controller", "arm,gic-v2m-frame")?;
        let vreg = v2m.require("reg", "/msi-controller")?;
        let (msi_frame_base, msi_frame_size) =
            vreg.reg_pair(0).ok_or(SurveyError::BadProperty {
                node: "/msi-controller",
                prop: "reg",
                reason: "missing MSI frame pair",
            })?;
        plan.declare_from(
            &vreg,
            "/msi-controller",
            msi_frame_base,
            msi_frame_size,
            RegionKind::SubstrateMmio,
        );
        let spi_base = v2m.require_u32("arm,msi-base-spi", "/msi-controller")?;
        let spi_count = v2m.require_u32("arm,msi-num-spis", "/msi-controller")?;
        v2m.ack("msi-controller");
        v2m.ack("phandle");
        v2m.ensure_drained()?;

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
}

/// The ARMv8 generic timer (`/timer`) — system-register driven, no MMIO.
struct Timer;

impl Timer {
    fn from_tree(t: &mut OwnedTree, _plan: &mut ResourcePlan) -> Result<(), SurveyError> {
        let mut timer = t
            .root_mut()
            .remove_child("timer")
            .ok_or(SurveyError::MissingNode("/timer"))?;
        require_compatible(&mut timer, "/timer", "arm,armv8-timer")?;
        timer.ack("interrupts");
        timer.ack("interrupt-parent");
        timer.ack("always-on");
        timer.ensure_drained()
    }
}

impl Psci {
    /// Claim the root-level `/psci` node (`arm,psci.yaml`); there is no
    /// `/firmware` wrapper in the device model.
    fn from_tree(t: &mut OwnedTree, _plan: &mut ResourcePlan) -> Result<Psci, SurveyError> {
        let mut psci = t
            .root_mut()
            .remove_child("psci")
            .ok_or(SurveyError::MissingNode("/psci"))?;

        let compat = psci.require("compatible", "/psci")?;
        let has_psci = compat
            .as_strs()
            .is_some_and(|mut it| it.any(|s| s.starts_with("arm,psci-")));
        if !has_psci {
            return Err(SurveyError::Unsupported {
                node: "/psci",
                prop: "compatible",
                value: compat.as_str().unwrap_or("<list>").to_string(),
            });
        }
        let method = match psci.require("method", "/psci")?.as_str() {
            Some("hvc") => PsciMethod::Hvc,
            Some("smc") => PsciMethod::Smc,
            other => {
                return Err(SurveyError::Unsupported {
                    node: "/psci",
                    prop: "method",
                    value: other.unwrap_or("<non-string>").to_string(),
                });
            }
        };
        psci.ensure_drained()?;
        Ok(Psci { method })
    }
}

impl Uart {
    /// Claim `serial@*` (`ns16550a`, a 16550 over MMIO) — present only under
    /// `--serial`. Absent ⇒ `Ok(None)`. There is no `/apb-pclk` clock node in
    /// the device model.
    fn from_tree(t: &mut OwnedTree, plan: &mut ResourcePlan) -> Result<Option<Uart>, SurveyError> {
        let root = t.root_mut();
        let Some(name) = child_name_prefixed(root, "serial@") else {
            return Ok(None); // no serial in this base
        };
        let mut serial = root.remove_child(&name).expect("just located");
        require_compatible(&mut serial, "/serial", "ns16550a")?;
        let reg = serial.require("reg", "/serial")?;
        let (base, size) = reg.reg_pair(0).ok_or(SurveyError::BadProperty {
            node: "/serial",
            prop: "reg",
            reason: "missing reg pair",
        })?;
        plan.declare_from(&reg, "/serial", base, size, RegionKind::Mmio);
        let reg_shift = serial.require_u32("reg-shift", "/serial")?;
        let ints = serial.require("interrupts", "/serial")?;
        let irq = ints
            .as_u32s()
            .and_then(|mut it| it.next())
            .ok_or(SurveyError::BadProperty {
                node: "/serial",
                prop: "interrupts",
                reason: "expected at least one interrupt cell",
            })?;
        serial.ack("reg-io-width");
        serial.ack("clock-frequency");
        serial.ack("interrupt-parent");
        serial.ensure_drained()?;

        Ok(Some(Uart {
            base,
            size,
            reg_shift,
            irq,
        }))
    }
}

/// Every `virtio_mmio@*` transport slot (`virtio,mmio`). Each is a fixed MMIO
/// window + a wired IRQ, declared on the board whether or not a backend is
/// plugged. The survey drains ALL of them so none is an uncovered residual.
struct VirtioMmioSlots;

impl VirtioMmioSlots {
    fn from_tree(t: &mut OwnedTree, plan: &mut ResourcePlan) -> Result<(), SurveyError> {
        let root = t.root_mut();
        while let Some(name) = child_name_prefixed(root, "virtio_mmio@") {
            let mut node = root.remove_child(&name).expect("just located");
            require_compatible(&mut node, "/virtio_mmio", "virtio,mmio")?;
            let reg = node.require("reg", "/virtio_mmio")?;
            let (base, size) = reg.reg_pair(0).ok_or(SurveyError::BadProperty {
                node: "/virtio_mmio",
                prop: "reg",
                reason: "missing reg pair",
            })?;
            plan.declare_from(&reg, "/virtio_mmio", base, size, RegionKind::Mmio);
            node.ack("interrupts");
            node.ack("interrupt-parent");
            node.ensure_drained()?;
        }
        Ok(())
    }
}

impl Pcie {
    /// Claim the `pcie@*` ECAM host bridge (`pci-host-ecam-generic`), matched by
    /// name-prefix then verified by compatible. A `--pci-slots 0` microVM
    /// declares no bridge ⇒ `Ok(None)`.
    fn from_tree(t: &mut OwnedTree, plan: &mut ResourcePlan) -> Result<Option<Pcie>, SurveyError> {
        let root = t.root_mut();
        let Some(name) = child_name_prefixed(root, "pcie@") else {
            return Ok(None); // no host bridge in this base
        };
        let mut pci = root.remove_child(&name).expect("just located");

        require_compatible(&mut pci, "/pcie", "pci-host-ecam-generic")?;
        let reg = pci.require("reg", "/pcie")?;
        let (ecam_base, ecam_size) = reg.reg_pair(0).ok_or(SurveyError::BadProperty {
            node: "/pcie",
            prop: "reg",
            reason: "missing ECAM pair",
        })?;
        plan.declare_from(&reg, "/pcie", ecam_base, ecam_size, RegionKind::EcamWindow);

        let ranges = pci.require("ranges", "/pcie")?;
        let (mmio_base, mmio_size) = pci_window(&ranges).ok_or(SurveyError::BadProperty {
            node: "/pcie",
            prop: "ranges",
            reason: "expected at least one 7-cell window",
        })?;
        plan.declare_from(
            &ranges,
            "/pcie",
            mmio_base,
            mmio_size,
            RegionKind::BarWindow,
        );

        let bus = pci.require("bus-range", "/pcie")?;
        let bus_cells: Vec<u32> = bus
            .as_u32s()
            .ok_or(SurveyError::BadProperty {
                node: "/pcie",
                prop: "bus-range",
                reason: "not u32 cells",
            })?
            .collect();
        if bus_cells.len() != 2 {
            return Err(SurveyError::BadProperty {
                node: "/pcie",
                prop: "bus-range",
                reason: "expected two cells",
            });
        }

        pci.ack("device_type");
        pci.ack("#address-cells");
        pci.ack("#size-cells");
        pci.ack("dma-coherent");
        pci.ack("dma-ranges");
        pci.ack("msi-parent");
        pci.ensure_drained()?;

        Ok(Some(Pcie {
            ecam_base,
            ecam_size,
            bus_min: bus_cells[0] as u8,
            bus_max: bus_cells[1] as u8,
            mmio_base,
            mmio_size,
        }))
    }
}

/// The general device: runs last, claims root's own grammar and `/chosen`,
/// rejects a base `/cpus`, and sweeps any now-empty containers.
struct CoreVm;

impl CoreVm {
    fn from_tree(t: &mut OwnedTree, _plan: &mut ResourcePlan) -> Result<(), SurveyError> {
        let root = t.root_mut();
        if root.child("cpus").is_some() {
            return Err(SurveyError::BaseHasCpus);
        }
        root.ack("#address-cells");
        root.ack("#size-cells");
        root.ack("model");
        root.ack("compatible");

        if let Some(mut chosen) = root.remove_child("chosen") {
            chosen.ack("bootargs");
            chosen.ack("linux,initrd-start");
            chosen.ack("linux,initrd-end");
            chosen.ack("stdout-path");
            chosen.ensure_drained()?;
        }

        // Sweep now-empty containers a leaf device may have emptied.
        let empties: Vec<String> = root
            .children()
            .filter(|c| c.properties().next().is_none() && c.children().next().is_none())
            .map(|c| c.name().to_string())
            .collect();
        for name in empties {
            root.remove_child(&name);
        }
        Ok(())
    }
}

// ── x86-64 devices ─────────────────────────────────────────────────────

/// x86 LAPIC + IOAPIC — two SEPARATE `interrupt-controller@*` nodes
/// (`intel,ce4100-lapic` and `intel,ce4100-ioapic`), each with its own `reg`.
struct X86Intc;

impl X86Intc {
    fn from_tree(
        t: &mut OwnedTree,
        plan: &mut ResourcePlan,
    ) -> Result<(MmioRegion, MmioRegion), SurveyError> {
        let lapic = X86Intc::claim_one(t, "intel,ce4100-lapic", "/lapic", plan)?;
        let ioapic = X86Intc::claim_one(t, "intel,ce4100-ioapic", "/ioapic", plan)?;
        Ok((lapic, ioapic))
    }

    /// Claim one `interrupt-controller@*` node by compatible, declaring its
    /// `reg` window as substrate MMIO. Both x86 intc nodes share the
    /// `interrupt-controller@` prefix, so they are disambiguated by compatible.
    fn claim_one(
        t: &mut OwnedTree,
        compat: &'static str,
        path: &'static str,
        plan: &mut ResourcePlan,
    ) -> Result<MmioRegion, SurveyError> {
        let root = t.root_mut();
        let name = root
            .children()
            .find(|c| {
                c.name().starts_with("interrupt-controller@")
                    && c.property("compatible")
                        .and_then(|p| p.as_str())
                        .is_some_and(|s| s == compat)
            })
            .map(|c| c.name().to_string())
            .ok_or(SurveyError::MissingNode(path))?;
        let mut node = root.remove_child(&name).expect("just located");
        node.ack("compatible"); // matched above
        let reg = node.require("reg", path)?;
        let (base, size) = reg.reg_pair(0).ok_or(SurveyError::BadProperty {
            node: path,
            prop: "reg",
            reason: "missing reg pair",
        })?;
        plan.declare_from(&reg, path, base, size, RegionKind::SubstrateMmio);
        node.ack("#interrupt-cells");
        node.ack("interrupt-controller");
        node.ack("phandle");
        node.ensure_drained()?;
        Ok(MmioRegion { base, size })
    }
}

/// x86 power: standalone `poweroff@*` (`syscon-poweroff`) and optional
/// `reboot@*` (`syscon-reboot`), each with its OWN `reg` + trigger `value` (no
/// `/syscon` container, no `regmap`/`offset`/`mask`).
struct X86Syscon;

impl X86Syscon {
    fn from_tree(
        t: &mut OwnedTree,
        plan: &mut ResourcePlan,
    ) -> Result<(Syscon, Option<Syscon>), SurveyError> {
        let root = t.root_mut();
        let poweroff = syscon_action(root, "poweroff@", "syscon-poweroff", "/poweroff", plan)?
            .ok_or(SurveyError::MissingNode("/poweroff@*"))?;
        let reboot = syscon_action(root, "reboot@", "syscon-reboot", "/reboot", plan)?;
        Ok((poweroff, reboot))
    }
}

/// Claim a standalone `syscon-{poweroff,reboot}` action node (matched by
/// name-prefix, verified by compatible), declaring its own `reg` as MMIO. The
/// trigger is a write of `value` at the node's reg (offset 0), masking the low
/// byte — mirroring [`crate::Platform`]'s `read_syscon_action`.
fn syscon_action(
    root: &mut OwnedNode,
    prefix: &str,
    compat: &'static str,
    path: &'static str,
    plan: &mut ResourcePlan,
) -> Result<Option<Syscon>, SurveyError> {
    let Some(name) = child_name_prefixed(root, prefix) else {
        return Ok(None);
    };
    let mut node = root.remove_child(&name).expect("just located");
    require_compatible(&mut node, path, compat)?;
    let reg = node.require("reg", path)?;
    let (base, size) = reg.reg_pair(0).ok_or(SurveyError::BadProperty {
        node: path,
        prop: "reg",
        reason: "missing reg pair",
    })?;
    plan.declare_from(&reg, path, base, size, RegionKind::Mmio);
    let value = node.require_u32("value", path)?;
    node.ensure_drained()?;
    Ok(Some(Syscon {
        base,
        offset: 0,
        value,
        mask: 0xFF,
    }))
}

impl Serial8250 {
    /// Claim `serial@*` (`ns16550a`, a 16550 over MMIO) — present only under
    /// `--serial`. Absent ⇒ `Ok(None)`.
    ///
    /// FIELD-SHAPE COMPROMISE: the device model's x86 serial is MMIO with a
    /// 64-bit `reg` base, but [`Serial8250`]'s `io_base` is a `u16` (its
    /// pre-device-model port-I/O shape, which the constraint freezes). The full
    /// MMIO window is declared in the [`ResourcePlan`] (correct for coverage and
    /// disjointness); `io_base` is set to the base's low 16 bits as a lossy
    /// placeholder. `Serial8250` has no external consumer, so this affects only
    /// the survey path; the field should become an MMIO `(base, size)` when the
    /// shape can change. `irq` is the IO-APIC pin (`interrupts` cell 0).
    fn from_tree(
        t: &mut OwnedTree,
        plan: &mut ResourcePlan,
    ) -> Result<Option<Serial8250>, SurveyError> {
        let root = t.root_mut();
        let Some(name) = child_name_prefixed(root, "serial@") else {
            return Ok(None); // no serial in this base
        };
        let mut serial = root.remove_child(&name).expect("just located");
        require_compatible(&mut serial, "/serial", "ns16550a")?;
        let reg = serial.require("reg", "/serial")?;
        let (base, size) = reg.reg_pair(0).ok_or(SurveyError::BadProperty {
            node: "/serial",
            prop: "reg",
            reason: "missing reg pair",
        })?;
        plan.declare_from(&reg, "/serial", base, size, RegionKind::Mmio);
        // x86 `interrupts` is the IO-APIC 2-cell <pin, sense>; the pin is cell 0.
        let ints = serial.require("interrupts", "/serial")?;
        let irq = ints
            .as_u32s()
            .and_then(|mut it| it.next())
            .ok_or(SurveyError::BadProperty {
                node: "/serial",
                prop: "interrupts",
                reason: "expected <pin sense>",
            })?;
        serial.ack("reg-shift");
        serial.ack("reg-io-width");
        serial.ack("clock-frequency");
        serial.ack("interrupt-parent");
        serial.ensure_drained()?;
        Ok(Some(Serial8250 {
            io_base: base as u16,
            irq,
        }))
    }
}

// ── helpers ────────────────────────────────────────────────────────────

fn require_compatible(
    node: &mut OwnedNode,
    path: &'static str,
    expected: &'static str,
) -> Result<(), SurveyError> {
    let compat = node.require("compatible", path)?;
    match compat.as_str() {
        Some(s) if s == expected => Ok(()),
        Some(_) | None => Err(SurveyError::Unsupported {
            node: path,
            prop: "compatible",
            value: compat.as_str().unwrap_or("<non-string>").to_string(),
        }),
    }
}

fn child_name_prefixed(node: &OwnedNode, prefix: &str) -> Option<String> {
    node.children()
        .find(|c| c.name().starts_with(prefix))
        .map(|c| c.name().to_string())
}

/// First `ranges` window CPU base + size: `<phys.hi mid lo cpu.hi cpu.lo size.hi size.lo>`.
fn pci_window(prop: &OwnedProperty) -> Option<(u64, u64)> {
    let cells: Vec<u32> = prop.as_u32s()?.collect();
    let w = cells.get(0..7)?;
    let cpu_base = (u64::from(w[3]) << 32) | u64::from(w[4]);
    let size = (u64::from(w[5]) << 32) | u64::from(w[6]);
    Some((cpu_base, size))
}

fn residual_report(root: &OwnedNode) -> String {
    let mut parts: Vec<String> = Vec::new();
    let props: Vec<&str> = root.properties().map(OwnedProperty::name).collect();
    if !props.is_empty() {
        parts.push(format!("root props {props:?}"));
    }
    for c in root.children() {
        let cp: Vec<&str> = c.properties().map(OwnedProperty::name).collect();
        parts.push(format!("node /{} props {:?}", c.name(), cp));
    }
    parts.join("; ")
}

/// Draining/claiming helpers on the foreign owned types.
trait NodeExt {
    /// Remove a required property, or error if absent.
    fn require(
        &mut self,
        prop: &'static str,
        node: &'static str,
    ) -> Result<OwnedProperty, SurveyError>;
    /// Remove a required `u32` property, or error if absent/malformed.
    fn require_u32(&mut self, prop: &'static str, node: &'static str) -> Result<u32, SurveyError>;
    /// Remove a property that is acknowledged but carries no modeled value.
    fn ack(&mut self, prop: &'static str);
    /// Error unless the node has no remaining properties or children.
    fn ensure_drained(&self) -> Result<(), SurveyError>;
}

impl NodeExt for OwnedNode {
    fn require(
        &mut self,
        prop: &'static str,
        node: &'static str,
    ) -> Result<OwnedProperty, SurveyError> {
        self.remove_property(prop)
            .ok_or(SurveyError::MissingProperty {
                node: node.to_string(),
                prop,
            })
    }

    fn require_u32(&mut self, prop: &'static str, node: &'static str) -> Result<u32, SurveyError> {
        self.require(prop, node)?
            .as_u32()
            .ok_or(SurveyError::BadProperty {
                node,
                prop,
                reason: "expected single u32",
            })
    }

    fn ack(&mut self, prop: &'static str) {
        let _ = self.remove_property(prop);
    }

    fn ensure_drained(&self) -> Result<(), SurveyError> {
        let props: Vec<String> = self.properties().map(|p| p.name().to_string()).collect();
        let children: Vec<String> = self.children().map(|c| c.name().to_string()).collect();
        if props.is_empty() && children.is_empty() {
            Ok(())
        } else {
            Err(SurveyError::Leftover {
                node: self.name().to_string(),
                props,
                children,
            })
        }
    }
}

trait PropExt {
    /// Read the `idx`-th `(base, size)` pair from a root-governed (2-cell/2-cell)
    /// `reg` property.
    fn reg_pair(&self, idx: usize) -> Option<(u64, u64)>;
}

impl PropExt for OwnedProperty {
    fn reg_pair(&self, idx: usize) -> Option<(u64, u64)> {
        let cells: Vec<u32> = self.as_u32s()?.collect();
        let i = idx * 4;
        let b = cells.get(i..i + 4)?;
        let base = (u64::from(b[0]) << 32) | u64::from(b[1]);
        let size = (u64::from(b[2]) << 32) | u64::from(b[3]);
        Some((base, size))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // GICD 0x0800_0000/0x1_0000, GICR 0x0810_0000/0x200_0000.
    const GICD_BASE: u64 = 0x0800_0000;
    const GICR_BASE: u64 = 0x0810_0000;
    const V2M_BASE: u64 = 0x0A10_0000;
    const SERIAL_BASE: u64 = 0x0A20_0000;
    const VIRTIO_BASE: u64 = 0x0A30_0000;
    const ECAM_BASE: u64 = 0x0C00_0000;
    const PCI_MMIO_BASE: u64 = 0x0C10_0000;

    fn reg2(base: u64, size: u64) -> Vec<u32> {
        vec![
            (base >> 32) as u32,
            base as u32,
            (size >> 32) as u32,
            size as u32,
        ]
    }

    /// An aarch64 base DTB matching arma's device-model emission (with serial
    /// and one virtio-mmio slot).
    fn base_root() -> OwnedNode {
        let mut intc_reg = reg2(GICD_BASE, 0x1_0000);
        intc_reg.extend(reg2(GICR_BASE, 0x200_0000));

        let intc = OwnedNode::new("interrupt-controller@8000000")
            .with_property(OwnedProperty::new("compatible").with_str("arm,gic-v3"))
            .with_property(OwnedProperty::new("#interrupt-cells").with_u32(3))
            .with_property(OwnedProperty::new("interrupt-controller"))
            .with_property(OwnedProperty::new("reg").with_u32s(&intc_reg))
            .with_property(OwnedProperty::new("phandle").with_u32(1));

        let v2m = OwnedNode::new("msi-controller@a100000")
            .with_property(OwnedProperty::new("compatible").with_str("arm,gic-v2m-frame"))
            .with_property(OwnedProperty::new("msi-controller"))
            .with_property(OwnedProperty::new("reg").with_u32s(&reg2(V2M_BASE, 0x1_0000)))
            .with_property(OwnedProperty::new("arm,msi-base-spi").with_u32(64))
            .with_property(OwnedProperty::new("arm,msi-num-spis").with_u32(32))
            .with_property(OwnedProperty::new("phandle").with_u32(3));

        let timer = OwnedNode::new("timer")
            .with_property(OwnedProperty::new("compatible").with_str("arm,armv8-timer"))
            .with_property(
                OwnedProperty::new("interrupts")
                    .with_u32s(&[1, 13, 0xff08, 1, 14, 0xff08, 1, 11, 0xff08, 1, 10, 0xff08]),
            )
            .with_property(OwnedProperty::new("interrupt-parent").with_u32(1))
            .with_property(OwnedProperty::new("always-on"));

        let psci = OwnedNode::new("psci")
            .with_property(
                OwnedProperty::new("compatible").with_strs(&["arm,psci-1.0", "arm,psci-0.2"]),
            )
            .with_property(OwnedProperty::new("method").with_str("hvc"));

        let serial = OwnedNode::new("serial@9000000")
            .with_property(OwnedProperty::new("compatible").with_str("ns16550a"))
            .with_property(OwnedProperty::new("reg").with_u32s(&reg2(SERIAL_BASE, 0x1000)))
            .with_property(OwnedProperty::new("reg-shift").with_u32(2))
            .with_property(OwnedProperty::new("reg-io-width").with_u32(4))
            .with_property(OwnedProperty::new("clock-frequency").with_u32(3_686_400))
            .with_property(OwnedProperty::new("interrupt-parent").with_u32(1))
            .with_property(OwnedProperty::new("interrupts").with_u32s(&[0, 1, 4]));

        let virtio = OwnedNode::new("virtio_mmio@a000000")
            .with_property(OwnedProperty::new("compatible").with_str("virtio,mmio"))
            .with_property(OwnedProperty::new("reg").with_u32s(&reg2(VIRTIO_BASE, 0x200)))
            .with_property(OwnedProperty::new("interrupt-parent").with_u32(1))
            .with_property(OwnedProperty::new("interrupts").with_u32s(&[0, 16, 1]));

        let pci = OwnedNode::new("pcie@c000000")
            .with_property(OwnedProperty::new("compatible").with_str("pci-host-ecam-generic"))
            .with_property(OwnedProperty::new("device_type").with_str("pci"))
            .with_property(OwnedProperty::new("reg").with_u32s(&reg2(ECAM_BASE, 0x10_0000)))
            .with_property(OwnedProperty::new("bus-range").with_u32s(&[0, 0]))
            .with_property(OwnedProperty::new("#address-cells").with_u32(3))
            .with_property(OwnedProperty::new("#size-cells").with_u32(2))
            .with_property(OwnedProperty::new("ranges").with_u32s(&[
                0x0300_0000,
                0,
                PCI_MMIO_BASE as u32,
                0,
                PCI_MMIO_BASE as u32,
                0,
                0x03F0_0000,
            ]))
            .with_property(OwnedProperty::new("dma-coherent"))
            .with_property(OwnedProperty::new("msi-parent").with_u32(3));

        OwnedNode::new("")
            .with_property(OwnedProperty::new("#address-cells").with_u32(2))
            .with_property(OwnedProperty::new("#size-cells").with_u32(2))
            .with_property(OwnedProperty::new("model").with_str("Arma Virtual Platform"))
            .with_property(OwnedProperty::new("compatible").with_str("arma,v1"))
            .with_child(
                OwnedNode::new("chosen")
                    .with_property(OwnedProperty::new("bootargs").with_str("console=ttyS0")),
            )
            .with_child(intc)
            .with_child(v2m)
            .with_child(timer)
            .with_child(psci)
            .with_child(serial)
            .with_child(virtio)
            .with_child(pci)
    }

    fn dtb(root: OwnedNode) -> Vec<u8> {
        OwnedTree::new(root).encode().expect("encode base")
    }

    #[test]
    fn base_drains_to_empty_and_binds_gic() {
        let m = Machine::survey(&dtb(base_root()), Arch::Aarch64).expect("survey ok");

        let gic = m.gic.expect("gic bound");
        assert_eq!(gic.dist_base, GICD_BASE);
        assert_eq!(gic.dist_size, 0x1_0000);
        assert_eq!(gic.redist_base, GICR_BASE);
        assert_eq!(gic.redist_size, 0x200_0000);
        assert_eq!(gic.msi_frame_base, V2M_BASE);
        assert_eq!(gic.spi_base, 64);
        assert_eq!(gic.spi_count, 32);

        let uart = m.uart.expect("uart present");
        assert_eq!(uart.base, SERIAL_BASE);
        assert_eq!(uart.size, 0x1000);
        assert_eq!(uart.reg_shift, 2);

        assert!(m.has_pcie);
        assert_eq!(m.pcie.ecam_base, ECAM_BASE);
        assert_eq!(m.pcie.mmio_base, PCI_MMIO_BASE);
        assert_eq!(m.psci.map(|p| p.method), Some(PsciMethod::Hvc));

        // Regions: GICD, GICR, MSI frame, serial, virtio-mmio, ECAM, PCI MMIO.
        assert_eq!(m.plan.regions().len(), 7);
    }

    #[test]
    fn base_without_serial_has_no_uart_and_still_drains() {
        let mut root = base_root();
        root.remove_child("serial@9000000");
        let m = Machine::survey(&dtb(root), Arch::Aarch64).expect("survey ok");
        assert!(m.uart.is_none());
        // GICD, GICR, MSI frame, virtio-mmio, ECAM, PCI MMIO — no serial.
        assert_eq!(m.plan.regions().len(), 6);
    }

    #[test]
    fn firecracker_style_no_pci_drains_to_empty() {
        // A microVM with no PCIe bridge and several virtio-mmio slots.
        let mut root = base_root();
        root.remove_child("pcie@c000000");
        for (i, addr) in [0x0A40_0000u64, 0x0A41_0000, 0x0A42_0000]
            .iter()
            .enumerate()
        {
            root.set_child(
                OwnedNode::new(&format!("virtio_mmio@{addr:x}"))
                    .with_property(OwnedProperty::new("compatible").with_str("virtio,mmio"))
                    .with_property(OwnedProperty::new("reg").with_u32s(&reg2(*addr, 0x200)))
                    .with_property(OwnedProperty::new("interrupt-parent").with_u32(1))
                    .with_property(OwnedProperty::new("interrupts").with_u32s(&[
                        0,
                        17 + i as u32,
                        1,
                    ])),
            );
        }
        let m = Machine::survey(&dtb(root), Arch::Aarch64).expect("survey ok");
        assert!(!m.has_pcie);
        assert_eq!(m.pcie.ecam_base, 0); // ZEROED sentinel
        // GICD, GICR, MSI frame, serial, four virtio-mmio (1 base + 3 added).
        assert_eq!(m.plan.regions().len(), 8);
    }

    #[test]
    fn extra_node_is_uncovered() {
        let mut root = base_root();
        root.set_child(
            OwnedNode::new("bogus@0").with_property(OwnedProperty::new("foo").with_u32(1)),
        );
        let err = Machine::survey(&dtb(root), Arch::Aarch64).unwrap_err();
        assert!(matches!(err, SurveyError::Uncovered(_)), "got {err:?}");
    }

    #[test]
    fn extra_property_is_left_over() {
        let mut root = base_root();
        root.child_mut("timer")
            .unwrap()
            .set_property(OwnedProperty::new("surprise").with_u32(1));
        let err = Machine::survey(&dtb(root), Arch::Aarch64).unwrap_err();
        assert!(
            matches!(
                err,
                SurveyError::Leftover { .. } | SurveyError::Uncovered(_)
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn base_with_cpus_is_rejected() {
        let mut root = base_root();
        root.set_child(
            OwnedNode::new("cpus").with_property(OwnedProperty::new("#address-cells").with_u32(1)),
        );
        let err = Machine::survey(&dtb(root), Arch::Aarch64).unwrap_err();
        assert!(matches!(err, SurveyError::BaseHasCpus), "got {err:?}");
    }

    #[test]
    fn overlapping_regions_are_rejected() {
        let mut root = base_root();
        // Move the serial onto the ECAM window so two declared regions overlap.
        root.child_mut("serial@9000000")
            .unwrap()
            .property_mut("reg")
            .unwrap()
            .set_u32s(&reg2(ECAM_BASE, 0x1000));
        let err = Machine::survey(&dtb(root), Arch::Aarch64).unwrap_err();
        assert!(matches!(err, SurveyError::Overlap { .. }), "got {err:?}");
    }

    // x86-64 base layout (arma `src/base_dtb/x86_64.rs`).
    const LAPIC_BASE: u64 = 0xFEE0_0000;
    const IOAPIC_BASE: u64 = 0xFEC0_0000;
    const POWEROFF_BASE: u64 = 0x0901_0000;
    const REBOOT_BASE: u64 = 0x0902_0000;
    const X86_SERIAL_BASE: u64 = 0x0900_0000;
    const X86_ECAM_BASE: u64 = 0xB000_0000;
    const X86_PCI_MMIO_BASE: u64 = 0xC000_0000;

    /// An x86-64 base DTB matching arma's device-model emission (with serial).
    fn x86_base_root() -> OwnedNode {
        let lapic = OwnedNode::new("interrupt-controller@fee00000")
            .with_property(OwnedProperty::new("compatible").with_str("intel,ce4100-lapic"))
            .with_property(OwnedProperty::new("reg").with_u32s(&reg2(LAPIC_BASE, 0x1000)))
            .with_property(OwnedProperty::new("#interrupt-cells").with_u32(2))
            .with_property(OwnedProperty::new("interrupt-controller"))
            .with_property(OwnedProperty::new("phandle").with_u32(1));

        let ioapic = OwnedNode::new("interrupt-controller@fec00000")
            .with_property(OwnedProperty::new("compatible").with_str("intel,ce4100-ioapic"))
            .with_property(OwnedProperty::new("reg").with_u32s(&reg2(IOAPIC_BASE, 0x1000)))
            .with_property(OwnedProperty::new("#interrupt-cells").with_u32(2))
            .with_property(OwnedProperty::new("interrupt-controller"))
            .with_property(OwnedProperty::new("phandle").with_u32(2));

        let poweroff = OwnedNode::new("poweroff@9010000")
            .with_property(OwnedProperty::new("compatible").with_str("syscon-poweroff"))
            .with_property(OwnedProperty::new("reg").with_u32s(&reg2(POWEROFF_BASE, 0x4)))
            .with_property(OwnedProperty::new("value").with_u32(0x34));

        let reboot = OwnedNode::new("reboot@9020000")
            .with_property(OwnedProperty::new("compatible").with_str("syscon-reboot"))
            .with_property(OwnedProperty::new("reg").with_u32s(&reg2(REBOOT_BASE, 0x4)))
            .with_property(OwnedProperty::new("value").with_u32(0x1));

        let serial = OwnedNode::new("serial@9000000")
            .with_property(OwnedProperty::new("compatible").with_str("ns16550a"))
            .with_property(OwnedProperty::new("reg").with_u32s(&reg2(X86_SERIAL_BASE, 0x1000)))
            .with_property(OwnedProperty::new("reg-shift").with_u32(2))
            .with_property(OwnedProperty::new("reg-io-width").with_u32(4))
            .with_property(OwnedProperty::new("clock-frequency").with_u32(3_686_400))
            .with_property(OwnedProperty::new("interrupt-parent").with_u32(2))
            .with_property(OwnedProperty::new("interrupts").with_u32s(&[4, 1]));

        let pci = OwnedNode::new("pcie@b0000000")
            .with_property(OwnedProperty::new("compatible").with_str("pci-host-ecam-generic"))
            .with_property(OwnedProperty::new("device_type").with_str("pci"))
            .with_property(OwnedProperty::new("reg").with_u32s(&reg2(X86_ECAM_BASE, 0x10_0000)))
            .with_property(OwnedProperty::new("bus-range").with_u32s(&[0, 0]))
            .with_property(OwnedProperty::new("#address-cells").with_u32(3))
            .with_property(OwnedProperty::new("#size-cells").with_u32(2))
            .with_property(OwnedProperty::new("ranges").with_u32s(&[
                0x0300_0000,
                0,
                X86_PCI_MMIO_BASE as u32,
                0,
                X86_PCI_MMIO_BASE as u32,
                0,
                0x1000_0000,
            ]));

        OwnedNode::new("")
            .with_property(OwnedProperty::new("#address-cells").with_u32(2))
            .with_property(OwnedProperty::new("#size-cells").with_u32(2))
            .with_property(OwnedProperty::new("model").with_str("Arma Virtual Platform"))
            .with_property(OwnedProperty::new("compatible").with_str("arma,v1"))
            .with_child(
                OwnedNode::new("chosen")
                    .with_property(OwnedProperty::new("bootargs").with_str("console=ttyS0")),
            )
            .with_child(lapic)
            .with_child(ioapic)
            .with_child(poweroff)
            .with_child(reboot)
            .with_child(serial)
            .with_child(pci)
    }

    #[test]
    fn x86_base_drains_and_binds() {
        let m = Machine::survey(&dtb(x86_base_root()), Arch::X86_64).expect("survey ok");

        assert_eq!(m.lapic.expect("lapic").base, LAPIC_BASE);
        assert_eq!(m.ioapic.expect("ioapic").base, IOAPIC_BASE);
        let po = m.poweroff.expect("poweroff");
        assert_eq!((po.base, po.value, po.mask), (POWEROFF_BASE, 0x34, 0xFF));
        let rb = m.reboot.expect("reboot");
        assert_eq!((rb.base, rb.value), (REBOOT_BASE, 0x1));
        // FIELD-SHAPE COMPROMISE: io_base holds the MMIO base's low 16 bits.
        let s = m.serial8250.expect("serial");
        assert_eq!((s.io_base, s.irq), (X86_SERIAL_BASE as u16, 4));
        assert!(m.has_pcie);
        assert_eq!(m.pcie.ecam_base, X86_ECAM_BASE);
        assert_eq!(m.pcie.mmio_base, X86_PCI_MMIO_BASE);
        // aarch64-only fields stay empty.
        assert!(m.gic.is_none() && m.psci.is_none() && m.uart.is_none());
        // LAPIC, IOAPIC, poweroff, reboot, serial, ECAM, PCI MMIO.
        assert_eq!(m.plan.regions().len(), 7);
    }

    #[test]
    fn x86_base_without_serial_has_no_8250_and_still_drains() {
        let mut root = x86_base_root();
        root.remove_child("serial@9000000");
        let m = Machine::survey(&dtb(root), Arch::X86_64).expect("survey ok");
        assert!(m.serial8250.is_none());
    }

    #[test]
    fn x86_extra_node_is_uncovered() {
        let mut root = x86_base_root();
        root.set_child(
            OwnedNode::new("bogus@0").with_property(OwnedProperty::new("foo").with_u32(1)),
        );
        let err = Machine::survey(&dtb(root), Arch::X86_64).unwrap_err();
        assert!(matches!(err, SurveyError::Uncovered(_)), "got {err:?}");
    }
}
