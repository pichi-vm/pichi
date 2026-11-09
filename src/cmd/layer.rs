// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Streaming layer preparation shared by pull and load.

use std::io::{self, BufReader, BufWriter, Read, Seek, Write};
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
    pub data_size: u64,
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
            data_size: header.parameters.data_size()?,
        })
    }
}

pub(super) struct PreparedLayer {
    source: Option<NamedTempFile>,
    scute: Option<PreparedScute>,
    pmi: Option<NamedTempFile>,
    bootconfig: Option<NamedTempFile>,
}

struct PreparedScute {
    header: devmap_verity::Header,
    deflated: Option<NamedTempFile>,
    verity: NamedTempFile,
    root: [u8; 32],
}

impl PreparedLayer {
    /// Select an already prepared PMI. Launch never repairs or decodes cache files.
    pub(super) async fn pmi_file(layer: &Layer, store: &dyn BlobStore) -> Result<PathBuf> {
        let source = store.blob_path(&layer.digest_str().parse()?);
        let path = match layer {
            Layer::Pmi(_) => source,
            Layer::PmiZstd(_) => Self::pmi_path(&source),
            _ => bail!("layer is not PMI"),
        };
        anyhow::ensure!(
            tokio::fs::metadata(&path)
                .await
                .with_context(|| format!(
                    "prepared PMI missing: {}; pull --pull=always or load the image again",
                    path.display()
                ))?
                .is_file(),
            "prepared PMI is not a file"
        );
        Ok(path)
    }

    fn pmi_path(source: &Path) -> PathBuf {
        source.prepared_dir().join("pmi")
    }

    fn decoder<R: Read>(input: R) -> Result<LayerDecoder<R>> {
        LayerDecoder::new(input)
    }

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

    /// Reconstruct canonical bytes and verity without trusting cache sidecars.
    pub(super) async fn prepare_cached(
        manifest: &Manifest,
        layer: &Layer,
        store: &dyn BlobStore,
    ) -> Result<Option<[u8; 32]>> {
        Self::prepare_cached_reusing(manifest, layer, store, None).await
    }

