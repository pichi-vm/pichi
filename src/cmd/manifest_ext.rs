// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `ManifestExt`: pichi-side helpers on `conglobate::artifact::Manifest`.
//!
//! `Manifest` is defined in `conglobate::artifact` (it mirrors the normative wire
//! schema), so per the repo's "extension trait when you don't own the type"
//! rule these run/build-specific queries live here as an extension trait rather
//! than as free functions scattered across the command modules.

use crate::cmd::platform::PlatformExt as _;

use anyhow::{Context as _, Result, anyhow, bail};

use conglobate::artifact::{BootConfigDescriptor, Layer, Manifest, ScuteDescriptor};
use devmap_core::Geometry;
use devmap_verity::{Algorithm, HashType, Header, Parameters};

/// pichi-side queries and validations on a cached [`Manifest`].
pub trait ManifestExt {
    /// Decode and validate a manifest at a Pichi input boundary.
    fn decode_json(reader: impl std::io::Read) -> Result<Self>
    where
        Self: Sized;
    /// The native Linux boot configuration accompanying the PMI, if present.
    fn bootconfig_layer(&self) -> Option<&BootConfigDescriptor>;
    /// Partition layers into the single PMI layer (retaining its encoding) + scute layers.
    fn partition_layers(&self) -> Result<(&Layer, Vec<&Layer>)>;
    /// Collect scute layers with no PMI requirement (build `from:` sources).
    fn scute_layers(&self) -> Vec<&Layer>;
    /// Fail closed when the artifact's declared architecture ≠ the host.
    fn check_architecture(&self) -> Result<()>;
    /// Header for a scute using the manifest's reconstruction parameters.
    fn verity_header(&self, scute: &ScuteDescriptor, size: Option<u64>) -> Result<Header>;
}

impl ManifestExt for Manifest {
    fn decode_json(reader: impl std::io::Read) -> Result<Self> {
        let manifest: Self = serde_json::from_reader(reader)?;
        manifest.validate()?;
        Ok(manifest)
    }

    fn bootconfig_layer(&self) -> Option<&BootConfigDescriptor> {
        self.layers.iter().find_map(|l| match l {
            Layer::BootConfig(d) => Some(d),
            _ => None,
        })
    }

    fn partition_layers(&self) -> Result<(&Layer, Vec<&Layer>)> {
        let pmi = self.layers.iter().find_map(|layer| match layer {
            Layer::Pmi(_) => Some(layer),
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
            .filter(|layer| matches!(layer, Layer::Scute(_)))
            .collect()
    }

    fn check_architecture(&self) -> Result<()> {
        let Some(arch) = self.annotations.oci.architecture.as_deref() else {
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
        self.carapace_root()?
            .ok_or_else(|| anyhow!("missing carapace root"))?;
        let header = Header {
            uuid: [0; 16],
            parameters: Parameters {
                hash_type: HashType::Normal,
                algorithm: Algorithm::Sha256,
                salt: *scute.annotations.salt,
                data: Geometry {
                    size: *self.annotations.verity.data_block_size,
                    count: *scute.annotations.data_blocks,
                },
                hash: *self.annotations.verity.hash_block_size,
            },
        };
        if size.is_some_and(|size| {
            size.div_ceil(u64::from(header.parameters.data.size.bytes().get()))
                != header.parameters.data.count.get()
        }) {
            bail!("scute data_blocks disagrees with COW length");
        }
        Ok(header)
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
        let Layer::Scute(scute) = layer else {
            bail!("non-scute layer supplied a verity root");
        };
        let salt = scute.annotations.salt.as_ref();
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

    /// Bind the complete ordered reconstruction to the manifest commitment.
    pub(super) fn verify(&self, manifest: &Manifest) -> Result<Option<[u8; 32]>> {
        let expected = manifest
            .layers
            .iter()
            .filter(|l| matches!(l, Layer::Scute(_)))
            .count();
        if self.count != expected {
            bail!("not every manifest scute was reconstructed");
        }
        let root = (self.count > 0).then_some(self.root);
        if root != manifest.carapace_root()? {
            bail!("reconstructed carapace root does not match the manifest commitment");
        }
        Ok(root)
    }
}
