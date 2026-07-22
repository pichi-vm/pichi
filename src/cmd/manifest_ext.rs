// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `ManifestExt`: pichi-side helpers on `pichi_artifact::Manifest`.
//!
//! `Manifest` is defined in `pichi-artifact` (it mirrors the normative wire
//! schema), so per the repo's "extension trait when you don't own the type"
//! rule these run/build-specific queries live here as an extension trait rather
//! than as free functions scattered across the command modules.

use std::collections::HashSet;

use anyhow::{Context as _, Result, anyhow, bail};

use pichi_artifact::{Digest, DtbDescriptor, Layer, Manifest, PmiDescriptor, ScuteDescriptor};
use pichi_import::verity::VerityParams;
use pichi_storage::BlobStore;

/// Chain-wide verity annotation keys + the carapace-locked default block size.
const ANN_DATA_BLOCK_SIZE: &str = "dev.pichi.carapace.verity.data-block-size";
const ANN_HASH_BLOCK_SIZE: &str = "dev.pichi.carapace.verity.hash-block-size";
const DEFAULT_BLOCK_SIZE: u32 = 4096;

/// Standard OCI image-architecture annotation key.
const ARCH_ANNOTATION: &str = "org.opencontainers.image.architecture";

/// pichi-side queries and validations on a cached [`Manifest`].
#[allow(async_fn_in_trait)] // used only via static dispatch on Manifest values
pub trait ManifestExt {
    /// dm-verity data block size (annotation, falling back to the locked 4096).
    fn data_block_size(&self) -> u32;
    /// dm-verity hash block size (annotation, falling back to the locked 4096).
    fn hash_block_size(&self) -> u32;
    /// The base-DTB layer, if the artifact carries one (detached-mode PMI).
    fn dtb_layer(&self) -> Option<&DtbDescriptor>;
    /// Partition layers into the single PMI descriptor + ordered scute list.
    /// `+zstd` scutes are not yet supported by `run`.
    fn partition_layers(&self) -> Result<(&PmiDescriptor, Vec<&ScuteDescriptor>)>;
    /// Collect scute layers with no PMI requirement (build `from:` sources).
    fn scute_layers(&self) -> Result<Vec<&ScuteDescriptor>>;
    /// Fail closed when the artifact's declared architecture ≠ the host.
    fn check_architecture(&self) -> Result<()>;
    /// The carapace trust anchor `rootₙ₋₁`: the top scute's dm-verity root.
    async fn carapace_top_root(&self, blob_store: &dyn BlobStore) -> Result<[u8; 32]>;
    /// Recompute the carapace top root from the (undistributed) scute cows and
    /// verify it matches the declared `dev.pichi.carapace.verity.hash`
    /// annotation. D-04: never trust distributed verity — recompute and
    /// compare. Errors on mismatch (corrupt download / tampered manifest).
    /// No-op for scute-less manifests, and for zstd-scute carapaces (whose
    /// recompute path is not yet supported — consistent with `run`/`update`).
    async fn verify_carapace_root(&self, blob_store: &dyn BlobStore) -> Result<()>;
}

impl ManifestExt for Manifest {
    fn data_block_size(&self) -> u32 {
        annotation_u32(self, ANN_DATA_BLOCK_SIZE)
    }

    fn hash_block_size(&self) -> u32 {
        annotation_u32(self, ANN_HASH_BLOCK_SIZE)
    }

    fn dtb_layer(&self) -> Option<&DtbDescriptor> {
        self.layers.iter().find_map(|l| match l {
            Layer::Dtb(d) => Some(d),
            _ => None,
        })
    }

    fn partition_layers(&self) -> Result<(&PmiDescriptor, Vec<&ScuteDescriptor>)> {
        let mut pmi: Option<&PmiDescriptor> = None;
        let mut scutes: Vec<&ScuteDescriptor> = Vec::new();
        for layer in &self.layers {
            match layer {
                Layer::Pmi(d) => pmi = Some(d),
                Layer::Scute(d) => scutes.push(d),
                Layer::Dtb(_) => {}
                Layer::ScuteZstd(_) => bail!(
                    "this artifact has zstd-compressed scute layers; `pichi run` does not yet \
                     support them (decompressed-COW handling is deferred)"
                ),
            }
        }
        let pmi = pmi.ok_or_else(|| {
            anyhow!("artifact is not bootable (no PMI layer); usable as a `from:` source only")
        })?;
        Ok((pmi, scutes))
    }