    /// Reuse matching bytes in a private clone, repairing differences while the
    /// canonical source is decoded and verified once.
    pub(super) async fn prepare_cached_reusing(
        manifest: &Manifest,
        layer: &Layer,
        store: &dyn BlobStore,
        reuse: Option<&dyn BlobStore>,
    ) -> Result<Option<[u8; 32]>> {
        let digest: Digest = layer.digest_str().parse()?;
        let path = store.blob_path(&digest);
        let scratch = store.scratch_dir().await?;
        let seed = if matches!(layer, Layer::Scute(_) | Layer::ScuteZstd(_)) {
            reuse
                .map(|store| ScuteFiles::new(manifest, layer, store).map(|files| files.data))
                .transpose()?
        } else {
            None
        };
        let manifest = manifest.clone();
        let layer = layer.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            let input = BufReader::new(std::fs::File::open(path)?);
            Self::read_into(
                input,
                io::sink(),
                &manifest,
                &layer,
                &scratch,
                seed.as_deref(),
            )
        })
        .await
        .context("layer formatting task panicked")??;
        prepared.commit(store, &digest).await
    }

    fn read<R: Read + Send>(
        input: R,
        manifest: &Manifest,
        layer: &Layer,
        scratch: &Path,
    ) -> Result<Self> {
        let source = NamedTempFile::new_in(scratch)?;
        let mut prepared = Self::read_into(
            input,
            BufWriter::new(source.as_file()),
            manifest,
            layer,
            scratch,
            None,
        )?;
        source.as_file().sync_all()?;
        prepared.source = Some(source);
        Ok(prepared)
    }

    fn read_into<R: Read + Send, W: Write + Send>(
        input: R,
        output: W,
        manifest: &Manifest,
        layer: &Layer,
        scratch: &Path,
        data_seed: Option<&Path>,
    ) -> Result<Self> {
        let mut wire = WireReader {
            input,
            output,
            hash: Sha256::new(),
            count: 0,
            expected: layer.size(),
        };
        let mut pmi = None;
        let mut bootconfig = None;
        let scute = match layer {
            Layer::Scute(scute) => {
                let mut output = NamedTempFile::new_in(scratch)?;
                let header = manifest.verity_header(scute, Some(scute.size))?;
                let (deflated, root) = if header.parameters.data_size()? == scute.size {
                    (None, header.format_padded(&mut wire, output.as_file_mut())?)
                } else {
                    let (data, root) = Self::prepare_data(
                        &mut wire,
                        &header,
                        output.as_file_mut(),
                        scratch,
                        data_seed,
                    )?;
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
                let decoder = Self::decoder(&mut wire)?;
                let header = manifest.verity_header(scute, None)?;
                let (data, root) = DecodePipeline::run(decoder, |input| {
                    Self::prepare_data(input, &header, output.as_file_mut(), scratch, data_seed)
                })?;
                Some(PreparedScute {
                    header,
                    root,
                    verity: output,
                    deflated: Some(data),
                })
            }
            Layer::BootConfig(_) => {
                let mut bytes = Vec::new();
                wire.read_to_end(&mut bytes)?;
                let config = super::boot_config::BootConfigFile::try_from(bytes)?;
                anyhow::ensure!(!config.text.is_empty(), "empty bootconfig must be omitted");
                let mut output = NamedTempFile::new_in(scratch)?;
                output.write_all(config.text.as_bytes())?;
                bootconfig = Some(output);
                None
            }
            Layer::PmiZstd(_) => {
                let output = NamedTempFile::new_in(scratch)?;
                let mut decoded = DecodedReader {
                    input: Self::decoder(&mut wire)?,
                    output: BufWriter::new(output.as_file()),
                    count: 0,
                    capacity: fs4::available_space(scratch)?,
                };
                io::copy(&mut decoded, &mut io::sink())?;
                decoded.output.flush()?;
                drop(decoded);
                pmi = Some(output);
                None
            }
            Layer::Pmi(_) => {
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
        if let Some(pmi) = &pmi {
            pmi.as_file().sync_all()?;
        }
        if let Some(config) = &bootconfig {
            config.as_file().sync_all()?;
        }
        Ok(Self {
            source: None,
            scute,
            pmi,
            bootconfig,
        })
    }

    /// Store the data stream while hashing it, then materialize final-block padding.
    fn prepare_data(
        input: impl Read,
        header: &devmap_verity::Header,
        hash: &mut std::fs::File,
        scratch: &Path,
        seed: Option<&Path>,
    ) -> Result<(NamedTempFile, [u8; 32])> {
        let mut data = NamedTempFile::new_in(scratch)?;
        // Storage capacity and the header's expansion bound are independent.
        // Concurrent allocations and tree writes can still fail with I/O errors.
        let capacity = fs4::available_space(scratch)?;
        let padded_size = header.parameters.data_size()?;
        if padded_size > capacity {
            bail!("prepared data extent exceeds available scratch capacity");
        }
        let output = BufWriter::new(PreparedDataWriter::new(data.as_file_mut(), seed)?);
        let mut decoded = DecodedReader {
            input,
            output,
            count: 0,
            capacity,
        };
        let root = header.format_padded(&mut decoded, hash)?;
        decoded.output.flush()?;
        let count = decoded.count;
        drop(decoded);
        // Recreate canonical zero padding, even if the clone had a stale tail.
        data.as_file().set_len(count)?;
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
        if let Some(pmi) = &self.pmi {
            tokio::fs::create_dir_all(path.prepared_dir()).await?;
            tokio::fs::rename(pmi.path(), Self::pmi_path(&path)).await?;
        }
        if let Some(config) = &self.bootconfig {
            tokio::fs::create_dir_all(path.prepared_dir()).await?;
            tokio::fs::rename(
                config.path(),
                super::boot_config::BootConfigFile::path(&path),
            )
            .await?;
        }
        if let Some(source) = &self.source {
            store.put_blob_from_path(source.path(), digest).await?;
        }
        Ok(self.scute.map(|scute| scute.root))
    }
}

/// Decode one bounded frame without silently consuming trailing wire bytes.
struct LayerDecoder<R: Read> {
    inner: zstd::stream::read::Decoder<'static, BufReader<io::Chain<io::Cursor<[u8; 4]>, R>>>,
}

impl<R: Read> LayerDecoder<R> {
    fn new(mut input: R) -> Result<Self> {
        // Match ruzstd's single standard-frame contract. The native decoder
        // also accepts skippable frames, which are not layer payloads.
        let mut magic = [0; 4];
        input.read_exact(&mut magic)?;
        anyhow::ensure!(
            magic == [0x28, 0xb5, 0x2f, 0xfd],
            "expected a standard zstd frame"
        );
        // Replay below WireReader so canonical bytes are hashed exactly once.
        let input = io::Cursor::new(magic).chain(input);
        let mut inner = zstd::stream::read::Decoder::new(input)?.single_frame();
        // Preserve the existing 128 MiB maximum decoder window.
        inner.window_log_max(27)?;
        Ok(Self { inner })
    }
}

impl<R: Read> Read for LayerDecoder<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        let count = self.inner.read(output).map_err(|error| {
            if error.to_string().contains("too much memory") {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "zstd window size too large (limit 128 MiB)",
                )
            } else {
                error
            }
        })?;
        // The native decoder buffers input. Bytes read past this frame must
        // be rejected here; WireReader::finish checks the unbuffered remainder.
        if count == 0 && !self.inner.get_ref().buffer().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "trailing bytes after compressed layer",
            ));
        }
        Ok(count)
    }
}

