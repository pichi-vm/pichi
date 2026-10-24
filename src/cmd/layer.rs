// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Streaming layer preparation shared by pull and load.

use std::io::{self, BufReader, BufWriter, Read, Seek, Write};
use std::path::Path;

use crate::cmd::verity::VerityFormat as _;
use anyhow::{Context as _, Result, bail};
use pichi_artifact::{Digest, Layer, Manifest, Reference};
use pichi_registry::Registry;
use pichi_storage::{BlobSidecarExt, BlobStore};
use sha2::{Digest as _, Sha256};
use tempfile::NamedTempFile;
use tokio_util::io::{StreamReader, SyncIoBridge};

use crate::cmd::manifest_ext::ManifestExt as _;

const DECOMPRESSED_CAP: u64 = 16 * 1024 * 1024 * 1024;

pub(super) struct PreparedLayer {
    source: Option<NamedTempFile>,
    deflated: Option<NamedTempFile>,
    verity: Option<NamedTempFile>,
    root: Option<[u8; 32]>,
}

impl PreparedLayer {
    pub(super) async fn fetch<R: Registry>(
        registry: &R,
        target: &Reference,
        manifest: &Manifest,
        layer: &Layer,
        store: &dyn BlobStore,
    ) -> Result<Option<[u8; 32]>> {
        let digest: Digest = layer.digest_str().parse()?;
        let scratch = store.scratch_dir().await?;
        let stream = registry
            .pull_blob_stream(&target.registry, &target.repo, &digest, layer.size())
            .await?;
        // Capture the runtime handle before entering the blocking pool. Each
        // read polls the network only when the formatter needs more bytes.
        let input = SyncIoBridge::new(StreamReader::new(stream));
        let manifest = manifest.clone();
        let layer = layer.clone();
        let prepared =
            tokio::task::spawn_blocking(move || Self::read(input, &manifest, &layer, &scratch))
                .await
                .context("layer formatting task panicked")??;
        prepared.commit(store, &digest).await
    }

