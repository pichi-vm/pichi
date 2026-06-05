//! aarch64 CPU identity: host `MIDR_EL1` → registered DT `compatible`.
//!
//! Per PMI `merged.md` §2 the host overlay authors the cpu `compatible`.
//! On aarch64 dillo derives it from the host `MIDR_EL1` (= the guest's,
//! under KVM passthrough) and maps it to the registered core string. A
//! core we don't recognize yields `None`, and the overlay then omits the
//! property entirely — no generic placeholder, no invented string.

/// Map an aarch64 `MIDR_EL1` value to its registered devicetree
/// `compatible`. Implementer is bits `[31:24]`, part number `[15:4]`.
/// Returns `None` for cores not in the table.
///
/// Part numbers per `arch/arm64/include/asm/cputype.h`; the returned
/// strings are members of the `arm/cpus.yaml` upstream binding enum.
pub(crate) fn midr_to_compatible(midr: u64) -> Option<&'static str> {
    let implementer = (midr >> 24) & 0xff;
    let partnum = (midr >> 4) & 0xfff;
    match (implementer, partnum) {
        // Arm Ltd (implementer 0x41).
        (0x41, 0xd03) => Some("arm,cortex-a53"),
        (0x41, 0xd07) => Some("arm,cortex-a57"),
        (0x41, 0xd08) => Some("arm,cortex-a72"),
        (0x41, 0xd0b) => Some("arm,cortex-a76"),
        (0x41, 0xd0c) => Some("arm,neoverse-n1"),
        (0x41, 0xd40) => Some("arm,neoverse-v1"),
        (0x41, 0xd49) => Some("arm,neoverse-n2"),
        (0x41, 0xd4f) => Some("arm,neoverse-v2"),
        _ => None,
    }
}

/// The host CPU's registered `compatible` for authoring cpu instances in
/// the overlay. `None` on non-aarch64 (x86-64 has no DT cpu-compatible
/// vocabulary and no consumer for one) and for aarch64 cores not in the
/// table — in both cases the overlay omits the property.
pub(crate) fn host_cpu_compatible(arch: dillo_platform::Arch) -> Option<&'static str> {
    if arch != dillo_platform::Arch::Aarch64 {
        return None;
    }
    // Host MIDR_EL1 via sysfs (equals the guest's under KVM passthrough).
    let raw = std::fs::read_to_string("/sys/devices/system/cpu/cpu0/regs/identification/midr_el1")
        .ok()?;
    let s = raw.trim();
    let s = s.strip_prefix("0x").unwrap_or(s);
    let midr = u64::from_str_radix(s, 16).ok()?;
    midr_to_compatible(midr)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a MIDR with the given implementer `[31:24]` and part `[15:4]`.
    fn midr(implementer: u64, part: u64) -> u64 {
        (implementer << 24) | (part << 4)
    }

    #[test]
    fn known_cores_map_to_registered_compatibles() {
        assert_eq!(
            midr_to_compatible(midr(0x41, 0xd0c)),
            Some("arm,neoverse-n1")
        );
        assert_eq!(
            midr_to_compatible(midr(0x41, 0xd4f)),
            Some("arm,neoverse-v2")
        );
        assert_eq!(
            midr_to_compatible(midr(0x41, 0xd08)),
            Some("arm,cortex-a72")
        );
        assert_eq!(
            midr_to_compatible(midr(0x41, 0xd03)),
            Some("arm,cortex-a53")
        );
    }

    #[test]
    fn unknown_core_is_none_no_generic() {
        // Unknown part number under the Arm implementer.
        assert_eq!(midr_to_compatible(midr(0x41, 0xfff)), None);
        // Non-Arm implementer (e.g. Apple = 0x61).
        assert_eq!(midr_to_compatible(midr(0x61, 0x022)), None);
    }

    #[test]
    fn host_compatible_is_none_on_x86() {
        assert_eq!(host_cpu_compatible(dillo_platform::Arch::X86_64), None);
    }
}
