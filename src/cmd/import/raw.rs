// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Stream a planned base scute into the artifact cache and its verity sidecar.
//! The source image must remain unchanged during planning and streaming.

mod manifest;

use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use carapace::image::{Plan as _, Planned};
use pichi_artifact::{Digest, Manifest, Reference};
use pichi_storage::{BlobSidecarExt, BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};
use sha2::{Digest as _, Sha256};
use tempfile::NamedTempFile;

use self::manifest::BaseManifest as _;
use crate::cmd::verity::{VerityFormat as _, VerityParams};

#[derive(Debug)]
pub(crate) struct ImportArgs {
    /// Path to the raw image file to import.
    pub raw_image: PathBuf,
    /// Tag to assign (e.g. `myapp:base`). `None` caches the carapace without
    /// tagging it. The manifest digest is printed in either case.
    pub tag: Option<String>,
    /// Optional author-supplied salt suffix bytes (already hex-decoded).
    /// `None` = use just the 32-byte zero prefix (D-01 default).
    pub salt_suffix: Option<Vec<u8>>,
    /// If true, suppress progress reporting.
    pub quiet: bool,
    /// Creation timestamp supplied by the command or test fixture.
    pub created_rfc3339: String,
    /// Extra OCI/provenance annotations to stamp on the manifest, already
    /// parsed from `KEY=VALUE`. Merged verbatim; the structural
    /// `dev.pichi.carapace.verity.*` keys always take precedence, a
    /// caller-supplied tip hash is dropped, and a caller-supplied
    /// `org.opencontainers.image.created` wins over the timestamp above.
    pub annotations: std::collections::BTreeMap<String, String>,
}

impl ImportArgs {
    pub(crate) async fn run(self, graphroot: &Path) -> Result<()> {
        let store = FilesystemBlobStore::new(graphroot);
        let scratch = store.scratch_dir().await?;
        let quiet = self.quiet;
        let staged = tokio::task::spawn_blocking(move || StagedImport::new(self, &scratch))
            .await
            .context("import staging task panicked")??;

        let path = store.blob_path(&staged.cow_digest);
        tokio::fs::create_dir_all(path.parent().context("blob path has no parent")?).await?;
        tokio::fs::rename(staged.verity.path(), path.verity_path()).await?;
        store
            .put_blob_from_path(staged.cow.path(), &staged.cow_digest)
            .await?;
        store
            .put_blob(&staged.manifest_digest, &staged.manifest_bytes)
            .await?;
        if let Some(tag) = &staged.tag {
            FilesystemTagDb::open(graphroot)?
                .set_tag(tag, &staged.manifest_digest)
                .await?;
        }
        if !quiet {
            log::info!(
                "pichi import: cached manifest {} (tag: {})",
                staged.manifest_digest,
                staged.tag.as_deref().unwrap_or("<none>")
            );
        }
        println!("{}", staged.manifest_digest);
        Ok(())
    }
}

struct StagedImport {
    cow: NamedTempFile,
    cow_digest: Digest,
    verity: NamedTempFile,
    manifest_bytes: Vec<u8>,
    manifest_digest: Digest,
    tag: Option<String>,
}

