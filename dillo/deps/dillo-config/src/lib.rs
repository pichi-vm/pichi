//! Device layout: the shared serde schema for dillo's `--blk`/`--gpt`/`--console`
//! key/value flags and the `--layout` JSON file, plus resolution and a
//! bus/slot allocator.
//!
//! One set of types is deserialized from BOTH sources:
//! - CLI: each flag's `k=v,…` string via [`serde_keyvalue`] (the spec types
//!   implement [`argh::FromArgValue`], behind the default-on `argh` feature).
//! - JSON: the whole [`Layout`] via `serde_json` (`--layout`).
//!
//! Nesting is expressed with explicit fields + brackets (not `#[serde(flatten)]`),
//! so `deny_unknown_fields` stays on every struct and a gpt device's
//! `partitions` array works identically in both forms:
//!
//! ```text
//! CLI:  --gpt partitions=[[path=a.img,partuuid=…,typeguid=…,label=esp]]
//! JSON: { "gpt": { "partitions": [ { "path": "a.img", … } ] } }
//! ```
//!
//! [`resolve`] turns a parsed [`Layout`] into validated [`Resolved`] facts
//! (hex parsing, GUID derivation, endpoint checks); [`allocate`] then assigns
//! each device a bus + slot.

use std::path::PathBuf;

use serde::Deserialize;
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Schema (shared by CLI key/value and JSON)
// ---------------------------------------------------------------------------

/// Which guest bus a device sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Bus {
    Pci,
    Mmio,
}

impl Bus {
    fn other(self) -> Bus {
        match self {
            Bus::Pci => Bus::Mmio,
            Bus::Mmio => Bus::Pci,
        }
    }
}

/// Full device layout. The top-level `bus` is the fleet default (used by
/// devices that don't pin one); per-device `bus` overrides it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Layout {
    #[serde(default)]
    pub bus: Option<Bus>,
    #[serde(default)]
    pub console: Option<ConsoleSpec>,
    #[serde(default)]
    pub devices: Vec<Device>,
}

/// One device entry. JSON externally-tagged: `{"blk": {…}}` / `{"gpt": {…}}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Device {
    Blk(BlkSpec),
    Gpt(GptSpec),
}

/// Console placement + endpoint.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ConsoleSpec {
    #[serde(default = "default_console_endpoint")]
    pub endpoint: String,
    #[serde(default)]
    pub bus: Option<Bus>,
    #[serde(default)]
    pub slot: Option<u32>,
}

fn default_console_endpoint() -> String {
    "stdio".to_string()
}

/// Traditional single-file virtio-blk device.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct BlkSpec {
    pub path: PathBuf,
    #[serde(default)]
    pub readonly: bool,
    #[serde(default)]
    pub bus: Option<Bus>,
    #[serde(default)]
    pub slot: Option<u32>,
}

/// Virtualized-GPT device: a synthesized GPT over the partition backing files.
/// `device-id`/`disk-guid` are derived from the partition PARTUUIDs when omitted.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct GptSpec {
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub disk_guid: Option<String>,
    pub partitions: Vec<PartSpec>,
    #[serde(default)]
    pub bus: Option<Bus>,
    #[serde(default)]
    pub slot: Option<u32>,
}

/// One GPT partition. Fields are stamped verbatim into the synthesized entry.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct PartSpec {
    pub path: PathBuf,
    pub partuuid: uuid::Uuid,
    pub typeguid: uuid::Uuid,
    pub label: String,
    /// 64-bit attributes as 16 hex chars; default 0.
    #[serde(default)]
    pub attrs: Option<String>,
}

// ---------------------------------------------------------------------------
// argh integration (CLI key/value → spec, via serde_keyvalue)
// ---------------------------------------------------------------------------

#[cfg(feature = "argh")]
macro_rules! impl_kv_arg {
    ($($t:ty),* $(,)?) => {$(
        impl argh::FromArgValue for $t {
            fn from_arg_value(value: &str) -> Result<Self, String> {
                serde_keyvalue::from_key_values(value).map_err(|e| e.to_string())
            }
        }
    )*};
}

