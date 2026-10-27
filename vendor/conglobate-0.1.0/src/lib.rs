// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! conglobate's library half: the `pichi.build` recipe I/O types shared
//! with the host — parse + validate for `carapace.yaml`, `pmi.yaml`,
//! `refs.lock` (BUILD.md §2.1, §2.2, §2.4), and the `requirements.yaml`
//! launch contract (§7).
//!
//! Shared by the host (`pichi build` / `pichi update` / `pichi run`) and the
//! in-guest build driver (`conglobate`). The types are the on-disk YAML
//! schema; a `parse` constructor on each runs `serde_yaml` then a structural
//! `validate` pass. `requirements.yaml` is currently authored by hand (the
//! official build image ships one to bootstrap the platform); the
//! `config.yaml` → `requirements.yaml` filter is still deferred.

use std::collections::BTreeMap;
use std::fmt;

use bytesize::ByteSize;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Recipe parse / validation failures.
#[derive(Debug, Error)]
pub enum RecipeError {
    #[error("parse: {0}")]
    Parse(#[from] serde_yaml::Error),

    #[error("invalid recipe: {0}")]
    Invalid(String),
}

/// `pichi.build/carapace.yaml` — derive a read-only carapace from one base
/// registry reference (BUILD.md §2.1).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CarapaceRecipe {
    /// Exactly one registry reference to derive from (v1: no
    /// `raw:`/`tarball:`/`oci:` — run `pichi import` first).
    pub from: String,

    /// Ordered directives, each producing one retained scute. Optional.
    #[serde(default)]
    pub derive: Vec<Directive>,
}

/// `pichi.build/pmi.yaml` — produce a single `.pmi` (BUILD.md §2.2).
/// Nothing but the file named by `into` is retained; steps are not
/// materialized as scutes.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PmiRecipe {
    /// Omit to build against the carapace produced by `carapace.yaml`
    /// (the common case). Required when there is no `carapace.yaml`, or
    /// to build against a different carapace.
    #[serde(default)]
    pub from: Option<String>,

    /// Ordered `run:`/`copy:` sequence run in a working filesystem; all
    /// intermediate state is discarded.
    #[serde(default)]
    pub derive: Vec<Directive>,

    /// Path where the `derive` sequence writes the finished `.pmi`.
    /// Required; no default.
    pub into: String,
}

/// One ordered build directive: run a shell command, or copy files from
/// the build context into the guest filesystem.
///
/// On the wire each directive is a single-key mapping (`{run: …}` or
/// `{copy: …}`), optionally with `network: <bool>` on a `run:` — serde_yaml
/// reserves the externally-tagged enum form for YAML `!tags`, so
/// [`Deserialize`] is hand-written over a `{run?, copy?, network?}` shape that
/// enforces exactly one of `run`/`copy`.
#[derive(Debug, Clone)]
pub enum Directive {
    /// Execute a shell command inside the build VM (working dir `/`); tools
    /// come from the parent scute. `network` requests an up interface for this
    /// stage only — conglobate brings the host-provided NIC up before the
    /// command and tears it back down after (default: no network).
    Run {
        /// The shell command to execute.
        command: String,
        /// Whether this stage runs with the network interface up.
        network: bool,
    },

    /// Copy from the build context into the guest filesystem, optionally
    /// setting ownership/mode in the same scute.
    Copy(CopyDirective),
}

impl<'de> Deserialize<'de> for Directive {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            #[serde(default)]
            run: Option<String>,
            #[serde(default)]
            copy: Option<CopyDirective>,
            #[serde(default)]
            network: bool,
        }
        let raw = Raw::deserialize(deserializer)?;
        match (raw.run, raw.copy) {
            (Some(command), None) => Ok(Directive::Run {
                command,
                network: raw.network,
            }),
            (None, Some(c)) => {
                if raw.network {
                    return Err(serde::de::Error::custom(
                        "`network` is only valid on a `run:` directive",
                    ));
                }
                Ok(Directive::Copy(c))
            }
            (None, None) => Err(serde::de::Error::custom(
                "directive must be a `run:` or `copy:` mapping",
            )),
            (Some(_), Some(_)) => Err(serde::de::Error::custom(
                "directive has both `run` and `copy`; use one per list item",
            )),
        }
    }
}

