//! Devicetree validation.
//!
//! The host-supplied DTBO is adversarial: [`validate_host_dtbo`]
//! enforces the merged-extension allowlist (per `pmi/spec/merged.md`
//! §2) before merging.
//!
//! The merged tree (base + host DTBO) is then validated by
//! [`validate_merged`], but only on surfaces the host can actually
//! contribute to per §2's allowlist:
//!
//! - `/cpus/cpu@N` count, bounded by [`MAX_CPUS`] for DoS protection.
//! - `/memory@*` `reg` regions, bounded by [`MAX_MEMORY_REGIONS`].
//! - The §4.4 check 2 memory-vs-device overlap test, which requires
//!   walking `/intc`, `/syscon`, and `/pci` for their `reg` regions.
//!
//! Base-DTB-only content (root attrs, `/firmware`, `/poweroff`,
//! `/reboot`, `/timer`, `/chosen`, `/reserved-memory`, plus the
//! property whitelists on `/intc`, `/syscon`, `/pci`) is arma's
//! responsibility. arma is trusted; we don't re-validate it.
//!
//! All buffers are bounded by `ArrayVec`; the validation pass has
//! no heap and no dynamic allocation.

use arrayvec::ArrayVec;
use devtree::{Error as DtError, Fragment, NodeView, OverlayView, PropertyView, Target, TreeView};

// §4.5 compile-time maxima.
pub const MAX_CPUS: usize = 2048;
pub const MAX_MEMORY_REGIONS: usize = 32;
// ArrayVec cap for platform-device `reg` pairs collected for the §4.4
// memory-vs-device overlap check. Covers GIC (dist+redist) + v2m + serial +
// PCIe ECAM plus a generous virtio-mmio slot count; the bound is mechanical
// (the type needs a const size), not load-bearing — all base-only.
pub const MAX_DEVICE_RANGES: usize = 64;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ValidationError {
    // Host-DTBO allowlist (pmi/spec/merged.md §2).
    HostOverlayUnsupportedTarget,
    HostOverlayProhibitedPath,
    HostOverlayCpuPhandleProhibited,
    HostOverlayConflictingProperty,
    HostOverlayMemoryOverflow,

    // §4.4 — semantic.
    MemoryOverlapsDevice,
    NonCanonicalGpa,
    ZeroSize,
    AddressOverflow,

    // §4.5 — maxima.
    TooManyCpus,
    TooManyMemoryRegions,
    TooManyDeviceRanges,

    // Property shape / parse.
    BadPropertyShape,
    Devtree(DtError),
}

impl From<DtError> for ValidationError {
    fn from(e: DtError) -> Self {
        ValidationError::Devtree(e)
    }
}

// ---------------------------------------------------------------------------
// Host-DTBO allowlist (pmi/spec/merged.md §2).
// ---------------------------------------------------------------------------

/// Enforce the merged-extension allowlist on the host-supplied DTBO
/// against the measured base DTB. The four allowed categories:
///
/// 1. `/cpus`: the overlay authors the entire subtree — it creates the
///    `/cpus` node itself (carrying only `#address-cells`/`#size-cells`)
///    and may add `cpu@N` nodes (the base declares no `/cpus`). The
///    cpu-node properties are host-authored; only `phandle` /
///    `linux,phandle` are prohibited. The count is DoS-bounded by
///    [`MAX_CPUS`] in [`validate_merged`], not template-matched.
/// 2. Nodes and properties under `/memory@*`.
/// 3. Nodes and properties under `/distance-map`.
/// 4. The `numa-node-id` property added to any base-declared node
///    (and it MUST NOT appear with any other host-contributed
///    property on the same node).
///
/// Fragments using a label-style target are rejected: the merged
/// model expects hosts to address the base via explicit paths.
pub fn validate_host_dtbo<'a, O, T>(overlay: &O, base: &T) -> Result<(), ValidationError>
where
    O: OverlayView<'a>,
    T: TreeView,
{
    for frag in overlay.fragments() {
        let Target::Path(path) = frag.target() else {
            return Err(ValidationError::HostOverlayUnsupportedTarget);
        };
        classify_fragment(path, frag, base)?;
    }
    Ok(())
}

