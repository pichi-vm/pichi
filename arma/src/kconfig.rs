//! Kernel `.config` parsing + transport support / slot inference
//! (device-model.md §6 "Slot composition").
//!
//! A consumer of the kernel's build config: Arma reads which device
//! transports the kernel can drive and sizes the board's slot capacity
//! accordingly. It never presumes — a guest with no drivable transport is
//! rejected rather than shipped unusable.

use std::io::Read;

use thiserror::Error;

use crate::kernel::Arch;

/// A parsed kernel build config (text Kconfig — `CONFIG_x=y/m` / `# … is not
/// set` lines). Held verbatim; symbols are matched on demand.
#[derive(Debug)]
pub(crate) struct KernelConfig {
    text: String,
}

#[derive(Debug, Error)]
pub(crate) enum SlotError {
    #[error(
        "kernel supports neither virtio-mmio nor PCI \
         (no CONFIG_VIRTIO_MMIO, and no CONFIG_PCI+CONFIG_VIRTIO_PCI); \
         a guest with no device-attach surface cannot be used"
    )]
    NoTransport,

    #[error("--mmio-slots requested but the kernel lacks CONFIG_VIRTIO_MMIO")]
    MmioUnsupported,

    #[error(
        "--pci-slots requested but the kernel lacks PCI support \
         (needs CONFIG_PCI + CONFIG_VIRTIO_PCI, plus CONFIG_PCI_HOST_GENERIC on aarch64)"
    )]
    PciUnsupported,
}

impl KernelConfig {
    pub(crate) fn parse(text: String) -> Self {
        Self { text }
    }

    /// C5: extract the kernel's embedded build config (`CONFIG_IKCONFIG`) — a
    /// gzip blob bracketed by `IKCFG_ST`/`IKCFG_ED` in the (decompressed)
    /// kernel image. Returns `None` if the kernel carries no embedded config.
    pub(crate) fn from_ikconfig(kernel: &[u8]) -> Option<Self> {
        const ST: &[u8] = b"IKCFG_ST";
        const ED: &[u8] = b"IKCFG_ED";
        let start = kernel.windows(ST.len()).position(|w| w == ST)? + ST.len();
        let end = start + kernel[start..].windows(ED.len()).position(|w| w == ED)?;
        let mut text = String::new();
        flate2::read::GzDecoder::new(&kernel[start..end])
            .read_to_string(&mut text)
            .ok()?;
        Some(Self::parse(text))
    }

    /// True iff `CONFIG_<sym>` is set to `y` or `m` (built-in or module).
    fn is_set(&self, sym: &str) -> bool {
        self.text.lines().any(|line| {
            line.trim_start()
                .strip_prefix(sym)
                .and_then(|rest| rest.strip_prefix('='))
                .is_some_and(|v| v == "y" || v == "m")
        })
    }