/// `copy:` directive payload (BUILD.md §2.1).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopyDirective {
    /// A single source path or a list of paths (relative to the build
    /// context). When a list, `into` must be a directory.
    pub from: FromSpec,

    /// Destination path in the guest filesystem.
    pub into: String,

    /// Owner to set (a name resolved against the parent scute's
    /// `/etc/passwd`, or a numeric id as a quoted string). Optional.
    #[serde(default)]
    pub owner: Option<String>,

    /// Group to set (name or quoted numeric id). Optional.
    #[serde(default)]
    pub group: Option<String>,

    /// Quoted octal mode (e.g. `"0755"`). A build error when `from` is a
    /// list (see [`CarapaceRecipe::validate`]). Optional.
    #[serde(default)]
    pub mode: Option<String>,
}

/// A `copy:` source: one path or many.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum FromSpec {
    One(String),
    Many(Vec<String>),
}

impl FromSpec {
    /// True when this is a list of paths (`into` must be a directory).
    pub fn is_list(&self) -> bool {
        matches!(self, FromSpec::Many(_))
    }
}

/// `pichi.build/refs.lock` — machine-written ref → (manifest, carapace)
/// pins (BUILD.md §2.4). The two hashes are independent commitments to
/// the same content: `manifest` is a flat SHA-256 over the OCI manifest
/// bytes; `carapace` is the dm-verity root over scute blocks.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RefsLock {
    pub entries: BTreeMap<String, LockEntry>,
}

/// One `refs.lock` entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockEntry {
    /// SHA-256 over the OCI manifest bytes (`sha256:…`).
    pub manifest: String,

    /// dm-verity Merkle root over the carapace's scute blocks (`sha256:…`).
    pub carapace: String,
}

/// The build VM's output manifest (`output/build.yaml`), written by
/// conglobate and read by `pichi build` to package the artifact. Filenames
/// are relative to the output sink directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildOutput {
    /// Scutes in chain order (base first, top last).
    pub scutes: Vec<ScuteOut>,

    /// The sealed PMI filename, if `pmi.yaml` ran. Absent for a
    /// carapace-only build.
    #[serde(default)]
    pub pmi: Option<String>,
}

/// One emitted scute in [`BuildOutput`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScuteOut {
    /// COW blob filename (relative to the output dir).
    pub cow: String,

    /// dm-verity blob filename (relative to the output dir).
    pub verity: String,

    /// Full dm-verity salt, hex (the scute's salt-chain prefix + suffix).
    pub salt: String,
}

impl BuildOutput {
    /// Parse `output/build.yaml`.
    pub fn parse(yaml: &str) -> Result<Self, RecipeError> {
        let out: Self = serde_yaml::from_str(yaml)?;
        Ok(out)
    }

    /// Serialize to YAML (what conglobate writes).
    pub fn to_yaml(&self) -> Result<String, RecipeError> {
        Ok(serde_yaml::to_string(self)?)
    }
}

impl CarapaceRecipe {
    /// Parse and validate a `carapace.yaml` document.
    pub fn parse(yaml: &str) -> Result<Self, RecipeError> {
        let recipe: Self = serde_yaml::from_str(yaml)?;
        recipe.validate()?;
        Ok(recipe)
    }

    /// Structural checks beyond serde's shape enforcement.
    fn validate(&self) -> Result<(), RecipeError> {
        if self.from.trim().is_empty() {
            return Err(RecipeError::Invalid(
                "carapace.yaml: `from` is empty".into(),
            ));
        }
        validate_directives(&self.derive)
    }
}

impl PmiRecipe {
    /// Parse and validate a `pmi.yaml` document.
    pub fn parse(yaml: &str) -> Result<Self, RecipeError> {
        let recipe: Self = serde_yaml::from_str(yaml)?;
        recipe.validate()?;
        Ok(recipe)
    }

