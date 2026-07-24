// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The artifact **config blob** (`application/vnd.pichi.config.v1+json`) — the
//! image-level metadata carried in the OCI manifest `config` descriptor
//! (BUILD.md §7.1). It holds the **launch contract** (`requirements`): what the
//! host must provide to launch the artifact.
//!
//! The blob is self-identifying: a top-level `version` (currently `1`) states
//! the schema, so meaning changes (e.g. the memory unit) are unambiguous rather
//! than silently reinterpreted. Sizes are integers in the units named on each
//! field (memory in **MiB**), not raw bytes.
//!
//! The `Requirements` model (and its `Band`/`PortSpec`/`Interface`/`Ingress`
//! types) lives here — `pichi-artifact` owns the artifact's config schema. It is
//! JSON on the wire; the human-authored `config.yaml` is a build-time input the
//! build filters into this (that filtering is build-side).

use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The config-blob schema version carried in-body. Bumped when the meaning of a
/// field changes; consumers reject anything they don't understand.
pub const CONFIG_SCHEMA_VERSION: u32 = 1;

/// The pichi artifact config blob (`application/vnd.pichi.config.v1+json`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Schema version (see [`CONFIG_SCHEMA_VERSION`]). Required on the wire so
    /// the blob is self-identifying independent of the media-type suffix.
    pub version: u32,

    /// The host-facing launch contract.
    #[serde(default)]
    pub requirements: Requirements,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_SCHEMA_VERSION,
            requirements: Requirements::default(),
        }
    }
}

/// Errors constructing or validating a [`Config`].
#[derive(Debug, Error)]
pub enum ConfigError {
    /// JSON (de)serialisation failed.
    #[error("config JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// The blob declares a schema version this build does not understand.
    #[error("unsupported config schema version {got} (this build understands {want})")]
    UnsupportedVersion {
        /// The version found in the blob.
        got: u32,
        /// The version this build supports.
        want: u32,
    },
}

impl Config {
    /// Serialise to the config-blob JSON bytes (compact, canonical).
    ///
    /// The requirements are canonicalised first (see
    /// [`Requirements::canonicalize`]) so semantically-equal configs produce
    /// byte-identical blobs — the config is content-addressed, so canonical
    /// form is what lets equivalent launch contracts dedup.
    ///
    /// # Errors
    /// [`ConfigError::Json`] on serialisation failure.
    pub fn to_json(&self) -> Result<Vec<u8>, ConfigError> {
        let mut canonical = self.clone();
        canonical.requirements.canonicalize();
        Ok(serde_json::to_vec(&canonical)?)
    }

    /// Parse + validate config-blob JSON bytes.
    ///
    /// # Errors
    /// [`ConfigError`] on parse failure, an unsupported schema version, or an
    /// inconsistent requirement band.
    pub fn from_json(bytes: &[u8]) -> Result<Self, ConfigError> {
        let cfg: Self = serde_json::from_slice(bytes)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Check the schema version is understood. Requirement invariants (nonzero,
    /// 2-MiB memory, band ordering) are enforced by the field types at parse, so
    /// there is nothing else to check here. Shared by [`Self::from_json`] and by
    /// callers that parse the blob through another format (e.g. `--config` YAML).
    ///
    /// # Errors
    /// [`ConfigError::UnsupportedVersion`] if the version is not understood.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.version != CONFIG_SCHEMA_VERSION {
            return Err(ConfigError::UnsupportedVersion {
                got: self.version,
                want: CONFIG_SCHEMA_VERSION,
            });
        }
        Ok(())
    }
}

/// The host-facing launch contract (BUILD.md §7): what the host MUST provide to
/// launch a pichi artifact. Models the fields pichi acts on — `cpus`/`memory`
/// (VM sizing) and `interfaces` (network); `carapaces`/`volumes` (enforced
/// in-guest) are defined with the build effort, not here.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Requirements {
    /// vCPU band. The floor is 1 (a VM always has at least one vCPU), so a
    /// value of `1` carries no information and is canonicalised away; omit the
    /// band entirely to leave sizing to the operator / dillo default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<Band<NonZeroU32>>,

    /// Guest-memory band. Each tier is a [`Memory`] — MiB quantised to 2 MiB
    /// hugepages, nonzero — so a non-conforming size is unrepresentable rather
    /// than caught by a check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<Band<Memory>>,

    /// Network interfaces the host must wire (BUILD.md §7). One user-mode
    /// virtio-net device per entry; the map key is the operator-facing label.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub interfaces: BTreeMap<String, Interface>,
}