/// Skip matching private-clone writes; repair every other byte from the source.
struct PreparedDataWriter<'a> {
    file: &'a mut std::fs::File,
    comparison: Option<Vec<u8>>,
}

impl<'a> PreparedDataWriter<'a> {
    fn new(file: &'a mut std::fs::File, seed: Option<&Path>) -> io::Result<Self> {
        let cloned = seed.is_some_and(|seed| {
            std::fs::symlink_metadata(seed).is_ok_and(|metadata| metadata.is_file())
                && std::fs::File::open(seed)
                    .is_ok_and(|source| Self::clone_data(&source, file).is_ok())
        });
        if !cloned {
            file.set_len(0)?;
        }
        file.rewind()?;
        Ok(Self {
            file,
            comparison: cloned.then(Vec::new),
        })
    }

    fn clone_data(source: &std::fs::File, target: &std::fs::File) -> io::Result<()> {
        #[cfg(all(
            target_os = "linux",
            not(any(target_arch = "sparc", target_arch = "sparc64"))
        ))]
        {
            rustix::fs::ioctl_ficlone(target, source).map_err(Into::into)
        }
        #[cfg(not(all(
            target_os = "linux",
            not(any(target_arch = "sparc", target_arch = "sparc64"))
        )))]
        {
            let _ = (source, target);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "data reflinks unavailable",
            ))
        }
    }
}

impl Write for PreparedDataWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Some(buffer) = &mut self.comparison {
            let position = self.file.stream_position()?;
            buffer.resize(bytes.len(), 0);
            if self.file.read_exact(buffer).is_ok() && buffer.as_slice() == bytes {
                return Ok(bytes.len());
            }
            // A short or failed comparison must not move the canonical write.
            self.file.seek(io::SeekFrom::Start(position))?;
        }
        self.file.write_all(bytes)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Bounded decompression read-ahead, joined before canonical verification finishes.
struct DecodePipeline {
    receiver: std::sync::mpsc::Receiver<io::Result<Vec<u8>>>,
    current: io::Cursor<Vec<u8>>,
    eof: bool,
}