    fn validate(&self) -> Result<(), RecipeError> {
        if self.into.trim().is_empty() {
            return Err(RecipeError::Invalid("pmi.yaml: `into` is empty".into()));
        }
        if let Some(from) = &self.from
            && from.trim().is_empty()
        {
            return Err(RecipeError::Invalid(
                "pmi.yaml: `from` is present but empty (omit it to build against carapace.yaml)"
                    .into(),
            ));
        }
        validate_directives(&self.derive)
    }
}

impl RefsLock {
    /// Parse a `refs.lock` document. An empty document is a valid empty
    /// lock (a project with no carapace references).
    pub fn parse(yaml: &str) -> Result<Self, RecipeError> {
        if yaml.trim().is_empty() {
            return Ok(Self::default());
        }
        let lock: Self = serde_yaml::from_str(yaml)?;
        for (reference, entry) in &lock.entries {
            entry.validate(reference)?;
        }
        Ok(lock)
    }

    /// Look up the pins for a reference exactly as written in a recipe.
    pub fn get(&self, reference: &str) -> Option<&LockEntry> {
        self.entries.get(reference)
    }

    /// Serialize to YAML (the body of `refs.lock`, sans the generated-file
    /// header `pichi update` prepends). Keys are emitted in sorted order
    /// (`BTreeMap`) for a stable, diff-friendly file.
    pub fn to_yaml(&self) -> Result<String, RecipeError> {
        Ok(serde_yaml::to_string(self)?)
    }
}

impl LockEntry {
    fn validate(&self, reference: &str) -> Result<(), RecipeError> {
        for (field, value) in [("manifest", &self.manifest), ("carapace", &self.carapace)] {
            if !value.starts_with("sha256:") || value.len() <= "sha256:".len() {
                return Err(RecipeError::Invalid(format!(
                    "refs.lock: {reference}: `{field}` is not a sha256 digest: {value:?}"
                )));
            }
        }
        Ok(())
    }
}

/// `mode` is a build error when `from` is a list (BUILD.md §2.1). The
/// single-path-resolving-to-a-directory case is a guest-side runtime
/// check (it needs the filesystem) and is not enforced here.
fn validate_directives(derive: &[Directive]) -> Result<(), RecipeError> {
    for d in derive {
        if let Directive::Copy(c) = d
            && c.mode.is_some()
            && c.from.is_list()
        {
            return Err(RecipeError::Invalid(format!(
                "copy into {:?}: `mode` is not allowed when `from` is a list of paths",
                c.into
            )));
        }
    }
    Ok(())
}

/// `requirements.yaml` — the host-facing launch contract (BUILD.md §7): what
/// the host must provide to launch a pichi VM. v1 models the fields pichi acts
/// on — `cpus`/`memory` (VM sizing) and `interfaces` (network) — but not yet
/// `carapaces`/`volumes` (enforced inside the guest by corium). Authored by
/// hand to bootstrap the platform — the `config.yaml` → `requirements.yaml`
/// filter is still deferred.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requirements {
    /// vCPU band. Omit to leave sizing to the operator / dillo default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<Band<u32>>,

    /// Guest-memory band; sizes are written like `2GiB` (binary units).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<Band<ByteSize>>,

    /// Network interfaces the host must wire (BUILD.md §7). pichi attaches one
    /// user-mode virtio-net device per entry; the guest brings each up/down per
    /// stage. The map key is the operator-facing label (identity is
    /// underspecified in v1 — most artifacts have a single interface).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub interfaces: BTreeMap<String, Interface>,
}

