// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Resolve image transport preferences before handing explicit devices to Dillo.

#![forbid(unsafe_code)]

use std::{fs::File, path::Path};

use anyhow::{Context, Result, bail, ensure};
use conglobate::artifact::{Requirements, Slot};
use dillo_devtree::platform::{Arch, Machine};
use object::{Object, ObjectSection, read::ReadCache, read::pe::PeFile64};
use pmi::{Target, vm::Spec, vm::vcpu};

use super::requirements::SlotArgument as _;

/// Remaining declared capacity. No image preference authorizes extra hardware.
pub(super) struct DeviceBuses {
    pci: u32,
    mmio: u32,
}

impl DeviceBuses {
    /// Read only the PE metadata, target specification and bundled DT, not the
    /// guest payload. Dillo still validates and executes the complete image.
    fn from_image(path: &Path) -> Result<Self> {
        let cache = ReadCache::new(File::open(path).context("opening PMI for device placement")?);
        let pe = PeFile64::parse(&cache).context("reading PMI PE metadata")?;
        let section = pe
            .section_by_name(Spec::<vcpu::x86_64::CpuState>::SECTION)
            .context("PMI has no VM target")?;
        // Match Dillo's bounded target decoder. Section bytes include PE padding.
        ensure!(
            section.file_range().is_some_and(|(_, size)| size <= 4096),
            "PMI VM target exceeds 4 KiB or has no data"
        );
        let bytes = section.data().context("reading PMI VM target")?;
        let (arch, dt) = match pe.architecture() {
            object::Architecture::X86_64 => {
                let spec: Spec<vcpu::x86_64::CpuState> =
                    ciborium::de::from_reader_with_recursion_limit(bytes, 8)?;
                (Arch::X86_64, spec.dt_dtb)
            }
            object::Architecture::Aarch64 => {
                let spec: Spec<vcpu::aarch64::CpuState> =
                    ciborium::de::from_reader_with_recursion_limit(bytes, 8)?;
                (Arch::Aarch64, spec.dt_dtb)
            }
            other => bail!("unsupported PMI architecture: {other:?}"),
        };
        let dt = dt.context("device placement requires a bundled PMI base DTB")?;
        let section = pe
            .section_by_name(&dt)
            .context("PMI base DTB section is missing")?;
        ensure!(
            section
                .file_range()
                .is_some_and(|(_, size)| size <= 16 * 1024 * 1024),
            "PMI base DTB exceeds 16 MiB or has no data"
        );
        let platform =
            Machine::survey(section.data()?, arch).context("surveying image device slots")?;
        // Dillo assigns device numbers 1..=31 with a 0x2000 BAR stride.
        // Clamp before conversion so a large declared window cannot wrap.
        let pci = if platform.has_pcie {
            (platform.pcie.mmio_size / 0x2000).min(31) as u32
        } else {
            0
        };
        Ok(Self {
            pci,
            mmio: u32::try_from(platform.virtio_mmio.len())?,
        })
    }

    /// These are Pichi's own flag/value pairs, before conversion to native argv.
    /// Resolve in Dillo's device order so capacity fallback is deterministic.
    pub(super) fn prepare(
        argv: &mut Vec<String>,
        requirements: &Requirements,
        interfaces: impl Iterator<Item = Option<Slot>>,
    ) -> Result<()> {
        let pmi = argv
            .windows(2)
            .find(|pair| pair[0] == "--pmi")
            .context("device placement requires a PMI")?;
        Self::from_image(Path::new(&pmi[1]))?.assign(argv, requirements, interfaces)
    }

    fn assign(
        &mut self,
        argv: &mut Vec<String>,
        requirements: &Requirements,
        mut interfaces: impl Iterator<Item = Option<Slot>>,
    ) -> Result<()> {
        ensure!(
            argv.len().is_multiple_of(2),
            "incomplete Dillo argument pair"
        );
        if !argv.chunks_exact(2).any(|pair| pair[0] == "--console") {
            argv.extend(["--console".into(), "endpoint=stdio".into()]);
        }
        for flag in ["--console", "--blk", "--gpt", "--fs", "--net"] {
            for pair in argv.chunks_exact_mut(2).filter(|pair| pair[0] == flag) {
                let preference = match flag {
                    "--blk" => requirements
                        .storage
                        .as_ref()
                        .and_then(|storage| storage.slot),
                    "--net" => interfaces.next().flatten(),
                    _ => None,
                };
                let bus = self.take(preference.or(requirements.slot).unwrap_or(Slot::Pci))?;
                pair[1].push_str(",bus=");
                pair[1].push_str(bus.as_argument());
            }
        }
        Ok(())
    }

    fn take(&mut self, preference: Slot) -> Result<Slot> {
        let other = match preference {
            Slot::Pci => Slot::Mmio,
            Slot::Mmio => Slot::Pci,
        };
        for bus in [preference, other] {
            let remaining = match bus {
                Slot::Pci => &mut self.pci,
                Slot::Mmio => &mut self.mmio,
            };
            if *remaining > 0 {
                *remaining -= 1;
                return Ok(bus);
            }
        }
        bail!("image has insufficient declared PCI/virtio-mmio slots for the device set")
    }
}

