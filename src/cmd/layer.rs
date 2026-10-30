// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Streaming layer preparation shared by pull and load.

use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use crate::cmd::verity::VerityFormat as _;
use anyhow::{Context as _, Result, bail};
use conglobate::artifact::{Digest, Layer, Manifest, Reference};
use pichi_registry::Registry;
use pichi_storage::{BlobSidecarExt, BlobStore};
use sha2::{Digest as _, Sha256};
use tempfile::NamedTempFile;
use tokio_util::io::{StreamReader, SyncIoBridge};

use crate::cmd::manifest_ext::ManifestExt as _;

/// The exact cache files selected by a manifest's reconstruction parameters.
pub(super) struct ScuteFiles {
    pub data: PathBuf,
    pub verity: PathBuf,
}

impl ScuteFiles {
    pub(super) fn new(manifest: &Manifest, layer: &Layer, store: &dyn BlobStore) -> Result<Self> {
        let (scute, compressed) = match layer {
            Layer::Scute(scute) => (scute, false),
            Layer::ScuteZstd(scute) => (scute, true),
            _ => bail!("metadata layer has no scute files"),
        };
        let header = manifest.verity_header(scute, (!compressed).then_some(scute.size))?;
        Self::from_header(
            &store.blob_path(&scute.digest.parse()?),
            compressed || scute.size != header.parameters.data_size()?,
            &header,
        )
    }

    pub(super) fn from_header(
        source: &Path,
        materialized: bool,
        header: &devmap_verity::Header,
    ) -> Result<Self> {
        let key = Digest::Sha256(Sha256::digest(header.encode()).into());
        let data = if materialized {
            source.deflated_path(header.parameters.data_size()?)
        } else {
            source.to_owned()
        };
        Ok(Self {
            data,
            verity: source.verity_path(&key),
        })
    }
}

pub(super) struct PreparedLayer {
    source: Option<NamedTempFile>,
    scute: Option<PreparedScute>,
}

struct PreparedScute {
    header: devmap_verity::Header,
    deflated: Option<NamedTempFile>,
    verity: NamedTempFile,
    root: [u8; 32],
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

    /// Import must rebuild even when a cache sidecar already exists.
    pub(super) async fn prepare_cached(
        manifest: &Manifest,
        layer: &Layer,
        store: &dyn BlobStore,
    ) -> Result<Option<[u8; 32]>> {
        let digest: Digest = layer.digest_str().parse()?;
        let path = store.blob_path(&digest);
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
        let scute = match layer {
            Layer::Scute(scute) => {
                let mut output = NamedTempFile::new_in(scratch)?;
                let header = manifest.verity_header(scute, Some(scute.size))?;
                let (deflated, root) = if header.parameters.data_size()? == scute.size {
                    (None, header.format_padded(&mut wire, output.as_file_mut())?)
                } else {
                    let (data, root) =
                        Self::prepare_data(&mut wire, &header, output.as_file_mut(), scratch)?;
                    (Some(data), root)
                };
                Some(PreparedScute {
                    header,
                    root,
                    verity: output,
                    deflated,
                })
            }
            Layer::ScuteZstd(scute) => {
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
                let header = manifest.verity_header(scute, None)?;
                let (data, root) =
                    Self::prepare_data(decoder, &header, output.as_file_mut(), scratch)?;
                Some(PreparedScute {
                    header,
                    root,
                    verity: output,
                    deflated: Some(data),
                })
            }
            Layer::Pmi(_) | Layer::Dtb(_) => {
                io::copy(&mut wire, &mut io::sink())?;
                None
            }
        };
        wire.finish(&layer.digest_str().parse()?)?;
        if let Some(scute) = &scute {
            scute.verity.as_file().sync_all()?;
            if let Some(data) = &scute.deflated {
                data.as_file().sync_all()?;
            }
        }
        Ok(Self {
            source: None,
            scute,
        })
    }