/// One declared network interface (BUILD.md §7). pichi wires one user-mode NIC
/// per entry; it does not yet act on `ingress`/`slot` (ingress exposure is
/// enforced in-guest by corium). The fields are carried so a full
/// `requirements.yaml` parses.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
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
    /// Parse and validate a `requirements.yaml` document. An empty document is
    /// a valid empty contract (no host obligations).
    pub fn parse(yaml: &str) -> Result<Self, RecipeError> {
        if yaml.trim().is_empty() {
            return Ok(Self::default());
        }
        let req: Self = serde_yaml::from_str(yaml)?;
        req.validate()?;
        Ok(req)
    }

    fn validate(&self) -> Result<(), RecipeError> {
        if let Some(b) = &self.cpus {
            if b.required == Some(0) {
                return Err(RecipeError::Invalid(
                    "requirements.yaml: cpus.required is 0".into(),
                ));
            }
            if let (Some(r), Some(rec)) = (b.required, b.recommended)
                && rec < r
            {
                return Err(RecipeError::Invalid(
                    "requirements.yaml: cpus.recommended is below cpus.required".into(),
                ));
            }
        }
        if let Some(b) = &self.memory
            && let (Some(r), Some(rec)) = (b.required, b.recommended)
            && rec < r
        {
            return Err(RecipeError::Invalid(
                "requirements.yaml: memory.recommended is below memory.required".into(),
            ));
        }
        Ok(())
    }

    /// Required vCPU floor, if declared.
    pub fn cpus_required(&self) -> Option<u32> {
        self.cpus.and_then(|b| b.required)
    }

    /// Recommended vCPU count, if declared.
    pub fn cpus_recommended(&self) -> Option<u32> {
        self.cpus.and_then(|b| b.recommended)
    }

    /// Required memory floor in MiB (rounded up), if declared.
    pub fn memory_required_mib(&self) -> Option<u32> {
        self.memory.and_then(|b| b.required).map(byte_size_to_mib)
    }

    /// Recommended memory in MiB (rounded up), if declared.
    pub fn memory_recommended_mib(&self) -> Option<u32> {
        self.memory
            .and_then(|b| b.recommended)
            .map(byte_size_to_mib)
    }
}

