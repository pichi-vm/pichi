// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Small PE/CBOR/DT fixtures for launcher metadata tests, not bootable guests.

#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used)]

use dillo_devtree::{
    devtree::{OwnedNode as Node, OwnedProperty as Property, OwnedTree},
    platform::Arch,
};
use object::{LittleEndian as LE, pe, pod};

pub(super) struct Pmi;

impl Pmi {
    pub(super) fn native() -> Vec<u8> {
        let arch = if cfg!(target_arch = "aarch64") {
            Arch::Aarch64
        } else {
            Arch::X86_64
        };
        Self::with_slots(arch, 31, 8)
    }

    pub(super) fn with_slots(arch: Arch, pci: u32, mmio: u32) -> Vec<u8> {
        let (machine, profile, vcpu) = match arch {
            Arch::X86_64 => (
                pe::IMAGE_FILE_MACHINE_AMD64,
                "x86-64-v1",
                serde_json::json!({}),
            ),
            Arch::Aarch64 => (
                pe::IMAGE_FILE_MACHINE_ARM64,
                "armv8.0-a",
                serde_json::json!({"pstate":5}),
            ),
        };
        let mut spec = Vec::new();
        ciborium::into_writer(
            &serde_json::json!({
                "version":1,"actions":[],"vm:vcpu":vcpu,"cpu:profile":profile,"dt:dtb":".dtb"
            }),
            &mut spec,
        )
        .unwrap();
        let dtb = Self::base(arch, pci, mmio).encode().unwrap();
        Self::pe(machine, &spec, &dtb)
    }

    fn node(name: &str, compatible: &str, base: u32, size: u32) -> Node {
        Node::new(name)
            .with_property(Property::new("compatible").with_str(compatible))
            .with_property(Property::new("reg").with_u32s(&[0, base, 0, size]))
    }

    fn base(arch: Arch, pci: u32, mmio: u32) -> OwnedTree {
        let mut root = Node::new("")
            .with_property(Property::new("#address-cells").with_u32(2))
            .with_property(Property::new("#size-cells").with_u32(2))
            .with_property(Property::new("model").with_str("Pichi placement fixture"))
            .with_property(Property::new("compatible").with_str("arma,v1"));
        let parent = match arch {
            Arch::X86_64 => {
                for (name, compatible, base, phandle) in [
                    (
                        "interrupt-controller@fee00000",
                        "intel,ce4100-lapic",
                        0xfee00000,
                        1,
                    ),
                    (
                        "interrupt-controller@fec00000",
                        "intel,ce4100-ioapic",
                        0xfec00000,
                        2,
                    ),
                ] {
                    root = root.with_child(
                        Self::node(name, compatible, base, 0x1000)
                            .with_property(Property::new("#interrupt-cells").with_u32(2))
                            .with_property(Property::new("interrupt-controller"))
                            .with_property(Property::new("phandle").with_u32(phandle)),
                    );
                }
                root = root.with_child(
                    Self::node("poweroff@9010000", "syscon-poweroff", 0x9010000, 4)
                        .with_property(Property::new("value").with_u32(0x34)),
                );
                2
            }
            Arch::Aarch64 => {
                root = root
                    .with_child(
                        Node::new("interrupt-controller@8000000")
                            .with_property(Property::new("compatible").with_str("arm,gic-v3"))
                            .with_property(
                                Property::new("reg").with_u32s(&[
                                    0, 0x8000000, 0, 0x10000, 0, 0x8100000, 0, 0x2000000,
                                ]),
                            )
                            .with_property(Property::new("#interrupt-cells").with_u32(3))
                            .with_property(Property::new("interrupt-controller"))
                            .with_property(Property::new("phandle").with_u32(1)),
                    )
                    .with_child(
                        Self::node(
                            "msi-controller@a100000",
                            "arm,gic-v2m-frame",
                            0xa100000,
                            0x10000,
                        )
                        .with_property(Property::new("msi-controller"))
                        .with_property(Property::new("arm,msi-base-spi").with_u32(64))
                        .with_property(Property::new("arm,msi-num-spis").with_u32(32))
                        .with_property(Property::new("phandle").with_u32(3)),
                    )
                    .with_child(
                        Node::new("timer")
                            .with_property(Property::new("compatible").with_str("arm,armv8-timer"))
                            .with_property(Property::new("interrupt-parent").with_u32(1))
                            .with_property(Property::new("interrupts").with_u32s(&[
                                1, 13, 0xff08, 1, 14, 0xff08, 1, 11, 0xff08, 1, 10, 0xff08,
                            ]))
                            .with_property(Property::new("always-on")),
                    )
                    .with_child(
                        Node::new("psci")
                            .with_property(
                                Property::new("compatible")
                                    .with_strs(&["arm,psci-1.0", "arm,psci-0.2"]),
                            )
                            .with_property(Property::new("method").with_str("hvc")),
                    );
                1
            }
        };
        for index in 0..mmio {
            let base = 0xa300000 + index * 0x200;
            let interrupts = match arch {
                Arch::X86_64 => vec![16 + index, 1],
                Arch::Aarch64 => vec![0, 16 + index, 1],
            };
            root = root.with_child(
                Self::node(&format!("virtio_mmio@{base:x}"), "virtio,mmio", base, 0x200)
                    .with_property(Property::new("interrupt-parent").with_u32(parent))
                    .with_property(Property::new("interrupts").with_u32s(&interrupts)),
            );
        }
        if pci > 0 {
            let mut node = Self::node(
                "pcie@b0000000",
                "pci-host-ecam-generic",
                0xb0000000,
                0x100000,
            )
            .with_property(Property::new("device_type").with_str("pci"))
            .with_property(Property::new("bus-range").with_u32s(&[0, 0]))
            .with_property(Property::new("#address-cells").with_u32(3))
            .with_property(Property::new("#size-cells").with_u32(2))
            .with_property(Property::new("ranges").with_u32s(&[
                0x03000000,
                0,
                0xc0000000,
                0,
                0xc0000000,
                0,
                pci * 0x2000,
            ]));
            if matches!(arch, Arch::Aarch64) {
                node = node.with_property(Property::new("msi-parent").with_u32(3));
            }
            root = root.with_child(node);
        }
        OwnedTree::new(root)
    }