impl DecodePipeline {
    fn run<R: Read + Send, T>(
        mut input: R,
        consume: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        std::thread::scope(|scope| {
            // At most two queued chunks, one being produced and one consumed.
            let (sender, receiver) = std::sync::mpsc::sync_channel(2);
            let worker = std::thread::Builder::new()
                .name("pichi-decode".into())
                .spawn_scoped(scope, move || {
                    loop {
                        let mut bytes = vec![0; 128 * 1024];
                        let count = loop {
                            match input.read(&mut bytes) {
                                Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                                    continue;
                                }
                                result => break result,
                            }
                        };
                        match count {
                            Ok(count) => {
                                bytes.truncate(count);
                                if sender.send(Ok(bytes)).is_err() || count == 0 {
                                    break;
                                }
                            }
                            Err(error) => {
                                let _ = sender.send(Err(error));
                                break;
                            }
                        }
                    }
                })?;
            let mut reader = Self {
                receiver,
                current: io::Cursor::new(Vec::new()),
                eof: false,
            };
            let result = consume(&mut reader);
            let eof = reader.eof;
            // A failed consumer must release the bounded sender before joining.
            drop(reader);
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("decode worker panicked"))?;
            let value = result?;
            anyhow::ensure!(eof, "decoded stream was not consumed through EOF");
            Ok(value)
        })
    }
}

impl Read for DecodePipeline {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() || self.eof {
            return Ok(0);
        }
        loop {
            let count = self.current.read(output)?;
            if count != 0 {
                return Ok(count);
            }
            let bytes = self
                .receiver
                .recv()
                .map_err(|_| io::Error::other("decode worker disconnected before EOF"))??;
            if bytes.is_empty() {
                self.eof = true;
                return Ok(0);
            }
            self.current = io::Cursor::new(bytes);
        }
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
            bail!("trailing bytes after compressed layer");
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