    fn scute_layers(&self) -> Result<Vec<&ScuteDescriptor>> {
        let mut scutes: Vec<&ScuteDescriptor> = Vec::new();
        for layer in &self.layers {
            match layer {
                Layer::Scute(d) => scutes.push(d),
                Layer::Pmi(_) | Layer::Dtb(_) => {}
                Layer::ScuteZstd(_) => bail!(
                    "this carapace has zstd-compressed scute layers; the build path does not yet \
                     support them (decompressed-COW handling is deferred)"
                ),
            }
        }
        Ok(scutes)
    }

    fn check_architecture(&self) -> Result<()> {
        let Some(arch) = self.annotations.get(ARCH_ANNOTATION) else {
            return Ok(());
        };
        let host = std::env::consts::ARCH;
        let host_norm = match host {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => other,
        };
        if arch != host && arch != host_norm {
            bail!(
                "artifact architecture {arch:?} does not match host {host:?} \
                 (host normalised as {host_norm:?}; supported synonyms: \
                 x86_64=amd64, aarch64=arm64)"
            );
        }
        Ok(())
    }

    async fn carapace_top_root(&self, blob_store: &dyn BlobStore) -> Result<[u8; 32]> {
        let data_block_size = self.data_block_size();
        let hash_block_size = self.hash_block_size();

        let mut roots: Vec<[u8; 32]> = Vec::new();
        let mut parents: HashSet<[u8; 32]> = HashSet::new();

        for layer in &self.layers {
            let scute = match layer {
                Layer::Scute(d) => d,
                Layer::Pmi(_) | Layer::Dtb(_) => continue,
                Layer::ScuteZstd(_) => bail!(
                    "artifact has zstd-compressed scute layers; pichi update does not yet support them"
                ),
            };
            let cow_digest: Digest = scute
                .digest
                .parse()
                .with_context(|| format!("invalid scute digest: {}", scute.digest))?;
            let cow_bytes = blob_store
                .get_blob(&cow_digest)
                .await
                .with_context(|| format!("reading scute cow blob {cow_digest}"))?;
            let salt = hex::decode(&scute.annotations.salt).with_context(|| {
                format!("scute salt is not valid hex: {}", scute.annotations.salt)
            })?;

            // Record the parent-root prefix (first 32 bytes; all-zero = base).
            if salt.len() >= 32 {
                let mut prefix = [0u8; 32];
                prefix.copy_from_slice(&salt[..32]);
                if prefix != [0u8; 32] {
                    parents.insert(prefix);
                }
            }

            // dm-verity is CPU-bound — compute the root off the runtime.
            let params = VerityParams {
                data_block_size,
                hash_block_size,
                salt,
                uuid: [0u8; 16],
            };
            let root = tokio::task::spawn_blocking(move || {
                params.compute(&cow_bytes).map(|o| o.root_hash)
            })
            .await
            .context("verity task panicked")?
            .context("dm-verity root computation failed")?;
            roots.push(root);
        }

        if roots.is_empty() {
            bail!("artifact has no scute layers (not a carapace)");
        }

        let tops: Vec<[u8; 32]> = roots
            .iter()
            .copied()
            .filter(|r| !parents.contains(r))
            .collect();
        match tops.as_slice() {
            [top] => Ok(*top),
            [] => bail!("carapace chain has no top scute (cycle?)"),
            _ => bail!("carapace chain is not linear ({} top scutes)", tops.len()),
        }
    }

    async fn verify_carapace_root(&self, blob_store: &dyn BlobStore) -> Result<()> {
        let has_plain_scute = self.layers.iter().any(|l| matches!(l, Layer::Scute(_)));
        let has_zstd_scute = self.layers.iter().any(|l| matches!(l, Layer::ScuteZstd(_)));
        // Nothing to verify without an uncompressed carapace; the zstd recompute
        // path is deferred (matches `carapace_top_root`'s own limitation).
        if !has_plain_scute || has_zstd_scute {
            return Ok(());
        }
        let declared = self
            .carapace_verity_hash()
            .ok_or_else(|| anyhow!("manifest is missing the carapace verity root annotation"))?;
        let computed = self
            .carapace_top_root(blob_store)
            .await
            .context("recomputing carapace verity root")?;
        if declared != computed {
            bail!(
                "carapace verity root mismatch: manifest declares {} but the scute cows \
                 recompute to {} (corrupt download or tampered manifest)",
                hex::encode(declared),
                hex::encode(computed)
            );
        }
        Ok(())
    }
}

/// Read a u32 chain annotation, falling back to the carapace-locked default.
fn annotation_u32(manifest: &Manifest, key: &str) -> u32 {
    manifest
        .annotations
        .get(key)
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(DEFAULT_BLOCK_SIZE)
}