    /// Store the data stream while hashing it, then materialize final-block padding.
    fn prepare_data(
        input: impl Read,
        header: &devmap_verity::Header,
        hash: &mut std::fs::File,
        scratch: &Path,
    ) -> Result<(NamedTempFile, [u8; 32])> {
        let data = NamedTempFile::new_in(scratch)?;
        // Storage capacity and the header's expansion bound are independent.
        // Concurrent allocations and tree writes can still fail with I/O errors.
        let capacity = fs4::available_space(scratch)?;
        let padded_size = header.parameters.data_size()?;
        if padded_size > capacity {
            bail!("prepared data extent exceeds available scratch capacity");
        }
        let mut decoded = DecodedReader {
            input,
            output: BufWriter::new(data.as_file()),
            count: 0,
            capacity,
        };
        let root = header.format_padded(&mut decoded, hash)?;
        decoded.output.flush()?;
        drop(decoded);
        data.as_file().set_len(padded_size)?;
        Ok((data, root))
    }

    async fn commit(self, store: &dyn BlobStore, digest: &Digest) -> Result<Option<[u8; 32]>> {
        let path = store.blob_path(digest);
        tokio::fs::create_dir_all(path.parent().context("blob path has no parent")?).await?;
        // Publish preparation first: the source blob marks cache presence.
        if let Some(scute) = &self.scute {
            let files = ScuteFiles::from_header(&path, scute.deflated.is_some(), &scute.header)?;
            tokio::fs::create_dir_all(path.prepared_dir()).await?;
            if let Some(data) = &scute.deflated {
                tokio::fs::rename(data.path(), files.data).await?;
            }
            tokio::fs::rename(scute.verity.path(), files.verity).await?;
        }
        if let Some(source) = &self.source {
            store.put_blob_from_path(source.path(), digest).await?;
        }
        Ok(self.scute.map(|scute| scute.root))
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
    capacity: u64,
}

impl<R: Read, W: Write> Read for DecodedReader<R, W> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let count = self.input.read(out)?;
        self.count = self
            .count
            .checked_add(count as u64)
            .ok_or(io::ErrorKind::InvalidData)?;
        if self.count > self.capacity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "decompressed size exceeds available scratch capacity",
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
        root: [u8; 32],
        manifest: Manifest,
    }

    impl Fixture {
        fn new(compressed: bool, declared: u64) -> Self {
            Self::with_size(compressed, declared, 4097)
        }

        fn with_size(compressed: bool, declared: u64, size: usize) -> Self {
            let data = vec![0x5a; size];
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
            let annotations = serde_json::json!({
                "dev.pichi.scute.verity.salt": hex::encode([0; 32]),
                "dev.pichi.scute.verity.data-blocks": declared.to_string(),
            });
            let manifest = serde_json::json!({
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "artifactType": conglobate::artifact::MEDIA_TYPE_PICHI_ARTIFACT_V1,
                "config": conglobate::artifact::ConfigDescriptor::canonical(),
                "layers": [{
                    "mediaType": if compressed { conglobate::artifact::MEDIA_TYPE_PICHI_SCUTE_V1_ZSTD } else { conglobate::artifact::MEDIA_TYPE_PICHI_SCUTE_V1 },
                    "digest": Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&wire).into()).to_string(),
                    "size": wire.len(), "annotations": annotations,
                }],
                "annotations": {
                    "dev.pichi.carapace.verity.algo": "sha256",
                    "dev.pichi.carapace.verity.data-block-size": "4096",
                    "dev.pichi.carapace.verity.hash-block-size": "4096",
                    "dev.pichi.carapace.verity.version": "1",
                    "dev.pichi.carapace.verity.hash-type": "1",
                },
            });
            Self {
                data,
                wire,
                root,
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
    fn streams_raw_and_zstd_with_declared_extents() {
        for compressed in [false, true] {
            let fixture = Fixture::new(compressed, 2);
            let scratch = tempfile::tempdir().unwrap();
            let result = fixture.prepare(scratch.path()).unwrap();
            assert_eq!(result.scute.as_ref().unwrap().root, fixture.root);
            assert_eq!(
                std::fs::read(result.source.as_ref().unwrap().path()).unwrap(),
                fixture.wire
            );
            let deflated = result.scute.as_ref().unwrap().deflated.as_ref().unwrap();
            let mut padded = fixture.data.clone();
            padded.resize(8192, 0);
            assert_eq!(std::fs::read(deflated.path()).unwrap(), padded);
            let params = VerityParams {
                data_block_size: 4096,
                hash_block_size: 4096,
                salt: vec![0; 32],
                uuid: [0; 16],
            };
            let verity_bytes = std::fs::read(result.scute.as_ref().unwrap().verity.path()).unwrap();
            let header =
                devmap_verity::Header::decode(verity_bytes[..512].try_into().unwrap()).unwrap();
            assert_eq!(header.parameters.data.count.get(), 2);
            assert_eq!(verity_bytes, params.compute(&fixture.data).unwrap().blob);
            drop(result);
            assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn aligned_raw_data_needs_no_prepared_copy() {
        for compressed in [false, true] {
            let fixture = Fixture::with_size(compressed, 2, 8192);
            let scratch = tempfile::tempdir().unwrap();
            let prepared = fixture.prepare(scratch.path()).unwrap();
            assert_eq!(
                prepared.scute.as_ref().unwrap().deflated.is_some(),
                compressed
            );
            assert_eq!(prepared.scute.as_ref().unwrap().root, fixture.root);
            assert_eq!(
                std::fs::read(prepared.source.as_ref().unwrap()).unwrap(),
                fixture.wire
            );
        }
    }

    #[tokio::test]
    async fn cached_preparation_rebuilds_and_repairs_sidecars() {
        for compressed in [false, true] {
            let fixture = Fixture::new(compressed, 2);
            let dir = tempfile::tempdir().unwrap();
            let store = pichi_storage::FilesystemBlobStore::new(dir.path());
            let layer = &fixture.manifest.layers[0];
            let digest = layer.digest_str().parse().unwrap();
            store.put_blob(&digest, &fixture.wire).await.unwrap();
            let path = store.blob_path(&digest);
            let expected = fixture.prepare(dir.path()).unwrap().scute.unwrap();
            let files = ScuteFiles::new(&fixture.manifest, layer, &store).unwrap();
            for _ in 0..2 {
                let root = PreparedLayer::prepare_cached(&fixture.manifest, layer, &store)
                    .await
                    .unwrap();
                assert_eq!(root, Some(expected.root));
                assert_eq!(
                    std::fs::read(&files.verity).unwrap(),
                    std::fs::read(expected.verity.path()).unwrap()
                );
                assert_eq!(std::fs::read(&path).unwrap(), fixture.wire);
            }
            {
                let mut padded = fixture.data.clone();
                padded.resize(8192, 0);
                for stale in [None, Some(vec![0x33; 4097]), Some(vec![0x33; 8192])] {
                    if let Some(bytes) = stale {
                        std::fs::write(&files.data, bytes).unwrap();
                    } else {
                        std::fs::remove_file(&files.data).unwrap();
                    }
                    let root = PreparedLayer::prepare_cached(&fixture.manifest, layer, &store)
                        .await
                        .unwrap();
                    assert_eq!(root, Some(expected.root));
                    assert_eq!(std::fs::read(&files.data).unwrap(), padded);
                }
            }
        }
    }

    #[tokio::test]
    async fn preparation_variants_share_only_identical_data_extents() {
        let mut fixture = Fixture::new(true, 2);
        let dir = tempfile::tempdir().unwrap();
        let store = pichi_storage::FilesystemBlobStore::new(dir.path());
        let digest = fixture.manifest.layers[0].digest_str().parse().unwrap();
        store.put_blob(&digest, &fixture.wire).await.unwrap();
        let mut variants = Vec::new();
        for (block_size, count, hash_size, hash_type, suffix) in [
            (4096, 2, 4096, 1, "aa"),
            (4096, 2, 4096, 1, "bb"),
            (1024, 5, 4096, 1, "aa"),
            (4096, 2, 1024, 1, "aa"),
            (4096, 2, 4096, 0, "aa"),
        ] {
            for (key, value) in [
                ("data-block-size", block_size),
                ("hash-block-size", hash_size),
                ("hash-type", hash_type),
            ] {
                fixture.manifest.annotations.insert(
                    format!("dev.pichi.carapace.verity.{key}"),
                    value.to_string(),
                );
            }
            let Layer::ScuteZstd(scute) = &mut fixture.manifest.layers[0] else {
                unreachable!()
            };
            scute.annotations.data_blocks = (count as u64).try_into().unwrap();
            scute.annotations.salt = format!("{}{suffix}", "00".repeat(32));
            PreparedLayer::prepare_cached(&fixture.manifest, &fixture.manifest.layers[0], &store)
                .await
                .unwrap();
            let files =
                ScuteFiles::new(&fixture.manifest, &fixture.manifest.layers[0], &store).unwrap();
            variants.push((files, fixture.prepare(dir.path()).unwrap().scute.unwrap()));
        }
        assert_eq!(variants[0].0.data, variants[1].0.data);
        assert_ne!(variants[0].0.data, variants[2].0.data);
        let trees: std::collections::HashSet<_> =
            variants.iter().map(|(files, _)| &files.verity).collect();
        assert_eq!(trees.len(), 5);
        for (files, expected) in variants {
            assert_eq!(
                std::fs::read(files.verity).unwrap(),
                std::fs::read(expected.verity.path()).unwrap()
            );
            assert_eq!(
                std::fs::read(files.data).unwrap(),
                std::fs::read(expected.deflated.unwrap().path()).unwrap()
            );
        }
        assert_eq!(store.blob_path(&digest).prepared_files().unwrap().len(), 7);
    }

    #[test]
    fn rejects_wrong_lengths_digests_and_trailing_frames_without_leaking_temps() {
        let scratch = tempfile::tempdir().unwrap();
        let over_capacity = fs4::available_space(scratch.path()).unwrap() / 4096 + 1;
        for declared in [1, 3, over_capacity, u64::MAX] {
            let fixture = Fixture::new(true, declared);
            assert!(fixture.prepare(scratch.path()).is_err());
        }
        for compressed in [false, true] {
            let mut fixture = Fixture::new(compressed, 2);
            fixture.wire.pop();
            assert!(fixture.prepare(scratch.path()).is_err());
            let mut fixture = Fixture::new(compressed, 2);
            let (Layer::Scute(d) | Layer::ScuteZstd(d)) = &mut fixture.manifest.layers[0] else {
                unreachable!()
            };
            d.digest =
                Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(b"wrong").into()).to_string();
            assert!(fixture.prepare(scratch.path()).is_err());
        }
        let mut fixture = Fixture::new(true, 2);
        fixture.wire.extend_from_within(..);
        let Layer::ScuteZstd(d) = &mut fixture.manifest.layers[0] else {
            unreachable!()
        };
        d.size = fixture.wire.len() as u64;
        d.digest = Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&fixture.wire).into())
            .to_string();
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
        // Independently of scratch capacity: the decoder must refuse the
        // frame header before any decompressed-size accounting runs.
        const WIRE: &[u8] = &[
            0x28, 0xb5, 0x2f, 0xfd, 0x04, 0x90, 0xf1, 0x00, 0x00, 0x77, 0x69, 0x6e, 0x64, 0x6f,
            0x77, 0x2d, 0x73, 0x69, 0x7a, 0x65, 0x20, 0x72, 0x65, 0x6a, 0x65, 0x63, 0x74, 0x69,
            0x6f, 0x6e, 0x20, 0x66, 0x69, 0x78, 0x74, 0x75, 0x72, 0x65, 0x0a, 0xd6, 0xc6, 0x80,
            0xf8,
        ];
        assert_eq!(DEFAULT_MAX_WINDOW_SIZE, 128 * 1024 * 1024);

        let digest = Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(WIRE).into());
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "artifactType": conglobate::artifact::MEDIA_TYPE_PICHI_ARTIFACT_V1,
            "config": conglobate::artifact::ConfigDescriptor::canonical(),
            "layers": [{
                "mediaType": conglobate::artifact::MEDIA_TYPE_PICHI_SCUTE_V1_ZSTD,
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
            let fixture = Fixture::new(compressed, 2);
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
    #[test]
    fn decoded_bytes_obey_the_supplied_storage_budget() {
        for (input, accepted) in [(b"1234".as_slice(), true), (b"12345".as_slice(), false)] {
            let mut reader = DecodedReader {
                input,
                output: Vec::new(),
                count: 0,
                capacity: 4,
            };
            assert_eq!(io::copy(&mut reader, &mut io::sink()).is_ok(), accepted);
            assert!(reader.output.len() <= 4);
        }
        let mut reader = DecodedReader {
            input: b"12345".as_slice(),
            output: Vec::new(),
            count: 0,
            capacity: 4,
        };
        let mut chunk = [0; 2];
        assert_eq!(reader.read(&mut chunk).unwrap(), 2);
        assert_eq!(reader.read(&mut chunk).unwrap(), 2);
        assert!(reader.read(&mut chunk).is_err());
        assert_eq!(reader.output, b"1234");
    }
}