    #[test]
    fn decode_pipeline_preserves_bytes_and_zero_length_reads() {
        let bytes = vec![0x39; 600_003];
        DecodePipeline::run(bytes.as_slice(), |reader| {
            assert_eq!(reader.read(&mut [])?, 0);
            let mut actual = Vec::new();
            reader.read_to_end(&mut actual)?;
            assert_eq!(actual, bytes);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn decode_pipeline_propagates_late_errors_and_retries_interruptions() {
        struct InterruptedOnce(bool);
        impl Read for InterruptedOnce {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                if self.0 {
                    b"".as_slice().read(output)
                } else {
                    self.0 = true;
                    Err(io::ErrorKind::Interrupted.into())
                }
            }
        }
        let error = DecodePipeline::run(LateFailure(b"decoded data"), |reader| {
            io::copy(reader, &mut io::sink())?;
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("late network failure"));

        DecodePipeline::run(InterruptedOnce(false), |reader| {
            assert_eq!(io::copy(reader, &mut io::sink())?, 0);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn decode_pipeline_joins_after_consumer_failure() {
        let error = DecodePipeline::run(io::repeat(0), |_reader| {
            Err::<(), _>(anyhow::anyhow!("consumer rejected data"))
        })
        .unwrap_err();
        assert!(error.to_string().contains("consumer rejected data"));
    }

    #[test]
    fn decode_pipeline_rejects_unconsumed_input_and_worker_panic() {
        struct Panics;
        impl Read for Panics {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                panic!("injected decoder failure");
            }
        }
        assert!(DecodePipeline::run(b"data".as_slice(), |_| Ok(())).is_err());
        assert!(
            DecodePipeline::run(Panics, |reader| {
                io::copy(reader, &mut io::sink())?;
                Ok(())
            })
            .is_err()
        );
    }

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
                    "dev.pichi.carapace.verity.data-block-size": "4096",
                    "dev.pichi.carapace.verity.hash-block-size": "4096",
                    "dev.pichi.carapace.verity.version": "1",
                    "dev.pichi.carapace.verity.hash-type": "1",
                    "dev.pichi.carapace.verity.hash": format!("sha256:{}", hex::encode(root)),
                },
            });
            Self {
                data,
                wire,
                root,
                manifest: serde_json::from_value(manifest).unwrap(),
            }
        }

        fn pmi(compressed: bool) -> Self {
            let mut fixture = Self::new(compressed, 2);
            let descriptor = conglobate::artifact::PmiDescriptor {
                digest: Digest::Sha256(Sha256::digest(&fixture.wire).into()).to_string(),
                size: fixture.wire.len() as u64,
            };
            fixture.manifest.layers = vec![if compressed {
                Layer::PmiZstd(descriptor)
            } else {
                Layer::Pmi(descriptor)
            }];
            fixture.manifest.annotations.clear();
            fixture.manifest.validate().unwrap();
            fixture
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

    #[tokio::test]
    async fn pmi_transport_preserves_exact_bytes_and_repairs_decoded_cache() {
        for compressed in [false, true] {
            let fixture = Fixture::pmi(compressed);
            let dir = tempfile::tempdir().unwrap();
            let store = pichi_storage::FilesystemBlobStore::new(dir.path());
            let layer = &fixture.manifest.layers[0];
            let digest = layer.digest_str().parse().unwrap();
            let prepared = fixture.prepare(dir.path()).unwrap();
            assert!(prepared.scute.is_none());
            assert_eq!(prepared.pmi.is_some(), compressed);
            assert_eq!(prepared.commit(&store, &digest).await.unwrap(), None);
            let source = store.blob_path(&digest);
            for stale in [None, Some(b"stale decoded PMI".as_slice())] {
                if compressed {
                    let path = PreparedLayer::pmi_path(&source);
                    if let Some(bytes) = stale {
                        std::fs::write(path, bytes).unwrap();
                    } else {
                        std::fs::remove_file(path).unwrap();
                    }
                }
                PreparedLayer::prepare_cached(&fixture.manifest, layer, &store)
                    .await
                    .unwrap();
                let path = PreparedLayer::pmi_file(layer, &store).await.unwrap();
                assert_eq!(path == source, !compressed);
                assert_eq!(std::fs::read(path).unwrap(), fixture.data);
                assert_eq!(std::fs::read(&source).unwrap(), fixture.wire);
            }
            if compressed {
                // A valid-looking decoded sidecar must never mask a damaged source.
                std::fs::write(&source, b"damaged wire bytes").unwrap();
                assert!(
                    PreparedLayer::prepare_cached(&fixture.manifest, layer, &store)
                        .await
                        .is_err()
                );
            }
        }
    }

    #[test]
    fn malformed_compressed_pmi_never_publishes_or_leaks_temporary_files() {
        let scratch = tempfile::tempdir().unwrap();
        for case in 0..5 {
            let mut fixture = Fixture::pmi(true);
            let Layer::PmiZstd(descriptor) = &mut fixture.manifest.layers[0] else {
                unreachable!()
            };
            match case {
                0 => {
                    fixture.wire.pop();
                }
                1 => fixture.wire = b"not a zstd frame".to_vec(),
                2 => fixture.wire.extend_from_within(..),
                3 => descriptor.size += 1,
                4 => descriptor.digest = Digest::Sha256([0; 32]).to_string(),
                _ => unreachable!(),
            }
            if case < 3 {
                descriptor.size = fixture.wire.len() as u64;
                descriptor.digest =
                    Digest::Sha256(Sha256::digest(&fixture.wire).into()).to_string();
            }
            assert!(fixture.prepare(scratch.path()).is_err(), "case {case}");
            assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
        }
        let fixture = Fixture::pmi(true);
        assert!(
            PreparedLayer::read(
                LateFailure(&fixture.wire),
                &fixture.manifest,
                &fixture.manifest.layers[0],
                scratch.path(),
            )
            .is_err()
        );
        assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
    }

    #[test]
    fn skippable_frames_are_not_compressed_layer_payloads() {
        let scratch = tempfile::tempdir().unwrap();
        for magic in 0x184D_2A50_u32..=0x184D_2A5F {
            for payload in [Vec::new(), vec![7; 31]] {
                for mut fixture in [Fixture::pmi(true), Fixture::new(true, 2)] {
                    fixture.wire = magic.to_le_bytes().to_vec();
                    fixture
                        .wire
                        .extend_from_slice(&(payload.len() as u32).to_le_bytes());
                    fixture.wire.extend_from_slice(&payload);
                    let (size, digest) = match &mut fixture.manifest.layers[0] {
                        Layer::PmiZstd(descriptor) => {
                            (&mut descriptor.size, &mut descriptor.digest)
                        }
                        Layer::ScuteZstd(descriptor) => {
                            (&mut descriptor.size, &mut descriptor.digest)
                        }
                        _ => unreachable!(),
                    };
                    *size = fixture.wire.len() as u64;
                    *digest = Digest::Sha256(Sha256::digest(&fixture.wire).into()).to_string();
                    assert!(fixture.prepare(scratch.path()).is_err(), "magic {magic:#x}");
                    assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
                }
            }
        }
    }

    #[test]
    fn compressed_layers_accept_fragmented_wire_input() {
        struct Fragments<'a>(&'a [u8]);
        impl Read for Fragments<'_> {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                let length = output.len().min(3);
                self.0.read(&mut output[..length])
            }
        }
        for fixture in [Fixture::pmi(true), Fixture::new(true, 2)] {
            let scratch = tempfile::tempdir().unwrap();
            let prepared = PreparedLayer::read(
                Fragments(&fixture.wire),
                &fixture.manifest,
                &fixture.manifest.layers[0],
                scratch.path(),
            )
            .unwrap();
            assert_eq!(
                std::fs::read(prepared.source.as_ref().unwrap().path()).unwrap(),
                fixture.wire
            );
            if let Some(pmi) = &prepared.pmi {
                assert_eq!(std::fs::read(pmi.path()).unwrap(), fixture.data);
            }
            if let Some(scute) = &prepared.scute {
                assert_eq!(scute.root, fixture.root);
            }
            drop(prepared);
            assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
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
    async fn data_hints_are_private_and_repair_stale_bytes_and_padding() {
        use pichi_storage::FilesystemBlobStore;
        for compressed in [false, true] {
            let fixture = Fixture::with_size(compressed, 40, 160_001);
            let dir = tempfile::tempdir().unwrap();
            let store = FilesystemBlobStore::new(dir.path().join("canonical"));
            let hints = FilesystemBlobStore::new(dir.path().join("hints"));
            let layer = &fixture.manifest.layers[0];
            let digest = layer.digest_str().parse().unwrap();
            store.put_blob(&digest, &fixture.wire).await.unwrap();
            let hint = ScuteFiles::new(&fixture.manifest, layer, &hints)
                .unwrap()
                .data;
            std::fs::create_dir_all(hint.parent().unwrap()).unwrap();
            let mut expected = fixture.data.clone();
            expected.resize(40 * 4096, 0);
            for case in [
                "valid",
                "missing",
                "payload",
                "padding",
                "short",
                "long",
                "directory",
                #[cfg(unix)]
                "symlink",
            ] {
                let _ = std::fs::remove_file(&hint);
                let _ = std::fs::remove_dir(&hint);
                let mut bytes = expected.clone();
                match case {
                    "payload" => bytes[65537] ^= 1,
                    "padding" => *bytes.last_mut().unwrap() = 1,
                    "short" => bytes.truncate(65_535),
                    "long" => bytes.extend_from_slice(&[1; 4096]),
                    _ => {}
                }
                #[cfg(unix)]
                if case == "symlink" {
                    let external = dir.path().join("external-data");
                    std::fs::write(&external, &expected).unwrap();
                    std::os::unix::fs::symlink(external, &hint).unwrap();
                }
                if case == "directory" {
                    std::fs::create_dir(&hint).unwrap();
                } else if case != "missing" && case != "symlink" {
                    std::fs::write(&hint, bytes).unwrap();
                }
                let probe = NamedTempFile::new_in(dir.path()).unwrap();
                let can_clone = std::fs::File::open(&hint).is_ok_and(|source| {
                    PreparedDataWriter::clone_data(&source, probe.as_file()).is_ok()
                });
                if can_clone && case != "symlink" {
                    let scratch = store.scratch_dir().await.unwrap();
                    let prepared = PreparedLayer::read_into(
                        fixture.wire.as_slice(),
                        io::sink(),
                        &fixture.manifest,
                        layer,
                        &scratch,
                        Some(&hint),
                    )
                    .unwrap();
                    assert_eq!(prepared.scute.as_ref().unwrap().root, fixture.root);
                    let data = prepared.scute.as_ref().unwrap().deflated.as_ref().unwrap();
                    assert_eq!(std::fs::read(data.path()).unwrap(), expected, "{case}");
                }
                let root = PreparedLayer::prepare_cached_reusing(
                    &fixture.manifest,
                    layer,
                    &store,
                    Some(&hints),
                )
                .await
                .unwrap();
                assert_eq!(root, Some(fixture.root), "{case}");
                let files = ScuteFiles::new(&fixture.manifest, layer, &store).unwrap();
                assert_eq!(std::fs::read(&files.data).unwrap(), expected, "{case}");
                if case == "valid" {
                    std::fs::write(&hint, b"changed after verification").unwrap();
                    assert_eq!(std::fs::read(&files.data).unwrap(), expected);
                }
            }
            assert_eq!(
                std::fs::read_dir(store.scratch_dir().await.unwrap())
                    .unwrap()
                    .count(),
                0
            );
        }
    }

    #[tokio::test]
    async fn matching_data_hint_does_not_bypass_canonical_verification() {
        use pichi_storage::FilesystemBlobStore;
        for damage in ["digest", "frame"] {
            let mut fixture = Fixture::new(true, 2);
            let wrong = if damage == "frame" {
                fixture.wire = b"invalid compressed input".to_vec();
                Digest::Sha256(Sha256::digest(&fixture.wire).into())
            } else {
                Digest::Sha256(Sha256::digest(b"wrong source identity").into())
            };
            let Layer::ScuteZstd(descriptor) = &mut fixture.manifest.layers[0] else {
                unreachable!()
            };
            descriptor.digest = wrong.to_string();
            descriptor.size = fixture.wire.len() as u64;
            let dir = tempfile::tempdir().unwrap();
            let store = FilesystemBlobStore::new(dir.path().join("canonical"));
            let hints = FilesystemBlobStore::new(dir.path().join("hints"));
            store.put_blob(&wrong, &fixture.wire).await.unwrap();
            let layer = &fixture.manifest.layers[0];
            let hint = ScuteFiles::new(&fixture.manifest, layer, &hints)
                .unwrap()
                .data;
            std::fs::create_dir_all(hint.parent().unwrap()).unwrap();
            let mut expected = fixture.data.clone();
            expected.resize(8192, 0);
            std::fs::write(&hint, expected).unwrap();
            let error = PreparedLayer::prepare_cached_reusing(
                &fixture.manifest,
                layer,
                &store,
                Some(&hints),
            )
            .await
            .unwrap_err();
            let message = if damage == "frame" {
                "standard zstd frame"
            } else {
                "digest"
            };
            assert!(format!("{error:#}").contains(message), "{error:#}");
            assert!(
                !ScuteFiles::new(&fixture.manifest, layer, &store)
                    .unwrap()
                    .data
                    .exists()
            );
            assert_eq!(
                std::fs::read_dir(store.scratch_dir().await.unwrap())
                    .unwrap()
                    .count(),
                0
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
                "dev.pichi.carapace.verity.data-block-size": "4096",
                "dev.pichi.carapace.verity.hash-block-size": "4096",
                "dev.pichi.carapace.verity.version": "1",
                "dev.pichi.carapace.verity.hash-type": "1",
                "dev.pichi.carapace.verity.hash": format!("sha256:{}", hex::encode([1; 32])),
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