fn classify_fragment<T>(path: &str, frag: Fragment<'_>, base: &T) -> Result<(), ValidationError>
where
    T: TreeView,
{
    let path = path.trim_end_matches('/');
    let path = if path.is_empty() { "/" } else { path };
    let overlay_node = frag.node();

    match path {
        "/" => {
            // Only allowed root-level additions: /memory@* and
            // /distance-map. Any property on '/' itself is rejected.
            if overlay_node.properties().next().is_some() {
                return Err(ValidationError::HostOverlayProhibitedPath);
            }
            for child in overlay_node.children() {
                let seg = top_segment(child.name());
                match seg {
                    "memory" => check_memory_node(&child)?,
                    "distance-map" => { /* contents unchecked beyond structural */ }
                    "cpus" => {
                        // The base declares no /cpus, so the host creates it
                        // via a root fragment; route through the cpus check
                        // (which permits the container's cell properties).
                        check_cpus_additions(&child)?;
                    }
                    _ => return Err(ValidationError::HostOverlayProhibitedPath),
                }
            }
        }
        "/cpus" => check_cpus_additions(&overlay_node)?,
        "/distance-map" => { /* allowed per allowlist 3 */ }
        p if memory_path(p) => check_memory_node(&overlay_node)?,
        _ => {
            // Allowed only if the overlay adds nothing but
            // numa-node-id on a base-declared node (rule 4).
            if base.find_path(path).is_none() {
                return Err(ValidationError::HostOverlayProhibitedPath);
            }
            let mut saw_numa = false;
            for p in overlay_node.properties() {
                if p.name() != "numa-node-id" {
                    return Err(ValidationError::HostOverlayConflictingProperty);
                }
                saw_numa = true;
            }
            if overlay_node.children().next().is_some() {
                return Err(ValidationError::HostOverlayProhibitedPath);
            }
            if !saw_numa {
                return Err(ValidationError::HostOverlayProhibitedPath);
            }
        }
    }
    Ok(())
}

/// True for any `/memory@<unit>` path. Avoids matching nested paths.
fn memory_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/memory") else {
        return false;
    };
    rest.starts_with('@') && !rest.contains('/')
}

fn check_cpus_additions<N>(overlay_cpus: &N) -> Result<(), ValidationError>
where
    N: NodeView,
{
    // The host authors the /cpus container, so it carries exactly the two
    // cell properties; any other property on /cpus itself is non-conformant.
    // Bad cell *values* are a boot-time DoS, not consumer-validated (§2).
    for p in overlay_cpus.properties() {
        if !matches!(p.name(), "#address-cells" | "#size-cells") {
            return Err(ValidationError::HostOverlayConflictingProperty);
        }
    }
    for child in overlay_cpus.children() {
        if top_segment(child.name()) != "cpu" {
            return Err(ValidationError::HostOverlayProhibitedPath);
        }
        // merged.md §2 cat 1: the host authors the cpu instances; their
        // `device_type` / `reg` / `status` / `enable-method` / `compatible`
        // are all allowed and DoS-bounded by count (in `validate_merged`),
        // not template-matched. Only `phandle` / `linux,phandle` are
        // prohibited — no host-contributed surface references cpus by
        // phandle, so we close that capability per the trust boundary.
        for p in child.properties() {
            if matches!(p.name(), "phandle" | "linux,phandle") {
                return Err(ValidationError::HostOverlayCpuPhandleProhibited);
            }
        }
    }
    Ok(())
}