    /// The kernel's ISA build floor (C2 clamp) — the lowest `--profile` the
    /// kernel can run on. Upstream Kconfig carries no clean x86-64 microarch
    /// symbol (it's a distro `-march` build flag), and aarch64 ISA features are
    /// runtime-detected via alternatives, so for stock kernels the floor is the
    /// architecture baseline; a distro that marks a higher level
    /// (`CONFIG_X86_64_V{2,3,4}`) raises it. Conservative by design — it never
    /// reports a floor the kernel doesn't actually require.
    pub(crate) fn isa_floor(&self, arch: Arch) -> &'static str {
        match arch {
            Arch::X86_64 => {
                if self.is_set("CONFIG_X86_64_V4") {
                    "x86-64-v4"
                } else if self.is_set("CONFIG_X86_64_V3") {
                    "x86-64-v3"
                } else if self.is_set("CONFIG_X86_64_V2") {
                    "x86-64-v2"
                } else {
                    "x86-64-v1" // the x86-64 baseline
                }
            }
            // aarch64 features (LSE, PAN, MTE, …) are optional/runtime-detected,
            // not a build-time floor, so stock kernels require only v8.0-a.
            Arch::Aarch64 => "armv8.0-a",
        }
    }

    fn supports_virtio_mmio(&self) -> bool {
        self.is_set("CONFIG_VIRTIO_MMIO")
    }

    /// PCI ⇔ `CONFIG_PCI` + `CONFIG_VIRTIO_PCI` (and, on aarch64, the ECAM host
    /// driver `CONFIG_PCI_HOST_GENERIC`). On x86 base config reaches the bridge
    /// through the architectural `0xcf8`/`0xcfc` ports regardless.
    fn supports_pci(&self, arch: Arch) -> bool {
        self.is_set("CONFIG_PCI")
            && self.is_set("CONFIG_VIRTIO_PCI")
            && (!matches!(arch, Arch::Aarch64) || self.is_set("CONFIG_PCI_HOST_GENERIC"))
    }

    /// Resolve `(mmio_slots, pci_slots)` per §6 Slot composition:
    ///
    /// - **Neither given** — 16 total, split by support (8/8 if both, else all
    ///   16 to whichever single transport the kernel builds).
    /// - **Either given** — exactly what was asked (a missing flag is `0`),
    ///   failing if asked to declare a transport the kernel can't drive.
    ///
    /// Either way, fail if the kernel supports neither transport.
    pub(crate) fn infer_slots(
        &self,
        arch: Arch,
        mmio_override: Option<u32>,
        pci_override: Option<u32>,
    ) -> Result<(u32, u32), SlotError> {
        let mmio_ok = self.supports_virtio_mmio();
        let pci_ok = self.supports_pci(arch);
        if !mmio_ok && !pci_ok {
            return Err(SlotError::NoTransport);
        }

        match (mmio_override, pci_override) {
            (None, None) => Ok(match (mmio_ok, pci_ok) {
                (true, true) => (8, 8),
                (true, false) => (16, 0),
                (false, true) => (0, 16),
                (false, false) => unreachable!("guarded above"),
            }),
            (m, p) => {
                let mmio = m.unwrap_or(0);
                let pci = p.unwrap_or(0);
                if mmio > 0 && !mmio_ok {
                    return Err(SlotError::MmioUnsupported);
                }
                if pci > 0 && !pci_ok {
                    return Err(SlotError::PciUnsupported);
                }
                Ok((mmio, pci))
            }
        }
    }
}

/// The ordered ISA levels for an arch (ascending), used to compare profiles.
fn isa_levels(arch: Arch) -> &'static [&'static str] {
    match arch {
        Arch::X86_64 => &["x86-64-v1", "x86-64-v2", "x86-64-v3", "x86-64-v4"],
        Arch::Aarch64 => &[
            "armv8.0-a", "armv8.1-a", "armv8.2-a", "armv8.3-a", "armv8.4-a", "armv8.5-a",
            "armv8.6-a",
        ],
    }
}

