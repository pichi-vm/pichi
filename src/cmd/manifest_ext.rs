// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `ManifestExt`: pichi-side helpers on `pichi_artifact::Manifest`.
//!
//! `Manifest` is defined in `pichi-artifact` (it mirrors the normative wire
//! schema), so per the repo's "extension trait when you don't own the type"
//! rule these run/build-specific queries live here as an extension trait rather
//! than as free functions scattered across the command modules.

use std::collections::{HashMap, HashSet};

use anyhow::{Context as _, Result, anyhow, bail};

use crate::cmd::verity::{VerityFormat as _, VerityParams};
use devmap_verity::{HashType, Header};
use pichi_artifact::{Digest, DtbDescriptor, Layer, Manifest, PmiDescriptor, ScuteDescriptor};
use pichi_storage::{BlobSidecarExt, BlobStore};

/// Chain-wide verity annotation keys.
const ANN_DATA_BLOCK_SIZE: &str = "dev.pichi.carapace.verity.data-block-size";
const ANN_HASH_BLOCK_SIZE: &str = "dev.pichi.carapace.verity.hash-block-size";

/// Standard OCI image-architecture annotation key.
const ARCH_ANNOTATION: &str = "org.opencontainers.image.architecture";

/// pichi-side queries and validations on a cached [`Manifest`].
#[allow(async_fn_in_trait)] // used only via static dispatch on Manifest values
pub trait ManifestExt {
    /// The base-DTB layer, if the artifact carries one (detached-mode PMI).
    fn dtb_layer(&self) -> Option<&DtbDescriptor>;
    /// Partition layers into the single PMI descriptor + ordered scute list.
    /// `+zstd` scutes are not yet supported by `run`.
    fn partition_layers(&self) -> Result<(&PmiDescriptor, Vec<&ScuteDescriptor>)>;
    /// Collect scute layers with no PMI requirement (build `from:` sources).
    fn scute_layers(&self) -> Result<Vec<&ScuteDescriptor>>;
    /// Fail closed when the artifact's declared architecture ≠ the host.
    fn check_architecture(&self) -> Result<()>;
    /// Header for a scute using the manifest's reconstruction parameters.
    fn verity_header(&self, scute: &ScuteDescriptor, size: Option<u64>) -> Result<Header>;
    /// Recompute a cached layer's root with bounded memory; metadata layers have no root.
    async fn cached_scute_root(
        &self,
        layer: &Layer,
        blob_store: &dyn BlobStore,
    ) -> Result<Option<[u8; 32]>>;
    /// Unique graph-derived tip: the top scute dm-verity root.
    /// Host trust is the OCI digest. This root is not read from an annotation.
    async fn carapace_top_root(&self, blob_store: &dyn BlobStore) -> Result<[u8; 32]>;
}

impl ManifestExt for Manifest {
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

    fn verity_header(&self, scute: &ScuteDescriptor, size: Option<u64>) -> Result<Header> {
        let annotation = |key: &str| {
            self.annotations
                .get(key)
                .ok_or_else(|| anyhow!("missing verity annotation {key}"))
        };
        if annotation("dev.pichi.carapace.verity.version")?.parse::<u32>()? != 1 {
            bail!("unsupported verity header version");
        }
        // Carapace trust anchors and salt-chain prefixes are SHA-256 digests.
        if annotation("dev.pichi.carapace.verity.algo")? != "sha256" {
            bail!("unsupported carapace verity algorithm");
        }
        let params = VerityParams {
            data_block_size: annotation(ANN_DATA_BLOCK_SIZE)?.parse()?,
            hash_block_size: annotation(ANN_HASH_BLOCK_SIZE)?.parse()?,
            salt: hex::decode(&scute.annotations.salt)?,
            uuid: [0; 16],
        };
        let mut header = match scute.annotations.data_blocks {
            Some(blocks) => params.header_for_blocks(blocks)?,
            None => params
                .header(size.context("scute has neither data_blocks nor a known byte length")?)?,
        };
        if size.is_some_and(|size| {
            size.div_ceil(u64::from(header.parameters.data.size.bytes().get()))
                != header.parameters.data.count.get()
        }) {
            bail!("scute data_blocks disagrees with COW length");
        }
        header.parameters.hash_type =
            match annotation("dev.pichi.carapace.verity.hash-type")?.parse::<u32>()? {
                0 => HashType::ChromeOs,
                1 => HashType::Normal,
                value => bail!("unsupported verity hash type {value}"),
            };
        Ok(header)
    }