/// One declared network interface (BUILD.md §7).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Interface {
    /// Human-facing description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Ports the host must/should expose inbound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress: Option<Ingress>,

    /// Placement hint (`pci`/`mmio`); omit to let dillo choose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
}

/// Inbound port bands for an [`Interface`] (BUILD.md §7).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Ingress {
    /// Ports the host MUST expose, else launch errors.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required: Vec<PortSpec>,

    /// Ports the host SHOULD expose, else a warning.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recommended: Vec<PortSpec>,
}

/// One ingress port-list entry: a single port, an inclusive `[low, high]`
/// range, or `*` (all ports).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortSpec {
    /// A single port.
    Port(u16),
    /// An inclusive `[low, high]` range.
    Range(u16, u16),
    /// All ports (`*`).
    All,
}

impl Serialize for PortSpec {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            PortSpec::Port(p) => s.serialize_u16(*p),
            PortSpec::Range(lo, hi) => {
                use serde::ser::SerializeSeq;
                let mut seq = s.serialize_seq(Some(2))?;
                seq.serialize_element(lo)?;
                seq.serialize_element(hi)?;
                seq.end()
            }
            PortSpec::All => s.serialize_str("*"),
        }
    }
}

impl<'de> Deserialize<'de> for PortSpec {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = PortSpec;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a port (integer), an inclusive [low, high] range, or \"*\"")
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<PortSpec, E> {
                u16::try_from(v)
                    .map(PortSpec::Port)
                    .map_err(|_| E::custom(format!("port {v} out of range")))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<PortSpec, E> {
                if v == "*" {
                    Ok(PortSpec::All)
                } else {
                    Err(E::custom(format!(
                        "expected a port, range, or \"*\", got {v:?}"
                    )))
                }
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<PortSpec, A::Error> {
                use serde::de::Error as _;
                let lo: u16 = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::custom("range needs [low, high]"))?;
                let hi: u16 = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::custom("range needs [low, high]"))?;
                if seq.next_element::<u16>()?.is_some() {
                    return Err(A::Error::custom("range has more than two elements"));
                }
                Ok(PortSpec::Range(lo, hi))
            }
        }
        d.deserialize_any(V)
    }
}

/// A two-tier requirement (BUILD.md §7): `required` MUST be met or launch
/// errors; `recommended` SHOULD be met or the instance starts with a warning.
/// Neither is a ceiling; both are individually optional.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, bound(serialize = "T: Serialize"))]
pub struct Band<T> {
    // Private: the ordering invariant (`recommended ≥ required`) is enforced by
    // `new`/`Deserialize`, so bands can only be built through a checked path.
    #[serde(skip_serializing_if = "Option::is_none")]
    required: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recommended: Option<T>,
}

// Manual `Default` (the derive would wrongly require `T: Default`; an empty band
// is `None`/`None` regardless of `T`, so `NonZeroU32`/`Memory` bands work).
impl<T> Default for Band<T> {
    fn default() -> Self {
        Self {
            required: None,
            recommended: None,
        }
    }
}

impl<T: PartialOrd> Band<T> {
    /// Build a band, or `None` if `recommended` is below `required` — the sole
    /// *relational* invariant (it relates two values, so it can't be a value
    /// newtype). This is the one place it's checked; [`Deserialize`] routes
    /// through it, so any band you hold — from code or the wire — is ordered.
    #[must_use]
    pub fn new(required: Option<T>, recommended: Option<T>) -> Option<Self> {
        if let (Some(req), Some(rec)) = (&required, &recommended)
            && rec < req
        {
            return None;
        }
        Some(Self {
            required,
            recommended,
        })
    }
}

impl<T: Copy> Band<T> {
    /// The `required` tier, if set.
    #[must_use]
    pub fn required(self) -> Option<T> {
        self.required
    }

    /// The `recommended` tier, if set.
    #[must_use]
    pub fn recommended(self) -> Option<T> {
        self.recommended
    }
}