/// C2 clamp: raise `chosen` up to `floor` if the kernel's build floor is higher
/// — the emitted `cpu:profile` must not sit below what the kernel requires.
/// An explicit profile arma doesn't recognize is left exactly as set (the
/// operator may have a vocabulary arma doesn't model).
pub(crate) fn raise_to_floor(chosen: &str, floor: &str, arch: Arch) -> String {
    let levels = isa_levels(arch);
    let rank = |p: &str| levels.iter().position(|&l| l == p);
    match (rank(chosen), rank(floor)) {
        (Some(c), Some(f)) if f > c => floor.to_string(),
        _ => chosen.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(s: &str) -> KernelConfig {
        KernelConfig::parse(s.to_string())
    }

    #[test]
    fn is_set_exact_and_module() {
        let c = cfg("CONFIG_PCI=y\nCONFIG_VIRTIO_MMIO=m\n# CONFIG_FOO is not set\n");
        assert!(c.is_set("CONFIG_PCI"));
        assert!(c.is_set("CONFIG_VIRTIO_MMIO")); // =m counts
        assert!(!c.is_set("CONFIG_FOO")); // "is not set"
        assert!(!c.is_set("CONFIG_PC")); // not a prefix match
    }

    #[test]
    fn both_transports_split_8_8() {
        // Alpine-like: PCI + ECAM + virtio-pci + virtio-mmio.
        let c = cfg(
            "CONFIG_PCI=y\nCONFIG_VIRTIO_PCI=y\nCONFIG_PCI_HOST_GENERIC=y\nCONFIG_VIRTIO_MMIO=m\n",
        );
        assert_eq!(c.infer_slots(Arch::Aarch64, None, None).unwrap(), (8, 8));
    }

    #[test]
    fn no_pci_all_to_mmio() {
        // Firecracker-like: no PCI, virtio-mmio only.
        let c = cfg("# CONFIG_PCI is not set\nCONFIG_VIRTIO_MMIO=y\n");
        assert_eq!(c.infer_slots(Arch::Aarch64, None, None).unwrap(), (16, 0));
    }

    #[test]
    fn aarch64_needs_host_generic_for_pci() {
        // PCI + virtio-pci but no ECAM host driver ⇒ not drivable on aarch64.
        let c = cfg("CONFIG_PCI=y\nCONFIG_VIRTIO_PCI=y\nCONFIG_VIRTIO_MMIO=y\n");
        assert_eq!(c.infer_slots(Arch::Aarch64, None, None).unwrap(), (16, 0));
        // x86 needs no host-generic driver (cf8/cfc base config).
        assert_eq!(c.infer_slots(Arch::X86_64, None, None).unwrap(), (8, 8));
    }

    #[test]
    fn neither_transport_is_rejected() {
        let c = cfg("# CONFIG_PCI is not set\n# CONFIG_VIRTIO_MMIO is not set\n");
        assert!(matches!(
            c.infer_slots(Arch::Aarch64, None, None),
            Err(SlotError::NoTransport)
        ));
    }

    #[test]
    fn explicit_slots_honored_and_checked() {
        let c = cfg("# CONFIG_PCI is not set\nCONFIG_VIRTIO_MMIO=y\n");
        // Explicit mmio honored; pci omitted (None ⇒ 0).
        assert_eq!(c.infer_slots(Arch::Aarch64, Some(4), None).unwrap(), (4, 0));
        // Asking for PCI the kernel can't drive ⇒ error.
        assert!(matches!(
            c.infer_slots(Arch::Aarch64, None, Some(4)),
            Err(SlotError::PciUnsupported)
        ));
    }

    #[test]
    fn from_ikconfig_extracts_embedded_config() {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write;
        let mut e = GzEncoder::new(Vec::new(), Compression::fast());
        e.write_all(b"CONFIG_PCI=y\nCONFIG_VIRTIO_MMIO=y\n").unwrap();
        let gz = e.finish().unwrap();
        // Embed between markers with junk on both sides (as in a real image).
        let mut kernel = vec![0xABu8; 64];
        kernel.extend_from_slice(b"IKCFG_ST");
        kernel.extend_from_slice(&gz);
        kernel.extend_from_slice(b"IKCFG_ED");
        kernel.extend_from_slice(&[0xCDu8; 32]);

        let c = KernelConfig::from_ikconfig(&kernel).expect("extract IKCONFIG");
        assert!(c.is_set("CONFIG_PCI"));
        assert!(c.is_set("CONFIG_VIRTIO_MMIO"));
        // A kernel without the markers yields None (caller errors / asks for --config).
        assert!(KernelConfig::from_ikconfig(b"no ikconfig here").is_none());
    }

    #[test]
    fn isa_floor_and_clamp() {
        // x86: no marker ⇒ baseline v1 floor; the v2 default is not lowered.
        let c = cfg("CONFIG_PCI=y\n");
        assert_eq!(c.isa_floor(Arch::X86_64), "x86-64-v1");
        assert_eq!(
            raise_to_floor("x86-64-v2", c.isa_floor(Arch::X86_64), Arch::X86_64),
            "x86-64-v2"
        );
        // x86: a v3-marked kernel raises the v2 default to v3 (the clamp).
        let c3 = cfg("CONFIG_X86_64_V3=y\n");
        assert_eq!(c3.isa_floor(Arch::X86_64), "x86-64-v3");
        assert_eq!(
            raise_to_floor("x86-64-v2", c3.isa_floor(Arch::X86_64), Arch::X86_64),
            "x86-64-v3"
        );
        // An explicit higher profile is kept (operator may require more).
        assert_eq!(raise_to_floor("x86-64-v4", "x86-64-v2", Arch::X86_64), "x86-64-v4");
        // aarch64 stock floor is the baseline; default stays.
        let a = cfg("CONFIG_ARM64_LSE_ATOMICS=y\n");
        assert_eq!(a.isa_floor(Arch::Aarch64), "armv8.0-a");
        // An unrecognized explicit profile is passed through unchanged.
        assert_eq!(raise_to_floor("vendor-custom", "x86-64-v3", Arch::X86_64), "vendor-custom");
    }
}