#[cfg(test)]
#[path = "../../tests/support/pmi.rs"]
mod fixture;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_pmi_architectures_supply_declared_capacity() {
        for arch in [Arch::X86_64, Arch::Aarch64] {
            for (pci, mmio) in [(31, 8), (0, 4), (2, 0), (0, 0)] {
                let file = tempfile::NamedTempFile::new().unwrap();
                std::fs::write(file.path(), fixture::Pmi::with_slots(arch, pci, mmio)).unwrap();
                let buses = DeviceBuses::from_image(file.path()).unwrap();
                assert_eq!((buses.pci, buses.mmio), (pci, mmio));
            }
        }
    }

    #[test]
    fn preferences_fall_back_without_creating_slots() {
        let requirements: Requirements = serde_json::from_value(serde_json::json!({
            "slot":"mmio", "storage":{"required":1,"slot":"pci"},
            "interfaces":{"public":{"slot":"mmio"}}
        }))
        .unwrap();
        // Argument order differs from Dillo's console, block, GPT, FS, net order.
        let mut argv: Vec<String> = [
            "--net",
            "backend=user",
            "--gpt",
            "partitions=[]",
            "--blk",
            "path=disk",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        DeviceBuses { pci: 2, mmio: 2 }
            .assign(
                &mut argv,
                &requirements,
                requirements.interfaces.values().map(|i| i.slot),
            )
            .unwrap();
        let value = |flag| &argv.chunks_exact(2).find(|pair| pair[0] == flag).unwrap()[1];
        assert_eq!(value("--console"), "endpoint=stdio,bus=mmio");
        assert_eq!(value("--blk"), "path=disk,bus=pci");
        assert_eq!(value("--gpt"), "partitions=[],bus=mmio");
        assert_eq!(value("--net"), "backend=user,bus=pci");
        let requests = [
            dillo_config::Bus::Mmio,
            dillo_config::Bus::Pci,
            dillo_config::Bus::Mmio,
            dillo_config::Bus::Pci,
        ]
        .map(|bus| dillo_config::Placement {
            bus: Some(bus),
            slot: None,
        });
        assert!(
            dillo_config::allocate(
                &dillo_config::Capacity {
                    pci: Some(2),
                    mmio: 2
                },
                &requests
            )
            .is_ok()
        );

        let mut buses = DeviceBuses { pci: 0, mmio: 1 };
        assert_eq!(buses.take(Slot::Pci).unwrap(), Slot::Mmio);
        assert!(buses.take(Slot::Pci).is_err());
        let mut buses = DeviceBuses { pci: 1, mmio: 0 };
        assert_eq!(buses.take(Slot::Mmio).unwrap(), Slot::Pci);
        assert!(buses.take(Slot::Mmio).is_err());
    }

    #[test]
    fn build_placement_preserves_named_port_handles_and_mounts() {
        let console = super::super::build_port::console_arg(101, 102);
        let mut argv = super::super::build::build_dillo_args(
            Path::new("fixture"),
            None,
            &["partitions=[]".into()],
            Path::new("repo"),
            Path::new("cache"),
            Path::new("output"),
            Some(&console),
            None,
            None,
        );
        let before = argv.clone();
        DeviceBuses { pci: 0, mmio: 6 }
            .assign(&mut argv, &Requirements::default(), std::iter::empty())
            .unwrap();
        for (old, new) in before.chunks_exact(2).zip(argv.chunks_exact(2)) {
            assert_eq!(old[0], new[0]);
            if old[0] == "--pmi" {
                assert_eq!(old[1], new[1]);
            } else {
                assert_eq!(new[1], format!("{},bus=mmio", old[1]));
            }
        }
        let console = &argv
            .chunks_exact(2)
            .find(|pair| pair[0] == "--console")
            .unwrap()[1];
        let parsed: dillo_config::ConsoleSpec = serde_keyvalue::from_key_values(console).unwrap();
        assert_eq!(parsed.bus, Some(dillo_config::Bus::Mmio));
        assert_eq!(parsed.ports[0].rx.map(|pipe| pipe.number()), Some(101));
        assert_eq!(parsed.ports[0].tx.map(|pipe| pipe.number()), Some(102));
        assert!(
            DeviceBuses { pci: 0, mmio: 5 }
                .assign(
                    &mut before.clone(),
                    &Requirements::default(),
                    std::iter::empty()
                )
                .is_err()
        );
    }

    #[test]
    fn truncated_or_missing_metadata_is_rejected() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let valid = fixture::Pmi::native();
        for size in [0, 4, 64, 512, valid.len() - 512] {
            std::fs::write(file.path(), &valid[..size]).unwrap();
            assert!(
                DeviceBuses::from_image(file.path()).is_err(),
                "accepted length {size}"
            );
        }
    }
}