    fn pe(machine: u16, spec: &[u8], dtb: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0; 512];
        let (dos, _) = pod::from_bytes_mut::<pe::ImageDosHeader>(&mut bytes).unwrap();
        dos.e_magic.set(LE, pe::IMAGE_DOS_SIGNATURE);
        dos.e_lfanew.set(LE, 64);
        let (nt, _) = pod::from_bytes_mut::<pe::ImageNtHeaders64>(&mut bytes[64..]).unwrap();
        nt.signature.set(LE, pe::IMAGE_NT_SIGNATURE);
        nt.file_header.machine.set(LE, machine);
        nt.file_header.number_of_sections.set(LE, 2);
        nt.file_header.size_of_optional_header.set(
            LE,
            (size_of::<pe::ImageOptionalHeader64>() + 16 * size_of::<pe::ImageDataDirectory>())
                as u16,
        );
        nt.optional_header
            .magic
            .set(LE, pe::IMAGE_NT_OPTIONAL_HDR64_MAGIC);
        nt.optional_header.section_alignment.set(LE, 4096);
        nt.optional_header.file_alignment.set(LE, 512);
        nt.optional_header.size_of_headers.set(LE, 512);
        nt.optional_header.number_of_rva_and_sizes.set(LE, 16);
        for (index, (name, data)) in [(b".pmi.vm\0", spec), (b".dtb\0\0\0\0", dtb)]
            .into_iter()
            .enumerate()
        {
            let offset = bytes.len();
            let padded = data.len().div_ceil(512) * 512;
            let start = 64
                + size_of::<pe::ImageNtHeaders64>()
                + 16 * size_of::<pe::ImageDataDirectory>()
                + index * size_of::<pe::ImageSectionHeader>();
            let (section, _) =
                pod::from_bytes_mut::<pe::ImageSectionHeader>(&mut bytes[start..]).unwrap();
            section.name = *name;
            section.virtual_size.set(LE, data.len() as u32);
            section.size_of_raw_data.set(LE, padded as u32);
            section.pointer_to_raw_data.set(LE, offset as u32);
            bytes.extend_from_slice(data);
            bytes.resize(offset + padded, 0);
        }
        bytes
    }
}