#[cfg(feature = "argh")]
impl_kv_arg!(BlkSpec, GptSpec);

#[cfg(feature = "argh")]
impl argh::FromArgValue for ConsoleSpec {
    fn from_arg_value(value: &str) -> Result<Self, String> {
        // Allow the terse `--console stdio` form: a bare value with no `=`
        // becomes `endpoint=<value>`.
        let normalized = if value.contains('=') {
            value.to_string()
        } else {
            format!("endpoint={value}")
        };
        serde_keyvalue::from_key_values(&normalized).map_err(|e| e.to_string())
    }
}

#[cfg(feature = "argh")]
impl argh::FromArgValue for Bus {
    fn from_arg_value(value: &str) -> Result<Self, String> {
        match value {
            "pci" => Ok(Bus::Pci),
            "mmio" => Ok(Bus::Mmio),
            other => Err(format!("unknown bus `{other}` (expected `pci` or `mmio`)")),
        }
    }
}

// ---------------------------------------------------------------------------
// Resolution (parse hex, derive GUIDs, validate)
// ---------------------------------------------------------------------------

/// Validated, placement-ready layout.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub default_bus: Option<Bus>,
    pub console: ResolvedConsole,
    pub devices: Vec<ResolvedDevice>,
}

#[derive(Debug, Clone)]
pub struct Placement {
    pub bus: Option<Bus>,
    pub slot: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct ResolvedConsole {
    pub placement: Placement,
}

#[derive(Debug, Clone)]
pub enum ResolvedDevice {
    Blk {
        path: PathBuf,
        readonly: bool,
        placement: Placement,
    },
    Gpt {
        device_id: [u8; 20],
        disk_guid: [u8; 16],
        partitions: Vec<ResolvedPart>,
        placement: Placement,
    },
}

impl ResolvedDevice {
    pub fn placement(&self) -> &Placement {
        match self {
            ResolvedDevice::Blk { placement, .. } | ResolvedDevice::Gpt { placement, .. } => {
                placement
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedPart {
    pub path: PathBuf,
    pub partuuid: uuid::Uuid,
    pub typeguid: uuid::Uuid,
    pub label: String,
    pub attrs: u64,
}

/// Maximum GPT partitions (UEFI spec).
const MAX_PARTITIONS: usize = 128;
/// UEFI partition-name bound (72 bytes = 36 UTF-16 code units).
const MAX_LABEL_UTF16: usize = 36;

/// Resolve a parsed [`Layout`] into validated facts.
pub fn resolve(layout: Layout) -> anyhow::Result<Resolved> {
    let console = match layout.console {
        Some(c) => {
            anyhow::ensure!(
                c.endpoint == "stdio",
                "console endpoint `{}` unsupported (only `stdio`)",
                c.endpoint
            );
            ResolvedConsole {
                placement: Placement {
                    bus: c.bus,
                    slot: c.slot,
                },
            }
        }
        None => ResolvedConsole {
            placement: Placement {
                bus: None,
                slot: None,
            },
        },
    };

    let mut devices = Vec::with_capacity(layout.devices.len());
    for device in layout.devices {
        devices.push(resolve_device(device)?);
    }

    Ok(Resolved {
        default_bus: layout.bus,
        console,
        devices,
    })
}

fn resolve_device(device: Device) -> anyhow::Result<ResolvedDevice> {
    match device {
        Device::Blk(b) => Ok(ResolvedDevice::Blk {
            path: b.path,
            readonly: b.readonly,
            placement: Placement {
                bus: b.bus,
                slot: b.slot,
            },
        }),
        Device::Gpt(g) => {
            anyhow::ensure!(
                !g.partitions.is_empty(),
                "gpt device requires at least one partition"
            );
            anyhow::ensure!(
                g.partitions.len() <= MAX_PARTITIONS,
                "gpt device has {} partitions (GPT max is {MAX_PARTITIONS})",
                g.partitions.len()
            );
            let mut partitions = Vec::with_capacity(g.partitions.len());
            for p in &g.partitions {
                let units = p.label.encode_utf16().count();
                anyhow::ensure!(
                    units <= MAX_LABEL_UTF16,
                    "partition label {:?} exceeds {MAX_LABEL_UTF16} UTF-16 code units",
                    p.label
                );
                let attrs = match &p.attrs {
                    Some(s) => parse_attrs(s)?,
                    None => 0,
                };
                partitions.push(ResolvedPart {
                    path: p.path.clone(),
                    partuuid: p.partuuid,
                    typeguid: p.typeguid,
                    label: p.label.clone(),
                    attrs,
                });
            }

            let (derived_id, derived_guid) = derive_ids(&partitions);
            let device_id = match g.device_id {
                Some(s) => parse_device_id(&s)?,
                None => derived_id,
            };
            let disk_guid = match g.disk_guid {
                Some(s) => parse_disk_guid(&s)?,
                None => derived_guid,
            };

            Ok(ResolvedDevice::Gpt {
                device_id,
                disk_guid,
                partitions,
                placement: Placement {
                    bus: g.bus,
                    slot: g.slot,
                },
            })
        }
    }
}

/// Derive `(device_id[20], disk_guid[16])` from a SHA-256 over the partition
/// PARTUUIDs (matches the PoC `dillo run` derivation). Distinct partition sets
/// yield distinct ids; stable across boots.
fn derive_ids(partitions: &[ResolvedPart]) -> ([u8; 20], [u8; 16]) {
    let mut hasher = Sha256::new();
    for p in partitions {
        hasher.update(p.partuuid.as_bytes());
    }
    let digest = hasher.finalize();
    let mut device_id = [0u8; 20];
    device_id.copy_from_slice(&digest[..20]);
    let mut disk_guid = [0u8; 16];
    disk_guid.copy_from_slice(&digest[..16]);
    (device_id, disk_guid)
}

/// Parse `device-id`: `0x` + 40 hex chars → 20 bytes, OR printable ASCII <=20
/// bytes (NUL-padded).
fn parse_device_id(s: &str) -> anyhow::Result<[u8; 20]> {
    if let Some(hex_str) = s.strip_prefix("0x") {
        anyhow::ensure!(
            hex_str.len() == 40,
            "device-id 0x-hex must be exactly 40 chars (got {})",
            hex_str.len()
        );
        let bytes = hex::decode(hex_str)
            .map_err(|e| anyhow::anyhow!("device-id 0x{hex_str} not hex: {e}"))?;
        bytes.try_into().map_err(|v: Vec<u8>| {
            anyhow::anyhow!("device-id decoded to {} bytes (want 20)", v.len())
        })
    } else {
        anyhow::ensure!(
            s.len() <= 20,
            "device-id ASCII must be <=20 bytes (got {}; use 0x-hex form)",
            s.len()
        );
        anyhow::ensure!(
            s.bytes().all(|b| (0x20..=0x7E).contains(&b)),
            "device-id ASCII must be printable: {s:?}"
        );
        let mut out = [0u8; 20];
        out[..s.len()].copy_from_slice(s.as_bytes());
        Ok(out)
    }
}

/// Parse `disk-guid`: exactly 32 hex chars (optionally `0x`-prefixed) → 16 bytes.
fn parse_disk_guid(s: &str) -> anyhow::Result<[u8; 16]> {
    let hex_str = s.strip_prefix("0x").unwrap_or(s);
    anyhow::ensure!(
        hex_str.len() == 32,
        "disk-guid must be exactly 32 hex chars (got {})",
        hex_str.len()
    );
    let bytes =
        hex::decode(hex_str).map_err(|e| anyhow::anyhow!("disk-guid {hex_str} not hex: {e}"))?;
    bytes
        .try_into()
        .map_err(|v: Vec<u8>| anyhow::anyhow!("disk-guid decoded to {} bytes (want 16)", v.len()))
}

/// Parse `attrs`: exactly 16 hex chars → u64.
fn parse_attrs(s: &str) -> anyhow::Result<u64> {
    let hex_str = s.strip_prefix("0x").unwrap_or(s);
    anyhow::ensure!(
        hex_str.len() == 16,
        "attrs must be exactly 16 hex chars (got {})",
        hex_str.len()
    );
    u64::from_str_radix(hex_str, 16).map_err(|e| anyhow::anyhow!("attrs {hex_str} not hex: {e}"))
}

// ---------------------------------------------------------------------------
// Allocator
// ---------------------------------------------------------------------------

/// Available placement targets derived from the platform.
#[derive(Debug, Clone, Copy)]
pub struct Capacity {
    /// Number of usable PCI functions (device numbers 1..=pci), or `None` if the
    /// platform has no PCIe root.
    pub pci: Option<u32>,
    /// Number of virtio-mmio slots (indices 0..mmio).
    pub mmio: u32,
}

/// A resolved bus + index for one device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placed {
    pub bus: Bus,
    /// PCI device number (when `bus == Pci`) or virtio-mmio slot index.
    pub index: u32,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AllocError {
    #[error("device {index} requests bus `{bus:?}` but the platform DTB declares no such bus")]
    BusUnavailable { index: usize, bus: Bus },

    #[error(
        "no usable bus for device {index}: the platform DTB declares neither PCIe nor virtio-mmio"
    )]
    NoBus { index: usize },

    #[error("device {index}: {bus:?} slot {slot} is out of range (0..{max})")]
    SlotOutOfRange {
        index: usize,
        bus: Bus,
        slot: u32,
        max: u32,
    },

    #[error("device {index}: {bus:?} slot {slot} is already taken")]
    SlotConflict { index: usize, bus: Bus, slot: u32 },

    #[error(
        "out of {bus:?} slots placing device {index}: {requested} device(s) need that bus but only {capacity} slot(s) exist — pin some with bus=/slot=, free a slot, or add virtio_mmio nodes"
    )]
    Exhausted {
        index: usize,
        bus: Bus,
        requested: usize,
        capacity: u32,
    },
}

/// PCI device numbers start at 1 (slot 0 is the host bridge).
const PCI_FIRST_SLOT: u32 = 1;

/// Assign every request a bus + slot.
///
/// Bus per request: explicit `bus` if present (error if that bus is absent from
/// the DTB), else `default_bus` when available, else the other available bus.
/// Slots: requests with an explicit `slot` are placed first (reserving it),
/// then auto requests take the lowest free slot on their bus, in input order.
pub fn allocate(
    cap: &Capacity,
    default_bus: Bus,
    requests: &[Placement],
) -> Result<Vec<Placed>, AllocError> {
    let available = |bus: Bus| match bus {
        Bus::Pci => cap.pci.is_some(),
        Bus::Mmio => cap.mmio > 0,
    };
    let resolve_bus = |index: usize, req: &Placement| -> Result<Bus, AllocError> {
        match req.bus {
            Some(bus) => {
                if available(bus) {
                    Ok(bus)
                } else {
                    Err(AllocError::BusUnavailable { index, bus })
                }
            }
            None => {
                if available(default_bus) {
                    Ok(default_bus)
                } else if available(default_bus.other()) {
                    Ok(default_bus.other())
                } else {
                    Err(AllocError::NoBus { index })
                }
            }
        }
    };

    // PCI device numbers run 1..=pci; mmio indices run 0..mmio.
    let slot_max = |bus: Bus| match bus {
        Bus::Pci => cap.pci.unwrap_or(0),
        Bus::Mmio => cap.mmio,
    };
    let slot_range = |bus: Bus| match bus {
        Bus::Pci => PCI_FIRST_SLOT..(PCI_FIRST_SLOT + cap.pci.unwrap_or(0)),
        Bus::Mmio => 0..cap.mmio,
    };

    let mut used_pci: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
    let mut used_mmio: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();

    let mut out: Vec<Option<Placed>> = vec![None; requests.len()];

    // Pass 1: explicit slots reserve first.
    for (index, req) in requests.iter().enumerate() {
        let Some(slot) = req.slot else { continue };
        let bus = resolve_bus(index, req)?;
        let range = slot_range(bus);
        if !range.contains(&slot) {
            return Err(AllocError::SlotOutOfRange {
                index,
                bus,
                slot,
                max: slot_max(bus),
            });
        }
        let set = match bus {
            Bus::Pci => &mut used_pci,
            Bus::Mmio => &mut used_mmio,
        };
        if !set.insert(slot) {
            return Err(AllocError::SlotConflict { index, bus, slot });
        }
        out[index] = Some(Placed { bus, index: slot });
    }

    // Pass 2: auto slots take the lowest free index on their bus.
    for (index, req) in requests.iter().enumerate() {
        if req.slot.is_some() {
            continue;
        }
        let bus = resolve_bus(index, req)?;
        let set = match bus {
            Bus::Pci => &mut used_pci,
            Bus::Mmio => &mut used_mmio,
        };
        let slot = slot_range(bus)
            .find(|s| !set.contains(s))
            .ok_or(AllocError::Exhausted {
                index,
                bus,
                requested: requests.len(),
                capacity: slot_max(bus),
            })?;
        set.insert(slot);
        out[index] = Some(Placed { bus, index: slot });
    }

    Ok(out.into_iter().map(|p| p.expect("all placed")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- schema parity: CLI key/value == JSON ---

    #[test]
    fn blk_kv_matches_json() {
        let kv: BlkSpec = serde_keyvalue::from_key_values("path=disk.img,readonly,bus=mmio,slot=2")
            .expect("kv parse");
        let json: BlkSpec =
            serde_json::from_str(r#"{"path":"disk.img","readonly":true,"bus":"mmio","slot":2}"#)
                .expect("json parse");
        assert_eq!(kv.path, json.path);
        assert_eq!(kv.readonly, json.readonly);
        assert_eq!(kv.bus, json.bus);
        assert_eq!(kv.slot, json.slot);
        assert_eq!(kv.bus, Some(Bus::Mmio));
        assert!(kv.readonly);
    }

    #[test]
    fn gpt_kv_nested_partitions_match_json() {
        let kv: GptSpec = serde_keyvalue::from_key_values(
            "bus=pci,partitions=[[path=esp.img,partuuid=11111111-2222-2222-3333-333333334444,typeguid=0fc63daf-8483-4772-8e79-3d69d84330f1,label=esp]]",
        )
        .expect("kv parse");
        let json: GptSpec = serde_json::from_str(
            r#"{"bus":"pci","partitions":[{"path":"esp.img","partuuid":"11111111-2222-2222-3333-333333334444","typeguid":"0fc63daf-8483-4772-8e79-3d69d84330f1","label":"esp"}]}"#,
        )
        .expect("json parse");
        assert_eq!(kv.partitions.len(), 1);
        assert_eq!(kv.partitions[0].partuuid, json.partitions[0].partuuid);
        assert_eq!(kv.partitions[0].typeguid, json.partitions[0].typeguid);
        assert_eq!(kv.partitions[0].label, json.partitions[0].label);
        assert_eq!(kv.bus, json.bus);
    }

    #[test]
    fn layout_json_round_trips() {
        let layout: Layout = serde_json::from_str(
            r#"{"bus":"pci","devices":[{"blk":{"path":"a.img"}},{"gpt":{"partitions":[{"path":"b.img","partuuid":"11111111-2222-2222-3333-333333334444","typeguid":"0fc63daf-8483-4772-8e79-3d69d84330f1","label":"root"}]}}]}"#,
        )
        .expect("layout parse");
        assert_eq!(layout.bus, Some(Bus::Pci));
        assert_eq!(layout.devices.len(), 2);
        let resolved = resolve(layout).expect("resolve");
        assert_eq!(resolved.devices.len(), 2);
        // device-id/disk-guid were derived (non-zero) for the gpt device.
        if let ResolvedDevice::Gpt { disk_guid, .. } = &resolved.devices[1] {
            assert_ne!(*disk_guid, [0u8; 16]);
        } else {
            panic!("expected gpt device");
        }
    }

    #[test]
    fn unknown_field_rejected() {
        assert!(serde_keyvalue::from_key_values::<BlkSpec>("path=x,bogus=1").is_err());
        assert!(serde_json::from_str::<BlkSpec>(r#"{"path":"x","bogus":1}"#).is_err());
    }

    #[cfg(feature = "argh")]
    #[test]
    fn console_bare_value_shim() {
        let c = <ConsoleSpec as argh::FromArgValue>::from_arg_value("stdio").expect("bare");
        assert_eq!(c.endpoint, "stdio");
        let c = <ConsoleSpec as argh::FromArgValue>::from_arg_value("endpoint=stdio,bus=mmio")
            .expect("kv");
        assert_eq!(c.bus, Some(Bus::Mmio));
    }

    // --- allocator ---

    fn auto(n: usize) -> Vec<Placement> {
        (0..n)
            .map(|_| Placement {
                bus: None,
                slot: None,
            })
            .collect()
    }

    #[test]
    fn auto_prefers_default_bus_when_both_present() {
        let cap = Capacity {
            pci: Some(8),
            mmio: 8,
        };
        let placed = allocate(&cap, Bus::Pci, &auto(3)).unwrap();
        assert!(placed.iter().all(|p| p.bus == Bus::Pci));
        // console=slot1, then 2,3
        assert_eq!(placed[0].index, 1);
        assert_eq!(placed[1].index, 2);
        assert_eq!(placed[2].index, 3);
    }

    #[test]
    fn auto_picks_only_present_bus() {
        let cap = Capacity { pci: None, mmio: 4 };
        let placed = allocate(&cap, Bus::Pci, &auto(2)).unwrap();
        assert!(placed.iter().all(|p| p.bus == Bus::Mmio));
        assert_eq!(placed[0].index, 0);
        assert_eq!(placed[1].index, 1);
    }

    #[test]
    fn explicit_absent_bus_errors() {
        let cap = Capacity { pci: None, mmio: 4 };
        let reqs = vec![Placement {
            bus: Some(Bus::Pci),
            slot: None,
        }];
        assert_eq!(
            allocate(&cap, Bus::Mmio, &reqs),
            Err(AllocError::BusUnavailable {
                index: 0,
                bus: Bus::Pci
            })
        );
    }

    #[test]
    fn explicit_slot_reserved_before_auto() {
        let cap = Capacity {
            pci: Some(8),
            mmio: 8,
        };
        // device 0 auto, device 1 pinned to pci slot 1 → device 0 must avoid it.
        let reqs = vec![
            Placement {
                bus: None,
                slot: None,
            },
            Placement {
                bus: Some(Bus::Pci),
                slot: Some(1),
            },
        ];
        let placed = allocate(&cap, Bus::Pci, &reqs).unwrap();
        assert_eq!(
            placed[1],
            Placed {
                bus: Bus::Pci,
                index: 1
            }
        );
        assert_ne!(placed[0].index, 1);
        assert_eq!(
            placed[0],
            Placed {
                bus: Bus::Pci,
                index: 2
            }
        );
    }

    #[test]
    fn slot_conflict_errors() {
        let cap = Capacity {
            pci: Some(8),
            mmio: 8,
        };
        let reqs = vec![
            Placement {
                bus: Some(Bus::Pci),
                slot: Some(3),
            },
            Placement {
                bus: Some(Bus::Pci),
                slot: Some(3),
            },
        ];
        assert_eq!(
            allocate(&cap, Bus::Pci, &reqs),
            Err(AllocError::SlotConflict {
                index: 1,
                bus: Bus::Pci,
                slot: 3
            })
        );
    }

    #[test]
    fn exhaustion_errors() {
        let cap = Capacity {
            pci: Some(2),
            mmio: 0,
        };
        // pci slots 1,2 only → 3 auto devices overflow.
        let err = allocate(&cap, Bus::Pci, &auto(3)).unwrap_err();
        assert!(matches!(err, AllocError::Exhausted { bus: Bus::Pci, .. }));
    }

    #[test]
    fn derive_ids_distinct_per_partition_set() {
        let p = |u: u128| ResolvedPart {
            path: PathBuf::from("x"),
            partuuid: uuid::Uuid::from_u128(u),
            typeguid: uuid::Uuid::from_u128(1),
            label: "l".into(),
            attrs: 0,
        };
        let (id_a, guid_a) = derive_ids(&[p(1), p(2)]);
        let (id_b, guid_b) = derive_ids(&[p(1), p(3)]);
        assert_ne!(id_a, id_b);
        assert_ne!(guid_a, guid_b);
    }
}