    async fn cached_scute_root(
        &self,
        layer: &Layer,
        blob_store: &dyn BlobStore,
    ) -> Result<Option<[u8; 32]>> {
        let (scute, compressed) = match layer {
            Layer::Scute(d) => (d, false),
            Layer::ScuteZstd(d) => (d, true),
            _ => return Ok(None),
        };
        let digest: Digest = scute.digest.parse()?;
        let path = blob_store.blob_path(&digest);
        let path = if compressed {
            path.deflated_path()
        } else {
            path
        };
        let size = tokio::fs::metadata(&path)
            .await
            .with_context(|| format!("reading cached scute {}", path.display()))?
            .len();
        if !compressed && size != scute.size {
            bail!("cached scute size mismatch: {digest}");
        }
        let header = self.verity_header(scute, Some(size))?;
        let root = tokio::task::spawn_blocking(move || {
            let input = std::io::BufReader::new(std::fs::File::open(path)?);
            header.root(input)
        })
        .await
        .context("verity task panicked")??;
        Ok(Some(root))
    }

    async fn carapace_top_root(&self, blob_store: &dyn BlobStore) -> Result<[u8; 32]> {
        let mut roots = ScuteRoots::default();
        for layer in &self.layers {
            if let Some(root) = self.cached_scute_root(layer, blob_store).await? {
                roots.add(layer, root)?;
            }
        }
        roots.top()
    }
}

/// Roots computed from the data in this operation, never read from sidecar metadata.
#[derive(Default)]
pub(super) struct ScuteRoots {
    roots: Vec<[u8; 32]>,
    parents: HashSet<[u8; 32]>,
}

impl ScuteRoots {
    pub(super) fn add(&mut self, layer: &Layer, root: [u8; 32]) -> Result<()> {
        let (Layer::Scute(scute) | Layer::ScuteZstd(scute)) = layer else {
            bail!("non-scute layer supplied a verity root");
        };
        let salt = hex::decode(&scute.annotations.salt)?;
        if let Some(prefix) = salt.get(..32) {
            let prefix: [u8; 32] = prefix.try_into()?;
            if prefix != [0; 32] {
                self.parents.insert(prefix);
            }
        }
        self.roots.push(root);
        Ok(())
    }

    fn top(&self) -> Result<[u8; 32]> {
        let tops: Vec<_> = self
            .roots
            .iter()
            .filter(|r| !self.parents.contains(*r))
            .collect();
        match tops.as_slice() {
            [top] => Ok(**top),
            [] => bail!("carapace chain has no top scute"),
            _ => bail!("carapace chain is not linear ({} top scutes)", tops.len()),
        }
    }

    /// OCI imports must carry a complete chain, including the zero-prefix
    /// base. Walking consumes every scute, rejecting missing parents and cycles.
    pub(super) fn verify_complete(&self, manifest: &Manifest) -> Result<()> {
        self.verify(manifest)?;
        let mut parents = HashMap::new();
        for (layer, root) in manifest
            .layers
            .iter()
            .filter(|l| matches!(l, Layer::Scute(_) | Layer::ScuteZstd(_)))
            .zip(&self.roots)
        {
            let (Layer::Scute(scute) | Layer::ScuteZstd(scute)) = layer else {
                unreachable!()
            };
            let salt = hex::decode(&scute.annotations.salt)?;
            let parent: [u8; 32] = salt
                .get(..32)
                .context("scute salt is shorter than its parent root")?
                .try_into()?;
            if parents.insert(*root, parent).is_some() {
                bail!("duplicate scute root");
            }
        }
        if parents.is_empty() {
            return Ok(());
        }
        let mut root = self.top()?;
        while root != [0; 32] {
            root = parents
                .remove(&root)
                .context("carapace chain has a missing parent or cycle")?;
        }
        if !parents.is_empty() {
            bail!("carapace chain contains unreachable scutes");
        }
        Ok(())
    }

    /// When scutes are present, require exactly one graph-derived tip.
    /// `dev.pichi.carapace.verity.hash` is ignored if a manifest still carries it.
    pub(super) fn verify(&self, manifest: &Manifest) -> Result<()> {
        if !manifest
            .layers
            .iter()
            .any(|layer| matches!(layer, Layer::Scute(_) | Layer::ScuteZstd(_)))
        {
            return Ok(());
        }
        self.top()?;
        Ok(())
    }
}
