// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Native configuration is image data. YAML plans remain inside Conglobate.
use anyhow::{Context as _, Result, ensure};
use conglobate::artifact::{BootConfigDescriptor, Layer, Manifest};
use oci_spec::image::Digest;
use pichi_storage::{BlobSidecarExt as _, BlobStore};
use sha2::{Digest as _, Sha256};
use std::{
    io::Read as _,
    path::{Path, PathBuf},
};
use tokio::io::AsyncReadExt as _;

#[derive(Debug)]
pub(super) struct BootConfigFile {
    bytes: Vec<u8>,
    text: String,
}

impl TryFrom<Vec<u8>> for BootConfigFile {
    type Error = anyhow::Error;

    fn try_from(bytes: Vec<u8>) -> Result<Self> {
        let mut decoder = ruzstd::decoding::StreamingDecoder::new(bytes.as_slice())
            .context("invalid compressed bootconfig")?;
        let mut text = String::new();
        decoder.read_to_string(&mut text)?;
        ensure!(
            decoder.into_inner().is_empty(),
            "trailing compressed bootconfig bytes"
        );
        Ok(Self { bytes, text })
    }
}

impl BootConfigFile {
    pub(super) async fn read(path: &Path) -> Result<Self> {
        let text = tokio::fs::read(path)
            .await
            .with_context(|| format!("read bootconfig {}", path.display()))?;
        ruzstd::encoding::compress_to_vec(
            text.as_slice(),
            ruzstd::encoding::CompressionLevel::Fastest,
        )
        .try_into()
    }

    pub(super) fn descriptor(&self) -> Option<BootConfigDescriptor> {
        (!self.text.is_empty()).then(|| BootConfigDescriptor {
            digest: format!("sha256:{}", hex::encode(Sha256::digest(&self.bytes)))
                .parse::<Digest>()
                .expect("SHA-256 digest")
                .to_string(),
            size: self.bytes.len() as u64,
        })
    }

    pub(super) async fn store(&self, store: &(impl BlobStore + ?Sized)) -> Result<()> {
        if let Some(descriptor) = self.descriptor() {
            store
                .put_blob(&descriptor.digest.parse()?, &self.bytes)
                .await?;
        }
        Ok(())
    }

    pub(super) async fn validate_image(
        manifest: &Manifest,
        store: &(impl BlobStore + ?Sized),
    ) -> Result<()> {
        Self::from_image(manifest, store).await?;
        Ok(())
    }

    async fn from_image(
        manifest: &Manifest,
        store: &(impl BlobStore + ?Sized),
    ) -> Result<Option<Self>> {
        let Some(descriptor) = manifest.layers.iter().find_map(|layer| match layer {
            Layer::BootConfig(descriptor) => Some(descriptor),
            _ => None,
        }) else {
            return Ok(None);
        };
        let bytes = Self::verified(store, &descriptor.digest.parse()?, descriptor.size).await?;
        let config = Self::try_from(bytes)?;
        ensure!(!config.text.is_empty(), "empty bootconfig must be omitted");
        Ok(Some(config))
    }

    /// Regenerate native text from verified wire bytes at every launch.
    pub(super) async fn prepare_image(
        manifest: &Manifest,
        store: &(impl BlobStore + ?Sized),
    ) -> Result<Option<PathBuf>> {
        let Some(config) = Self::from_image(manifest, store).await? else {
            return Ok(None);
        };
        let descriptor = config.descriptor().context("empty bootconfig")?;
        let directory = store.blob_path(&descriptor.digest.parse()?)?.prepared_dir();
        tokio::fs::create_dir_all(&directory).await?;
        let path = directory.join("bootconfig");
        pichi_storage::write_sidecar_atomic(
            &store.scratch_dir().await?,
            &path,
            config.text.as_bytes(),
        )
        .await?;
        Ok(Some(path))
    }

    async fn verified(
        store: &(impl BlobStore + ?Sized),
        digest: &Digest,
        size: u64,
    ) -> Result<Vec<u8>> {
        let limit = size.checked_add(1).context("boot payload size overflow")?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(usize::try_from(limit)?)?;
        store
            .open_blob(digest)
            .await?
            .take(limit)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(
            bytes.len() as u64 == size,
            "boot payload size mismatch: {digest}"
        );
        ensure!(
            format!("sha256:{}", hex::encode(Sha256::digest(&bytes)))
                .parse::<Digest>()
                .expect("SHA-256 digest")
                == *digest,
            "boot payload digest mismatch: {digest}"
        );
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compressed_text_preserves_spelling_without_reservation_padding() {
        let text = "# retained comment\nkernel { console = hvc0; quiet; }\n";
        let bytes = ruzstd::encoding::compress_to_vec(
            text.as_bytes(),
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        let config = BootConfigFile::try_from(bytes.clone()).unwrap();
        assert_eq!(config.text, text);
        assert_eq!(config.bytes, bytes);
        assert_eq!(config.descriptor().unwrap().size, bytes.len() as u64);
        let empty = ruzstd::encoding::compress_to_vec(
            b"".as_slice(),
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        assert!(
            BootConfigFile::try_from(empty)
                .unwrap()
                .descriptor()
                .is_none()
        );
    }

    #[test]
    fn malformed_compressed_frames_are_rejected() {
        let mut bytes = ruzstd::encoding::compress_to_vec(
            b"kernel.quiet".as_slice(),
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        bytes.pop();
        assert!(BootConfigFile::try_from(bytes).is_err());
        assert!(BootConfigFile::try_from(b"kernel.quiet".to_vec()).is_err());
    }

    #[tokio::test]
    async fn launch_prepares_exact_native_bytes_and_repairs_stale_sidecars() {
        let root = tempfile::tempdir().unwrap();
        let store = pichi_storage::FilesystemBlobStore::new(root.path());
        let text = format!("key = \"{}\";\n", "x".repeat(64 * 1024));
        let bytes = ruzstd::encoding::compress_to_vec(
            text.as_bytes(),
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        assert!(bytes.len() < text.len());
        let config = BootConfigFile::try_from(bytes).unwrap();
        config.store(&store).await.unwrap();
        let manifest = Manifest {
            schema_version: 2,
            media_type: conglobate::artifact::MEDIA_TYPE_OCI_MANIFEST_V1.into(),
            artifact_type: conglobate::artifact::MEDIA_TYPE_PICHI_ARTIFACT_V1.into(),
            config: conglobate::artifact::ConfigDescriptor::canonical(),
            layers: vec![
                Layer::Pmi(conglobate::artifact::PmiDescriptor {
                    digest: format!("sha256:{}", hex::encode(Sha256::digest(b"opaque PMI")))
                        .parse::<Digest>()
                        .expect("SHA-256 digest")
                        .to_string(),
                    size: 10,
                }),
                Layer::BootConfig(config.descriptor().unwrap()),
            ],
            annotations: std::collections::BTreeMap::default(),
        };
        let path = BootConfigFile::prepare_image(&manifest, &store)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tokio::fs::read(&path).await.unwrap(), text.as_bytes());
        tokio::fs::write(&path, b"stale").await.unwrap();
        BootConfigFile::prepare_image(&manifest, &store)
            .await
            .unwrap();
        assert_eq!(tokio::fs::read(&path).await.unwrap(), text.as_bytes());
    }
}