impl StagedImport {
    fn new(args: ImportArgs, scratch: &Path) -> Result<Self> {
        let tag = args
            .tag
            .as_deref()
            .map(|tag| {
                tag.parse::<Reference>()
                    .map(|r| r.to_string())
                    .with_context(|| format!("invalid tag reference: {tag}"))
            })
            .transpose()?;
        let mut salt = vec![0; 32];
        if let Some(suffix) = args.salt_suffix {
            if salt.len() + suffix.len() > 256 {
                bail!(
                    "--salt suffix too long: prefix(32) + suffix({}) > verity_sb.salt[256]",
                    suffix.len()
                );
            }
            salt.extend_from_slice(&suffix);
        }
        let image = File::open(&args.raw_image)
            .with_context(|| format!("opening input image: {}", args.raw_image.display()))?;
        if !args.quiet {
            log::info!(
                "pichi import: planning {} bytes from {}",
                image.metadata()?.len(),
                args.raw_image.display()
            );
        }
        let planned = Planned::plan(image).context("planning image-to-scute conversion")?;
        let size = planned.size();
        let exceptions = planned.exceptions();
        let cow = NamedTempFile::new_in(scratch)?;
        let mut verity = NamedTempFile::new_in(scratch)?;
        let mut reader = StoredScute {
            input: planned,
            output: BufWriter::new(cow.as_file()),
            hash: Sha256::new(),
        };
        let root = VerityParams {
            data_block_size: 4096,
            hash_block_size: 4096,
            salt: salt.clone(),
            uuid: [0; 16],
        }
        .header(size)?
        .format_padded(&mut reader, verity.as_file_mut())
        .context("streaming scute and formatting verity")?;
        let cow_digest = reader.finish()?;
        cow.as_file().sync_all()?;
        verity.as_file().sync_all()?;
        if !args.quiet {
            log::info!(
                "pichi import: cow blob {size} bytes, digest {cow_digest} ({exceptions} exceptions)"
            );
            log::info!("pichi import: verity root {}", hex::encode(root));
        }
        let manifest = Manifest::base(
            &cow_digest,
            size,
            &salt,
            &args.created_rfc3339,
            &args.annotations,
        )?;
        let manifest_bytes = serde_json::to_vec(&manifest)?;
        let manifest_digest =
            Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&manifest_bytes).into());
        Ok(Self {
            cow,
            cow_digest,
            verity,
            manifest_bytes,
            manifest_digest,
            tag,
        })
    }
}

/// Persist and content-hash exactly the COW bytes consumed by the formatter.
struct StoredScute<R, W> {
    input: R,
    output: W,
    hash: Sha256,
}

impl<R, W: Write> StoredScute<R, W> {
    fn finish(mut self) -> io::Result<Digest> {
        self.output.flush()?;
        Ok(Digest::Sha256(self.hash.finalize().into()))
    }
}