/// Round a byte size up to whole MiB, saturating at `u32::MAX` (dillo takes
/// `--memory` in MiB).
fn byte_size_to_mib(size: ByteSize) -> u32 {
    const MIB: u64 = 1 << 20;
    u32::try_from(size.as_u64().div_ceil(MIB)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_carapace_recipe() {
        let yaml = r#"
from: registry.example.com/base/fedora:43
derive:
  - run: dnf install -y python3 torch
  - copy:
      from: ./app
      into: /opt/app
      owner: appuser
      group: appuser
      mode: "0755"
"#;
        let r = CarapaceRecipe::parse(yaml).unwrap();
        assert_eq!(r.from, "registry.example.com/base/fedora:43");
        assert_eq!(r.derive.len(), 2);
        match &r.derive[0] {
            Directive::Run { command, network } => {
                assert_eq!(command, "dnf install -y python3 torch");
                assert!(!network, "network defaults to false");
            }
            other @ Directive::Copy(_) => panic!("expected run, got {other:?}"),
        }
        match &r.derive[1] {
            Directive::Copy(c) => {
                assert!(matches!(c.from, FromSpec::One(_)));
                assert_eq!(c.into, "/opt/app");
                assert_eq!(c.owner.as_deref(), Some("appuser"));
                assert_eq!(c.mode.as_deref(), Some("0755"));
            }
            other @ Directive::Run { .. } => panic!("expected copy, got {other:?}"),
        }
    }

    #[test]
    fn carapace_recipe_derive_is_optional() {
        let r = CarapaceRecipe::parse("from: reg/base:1").unwrap();
        assert!(r.derive.is_empty());
    }

    #[test]
    fn copy_accepts_a_list_of_sources() {
        let yaml = r"
from: reg/base:1
derive:
  - copy:
      from: [pichi.build/config.yaml, pichi.build/refs.lock]
      into: /usr/lib/corium/
";
        let r = CarapaceRecipe::parse(yaml).unwrap();
        match &r.derive[0] {
            Directive::Copy(c) => match &c.from {
                FromSpec::Many(v) => assert_eq!(v.len(), 2),
                FromSpec::One(_) => panic!("expected list"),
            },
            other @ Directive::Run { .. } => panic!("expected copy, got {other:?}"),
        }
    }

    #[test]
    fn run_directive_carries_network_flag() {
        let yaml = "from: reg/base:1\n\
                    derive:\n\
                    \x20 - run: dnf install -y kernel\n\
                    \x20   network: true\n\
                    \x20 - run: make\n";
        let r = CarapaceRecipe::parse(yaml).unwrap();
        match &r.derive[0] {
            Directive::Run { command, network } => {
                assert_eq!(command, "dnf install -y kernel");
                assert!(network);
            }
            other @ Directive::Copy(_) => panic!("expected run, got {other:?}"),
        }
        match &r.derive[1] {
            Directive::Run { network, .. } => assert!(!network, "defaults to false"),
            other @ Directive::Copy(_) => panic!("expected run, got {other:?}"),
        }
    }

    #[test]
    fn network_on_copy_is_rejected() {
        let yaml = "from: reg/base:1\n\
                    derive:\n\
                    \x20 - copy:\n\
                    \x20     from: a\n\
                    \x20     into: /b\n\
                    \x20   network: true\n";
        let err = CarapaceRecipe::parse(yaml).unwrap_err();
        assert!(matches!(err, RecipeError::Parse(_)), "{err:?}");
    }

    #[test]
    fn mode_with_list_source_is_rejected() {
        let yaml = r#"
from: reg/base:1
derive:
  - copy:
      from: [a, b]
      into: /dst/
      mode: "0644"
"#;
        let err = CarapaceRecipe::parse(yaml).unwrap_err();
        assert!(matches!(err, RecipeError::Invalid(_)), "{err:?}");
    }

    #[test]
    fn empty_from_is_rejected() {
        let err = CarapaceRecipe::parse("from: \"\"").unwrap_err();
        assert!(matches!(err, RecipeError::Invalid(_)), "{err:?}");
    }

    #[test]
    fn unknown_field_is_rejected() {
        let err = CarapaceRecipe::parse("from: reg/base:1\nbogus: 1").unwrap_err();
        assert!(matches!(err, RecipeError::Parse(_)), "{err:?}");
    }

    #[test]
    fn parses_full_pmi_recipe() {
        let yaml = r"
from: registry.example.com/base/kernel-builder:latest
derive:
  - run: dnf install -y kernel corium dracut
  - copy:
      from: [pichi.build/config.yaml, pichi.build/refs.lock]
      into: /usr/lib/corium/
  - run: dracut --add corium /tmp/initramfs.img
  - run: arma build --kernel /boot/vmlinuz-* --initramfs /tmp/initramfs.img -o /tmp/boot.pmi
into: /tmp/boot.pmi
";
        let r = PmiRecipe::parse(yaml).unwrap();
        assert_eq!(
            r.from.as_deref(),
            Some("registry.example.com/base/kernel-builder:latest")
        );
        assert_eq!(r.into, "/tmp/boot.pmi");
        assert_eq!(r.derive.len(), 4);
    }

    #[test]
    fn pmi_recipe_from_is_optional() {
        let r = PmiRecipe::parse("into: /tmp/x.pmi").unwrap();
        assert!(r.from.is_none());
        assert!(r.derive.is_empty());
    }

    #[test]
    fn pmi_recipe_requires_into() {
        let err = PmiRecipe::parse("from: reg/b:1").unwrap_err();
        assert!(matches!(err, RecipeError::Parse(_)), "{err:?}");
    }

    #[test]
    fn parses_refs_lock() {
        let yaml = r"
registry.example.com/models/llama:7b:
  manifest: sha256:abcdef0000000000000000000000000000000000000000000000000000000000
  carapace: sha256:def0000000000000000000000000000000000000000000000000000000000000
";
        let lock = RefsLock::parse(yaml).unwrap();
        let e = lock.get("registry.example.com/models/llama:7b").unwrap();
        assert!(e.manifest.starts_with("sha256:"));
        assert!(e.carapace.starts_with("sha256:"));
        assert!(lock.get("missing").is_none());
    }

    #[test]
    fn empty_refs_lock_is_valid() {
        assert!(RefsLock::parse("").unwrap().entries.is_empty());
        assert!(RefsLock::parse("   \n").unwrap().entries.is_empty());
    }

    #[test]
    fn refs_lock_rejects_non_sha256_digest() {
        let yaml = "reg/x:1:\n  manifest: deadbeef\n  carapace: sha256:aa\n";
        let err = RefsLock::parse(yaml).unwrap_err();
        assert!(matches!(err, RecipeError::Invalid(_)), "{err:?}");
    }

    #[test]
    fn parses_requirements() {
        let yaml = r"
cpus:
  required: 1
  recommended: 4
memory:
  required: 2GiB
  recommended: 4GiB
";
        let r = Requirements::parse(yaml).unwrap();
        assert_eq!(r.cpus_required(), Some(1));
        assert_eq!(r.cpus_recommended(), Some(4));
        assert_eq!(r.memory_required_mib(), Some(2048));
        assert_eq!(r.memory_recommended_mib(), Some(4096));
    }

    #[test]
    fn empty_requirements_is_valid() {
        let r = Requirements::parse("").unwrap();
        assert_eq!(r.cpus_required(), None);
        assert_eq!(r.memory_required_mib(), None);
    }

    #[test]
    fn partial_requirements_bands_are_allowed() {
        let r = Requirements::parse("memory:\n  required: 512MiB\n").unwrap();
        assert_eq!(r.memory_required_mib(), Some(512));
        assert_eq!(r.memory_recommended_mib(), None);
        assert_eq!(r.cpus_required(), None);
    }

    #[test]
    fn requirements_rejects_recommended_below_required() {
        let err = Requirements::parse("cpus:\n  required: 4\n  recommended: 1\n").unwrap_err();
        assert!(matches!(err, RecipeError::Invalid(_)), "{err:?}");
    }

    #[test]
    fn requirements_rejects_unknown_field() {
        let err = Requirements::parse("gpus:\n  required: 1\n").unwrap_err();
        assert!(matches!(err, RecipeError::Parse(_)), "{err:?}");
    }

    #[test]
    fn requirements_memory_rounds_up_to_mib() {
        // 1 byte over 1 MiB rounds to 2 MiB.
        let r = Requirements::parse("memory:\n  required: 1048577\n").unwrap();
        assert_eq!(r.memory_required_mib(), Some(2));
    }

    #[test]
    fn parses_requirements_interfaces() {
        let yaml = r#"
cpus:
  required: 1
interfaces:
  public:
    description: outbound
    ingress:
      required: [443, [8000, 8999], "*"]
      recommended: [80]
    slot: pci
  internal: {}
"#;
        let r = Requirements::parse(yaml).unwrap();
        assert_eq!(r.interfaces.len(), 2);
        let public = &r.interfaces["public"];
        assert_eq!(public.description.as_deref(), Some("outbound"));
        let ingress = public.ingress.as_ref().unwrap();
        assert_eq!(
            ingress.required,
            vec![
                PortSpec::Port(443),
                PortSpec::Range(8000, 8999),
                PortSpec::All
            ]
        );
        assert_eq!(ingress.recommended, vec![PortSpec::Port(80)]);
        assert_eq!(public.slot.as_deref(), Some("pci"));
        assert!(r.interfaces["internal"].ingress.is_none());
    }

    #[test]
    fn official_build_image_requirements_parse() {
        // The hand-authored launch contract shipped in the build image must
        // stay valid (publish-image.sh embeds it verbatim as a layer).
        let yaml = include_str!("../image/requirements.yaml");
        let r = Requirements::parse(yaml).unwrap();
        assert!(r.cpus_required().is_some());
        assert!(r.memory_required_mib().is_some());
        // The build image declares a network interface (outbound for dnf).
        assert!(
            !r.interfaces.is_empty(),
            "build image must declare a network interface"
        );
    }
}