    pub(super) async fn prepare_cached(
        manifest: &Manifest,
        layer: &Layer,
        store: &dyn BlobStore,
    ) -> Result<Option<[u8; 32]>> {
        let is_zstd = match layer {
            Layer::Scute(_) => false,
            Layer::ScuteZstd(_) => true,
            _ => return Ok(None),
        };
        let digest: Digest = layer.digest_str().parse()?;
        let path = store.blob_path(&digest);
        if tokio::fs::try_exists(path.verity_path()).await?
            && (!is_zstd || tokio::fs::try_exists(path.deflated_path()).await?)
        {
            return manifest.cached_scute_root(layer, store).await;
        }
        let scratch = store.scratch_dir().await?;
        let manifest = manifest.clone();
        let layer = layer.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            let input = BufReader::new(std::fs::File::open(path)?);
            Self::read_into(input, io::sink(), &manifest, &layer, &scratch)
        })
        .await
        .context("layer formatting task panicked")??;
        prepared.commit(store, &digest).await
    }

    fn read<R: Read>(input: R, manifest: &Manifest, layer: &Layer, scratch: &Path) -> Result<Self> {
        let source = NamedTempFile::new_in(scratch)?;
        let mut prepared = Self::read_into(
            input,
            BufWriter::new(source.as_file()),
            manifest,
            layer,
            scratch,
        )?;
        source.as_file().sync_all()?;
        prepared.source = Some(source);
        Ok(prepared)
    }

    fn read_into<R: Read, W: Write>(
        input: R,
        output: W,
        manifest: &Manifest,
        layer: &Layer,
        scratch: &Path,
    ) -> Result<Self> {
        let mut wire = WireReader {
            input,
            output,
            hash: Sha256::new(),
            count: 0,
            expected: layer.size(),
        };
        let mut deflated = None;
        let mut verity = None;
        let root = match layer {
            Layer::Scute(scute) => {
                let mut output = NamedTempFile::new_in(scratch)?;
                let root = manifest
                    .verity_header(scute, Some(scute.size))?
                    .format_padded(&mut wire, output.as_file_mut())?;
                verity = Some(output);
                Some(root)
            }
            Layer::ScuteZstd(scute) => {
                let mut data = NamedTempFile::new_in(scratch)?;
                let mut output = NamedTempFile::new_in(scratch)?;
                let decoder = match ruzstd::decoding::StreamingDecoder::new(&mut wire) {
                    Ok(decoder) => decoder,
                    Err(ruzstd::decoding::errors::FrameDecoderError::WindowSizeTooBig {
                        requested,
                        max,
                    }) => {
                        // ruzstd's default ceiling is 128 MiB (DEFAULT_MAX_WINDOW_SIZE).
                        // Surface a clean message on the pull/decode path rather than
                        // a Debug dump of the decoder error enum.
                        bail!(
                            "zstd window size too large: requested {requested} bytes (limit {max} bytes / {} MiB)",
                            max / (1024 * 1024)
                        );
                    }
                    Err(err) => return Err(err.into()),
                };
                let mut decoded = DecodedReader {
                    input: decoder,
                    output: BufWriter::new(data.as_file()),
                    count: 0,
                };
                let root = if scute.annotations.data_blocks.is_some() {
                    let header = manifest.verity_header(scute, None)?;
                    if header.parameters.data.count.get()
                        > DECOMPRESSED_CAP / u64::from(header.parameters.data.size.bytes().get())
                    {
                        bail!("decompressed size exceeds cap");
                    }
                    let root = header.format_padded(&mut decoded, output.as_file_mut())?;
                    decoded.output.flush()?;
                    drop(decoded);
                    root
                } else {
                    // Legacy compressed manifests lack the geometry needed to
                    // place tree levels. Decode once, then format the saved COW.
                    io::copy(&mut decoded, &mut io::sink())?;
                    decoded.output.flush()?;
                    let size = decoded.count;
                    drop(decoded);
                    data.as_file_mut().rewind()?;
                    manifest
                        .verity_header(scute, Some(size))?
                        .format_padded(BufReader::new(data.as_file()), output.as_file_mut())?
                };
                deflated = Some(data);
                verity = Some(output);
                Some(root)
            }
            Layer::Pmi(_) | Layer::Dtb(_) => {
                io::copy(&mut wire, &mut io::sink())?;
                None
            }
        };
        wire.finish(&layer.digest_str().parse()?)?;
        for file in deflated.iter().chain(verity.iter()) {
            file.as_file().sync_all()?;
        }
        Ok(Self {
            source: None,
            deflated,
            verity,
            root,
        })
    }

    async fn commit(self, store: &dyn BlobStore, digest: &Digest) -> Result<Option<[u8; 32]>> {
        let path = store.blob_path(digest);
        tokio::fs::create_dir_all(path.parent().context("blob path has no parent")?).await?;
        // Publish sidecars first: the source blob is the cache-presence marker.
        if let Some(file) = &self.deflated {
            tokio::fs::rename(file.path(), path.deflated_path()).await?;
        }
        if let Some(file) = &self.verity {
            tokio::fs::rename(file.path(), path.verity_path()).await?;
        }
        if let Some(source) = &self.source {
            store.put_blob_from_path(source.path(), digest).await?;
        }
        Ok(self.root)
    }
}

/// Wire-byte side effects, before decompression or verity padding.
struct WireReader<R, W> {
    input: R,
    output: W,
    hash: Sha256,
    count: u64,
    expected: u64,
}

impl<R: Read, W: Write> Read for WireReader<R, W> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let count = self.input.read(out)?;
        self.count = self
            .count
            .checked_add(count as u64)
            .ok_or(io::ErrorKind::InvalidData)?;
        if self.count > self.expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "layer exceeds descriptor size",
            ));
        }
        self.output.write_all(&out[..count])?;
        self.hash.update(&out[..count]);
        Ok(count)
    }
}