impl<T> Band<T> {
    /// True when neither tier is set (nothing to say).
    fn is_empty(&self) -> bool {
        self.required.is_none() && self.recommended.is_none()
    }
}

/// Deserialization mirror: parse the two tiers, then route through
/// [`Band::new`] so the ordering invariant holds for wire input too.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, bound(deserialize = "T: Deserialize<'de>"))]
struct BandWire<T> {
    #[serde(default)]
    required: Option<T>,
    #[serde(default)]
    recommended: Option<T>,
}

impl<'de, T> Deserialize<'de> for Band<T>
where
    T: Deserialize<'de> + PartialOrd,
{
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let w = BandWire::<T>::deserialize(d)?;
        Band::new(w.required, w.recommended)
            .ok_or_else(|| serde::de::Error::custom("recommended is below required"))
    }
}

/// Guest memory in MiB, quantised to whole 2 MiB hugepages. dillo backs guest
/// RAM with 2 MiB hugepages, so memory exists only in 2 MiB chunks; this newtype
/// makes a non-conforming size *unrepresentable* — construct only via
/// [`Memory::from_mib`] (nonzero and a multiple of [`Memory::CHUNK_MIB`]), and
/// the wire form (a MiB integer) is rejected at parse if it isn't. The invariant
/// then holds everywhere without a separate validation step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Memory(NonZeroU32);

impl Memory {
    /// The hugepage chunk memory is quantised to, in MiB.
    pub const CHUNK_MIB: u32 = 2;

    /// Build from a MiB size, or `None` if it is zero or not a multiple of
    /// [`Self::CHUNK_MIB`].
    #[must_use]
    pub fn from_mib(mib: u32) -> Option<Self> {
        if mib == 0 || !mib.is_multiple_of(Self::CHUNK_MIB) {
            return None;
        }
        NonZeroU32::new(mib).map(Self)
    }

    /// The size in MiB (always a positive multiple of [`Self::CHUNK_MIB`]).
    #[must_use]
    pub fn mib(self) -> u32 {
        self.0.get()
    }
}

impl Serialize for Memory {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u32(self.0.get())
    }
}

impl<'de> Deserialize<'de> for Memory {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mib = u32::deserialize(d)?;
        Self::from_mib(mib).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "memory {mib} MiB must be a positive multiple of {} (2 MiB hugepages)",
                Self::CHUNK_MIB
            ))
        })
    }
}

impl Requirements {
    /// Canonicalise for content-addressed emission: drop values that carry no
    /// information so semantically-equal contracts serialise identically.
    /// Currently: `cpus` has a hard floor of 1, so a `required`/`recommended`
    /// of `1` means "the floor" — indistinguishable from "unset" — and is
    /// removed; an emptied `cpus` band is dropped entirely.
    pub fn canonicalize(&mut self) {
        if let Some(b) = self.cpus.as_mut() {
            let floor = NonZeroU32::MIN; // 1
            if b.required == Some(floor) {
                b.required = None;
            }
            if b.recommended == Some(floor) {
                b.recommended = None;
            }
            if b.is_empty() {
                self.cpus = None;
            }
        }
    }

    /// Required vCPU floor, if declared (absent ⇒ the implicit floor of 1).
    #[must_use]
    pub fn cpus_required(&self) -> Option<u32> {
        self.cpus.and_then(|b| b.required).map(NonZeroU32::get)
    }

    /// Recommended vCPU count, if declared.
    #[must_use]
    pub fn cpus_recommended(&self) -> Option<u32> {
        self.cpus.and_then(|b| b.recommended).map(NonZeroU32::get)
    }

    /// Required memory floor in MiB, if declared.
    #[must_use]
    pub fn memory_required_mib(&self) -> Option<u32> {
        self.memory.and_then(|b| b.required).map(Memory::mib)
    }

