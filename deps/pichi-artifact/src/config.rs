// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The artifact **config blob** (`application/vnd.pichi.config.v1+json`) — the
//! image-level metadata carried in the OCI manifest `config` descriptor
//! (BUILD.md §7.1). In v1 it holds the **launch contract** (`requirements`):
//! what the host must provide to launch the artifact.
//!
//! The `Requirements` model (and its `Band`/`PortSpec`/`Interface`/`Ingress`
//! types) lives here — `pichi-artifact` owns the artifact's config schema. It is
//! JSON on the wire (the config blob); the human-authored `config.yaml` is a
//! build-time input the build filters into this (that filtering is build-side).

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The pichi artifact config blob (`application/vnd.pichi.config.v1+json`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The host-facing launch contract.
    #[serde(default)]
    pub requirements: Requirements,
}

/// Errors constructing or validating a [`Config`].
#[derive(Debug, Error)]
pub enum ConfigError {
    /// JSON (de)serialisation failed.
    #[error("config JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// A requirement band is internally inconsistent.
    #[error("invalid requirements: {0}")]
    Invalid(String),
}

impl Config {
    /// Serialise to the config-blob JSON bytes (compact).
    ///
    /// # Errors
    /// [`ConfigError::Json`] on serialisation failure.
    pub fn to_json(&self) -> Result<Vec<u8>, ConfigError> {
        Ok(serde_json::to_vec(self)?)
    }

    /// Parse + validate config-blob JSON bytes.
    ///
    /// # Errors
    /// [`ConfigError`] on parse failure or an inconsistent requirement band.
    pub fn from_json(bytes: &[u8]) -> Result<Self, ConfigError> {
        let cfg: Self = serde_json::from_slice(bytes)?;
        cfg.requirements.validate()?;
        Ok(cfg)
    }
}

/// The host-facing launch contract (BUILD.md §7): what the host MUST provide to
/// launch a pichi artifact. v1 models the fields pichi acts on — `cpus`/`memory`
/// (VM sizing) and `interfaces` (network); `carapaces`/`volumes` (enforced
/// in-guest) are not modelled yet.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Requirements {
    /// vCPU band. Omit to leave sizing to the operator / dillo default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<Band<u32>>,

    /// Guest-memory band, in **bytes** (the config blob is machine JSON;
    /// human-readable `2GiB` sizes belong to `config.yaml`, which the build
    /// converts to bytes here).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<Band<u64>>,

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
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Band<T> {
    /// The host MUST provide at least this; otherwise launch errors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<T>,

    /// The host SHOULD provide at least this; otherwise a warning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended: Option<T>,
}

impl Requirements {
    /// Check band consistency: a required floor of 0 is rejected, and a
    /// `recommended` below its `required` is rejected.
    ///
    /// # Errors
    /// [`ConfigError::Invalid`] on an inconsistent band.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if let Some(b) = &self.cpus {
            if b.required == Some(0) {
                return Err(ConfigError::Invalid("cpus.required is 0".into()));
            }
            if let (Some(r), Some(rec)) = (b.required, b.recommended)
                && rec < r
            {
                return Err(ConfigError::Invalid(
                    "cpus.recommended is below cpus.required".into(),
                ));
            }
        }
        if let Some(b) = &self.memory
            && let (Some(r), Some(rec)) = (b.required, b.recommended)
            && rec < r
        {
            return Err(ConfigError::Invalid(
                "memory.recommended is below memory.required".into(),
            ));
        }
        Ok(())
    }

    /// Required vCPU floor, if declared.
    #[must_use]
    pub fn cpus_required(&self) -> Option<u32> {
        self.cpus.and_then(|b| b.required)
    }

    /// Recommended vCPU count, if declared.
    #[must_use]
    pub fn cpus_recommended(&self) -> Option<u32> {
        self.cpus.and_then(|b| b.recommended)
    }

    /// Required memory floor in MiB (rounded up), if declared.
    #[must_use]
    pub fn memory_required_mib(&self) -> Option<u32> {
        self.memory
            .and_then(|b| b.required)
            .map(Self::mib_from_bytes)
    }

    /// Recommended memory in MiB (rounded up), if declared.
    #[must_use]
    pub fn memory_recommended_mib(&self) -> Option<u32> {
        self.memory
            .and_then(|b| b.recommended)
            .map(Self::mib_from_bytes)
    }
}

impl Requirements {
    /// Round a byte count up to whole MiB, saturating at `u32::MAX` (dillo takes
    /// `--memory` in MiB).
    fn mib_from_bytes(bytes: u64) -> u32 {
        const MIB: u64 = 1 << 20;
        u32::try_from(bytes.div_ceil(MIB)).unwrap_or(u32::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_round_trips() {
        let c = Config::default();
        let b = c.to_json().unwrap();
        assert_eq!(Config::from_json(&b).unwrap(), c);
    }

    #[test]
    fn requirements_round_trip_and_accessors() {
        let c = Config {
            requirements: Requirements {
                cpus: Some(Band {
                    required: Some(1),
                    recommended: Some(4),
                }),
                memory: Some(Band {
                    required: Some(2 * 1024 * 1024 * 1024),
                    recommended: Some(4 * 1024 * 1024 * 1024),
                }),
                interfaces: BTreeMap::new(),
            },
        };
        let b = c.to_json().unwrap();
        let c2 = Config::from_json(&b).unwrap();
        assert_eq!(c, c2);
        assert_eq!(c2.requirements.cpus_required(), Some(1));
        assert_eq!(c2.requirements.cpus_recommended(), Some(4));
        assert_eq!(c2.requirements.memory_required_mib(), Some(2048));
        assert_eq!(c2.requirements.memory_recommended_mib(), Some(4096));
    }

    #[test]
    fn rejects_recommended_below_required() {
        let c = Config {
            requirements: Requirements {
                cpus: Some(Band {
                    required: Some(4),
                    recommended: Some(1),
                }),
                ..Default::default()
            },
        };
        let b = serde_json::to_vec(&c).unwrap();
        assert!(matches!(
            Config::from_json(&b),
            Err(ConfigError::Invalid(_))
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