impl<R: Read, W: Write> WireReader<R, W> {
    fn finish(mut self, digest: &Digest) -> Result<()> {
        // A decoder can stop at a frame boundary. Reject trailing wire bytes
        // and observe late transport errors before accepting the layer.
        if self.read(&mut [0])? != 0 {
            bail!("trailing bytes after compressed scute");
        }
        if self.count != self.expected {
            bail!("layer size mismatch");
        }
        let actual = Digest::Sha256(self.hash.finalize().into());
        if &actual != digest {
            bail!("layer digest mismatch: expected {digest}, got {actual}");
        }
        self.output.flush()?;
        Ok(())
    }
}

/// Decoded-byte side effects; synthetic verity padding never reaches this writer.
struct DecodedReader<R, W> {
    input: R,
    output: W,
    count: u64,
}

impl<R: Read, W: Write> Read for DecodedReader<R, W> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let count = self.input.read(out)?;
        self.count = self
            .count
            .checked_add(count as u64)
            .ok_or(io::ErrorKind::InvalidData)?;
        if self.count > DECOMPRESSED_CAP {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "decompressed size exceeds cap",
            ));
        }
        self.output.write_all(&out[..count])?;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::verity::VerityParams;

    struct Fixture {
        data: Vec<u8>,
        wire: Vec<u8>,
        manifest: Manifest,
    }

    impl Fixture {
        fn new(compressed: bool, declared: Option<u64>) -> Self {
            let data = vec![0x5a; 4097];
            let wire = if compressed {
                ruzstd::encoding::compress_to_vec(
                    data.as_slice(),
                    ruzstd::encoding::CompressionLevel::Fastest,
                )
            } else {
                data.clone()
            };
            let root = VerityParams {
                data_block_size: 4096,
                hash_block_size: 4096,
                salt: vec![0; 32],
                uuid: [0; 16],
            }
            .compute(&data)
            .unwrap()
            .root_hash;
            let mut annotations =
                serde_json::json!({ "dev.pichi.scute.verity.salt": hex::encode([0; 32]) });
            if let Some(blocks) = declared {
                annotations["dev.pichi.scute.verity.data-blocks"] = blocks.to_string().into();
            }
            let manifest = serde_json::json!({
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "artifactType": pichi_artifact::MEDIA_TYPE_PICHI_ARTIFACT_V1,
                "config": pichi_artifact::ConfigDescriptor::canonical(),
                "layers": [{
                    "mediaType": if compressed { pichi_artifact::MEDIA_TYPE_PICHI_SCUTE_V1_ZSTD } else { pichi_artifact::MEDIA_TYPE_PICHI_SCUTE_V1 },
                    "digest": Digest::from_bytes_sha256(&wire).to_string(),
                    "size": wire.len(), "annotations": annotations,
                }],
                "annotations": {
                    "dev.pichi.carapace.verity.algo": "sha256",
                    "dev.pichi.carapace.verity.data-block-size": "4096",
                    "dev.pichi.carapace.verity.hash-block-size": "4096",
                    "dev.pichi.carapace.verity.version": "1",
                    "dev.pichi.carapace.verity.hash-type": "1",
                    "dev.pichi.carapace.verity.hash": hex::encode(root),
                },
            });
            Self {
                data,
                wire,
                manifest: serde_json::from_value(manifest).unwrap(),
            }
        }

        fn prepare(&self, scratch: &Path) -> Result<PreparedLayer> {
            PreparedLayer::read(
                self.wire.as_slice(),
                &self.manifest,
                &self.manifest.layers[0],
                scratch,
            )
        }
    }

    #[test]
    fn streams_raw_and_zstd_with_legacy_fallback() {
        for compressed in [false, true] {
            for declared in [None, Some(2)] {
                let fixture = Fixture::new(compressed, declared);
                let scratch = tempfile::tempdir().unwrap();
                let result = fixture.prepare(scratch.path()).unwrap();
                assert_eq!(result.root, fixture.manifest.carapace_verity_hash());
                assert_eq!(
                    std::fs::read(result.source.as_ref().unwrap().path()).unwrap(),
                    fixture.wire
                );
                if let Some(deflated) = result.deflated.as_ref() {
                    // The partial final block's padding is only a formatter input.
                    assert_eq!(std::fs::read(deflated.path()).unwrap(), fixture.data);
                }
                let params = VerityParams {
                    data_block_size: 4096,
                    hash_block_size: 4096,
                    salt: vec![0; 32],
                    uuid: [0; 16],
                };
                let verity_bytes = std::fs::read(result.verity.as_ref().unwrap().path()).unwrap();
                let header =
                    devmap_verity::Header::decode(verity_bytes[..512].try_into().unwrap()).unwrap();
                assert_eq!(header.parameters.data.count.get(), 2);
                assert_eq!(verity_bytes, params.compute(&fixture.data).unwrap().blob);
                drop(result);
                assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
            }
        }
    }

    #[tokio::test]
    async fn cached_preparation_formats_repairs_and_reuses_sidecars() {
        for compressed in [false, true] {
            for blocks in [None, Some(2)] {
                let fixture = Fixture::new(compressed, blocks);
                let dir = tempfile::tempdir().unwrap();
                let store = pichi_storage::FilesystemBlobStore::new(dir.path());
                let layer = &fixture.manifest.layers[0];
                let digest = layer.digest_str().parse().unwrap();
                store.put_blob(&digest, &fixture.wire).await.unwrap();
                let path = store.blob_path(&digest);
                let expected = fixture.prepare(dir.path()).unwrap();
                for _ in 0..2 {
                    let root = PreparedLayer::prepare_cached(&fixture.manifest, layer, &store)
                        .await
                        .unwrap();
                    assert_eq!(root, expected.root);
                    assert_eq!(
                        std::fs::read(path.verity_path()).unwrap(),
                        std::fs::read(expected.verity.as_ref().unwrap().path()).unwrap()
                    );
                    assert_eq!(std::fs::read(&path).unwrap(), fixture.wire);
                }
                if compressed {
                    std::fs::remove_file(path.deflated_path()).unwrap();
                    let root = PreparedLayer::prepare_cached(&fixture.manifest, layer, &store)
                        .await
                        .unwrap();
                    assert_eq!(root, expected.root);
                    assert_eq!(std::fs::read(path.deflated_path()).unwrap(), fixture.data);
                }
            }
        }
    }

    #[test]
    fn rejects_wrong_lengths_digests_and_trailing_frames_without_leaking_temps() {
        let scratch = tempfile::tempdir().unwrap();
        for declared in [0, 1, 3, DECOMPRESSED_CAP / 4096 + 1, u64::MAX] {
            let fixture = Fixture::new(true, Some(declared));
            assert!(fixture.prepare(scratch.path()).is_err());
        }
        for compressed in [false, true] {
            let mut fixture = Fixture::new(compressed, Some(2));
            fixture.wire.pop();
            assert!(fixture.prepare(scratch.path()).is_err());
            let mut fixture = Fixture::new(compressed, Some(2));
            let (Layer::Scute(d) | Layer::ScuteZstd(d)) = &mut fixture.manifest.layers[0] else {
                unreachable!()
            };
            d.digest = Digest::from_bytes_sha256(b"wrong").to_string();
            assert!(fixture.prepare(scratch.path()).is_err());
        }
        let mut fixture = Fixture::new(true, Some(2));
        fixture.wire.extend_from_within(..);
        let Layer::ScuteZstd(d) = &mut fixture.manifest.layers[0] else {
            unreachable!()
        };
        d.size = fixture.wire.len() as u64;
        d.digest = Digest::from_bytes_sha256(&fixture.wire).to_string();
        let error = fixture.prepare(scratch.path()).err().unwrap();
        assert!(error.to_string().contains("trailing bytes"), "{error:#}");
        assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
    }

    struct LateFailure<'a>(&'a [u8]);

    impl Read for LateFailure<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.0.is_empty() {
                return Err(io::Error::other("late network failure"));
            }
            self.0.read(out)
        }
    }

    #[test]
    fn rejects_zstd_window_above_default_max() {
        use ruzstd::decoding::DEFAULT_MAX_WINDOW_SIZE;

        // Minimal zstd frame with Window_Descriptor for 256 MiB — above
        // ruzstd's DEFAULT_MAX_WINDOW_SIZE (128 MiB). Bytes from
        // `printf 'window-size rejection fixture\n' | zstd --long=28`.
        // Orthogonal to DECOMPRESSED_CAP: the decoder must refuse the
        // frame header before any decompressed-size accounting runs.
        const WIRE: &[u8] = &[
            0x28, 0xb5, 0x2f, 0xfd, 0x04, 0x90, 0xf1, 0x00, 0x00, 0x77, 0x69, 0x6e, 0x64, 0x6f,
            0x77, 0x2d, 0x73, 0x69, 0x7a, 0x65, 0x20, 0x72, 0x65, 0x6a, 0x65, 0x63, 0x74, 0x69,
            0x6f, 0x6e, 0x20, 0x66, 0x69, 0x78, 0x74, 0x75, 0x72, 0x65, 0x0a, 0xd6, 0xc6, 0x80,
            0xf8,
        ];
        assert_eq!(DEFAULT_MAX_WINDOW_SIZE, 128 * 1024 * 1024);

        let digest = Digest::from_bytes_sha256(WIRE);
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "artifactType": pichi_artifact::MEDIA_TYPE_PICHI_ARTIFACT_V1,
            "config": pichi_artifact::ConfigDescriptor::canonical(),
            "layers": [{
                "mediaType": pichi_artifact::MEDIA_TYPE_PICHI_SCUTE_V1_ZSTD,
                "digest": digest.to_string(),
                "size": WIRE.len(),
                "annotations": {
                    "dev.pichi.scute.verity.salt": hex::encode([0; 32]),
                    "dev.pichi.scute.verity.data-blocks": "1",
                },
            }],
            "annotations": {
                "dev.pichi.carapace.verity.algo": "sha256",
                "dev.pichi.carapace.verity.data-block-size": "4096",
                "dev.pichi.carapace.verity.hash-block-size": "4096",
                "dev.pichi.carapace.verity.version": "1",
                "dev.pichi.carapace.verity.hash-type": "1",
                "dev.pichi.carapace.verity.hash": hex::encode([0; 32]),
            },
        });
        let manifest: Manifest = serde_json::from_value(manifest).unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let error = PreparedLayer::read(WIRE, &manifest, &manifest.layers[0], scratch.path())
            .err()
            .expect("oversized zstd window must fail");
        let message = format!("{error:#}");
        assert!(
            message.contains("zstd window size too large"),
            "expected clean user-facing window error, got: {message}"
        );
        assert!(
            message.contains(&DEFAULT_MAX_WINDOW_SIZE.to_string()) || message.contains("128"),
            "error should mention the 128 MiB limit, got: {message}"
        );
        assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
    }

    #[test]
    fn observes_network_error_after_declared_extent() {
        let scratch = tempfile::tempdir().unwrap();
        for compressed in [false, true] {
            let fixture = Fixture::new(compressed, Some(2));
            let error = PreparedLayer::read(
                LateFailure(&fixture.wire),
                &fixture.manifest,
                &fixture.manifest.layers[0],
                scratch.path(),
            )
            .err()
            .unwrap();
            assert!(format!("{error:#}").contains("late network failure"));
        }
        assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
    }
}
