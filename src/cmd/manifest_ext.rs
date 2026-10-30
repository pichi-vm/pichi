// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `ManifestExt`: pichi-side helpers on `conglobate::artifact::Manifest`.
//!
//! `Manifest` is defined in `conglobate::artifact` (it mirrors the normative wire
//! schema), so per the repo's "extension trait when you don't own the type"
//! rule these run/build-specific queries live here as an extension trait rather
//! than as free functions scattered across the command modules.

use crate::cmd::layer::{PreparedLayer, ScuteFiles};
use crate::cmd::platform::PlatformExt as _;

use anyhow::{Context as _, Result, anyhow, bail};

use crate::cmd::verity::{VerityFormat as _, VerityParams};
use conglobate::artifact::{
    Digest, DtbDescriptor, Layer, Manifest, PmiDescriptor, ScuteDescriptor,
};
use devmap_verity::{HashType, Header};
use pichi_storage::BlobStore;

/// Chain-wide verity annotation keys.
const ANN_DATA_BLOCK_SIZE: &str = "dev.pichi.carapace.verity.data-block-size";
const ANN_HASH_BLOCK_SIZE: &str = "dev.pichi.carapace.verity.hash-block-size";

/// Standard OCI image-architecture annotation key.
const ARCH_ANNOTATION: &str = "org.opencontainers.image.architecture";

/// pichi-side queries and validations on a cached [`Manifest`].
#[allow(async_fn_in_trait)] // used only via static dispatch on Manifest values
pub trait ManifestExt {
    /// Decode and validate a manifest at a Pichi input boundary.
    fn decode_json(reader: impl std::io::Read) -> Result<Self>
    where
        Self: Sized;
    /// The base-DTB layer, if the artifact carries one (detached-mode PMI).
    fn dtb_layer(&self) -> Option<&DtbDescriptor>;
    /// Partition layers into the single PMI descriptor + scute layers.
    fn partition_layers(&self) -> Result<(&PmiDescriptor, Vec<&Layer>)>;
    /// Collect scute layers with no PMI requirement (build `from:` sources).
    fn scute_layers(&self) -> Vec<&Layer>;
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
    /// Reconstructed tip after verifying scutes in manifest order.
    /// Host trust is the OCI digest. This root is not read from an annotation.
    async fn carapace_top_root(&self, blob_store: &dyn BlobStore) -> Result<[u8; 32]>;
}

impl ManifestExt for Manifest {
    fn decode_json(reader: impl std::io::Read) -> Result<Self> {
        let manifest: Self = serde_json::from_reader(reader)?;
        manifest.validate()?;
        Ok(manifest)
    }

    fn dtb_layer(&self) -> Option<&DtbDescriptor> {
        self.layers.iter().find_map(|l| match l {
            Layer::Dtb(d) => Some(d),
            _ => None,
        })
    }

    fn partition_layers(&self) -> Result<(&PmiDescriptor, Vec<&Layer>)> {
        let pmi = self.layers.iter().find_map(|layer| match layer {
            Layer::Pmi(descriptor) => Some(descriptor),
            _ => None,
        });
        let pmi = pmi.ok_or_else(|| {
            anyhow!("artifact is not bootable (no PMI layer); usable as a `from:` source only")
        })?;
        Ok((pmi, self.scute_layers()))
    }

    fn scute_layers(&self) -> Vec<&Layer> {
        self.layers
            .iter()
            .filter(|layer| matches!(layer, Layer::Scute(_) | Layer::ScuteZstd(_)))
            .collect()
    }

    fn check_architecture(&self) -> Result<()> {
        let Some(arch) = self.annotations.get(ARCH_ANNOTATION) else {
            return Ok(());
        };
        let host = std::env::consts::ARCH;
        let host_norm = conglobate::artifact::Platform::host_architecture();
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
        let mut header = params.header_for_blocks(scute.annotations.data_blocks.get())?;
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
        let files = ScuteFiles::new(self, layer, blob_store)?;
        if !tokio::fs::try_exists(&files.data).await?
            || !tokio::fs::try_exists(&files.verity).await?
        {
            return PreparedLayer::prepare_cached(self, layer, blob_store).await;
        }
        let path = files.data;
        let size = tokio::fs::metadata(&path)
            .await
            .with_context(|| format!("reading cached scute {}", path.display()))?
            .len();
        let header = self.verity_header(scute, (!compressed).then_some(scute.size))?;
        if size != header.parameters.data_size()? {
            bail!("prepared scute extent mismatch: {digest}");
        }
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
    root: [u8; 32],
    count: usize,
}

impl ScuteRoots {
    pub(super) fn add(&mut self, layer: &Layer, root: [u8; 32]) -> Result<()> {
        let (Layer::Scute(scute) | Layer::ScuteZstd(scute)) = layer else {
            bail!("non-scute layer supplied a verity root");
        };
        let salt = hex::decode(&scute.annotations.salt)?;
        let parent = salt
            .get(..32)
            .context("scute salt is shorter than its parent root")?;
        if parent != self.root {
            bail!("scutes must appear in base-to-tip manifest order");
        }
        self.root = root;
        self.count += 1;
        Ok(())
    }

    fn top(&self) -> Result<[u8; 32]> {
        if self.count == 0 {
            bail!("carapace chain has no top scute");
        }
        Ok(self.root)
    }

    /// Confirm that every scute was reconstructed in manifest order.
    pub(super) fn verify(&self, manifest: &Manifest) -> Result<()> {
        let expected = manifest
            .layers
            .iter()
            .filter(|l| matches!(l, Layer::Scute(_) | Layer::ScuteZstd(_)))
            .count();
        if self.count != expected {
            bail!("not every manifest scute was reconstructed");
        }
        Ok(())
    }
}
