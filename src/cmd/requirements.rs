// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Consumption of the launch contract (BUILD.md §7) that a bootable artifact
//! carries in its manifest **config blob** (`vnd.pichi.config.v1+json`).
//! `pichi build` and `pichi run` read it to size the VM: an explicit operator
//! value (CLI > config) wins but MUST meet the `required` floor; absent, the
//! `recommended` (else `required`) tier is used.

use anyhow::{Context, Result, bail};
use conglobate::artifact::{Config, Digest, Manifest, Recommendation, Requirements, Slot};
use pichi_storage::BlobStore;

/// Keep launcher argument spelling outside the shared image schema.
pub(super) trait SlotArgument {
    fn as_argument(&self) -> &'static str;
}

impl SlotArgument for Slot {
    fn as_argument(&self) -> &'static str {
        match self {
            Self::Pci => "pci",
            Self::Mmio => "mmio",
        }
    }
}

/// Load the artifact's launch contract from its config blob. Returns `Ok(None)`
/// for artifacts with the OCI empty config (no launch contract, e.g. plain
/// carapaces).
pub async fn load_requirements(
    manifest: &Manifest,
    blob_store: &impl BlobStore,
) -> Result<Option<Requirements>> {
    Ok(load_config(manifest, blob_store)
        .await?
        .map(|c| c.requirements))
}

/// Decode and validate the image config for launch and inspection.
pub(super) async fn load_config(
    manifest: &Manifest,
    blob_store: &impl BlobStore,
) -> Result<Option<Config>> {
    if manifest.config.is_empty() {
        return Ok(None);
    }
    let digest: Digest = manifest
        .config
        .digest
        .parse()
        .with_context(|| format!("invalid config digest: {}", manifest.config.digest))?;
    let bytes = blob_store
        .get_blob(&digest)
        .await
        .with_context(|| format!("reading config blob {digest}"))?;
    let config: Config = serde_json::from_slice(&bytes).context("parsing config blob")?;
    config.validate().context("invalid config blob")?;
    Ok(Some(config))
}

/// VM resources whose maximum can be detected from the host.
#[derive(Debug, Clone, Copy)]
pub(super) enum Sizing {
    Cpus,
    Memory,
}

impl Sizing {
    fn label(self) -> &'static str {
        match self {
            Self::Cpus => "cpus",
            Self::Memory => "memory (MiB)",
        }
    }

    fn available(self) -> Result<u32> {
        match self {
            Self::Cpus => u32::try_from(std::thread::available_parallelism()?.get())
                .context("available CPU count exceeds the launcher limit"),
            Self::Memory => {
                #[cfg(target_os = "linux")]
                let bytes = {
                    // Linux guests use the host's preallocated 2-MiB hugepage pool.
                    let pool = std::path::Path::new("/sys/kernel/mm/hugepages/hugepages-2048kB");
                    let free: u64 = std::fs::read_to_string(pool.join("free_hugepages"))
                        .context("reading available guest hugepages")?
                        .trim()
                        .parse()?;
                    let reserved: u64 = std::fs::read_to_string(pool.join("resv_hugepages"))
                        .context("reading reserved guest hugepages")?
                        .trim()
                        .parse()?;
                    free.saturating_sub(reserved)
                        .saturating_mul(2 * 1024 * 1024)
                };
                #[cfg(not(target_os = "linux"))]
                let bytes = {
                    let mut system = sysinfo::System::new().context("detecting host memory")?;
                    system
                        .refresh_memory_specifics(sysinfo::MemoryRefreshKind::nothing().with_ram());
                    system.available_memory()
                };
                let mib = (bytes / (1024 * 1024)).min(u64::from(u32::MAX));
                Ok((mib / 2 * 2) as u32)
            }
        }
    }

    pub(super) fn resolve(
        self,
        explicit: Option<u32>,
        required: Option<u32>,
        recommended: Option<Recommendation<u32>>,
    ) -> Result<Option<u32>> {
        self.resolve_with_available(explicit, required, recommended, || self.available())
    }

    fn resolve_with_available(
        self,
        explicit: Option<u32>,
        required: Option<u32>,
        recommended: Option<Recommendation<u32>>,
        available: impl FnOnce() -> Result<u32>,
    ) -> Result<Option<u32>> {
        let what = self.label();
        let value = match (explicit, recommended) {
            (Some(value), _) | (None, Some(Recommendation::Value(value))) => Some(value),
            (None, Some(Recommendation::Max)) => Some(available()?),
            (None, None) => required,
        };
        if let Some(value) = value {
            if value == 0 || required.is_some_and(|floor| value < floor) {
                bail!(
                    "requested {what} ({value}) is below the image's required minimum ({})",
                    required.unwrap_or(1)
                );
            }
            if explicit.is_some()
                && let Some(Recommendation::Value(rec)) = recommended
                && value < rec
            {
                log::warn!("requested {what} ({value}) is below the recommended {rec}");
            }
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recommendations_supply_defaults_and_explicit_values_win() {
        for resource in [Sizing::Cpus, Sizing::Memory] {
            assert_eq!(
                resource
                    .resolve(None, Some(2), Some(Recommendation::Value(8)))
                    .unwrap(),
                Some(8)
            );
            assert_eq!(
                resource
                    .resolve(Some(4), Some(2), Some(Recommendation::Value(8)))
                    .unwrap(),
                Some(4)
            );
            assert!(
                resource
                    .resolve(Some(1), Some(2), Some(Recommendation::Value(8)))
                    .is_err()
            );
            assert_eq!(resource.resolve(None, Some(2), None).unwrap(), Some(2));
            assert_eq!(resource.resolve(None, None, None).unwrap(), None);
        }
    }

    #[test]
    fn max_uses_detected_availability_only_without_an_operator_value() {
        for resource in [Sizing::Cpus, Sizing::Memory] {
            assert_eq!(
                resource
                    .resolve_with_available(None, Some(2), Some(Recommendation::Max), || Ok(12))
                    .unwrap(),
                Some(12)
            );
            assert_eq!(
                resource
                    .resolve_with_available(Some(4), Some(2), Some(Recommendation::Max), || panic!(
                        "explicit allocation must win"
                    ))
                    .unwrap(),
                Some(4)
            );
            assert!(
                resource
                    .resolve_with_available(None, Some(16), Some(Recommendation::Max), || Ok(12))
                    .is_err()
            );
            assert!(
                resource
                    .resolve_with_available(None, None, Some(Recommendation::Max), || Ok(0))
                    .is_err()
            );
        }
    }
}
