//! Host DTBO synthesis.
//!
//! Builds an FDT overlay (in the standard `fragment@K + __overlay__`
//! shape that devtree's parser expects) covering the merged-extension
//! allowlist: the cpu instances `cpu@0..N` under `/cpus`, and `/memory@*`
//! per planned region.
//!
//! See `dillo/ARCHITECTURE.md` §7.

use anyhow::{Result, anyhow};

use crate::fdt_writer::FdtBuilder;
use crate::placement::Region;

pub fn synthesize_dtbo(
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
    use devtree::{NodeView, PropertyView, Tree, TreeView};

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