impl<R: Read, W: Write> Read for StoredScute<R, W> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let count = self.input.read(output)?;
        self.output.write_all(&output[..count])?;
        self.hash.update(&output[..count]);
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_args(tmp: &TempDir, raw_image: PathBuf, tag: &str) -> (ImportArgs, PathBuf) {
        let graphroot = tmp.path().join("storage");
        std::fs::create_dir_all(&graphroot).unwrap();
        let args = ImportArgs {
            raw_image,
            tag: Some(tag.to_string()),
            salt_suffix: None,
            quiet: true,
            created_rfc3339: "2026-05-07T12:00:00Z".to_string(),
            annotations: std::collections::BTreeMap::new(),
        };
        (args, graphroot)
    }

    #[tokio::test]
    async fn run_writes_three_blobs_and_a_tag() {
        let tmp = TempDir::new().unwrap();
        let raw = tmp.path().join("input.raw");
        let mut data = vec![0u8; 64 * 1024]; // 4 chunks at chunk_size=32 sectors
        data[16384] = 0xCC; // chunk 1 non-zero
        std::fs::write(&raw, &data).unwrap();
        let (args, graphroot) = make_args(&tmp, raw, "myapp:base");

        args.run(&graphroot).await.unwrap();

        // Three blobs in <graphroot>/blobs/sha256/.
        let blobs_dir = graphroot.join("blobs").join("sha256");
        let entries: Vec<_> = std::fs::read_dir(&blobs_dir).unwrap().collect();
        assert_eq!(
            entries.len(),
            3,
            "expected exactly 3 blobs (cow, verity, manifest)"
        );

        let verity_path = std::fs::read_dir(&blobs_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "verity"))
            .unwrap();
        let bytes = std::fs::read(verity_path).unwrap();
        let header = devmap_verity::Header::decode(bytes[..512].try_into().unwrap()).unwrap();
        assert_eq!(header.uuid, [0; 16]);

        // Tag resolves (stored under canonical form after Reference::from_str normalization).
        let db = FilesystemTagDb::open(&graphroot).unwrap();
        let canonical_tag = "myapp:base".parse::<Reference>().unwrap().to_string();
        let resolved = db.resolve_tag(&canonical_tag).await.unwrap();
        assert!(
            resolved.is_some(),
            "tag must resolve (canonical: {canonical_tag})"
        );
    }

    #[tokio::test]
    async fn imports_at_the_carapace_scute_chunk_size() {
        // Carapaces are fixed at the spec-whitelisted chunk size; import is
        // no longer tunable and must emit exactly that value.
        let tmp = TempDir::new().unwrap();
        let raw = tmp.path().join("input.raw");
        let mut data = vec![0u8; 4096 * 3];
        data[5000] = 0xCC;
        std::fs::write(&raw, &data).unwrap();
        let (args, graphroot) = make_args(&tmp, raw, "x:y");
        args.run(&graphroot).await.unwrap();

        // The emitted cow blob's header records the chunk size; it must be 8.
        let blobs = graphroot.join("blobs").join("sha256");
        let cow = std::fs::read_dir(&blobs)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_none()) // skip .verity sidecar
            .map(|p| std::fs::read(&p).unwrap())
            .find(|b| b.len() >= 16 && &b[0..4] == b"SnAp") // dm-snapshot cow magic
            .expect("a cow blob");
        let chunk = u32::from_le_bytes(cow[12..16].try_into().unwrap());
        assert_eq!(chunk, 8);
    }
    /// Fixed digests/roots from the retired writer guard artifact identity,
    /// including zero images and the 256-exception metadata-area boundary.
    #[test]
    fn preserves_legacy_cow_bytes_and_roots() {
        for (count, size, digest, root) in [
            (
                0,
                8192,
                "sha256:ffa6fc16b82eaf8b962c4ea67cb989e04be1e2f527fb73ba7f63cfb5fc76ea7a",
                "8abcc8d290da4ad93311ce34758839f34b864af3166feb9ddbd8c0f51180a921",
            ),
            (
                1,
                12288,
                "sha256:a290263bd73e5826b543316e1d8aebc8456e7f1b8331394b7d09bd5b396a02ee",
                "bcf73c1988ddb8cfb6d44c34300a7102bfd0f58e881b64612be23c10906d66dd",
            ),
            (
                255,
                1_052_672,
                "sha256:bf7b5e83272a3dc7f03e0f9604ff10fb87110095898b8b9ae9d8c90149380a75",
                "d4e60bca08ef9b477a1013d6a9313f3c3dc57c777d6d50cf372fe60132d95c26",
            ),
            (
                256,
                1_060_864,
                "sha256:939b549f09d5cb99f0d2f58ec02dccd6fe3af96689fd31ded2c6a489bc75b581",
                "50836612b1c7db8735bcc3dbefd369ae5c12b41d42f3492fc3bf578d8f4ef60f",
            ),
            (
                257,
                1_064_960,
                "sha256:170c0ecc98f722518545ccfcda97a395a03c1c55757f54d953a7f6b4e80e630d",
                "29072e536956b3b4360861a2f743a243f0165d8a952a0663f36b62ae4ffe4c90",
            ),
            (
                512,
                2_113_536,
                "sha256:3694f1f9c62a23bb2fff1652c3d27a8ea2e650311bccb844fecb75d8e7893d3e",
                "367ed7f2ab3275c392782ca5fa00951f114a1c6e4aa8bdcbca461f9419809805",
            ),
        ] {
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join("image.raw");
            let mut data = vec![0; count * 4096 + 123];
            data[..count * 4096].fill(0x5a);
            std::fs::write(&path, data).unwrap();
            let (args, scratch) = make_args(&tmp, path, "base:1");
            let staged = StagedImport::new(args, &scratch).unwrap();
            assert_eq!(staged.cow_digest.to_string(), digest);
            assert_eq!(staged.cow.as_file().metadata().unwrap().len(), size);
            let bytes = std::fs::read(staged.cow.path()).unwrap();
            assert_eq!(
                Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&bytes).into()),
                staged.cow_digest
            );
            let manifest: Manifest = serde_json::from_slice(&staged.manifest_bytes).unwrap();
            assert!(manifest.carapace_verity_hash().is_none());
            let pichi_artifact::Layer::Scute(scute) = &manifest.layers[0] else {
                panic!("expected scute")
            };
            assert_eq!(scute.annotations.data_blocks, Some(size / 4096));
            let formatted = VerityParams {
                data_block_size: 4096,
                hash_block_size: 4096,
                salt: vec![0; 32],
                uuid: [0; 16],
            }
            .compute(&bytes)
            .unwrap();
            assert_eq!(hex::encode(formatted.root_hash), root);
            assert_eq!(std::fs::read(staged.verity.path()).unwrap(), formatted.blob);
            drop(staged);
            assert_eq!(std::fs::read_dir(scratch).unwrap().count(), 0);
        }
    }

    #[test]
    fn preserves_partial_nonzero_chunk() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("image.raw");
        std::fs::write(&path, vec![0x5a; 4097]).unwrap();
        let (args, scratch) = make_args(&tmp, path, "base:1");
        let staged = StagedImport::new(args, &scratch).unwrap();
        assert_eq!(
            staged.cow_digest.to_string(),
            "sha256:e912b9b25375cf235229c113b88eb7fcc038073c703696c4904f1712fa193823"
        );
        let manifest: Manifest = serde_json::from_slice(&staged.manifest_bytes).unwrap();
        assert!(manifest.carapace_verity_hash().is_none());
        let bytes = std::fs::read(staged.cow.path()).unwrap();
        let formatted = VerityParams {
            data_block_size: 4096,
            hash_block_size: 4096,
            salt: vec![0; 32],
            uuid: [0; 16],
        }
        .compute(&bytes)
        .unwrap();
        assert_eq!(
            hex::encode(formatted.root_hash),
            "4c7a21cbde4adb55ded7748c0d36471fe6f30942890580ac3d2a498719e7838e"
        );
    }

    #[test]
    fn sparse_and_dense_sources_produce_identical_artifacts() {
        use std::io::{Seek as _, SeekFrom};
        let tmp = TempDir::new().unwrap();
        let sparse_path = tmp.path().join("sparse.raw");
        let dense_path = tmp.path().join("dense.raw");
        let mut sparse = File::create(&sparse_path).unwrap();
        sparse.set_len(1024 * 1024 + 123).unwrap();
        sparse.seek(SeekFrom::Start(4096)).unwrap();
        sparse.write_all(&[0x5a; 4096]).unwrap();
        drop(sparse);
        std::fs::write(&dense_path, std::fs::read(&sparse_path).unwrap()).unwrap();
        let (args, scratch) = make_args(&tmp, sparse_path, "base:1");
        let sparse = StagedImport::new(args, &scratch).unwrap();
        let (args, _) = make_args(&tmp, dense_path, "base:1");
        let dense = StagedImport::new(args, &scratch).unwrap();
        assert_eq!(sparse.manifest_bytes, dense.manifest_bytes);
        assert_eq!(
            std::fs::read(sparse.cow.path()).unwrap(),
            std::fs::read(dense.cow.path()).unwrap()
        );
        assert_eq!(
            std::fs::read(sparse.verity.path()).unwrap(),
            std::fs::read(dense.verity.path()).unwrap()
        );
    }

    #[tokio::test]
    async fn invalid_sources_do_not_publish_a_tag() {
        // Unix truncation creates a sparse file without allocating 64 GiB.
        #[cfg(unix)]
        let sizes = [0, 64 * 1024 * 1024 * 1024 + 1];
        #[cfg(not(unix))]
        let sizes = [0];
        for size in sizes {
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join("invalid.raw");
            File::create(&path).unwrap().set_len(size).unwrap();
            let (args, graphroot) = make_args(&tmp, path, "base:1");
            assert!(args.run(&graphroot).await.is_err());
            let db = FilesystemTagDb::open(&graphroot).unwrap();
            let tag = "base:1".parse::<Reference>().unwrap().to_string();
            assert!(db.resolve_tag(&tag).await.unwrap().is_none());
            let scratch = FilesystemBlobStore::new(&graphroot)
                .scratch_dir()
                .await
                .unwrap();
            assert_eq!(std::fs::read_dir(scratch).unwrap().count(), 0);
        }
    }
}