    /// Recommended memory in MiB, if declared.
    #[must_use]
    pub fn memory_recommended_mib(&self) -> Option<u32> {
        self.memory.and_then(|b| b.recommended).map(Memory::mib)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nz(v: u32) -> NonZeroU32 {
        NonZeroU32::new(v).unwrap()
    }

    #[test]
    fn empty_config_round_trips() {
        let c = Config::default();
        let b = c.to_json().unwrap();
        assert_eq!(Config::from_json(&b).unwrap(), c);
    }

    #[test]
    fn requirements_round_trip_and_accessors() {
        let c = Config {
            version: CONFIG_SCHEMA_VERSION,
            requirements: Requirements {
                cpus: Some(Band {
                    required: Some(nz(2)),
                    recommended: Some(nz(4)),
                }),
                memory: Some(Band {
                    required: Memory::from_mib(2048),
                    recommended: Memory::from_mib(4096),
                }),
                interfaces: BTreeMap::new(),
            },
        };
        let b = c.to_json().unwrap();
        let c2 = Config::from_json(&b).unwrap();
        assert_eq!(c, c2);
        assert_eq!(c2.requirements.cpus_required(), Some(2));
        assert_eq!(c2.requirements.cpus_recommended(), Some(4));
        assert_eq!(c2.requirements.memory_required_mib(), Some(2048));
        assert_eq!(c2.requirements.memory_recommended_mib(), Some(4096));
    }

    #[test]
    fn cpus_floor_is_canonicalized_away() {
        // required == 1 is the floor (a VM always has ≥1 vCPU) → dropped, and an
        // emptied band disappears, so this equals a config with no cpus at all.
        let c = Config {
            requirements: Requirements {
                cpus: Some(Band {
                    required: Some(nz(1)),
                    recommended: None,
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let bytes = c.to_json().unwrap();
        assert_eq!(
            std::str::from_utf8(&bytes).unwrap(),
            r#"{"version":1,"requirements":{}}"#
        );
        // recommended==1 is likewise the floor, but a real recommended survives.
        let c2 = Config {
            requirements: Requirements {
                cpus: Some(Band {
                    required: Some(nz(1)),
                    recommended: Some(nz(4)),
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let back = Config::from_json(&c2.to_json().unwrap()).unwrap();
        assert_eq!(back.requirements.cpus_required(), None); // floor dropped
        assert_eq!(back.requirements.cpus_recommended(), Some(4));
    }

    #[test]
    fn rejects_recommended_below_required() {
        // The constructor rejects it — the relational invariant lives on the type...
        assert!(Band::new(Some(nz(4)), Some(nz(2))).is_none());
        assert!(Band::new(Some(nz(2)), Some(nz(4))).is_some());
        // ...and Deserialize routes through `new`, so the wire is rejected too.
        let err = Config::from_json(
            br#"{"version":1,"requirements":{"cpus":{"required":4,"recommended":2}}}"#,
        );
        assert!(matches!(err, Err(ConfigError::Json(_))));
    }

    #[test]
    fn rejects_zero_cpus_at_parse() {
        // Nonzero typing makes 0 unrepresentable — serde rejects it directly.
        let err = Config::from_json(br#"{"version":1,"requirements":{"cpus":{"required":0}}}"#);
        assert!(matches!(err, Err(ConfigError::Json(_))));
    }

    #[test]
    fn memory_must_be_2mib_aligned() {
        // The type makes a non-2-MiB size unrepresentable: no runtime check,
        // the constructor/parse refuses it.
        assert!(Memory::from_mib(2048).is_some());
        assert!(Memory::from_mib(2).is_some());
        assert!(Memory::from_mib(3).is_none()); // odd MiB
        assert!(Memory::from_mib(0).is_none()); // zero
        // ...and it's rejected at parse.
        let err = Config::from_json(br#"{"version":1,"requirements":{"memory":{"required":513}}}"#);
        assert!(matches!(err, Err(ConfigError::Json(_))));
    }

    #[test]
    fn rejects_unknown_version() {
        let err = Config::from_json(br#"{"version":2,"requirements":{}}"#);
        assert!(matches!(
            err,
            Err(ConfigError::UnsupportedVersion { got: 2, want: 1 })
        ));
    }

    #[test]
    fn port_spec_forms() {
        let ing = Ingress {
            required: vec![PortSpec::Port(443), PortSpec::Range(8000, 8999)],
            recommended: vec![PortSpec::All],
        };
        let j = serde_json::to_string(&ing).unwrap();
        assert_eq!(j, r#"{"required":[443,[8000,8999]],"recommended":["*"]}"#);
        let back: Ingress = serde_json::from_str(&j).unwrap();
        assert_eq!(back, ing);
    }
}