fn check_memory_node<N: NodeView>(node: &N) -> Result<(), ValidationError> {
    let Some(reg) = NodeView::property(node, "reg") else {
        return Ok(());
    };
    let bytes = reg.as_ref();
    if !bytes.len().is_multiple_of(16) {
        return Err(ValidationError::HostOverlayMemoryOverflow);
    }
    for chunk in bytes.chunks_exact(16) {
        let r = parse_one_region(chunk);
        if r.end().is_none() {
            return Err(ValidationError::HostOverlayMemoryOverflow);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Merged-tree validation: only host-touchable surfaces + the
// memory-vs-device overlap check 2.
// ---------------------------------------------------------------------------

pub fn validate_merged<T: TreeView>(tree: &T) -> Result<(), ValidationError> {
    let mut memory: ArrayVec<Region, MAX_MEMORY_REGIONS> = ArrayVec::new();
    let mut devices: ArrayVec<Region, MAX_DEVICE_RANGES> = ArrayVec::new();
    let mut cpu_count: usize = 0;

    for child in tree.root().children() {
        match top_segment(child.name()) {
            "cpus" => count_cpus(&child, &mut cpu_count)?,
            "memory" => extract_memory(&child, &mut memory)?,
            // Any other top-level node carrying a `reg` is a platform device
            // (E1, §4.4). Matching by structure (has `reg`), not by node name,
            // is robust to the device-model's names (interrupt-controller@,
            // msi-controller@, serial@, virtio_mmio@, pcie@, …) and collects
            // every device MMIO region rather than a hand-listed subset.
            _ => {
                if NodeView::property(&child, "reg").is_some() {
                    extract_device_regs(&child, &mut devices)?;
                }
            }
        }
    }

    semantic_overlap_checks(&memory, &devices)?;
    Ok(())
}

/// Count `/cpus/cpu@N` children for the [`MAX_CPUS`] DoS bound.
/// Host can add cpu@N nodes per §4.2, so this bound is load-bearing.
fn count_cpus<N: NodeView>(node: &N, count: &mut usize) -> Result<(), ValidationError> {
    for _ in node.children() {
        *count = count.checked_add(1).ok_or(ValidationError::TooManyCpus)?;
        if *count > MAX_CPUS {
            return Err(ValidationError::TooManyCpus);
        }
    }
    Ok(())
}

/// Extract a `/memory@*` node's `reg` pairs into `regions`. Host
/// can add memory nodes per §4.2, so validation here is adversarial
/// (`reg` byte-shape and per-region overflow/canonical/nonzero per
/// merged.md §2's "Address-bearing values" rule).
fn extract_memory<N: NodeView>(
    node: &N,
    regions: &mut ArrayVec<Region, MAX_MEMORY_REGIONS>,
) -> Result<(), ValidationError> {
    let Some(reg) = NodeView::property(node, "reg") else {
        return Ok(());
    };
    let bytes = reg.as_ref();
    if !bytes.len().is_multiple_of(16) {
        return Err(ValidationError::BadPropertyShape);
    }
    for chunk in bytes.chunks_exact(16) {
        let r = parse_one_region(chunk);
        validate_region(r)?;
        regions
            .try_push(r)
            .map_err(|_| ValidationError::TooManyMemoryRegions)?;
    }
    Ok(())
}

/// Extract `reg` pairs from a device node (`/intc`, `/syscon`,
/// `/pci`) into `devices` for the §4.4 check 2 overlap test.
/// The host can place `/memory@*` overlapping any of these device
/// bases, which IS adversarial; the extraction itself is base-only.
fn extract_device_regs<N: NodeView>(
    node: &N,
    devices: &mut ArrayVec<Region, MAX_DEVICE_RANGES>,
) -> Result<(), ValidationError> {
    let Some(reg) = NodeView::property(node, "reg") else {
        return Ok(());
    };
    let bytes = reg.as_ref();
    if !bytes.len().is_multiple_of(16) {
        return Err(ValidationError::BadPropertyShape);
    }
    for chunk in bytes.chunks_exact(16) {
        let r = parse_one_region(chunk);
        validate_region(r)?;
        devices
            .try_push(r)
            .map_err(|_| ValidationError::TooManyDeviceRanges)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Semantic checks (§4.4).
// ---------------------------------------------------------------------------

fn semantic_overlap_checks(memory: &[Region], devices: &[Region]) -> Result<(), ValidationError> {
    // §4.4 check 2: /memory@* does not overlap any device region.
    for m in memory {
        for d in devices {
            if regions_overlap(*m, *d) {
                return Err(ValidationError::MemoryOverlapsDevice);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Region {
    pub gpa: u64,
    pub size: u64,
}

impl Region {
    fn end(self) -> Option<u64> {
        self.gpa.checked_add(self.size)
    }
}

/// Validate a region's invariants: nonzero size, no end-overflow,
/// canonical GPA. Per merged.md §2 the overflow + canonical checks
/// are required on host-contributed addresses.
fn validate_region(r: Region) -> Result<(), ValidationError> {
    if r.size == 0 {
        return Err(ValidationError::ZeroSize);
    }
    let end = r.end().ok_or(ValidationError::AddressOverflow)?;
    if end > (1u64 << 48) {
        return Err(ValidationError::NonCanonicalGpa);
    }
    Ok(())
}

fn regions_overlap(a: Region, b: Region) -> bool {
    let a_end = match a.end() {
        Some(e) => e,
        None => return true,
    };
    let b_end = match b.end() {
        Some(e) => e,
        None => return true,
    };
    a.gpa < b_end && b.gpa < a_end
}

/// Parse one 16-byte `reg` chunk: a (u64 address, u64 size) pair
/// in big-endian byte order. Assumes the parent's `#address-cells`
/// and `#size-cells` are both 2 — the qemu/virt convention that
/// arma+dillo produce. Callers gate on `len() % 16 == 0` before
/// chunking, so this function is total.
fn parse_one_region(chunk: &[u8]) -> Region {
    debug_assert_eq!(chunk.len(), 16);
    let gpa = u64::from_be_bytes([
        chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
    ]);
    let size = u64::from_be_bytes([
        chunk[8], chunk[9], chunk[10], chunk[11], chunk[12], chunk[13], chunk[14], chunk[15],
    ]);
    Region { gpa, size }
}

/// Strip the `@unit-address` suffix from a node name to get the
/// canonical node type segment (e.g. `memory@40000000` → `memory`).
fn top_segment(name: &str) -> &str {
    match name.split_once('@') {
        Some((seg, _)) => seg,
        None => name,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use devtree::{Overlay, Tree};

    #[test]
    fn region_validation_rejects_zero_size() {
        assert_eq!(
            validate_region(Region {
                gpa: 0x1000,
                size: 0
            }),
            Err(ValidationError::ZeroSize)
        );
    }

    #[test]
    fn region_validation_rejects_overflow() {
        assert_eq!(
            validate_region(Region {
                gpa: u64::MAX - 0xFFF,
                size: 0x1000
            }),
            Err(ValidationError::AddressOverflow)
        );
    }

    #[test]
    fn region_validation_rejects_non_canonical() {
        assert_eq!(
            validate_region(Region {
                gpa: 1u64 << 48,
                size: 0x1000
            }),
            Err(ValidationError::NonCanonicalGpa)
        );
    }

    #[test]
    fn overlap_detection() {
        let a = Region {
            gpa: 0x1000,
            size: 0x2000,
        };
        let b = Region {
            gpa: 0x2000,
            size: 0x1000,
        };
        let c = Region {
            gpa: 0x4000,
            size: 0x1000,
        };
        assert!(regions_overlap(a, b));
        assert!(!regions_overlap(a, c));
        assert!(!regions_overlap(b, c));
    }

    #[test]
    fn parse_one_region_basic() {
        let chunk: &[u8] = &[
            0, 0, 0, 0, 0x40, 0, 0, 0, // gpa 0x40000000
            0, 0, 0, 0, 0x10, 0, 0, 0, // size 0x10000000
        ];
        assert_eq!(
            parse_one_region(chunk),
            Region {
                gpa: 0x40000000,
                size: 0x10000000
            }
        );
    }

    // ----- DTB-driven tests using devtree on real fixtures --------------

    #[test]
    fn merged_dtb_basic_fixture_passes() {
        let blob = include_bytes!("../../dtb2acpi/tests/data/basic.dtb");
        let tree: devtree::Tree<'_> = devtree::Tree::parse(blob).unwrap();
        validate_merged(&tree).expect("basic fixture should pass");
    }

    // ----- Host-DTBO allowlist tests (merged.md §2) ---------------------
    //
    // The host authors the entire /cpus subtree via a root fragment (the
    // base declares no /cpus). These compile DTS with `dtc` at run time
    // and skip gracefully when it is absent, mirroring dillo's adversarial
    // suite — no committed binary fixtures.

    fn dtc_compile(dts: &str) -> Option<Vec<u8>> {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut child = Command::new("dtc")
            .args(["-I", "dts", "-O", "dtb"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        child.stdin.take()?.write_all(dts.as_bytes()).ok()?;
        let out = child.wait_with_output().ok()?;
        out.status.success().then_some(out.stdout)
    }

    const MINIMAL_BASE: &str = r#"/dts-v1/;
/ { #address-cells = <2>; #size-cells = <2>; };"#;

    /// Build a root-targeting overlay whose `__overlay__` holds `body`.
    fn root_overlay(body: &str) -> String {
        format!(
            "/dts-v1/;\n/ {{ fragment@0 {{ target-path = \"/\";\n\
             __overlay__ {{ {body} }}; }}; }};"
        )
    }

    /// T2: a host-authored /cpus subtree (container cell props + a full
    /// cpu@N) delivered via a root fragment is accepted.
    #[test]
    fn host_authored_cpus_subtree_accepted() {
        let overlay_dts = root_overlay(
            r#"cpus { #address-cells = <1>; #size-cells = <0>;
                 cpu@0 { device_type = "cpu"; reg = <0>; status = "okay";
                         enable-method = "psci"; compatible = "arm,neoverse-v2"; }; };"#,
        );
        let (Some(ob), Some(bb)) = (dtc_compile(&overlay_dts), dtc_compile(MINIMAL_BASE)) else {
            eprintln!("skipping: dtc not available");
            return;
        };
        let overlay: Overlay = Overlay::parse(&ob).unwrap();
        let base: Tree = Tree::parse(&bb).unwrap();
        validate_host_dtbo(&overlay, &base).expect("host-authored /cpus must be accepted");
    }

    /// T4: a `phandle` on a host cpu node is rejected.
    #[test]
    fn cpu_phandle_rejected() {
        let overlay_dts = root_overlay(
            r#"cpus { #address-cells = <1>; #size-cells = <0>;
                 cpu@0 { device_type = "cpu"; reg = <0>; phandle = <1>; }; };"#,
        );
        let (Some(ob), Some(bb)) = (dtc_compile(&overlay_dts), dtc_compile(MINIMAL_BASE)) else {
            eprintln!("skipping: dtc not available");
            return;
        };
        let overlay: Overlay = Overlay::parse(&ob).unwrap();
        let base: Tree = Tree::parse(&bb).unwrap();
        assert_eq!(
            validate_host_dtbo(&overlay, &base),
            Err(ValidationError::HostOverlayCpuPhandleProhibited)
        );
    }

    /// T1: a non-cell property on the /cpus node itself is rejected.
    #[test]
    fn non_cell_property_on_cpus_rejected() {
        let overlay_dts =
            root_overlay(r#"cpus { #address-cells = <1>; #size-cells = <0>; bogus = <1>; };"#);
        let (Some(ob), Some(bb)) = (dtc_compile(&overlay_dts), dtc_compile(MINIMAL_BASE)) else {
            eprintln!("skipping: dtc not available");
            return;
        };
        let overlay: Overlay = Overlay::parse(&ob).unwrap();
        let base: Tree = Tree::parse(&bb).unwrap();
        assert_eq!(
            validate_host_dtbo(&overlay, &base),
            Err(ValidationError::HostOverlayConflictingProperty)
        );
    }

    /// T4: a fragment targeting an off-allowlist path absent from the base
    /// is rejected.
    #[test]
    fn off_allowlist_path_rejected() {
        let overlay_dts = "/dts-v1/;\n/ { fragment@0 { target-path = \"/soc\";\n\
             __overlay__ { foo = <1>; }; }; };";
        let (Some(ob), Some(bb)) = (dtc_compile(overlay_dts), dtc_compile(MINIMAL_BASE)) else {
            eprintln!("skipping: dtc not available");
            return;
        };
        let overlay: Overlay = Overlay::parse(&ob).unwrap();
        let base: Tree = Tree::parse(&bb).unwrap();
        assert_eq!(
            validate_host_dtbo(&overlay, &base),
            Err(ValidationError::HostOverlayProhibitedPath)
        );
    }

    /// T3: a merged tree whose /cpus exceeds MAX_CPUS is rejected.
    #[test]
    fn too_many_cpus_rejected() {
        let mut body = String::from("/dts-v1/;\n/ { cpus {\n");
        for n in 0..=MAX_CPUS {
            body.push_str(&format!(
                "cpu@{n} {{ device_type = \"cpu\"; reg = <{n}>; }};\n"
            ));
        }
        body.push_str("}; };");
        let Some(bb) = dtc_compile(&body) else {
            eprintln!("skipping: dtc not available");
            return;
        };
        let tree: Tree = Tree::parse(&bb).unwrap();
        assert_eq!(validate_merged(&tree), Err(ValidationError::TooManyCpus));
    }
}
