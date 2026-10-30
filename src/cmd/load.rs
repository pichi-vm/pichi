// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Import complete OCI layouts into the cache. Build and load share this
//! boundary: snapshot and verify guest bytes, rebuild verity, then publish tags.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use cap_std::fs::Dir;
use pichi_artifact::{Digest, Manifest, Reference};
use pichi_storage::{BlobSidecarExt, BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::cli::LoadArgs;
use crate::cmd::layer::PreparedLayer;
use crate::cmd::manifest_ext::{ManifestExt as _, ScuteRoots};
use crate::config::Config;

const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const INDEX_TYPE: &str = "application/vnd.oci.image.index.v1+json";
const REF_NAME_ANNOTATION: &str = "org.opencontainers.image.ref.name";
const METADATA_CAP: u64 = 1024 * 1024;
const FILE_CAP: usize = 4096;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LayoutMarker {
    image_layout_version: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LayoutIndex {
    schema_version: u32,
    media_type: String,
    manifests: Vec<LayoutEntry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LayoutEntry {
    media_type: String,
    artifact_type: Option<String>,
    digest: String,
    size: u64,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
}

struct ImportedManifest {
    digest: Digest,
    manifest: Manifest,
    tag: Option<String>,
}

/// Owns a private snapshot. No guest path or cache sidecar is authoritative.
/// The VM must have exited before build calls `read`.
pub(super) struct OciLayout {
    capacity: u64,
    staging: tempfile::TempDir,
    store: FilesystemBlobStore,
    manifests: Vec<ImportedManifest>,
    blobs: HashMap<Digest, u64>,
    guest_verity: Vec<(Digest, PathBuf)>,
}

impl OciLayout {
    pub(super) async fn read(dir: &Path, destination: &FilesystemBlobStore) -> Result<Self> {
        let scratch = destination.scratch_dir().await?;
        let capacity = fs4::available_space(&scratch)?;
        Self::read_with_capacity(dir, destination, capacity).await
    }

    async fn read_with_capacity(
        dir: &Path,
        destination: &FilesystemBlobStore,
        capacity: u64,
    ) -> Result<Self> {
        let staging = tempfile::tempdir_in(destination.scratch_dir().await?)?;
        let dir = dir.to_owned();
        let layout = tokio::task::spawn_blocking(move || Self::snapshot(&dir, staging, capacity))
            .await
            .context("OCI snapshot task panicked")?
            .context("reading complete OCI output layout")?;
        // The snapshot contains only OCI blobs. No pre-existing sidecars can
        // short-circuit rebuilding: every scute is rebuilt from cow + salt.
        let mut scutes = HashMap::new();
        let mut expanded_size = 0u64;
        for imported in &layout.manifests {
            let mut roots = ScuteRoots::default();
            for layer in &imported.manifest.layers {
                if let Some(root) =
                    PreparedLayer::prepare_cached(&imported.manifest, layer, &layout.store).await?
                {
                    let digest: Digest = layer.digest_str().parse()?;
                    if let Some(previous) = scutes.insert(digest, root) {
                        ensure!(
                            previous == root,
                            "conflicting verity parameters for the same COW"
                        );
                    }
                    let path = layout.store.blob_path(&layer.digest_str().parse()?);
                    let size = if matches!(layer, pichi_artifact::Layer::ScuteZstd(_)) {
                        std::fs::metadata(path.deflated_path())?.len()
                    } else {
                        layer.size()
                    };
                    expanded_size = expanded_size
                        .checked_add(size)
                        .context("expanded OCI size overflow")?;
                    ensure!(
                        expanded_size <= layout.capacity,
                        "expanded OCI scutes exceed scratch capacity"
                    );
                    roots.add(layer, root)?;
                }
            }
            roots
                .verify_complete(&imported.manifest)
                .context("invalid carapace tip")?;
        }
        layout.check_guest_verity()?;
        Ok(layout)
    }

    fn snapshot(dir: &Path, staging: tempfile::TempDir, capacity: u64) -> Result<Self> {
        ensure!(
            std::fs::symlink_metadata(dir)?.file_type().is_dir(),
            "OCI output must be a directory, not a symlink"
        );
        let root = Dir::open_ambient_dir(dir, cap_std::ambient_authority())?;
        let mut files = BTreeMap::new();
        Self::scan(&root, Path::new(""), &mut files, &mut 0, capacity)?;
        let marker: LayoutMarker =
            serde_json::from_slice(&Self::small(&root, Path::new("oci-layout"), 4096)?)?;
        ensure!(
            marker.image_layout_version == "1.0.0",
            "unsupported OCI layout version"
        );
        let index: LayoutIndex =
            serde_json::from_slice(&Self::small(&root, Path::new("index.json"), METADATA_CAP)?)?;
        ensure!(
            index.schema_version == 2 && index.media_type == INDEX_TYPE,
            "invalid OCI index schema or media type"
        );
        ensure!(!index.manifests.is_empty(), "OCI index has no manifests");
        let store = FilesystemBlobStore::new(staging.path());
        std::fs::create_dir_all(staging.path().join("blobs/sha256"))?;
        let mut layout = Self {
            capacity,
            staging,
            store,
            manifests: Vec::new(),
            blobs: HashMap::new(),
            guest_verity: Vec::new(),
        };
        let mut expected: HashSet<PathBuf> = ["oci-layout", "index.json"]
            .into_iter()
            .map(PathBuf::from)
            .collect();
        let mut seen = HashSet::new();
        for entry in index.manifests {
            ensure!(
                entry.media_type == MANIFEST_TYPE,
                "unsupported index entry media type"
            );
            ensure!(
                entry
                    .artifact_type
                    .as_deref()
                    .is_none_or(|t| t == pichi_artifact::MEDIA_TYPE_PICHI_ARTIFACT_V1),
                "invalid index artifact type"
            );
            ensure!(entry.size <= METADATA_CAP, "manifest exceeds metadata cap");
            let digest = layout.copy_blob(&root, &entry.digest, entry.size, &mut expected)?;
            ensure!(seen.insert(digest.clone()), "duplicate OCI manifest");
            let bytes = std::fs::read(layout.store.blob_path(&digest))?;
            let manifest =
                Manifest::decode_json(bytes.as_slice()).context("invalid OCI manifest")?;
            ensure!(
                manifest.schema_version == 2 && manifest.media_type == MANIFEST_TYPE,
                "invalid manifest schema or media type"
            );
            ensure!(!manifest.layers.is_empty(), "OCI artifact has no layers");
            let config = &manifest.config;
            ensure!(config.size <= METADATA_CAP, "config exceeds metadata cap");
            if !config.is_empty() || files.contains_key(&Self::blob_name(&config.digest)?) {
                ensure!(
                    config.is_empty() || config.data.is_none(),
                    "inline pichi config is unsupported"
                );
                let digest = layout.copy_blob(&root, &config.digest, config.size, &mut expected)?;
                if !config.is_empty() {
                    let config: pichi_artifact::Config =
                        serde_json::from_slice(&std::fs::read(layout.store.blob_path(&digest))?)
                            .context("invalid launch config")?;
                    config.validate().context("invalid launch config")?;
                }
            }
            if manifest.layers.iter().any(|l| {
                matches!(
                    l,
                    pichi_artifact::Layer::Scute(_) | pichi_artifact::Layer::ScuteZstd(_)
                )
            }) {
                for key in [
                    "dev.pichi.carapace.verity.data-block-size",
                    "dev.pichi.carapace.verity.hash-block-size",
                ] {
                    ensure!(
                        manifest.annotations.get(key).map(String::as_str) == Some("4096"),
                        "carapace block sizes must be 4096"
                    );
                }
            }
            let mut layers = HashSet::new();
            for layer in &manifest.layers {
                ensure!(layers.insert(layer.digest_str()), "duplicate OCI layer");
                layout.copy_blob(&root, layer.digest_str(), layer.size(), &mut expected)?;
                if matches!(
                    layer,
                    pichi_artifact::Layer::Scute(_) | pichi_artifact::Layer::ScuteZstd(_)
                ) {
                    ensure!(
                        manifest
                            .annotations
                            .get("dev.pichi.carapace.verity.hash-type")
                            .map(String::as_str)
                            == Some("1"),
                        "carapace hash type must be 1"
                    );
                    // Optional guest sidecars have only digest-derived names.
                    // They are compared with the fresh tree, never imported.
                    let sidecar = Self::blob_name(layer.digest_str())?.with_extension("verity");
                    if let Some(size) = files.get(&sidecar) {
                        layout.copy_guest_verity(
                            &root,
                            &sidecar,
                            *size,
                            layer.digest_str().parse()?,
                        )?;
                        expected.insert(sidecar);
                    }
                }
            }
            layout.manifests.push(ImportedManifest {
                digest,
                manifest,
                tag: entry.annotations.get(REF_NAME_ANNOTATION).cloned(),
            });
        }
        for path in files.keys() {
            ensure!(
                expected.contains(path),
                "unexpected OCI file: {}",
                path.display()
            );
        }
        Ok(layout)
    }

    /// Only known directories and regular bounded files; never recurse through
    /// symlinks. Dir keeps all subsequent opens confined to the output root.
    fn scan(
        root: &Dir,
        path: &Path,
        files: &mut BTreeMap<PathBuf, u64>,
        total: &mut u64,
        capacity: u64,
    ) -> Result<()> {
        for entry in root.read_dir(if path.as_os_str().is_empty() {
            Path::new(".")
        } else {
            path
        })? {
            let entry = entry?;
            let name = path.join(entry.file_name());
            let metadata = root.symlink_metadata(&name)?;
            if metadata.is_dir() {
                ensure!(
                    name == Path::new("blobs") || name == Path::new("blobs/sha256"),
                    "unexpected OCI directory: {}",
                    name.display()
                );
                Self::scan(root, &name, files, total, capacity)?;
                continue;
            }
            ensure!(
                metadata.is_file(),
                "OCI file must be regular (no symlinks or special files): {}",
                name.display()
            );
            #[cfg(unix)]
            {
                use cap_std::fs::MetadataExt as _;
                ensure!(metadata.nlink() == 1, "OCI hard links are not allowed");
            }
            let cap = match name.to_str() {
                Some("oci-layout") => 4096,
                Some("index.json") => METADATA_CAP,
                _ if name.parent() == Some(Path::new("blobs/sha256")) => capacity,
                _ => bail!("unexpected OCI file: {}", name.display()),
            };
            ensure!(
                metadata.len() <= cap,
                "OCI file exceeds size cap: {}",
                name.display()
            );
            *total = total
                .checked_add(metadata.len())
                .context("OCI size overflow")?;
            ensure!(
                *total <= capacity && files.len() < FILE_CAP,
                "OCI layout exceeds size or file count cap"
            );
            files.insert(name, metadata.len());
        }
        Ok(())
    }

    fn small(root: &Dir, path: &Path, cap: u64) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        root.open(path)?.take(cap + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() as u64 <= cap, "OCI metadata exceeds cap");
        Ok(bytes)
    }

    fn blob_name(value: &str) -> Result<PathBuf> {
        let digest: Digest = value.parse().context("invalid OCI digest")?;
        ensure!(
            value == digest.to_string(),
            "OCI digest must use lowercase sha256 hex"
        );
        Ok(Path::new("blobs/sha256").join(digest.hex()))
    }

    fn copy_blob(
        &mut self,
        root: &Dir,
        value: &str,
        size: u64,
        expected: &mut HashSet<PathBuf>,
    ) -> Result<Digest> {
        ensure!(size <= self.capacity, "OCI blob exceeds scratch capacity");
        let path = Self::blob_name(value)?;
        let digest: Digest = value.parse()?;
        expected.insert(path.clone());
        if let Some(previous) = self.blobs.get(&digest) {
            ensure!(*previous == size, "conflicting OCI descriptor sizes");
            return Ok(digest);
        }
        let output = File::create(self.store.blob_path(&digest))?;
        ensure!(
            Self::copy_content(root, &path, size, output)? == digest,
            "OCI blob digest mismatch: {digest}"
        );
        self.blobs.insert(digest.clone(), size);
        Ok(digest)
    }

    fn copy_content(
        root: &Dir,
        path: &Path,
        size: u64,
        mut output: impl std::io::Write,
    ) -> Result<Digest> {
        let mut input = root
            .open(path)
            .with_context(|| format!("read OCI file {}", path.display()))?;
        ensure!(
            input.metadata()?.is_file() && input.metadata()?.len() == size,
            "OCI file size mismatch: {}",
            path.display()
        );
        let mut hash = Sha256::new();
        let mut count = 0u64;
        let mut buffer = [0; 64 * 1024];
        loop {
            let length = input.read(&mut buffer)?;
            if length == 0 {
                break;
            }
            count = count
                .checked_add(length as u64)
                .context("OCI copy size overflow")?;
            ensure!(count <= size, "OCI file exceeds declared size");
            hash.update(&buffer[..length]);
            output.write_all(&buffer[..length])?;
        }
        ensure!(count == size, "truncated OCI file");
        Ok(Digest::Sha256(hash.finalize().into()))
    }

    fn copy_guest_verity(
        &mut self,
        root: &Dir,
        name: &Path,
        size: u64,
        digest: Digest,
    ) -> Result<()> {
        ensure!(
            size <= self.capacity,
            "guest verity exceeds scratch capacity"
        );
        let target = self
            .staging
            .path()
            .join(format!("guest-verity-{}", self.guest_verity.len()));
        Self::copy_content(root, name, size, File::create(&target)?)?;
        self.guest_verity.push((digest, target));
        Ok(())
    }

    fn check_guest_verity(&self) -> Result<()> {
        for (digest, path) in &self.guest_verity {
            let mut guest = File::open(path)?;
            let mut rebuilt = File::open(self.store.blob_path(digest).verity_path())?;
            ensure!(
                guest.metadata()?.len() == rebuilt.metadata()?.len(),
                "guest verity size mismatch"
            );
            let mut a = [0; 64 * 1024];
            let mut b = [0; 64 * 1024];
            loop {
                let count = guest.read(&mut a)?;
                if count == 0 {
                    break;
                }
                rebuilt.read_exact(&mut b[..count])?;
                ensure!(
                    a[..count] == b[..count],
                    "guest verity does not match cow + salt"
                );
            }
        }
        Ok(())
    }

    pub(super) async fn import_build(
        self,
        store: &FilesystemBlobStore,
        db: &FilesystemTagDb,
        tag: &str,
    ) -> Result<Digest> {
        ensure!(
            self.manifests.len() == 1,
            "build OCI index must contain exactly one manifest"
        );
        let digest = self.manifests[0].digest.clone();
        let tag = tag
            .parse::<Reference>()
            .with_context(|| format!("invalid tag {tag}"))?
            .to_string();
        self.commit(store).await?;
        db.set_tag(&tag, &digest).await?;
        Ok(digest)
    }

    async fn commit(&self, store: &FilesystemBlobStore) -> Result<()> {
        // Only fully validated private bytes reach the cache. Rename sidecars
        // even on cache hits, so old sidecars cannot bypass the fresh rebuild.
        for digest in self.blobs.keys() {
            let source = self.store.blob_path(digest);
            let target = store.blob_path(digest);
            tokio::fs::create_dir_all(target.parent().context("blob has no parent")?).await?;
            for (src, dst) in [
                (source.verity_path(), target.verity_path()),
                (source.deflated_path(), target.deflated_path()),
            ] {
                if src.exists() {
                    tokio::fs::rename(src, dst).await?;
                }
            }
            store.put_blob_from_path(&source, digest).await?;
        }
        Ok(())
    }
}

pub async fn run(args: LoadArgs, config: &Config) -> Result<()> {
    let layout = config.resolve_layout()?;
    let store = FilesystemBlobStore::new(&layout.graphroot);
    let db = FilesystemTagDb::open(&layout.graphroot)?;
    let imported = OciLayout::read(&args.input, &store).await?;
    let mut tags = BTreeMap::new();
    for entry in &imported.manifests {
        if let Some(tag) = args.tag.as_ref().or(entry.tag.as_ref()) {
            let tag = tag
                .parse::<Reference>()
                .with_context(|| format!("invalid tag {tag}"))?
                .to_string();
            ensure!(
                tags.insert(tag, entry.digest.clone()).is_none(),
                "duplicate OCI tag"
            );
        }
    }
    imported.commit(&store).await?;
    for entry in &imported.manifests {
        if args.tag.is_none() && entry.tag.is_none() {
            println!("loaded {} (untagged)", entry.digest);
        }
    }
    for (tag, digest) in tags {
        db.set_tag(&tag, &digest).await?;
        println!("loaded {tag}");
    }
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::cmd::test_support::ScuteFixture as _;
    use crate::cmd::verity::VerityParams;
    use serde_json::{Value, json};

    const TEST_CAPACITY: u64 = 4 * 1024 * 1024;

    pub(crate) struct LayoutFixture {
        manifest: Value,
        blobs: Vec<Vec<u8>>,
        trees: Vec<Vec<u8>>,
        tip: [u8; 32],
    }

    impl LayoutFixture {
        pub(crate) fn new() -> Self {
            let mut fixture = Self {
                manifest: json!({
                    "schemaVersion": 2,
                    "mediaType": MANIFEST_TYPE,
                    "artifactType": pichi_artifact::MEDIA_TYPE_PICHI_ARTIFACT_V1,
                    "config": pichi_artifact::ConfigDescriptor::canonical(),
                    "layers": [],
                    "annotations": {
                        "dev.pichi.carapace.verity.algo": "sha256",
                        "dev.pichi.carapace.verity.data-block-size": "4096",
                        "dev.pichi.carapace.verity.hash-block-size": "4096",
                        "dev.pichi.carapace.verity.version": "1",
                        "dev.pichi.carapace.verity.hash-type": "1",
                    },
                }),
                blobs: Vec::new(),
                trees: Vec::new(),
                tip: [0; 32],
            };
            fixture.add_scute(false);
            fixture
        }

        fn add_scute(&mut self, compressed: bool) {
            let cow = vec![self.blobs.len() as u8 + 1; 8192].scute();
            let output = VerityParams {
                data_block_size: 4096,
                hash_block_size: 4096,
                salt: self.tip.to_vec(),
                uuid: [0; 16],
            }
            .compute(&cow)
            .unwrap();
            let wire = if compressed {
                ruzstd::encoding::compress_to_vec(
                    cow.as_slice(),
                    ruzstd::encoding::CompressionLevel::Fastest,
                )
            } else {
                cow.clone()
            };
            self.manifest["layers"].as_array_mut().unwrap().push(json!({
                "mediaType": if compressed { pichi_artifact::MEDIA_TYPE_PICHI_SCUTE_V1_ZSTD } else { pichi_artifact::MEDIA_TYPE_PICHI_SCUTE_V1 },
                "digest": Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&wire).into()).to_string(),
                "size": wire.len(),
                "annotations": {
                    "dev.pichi.scute.verity.salt": hex::encode(self.tip),
                    "dev.pichi.scute.verity.data-blocks": (cow.len() as u64).div_ceil(4096).to_string(),
                },
            }));
            self.blobs.push(wire);
            self.trees.push(output.blob);
            self.tip = output.root_hash;
        }

        pub(crate) fn write(&self, dir: &Path) -> Digest {
            std::fs::create_dir_all(dir.join("blobs/sha256")).unwrap();
            for blob in &self.blobs {
                std::fs::write(
                    dir.join("blobs/sha256").join(
                        Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(blob).into()).hex(),
                    ),
                    blob,
                )
                .unwrap();
            }
            let bytes = serde_json::to_vec(&self.manifest).unwrap();
            let digest = Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&bytes).into());
            std::fs::write(dir.join("blobs/sha256").join(digest.hex()), &bytes).unwrap();
            std::fs::write(dir.join("oci-layout"), br#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();
            std::fs::write(
                dir.join("index.json"),
                serde_json::to_vec(&json!({
                    "schemaVersion": 2, "mediaType": INDEX_TYPE,
                    "manifests": [{
                        "mediaType": MANIFEST_TYPE,
                        "artifactType": pichi_artifact::MEDIA_TYPE_PICHI_ARTIFACT_V1,
                        "digest": digest.to_string(), "size": bytes.len(),
                        "annotations": { REF_NAME_ANNOTATION: "guest:ignored" },
                    }],
                }))
                .unwrap(),
            )
            .unwrap();
            digest
        }

        fn sidecars(&self, dir: &Path) {
            for (blob, tree) in self.blobs.iter().zip(&self.trees) {
                let path = dir.join("blobs/sha256").join(
                    Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(blob).into()).hex(),
                );
                std::fs::write(path.verity_path(), tree).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn imports_guest_manifest_verbatim_and_rebuilds_on_cache_hits() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("output");
        let store = FilesystemBlobStore::new(dir.path().join("cache"));
        let db = FilesystemTagDb::open(dir.path().join("cache")).unwrap();
        let mut fixture = LayoutFixture::new();
        fixture.add_scute(false);
        fixture.add_scute(true);
        // Descriptor order has no bearing on the salt graph.
        fixture.manifest["layers"].as_array_mut().unwrap().reverse();
        let digest = fixture.write(&output);
        fixture.sidecars(&output);
        for _ in 0..2 {
            let imported = OciLayout::read(&output, &store).await.unwrap();
            assert_eq!(
                imported
                    .import_build(&store, &db, "built:v1")
                    .await
                    .unwrap(),
                digest
            );
            let key = "built:v1".parse::<Reference>().unwrap().to_string();
            assert_eq!(db.resolve_tag(&key).await.unwrap(), Some(digest.clone()));
            assert!(
                db.resolve_tag(&"guest:ignored".parse::<Reference>().unwrap().to_string())
                    .await
                    .unwrap()
                    .is_none()
            );
            let bytes = store.get_blob(&digest).await.unwrap();
            assert_eq!(bytes, serde_json::to_vec(&fixture.manifest).unwrap());
            let manifest = Manifest::decode_json(&bytes[..]).unwrap();
            assert_eq!(
                manifest.carapace_top_root(&store).await.unwrap(),
                fixture.tip
            );
            assert!(manifest.carapace_verity_hash().is_none());
            for (blob, tree) in fixture.blobs.iter().zip(&fixture.trees) {
                let path = store
                    .blob_path(&Digest::Sha256(
                        <sha2::Sha256 as sha2::Digest>::digest(blob).into(),
                    ))
                    .verity_path();
                assert_eq!(std::fs::read(&path).unwrap(), *tree);
                std::fs::write(path, b"stale cache tree").unwrap();
            }
        }
    }

    #[tokio::test]
    async fn rejects_retired_output_files() {
        for name in [
            "build.yaml",
            "inventory.cbor",
            "0000.cow",
            "0000.verity",
            "boot.pmi",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let output = dir.path().join("output");
            let store = FilesystemBlobStore::new(dir.path().join("cache"));
            LayoutFixture::new().write(&output);
            std::fs::write(output.join(name), b"retired output").unwrap();
            let error = OciLayout::read(&output, &store).await.err().unwrap();
            assert!(
                format!("{error:#}").contains("unexpected OCI file"),
                "{name}: {error:#}"
            );
        }
    }

    #[tokio::test]
    async fn rejects_bad_or_incomplete_layout_before_publishing() {
        for case in [
            "missing marker",
            "bad marker",
            "bad index",
            "empty index",
            "index schema",
            "index type",
            "entry type",
            "entry artifact",
            "entry digest",
            "entry size",
            "missing manifest",
            "manifest schema",
            "manifest type",
            "artifact type",
            "empty layers",
            "layer type",
            "layer digest",
            "layer size",
            "missing cow",
            "tampered cow",
            "config type",
            "missing config",
            "bad config",
            "inline config",
            "salt",
            "disconnected salt",
            "fork",
            "duplicate layer",
            "duplicate manifest",
            "bad blocks",
            "bad geometry",
            "unexpected file",
            "unreferenced blob",
            "short salt",
            "missing base",
            "salt mismatch",
            "hash type",
            "oversized hash block",
            "guest verity",
            "truncated verity",
            "oversized metadata",
            "oversized blob",
            "oversized total",
            "too many files",
            "unexpected directory",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let output = dir.path().join("output");
            let store = FilesystemBlobStore::new(dir.path().join("cache"));
            let db = FilesystemTagDb::open(dir.path().join("cache")).unwrap();
            let key = "built:v1".parse::<Reference>().unwrap().to_string();
            let old =
                Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(b"existing tag").into());
            db.set_tag(&key, &old).await.unwrap();
            let mut fixture = LayoutFixture::new();
            match case {
                "manifest schema" => fixture.manifest["schemaVersion"] = 1.into(),
                "manifest type" => fixture.manifest["mediaType"] = INDEX_TYPE.into(),
                "artifact type" => fixture.manifest["artifactType"] = "text/plain".into(),
                "empty layers" => {
                    fixture.manifest["layers"] = json!([]);
                    fixture.blobs.clear();
                }
                "layer type" => fixture.manifest["layers"][0]["mediaType"] = "text/plain".into(),
                "layer digest" => {
                    fixture.manifest["layers"][0]["digest"] = "sha256:../../escape".into()
                }
                "layer size" => fixture.manifest["layers"][0]["size"] = 1.into(),
                "config type" => fixture.manifest["config"]["mediaType"] = "text/plain".into(),
                "missing config" | "bad config" | "inline config" => {
                    fixture.manifest["config"] = json!({
                        "mediaType": pichi_artifact::MEDIA_TYPE_PICHI_CONFIG_V1,
                        "digest": Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(b"bad config").into()).to_string(),
                        "size": 10,
                    });
                    if case != "missing config" {
                        fixture.blobs.push(b"bad config".to_vec());
                    }
                    if case == "inline config" {
                        fixture.manifest["config"]["data"] = "e30=".into();
                    }
                }
                "short salt" => {
                    fixture.manifest["layers"][0]["annotations"]["dev.pichi.scute.verity.salt"] =
                        "00".into()
                }
                "missing base" => {
                    fixture.manifest["layers"][0]["annotations"]["dev.pichi.scute.verity.salt"] =
                        hex::encode([42; 32]).into()
                }
                "salt mismatch" => {
                    fixture.manifest["layers"][0]["annotations"]["dev.pichi.scute.verity.salt"] =
                        hex::encode([0; 64]).into()
                }
                "hash type" => {
                    fixture.manifest["annotations"]["dev.pichi.carapace.verity.hash-type"] =
                        "0".into()
                }
                "oversized hash block" => {
                    fixture.manifest["annotations"]["dev.pichi.carapace.verity.hash-block-size"] =
                        "2147483648".into()
                }
                "salt" => {
                    fixture.manifest["layers"][0]["annotations"]["dev.pichi.scute.verity.salt"] =
                        "zz".into()
                }
                "disconnected salt" => {
                    fixture.add_scute(false);
                    fixture.manifest["layers"][1]["annotations"]["dev.pichi.scute.verity.salt"] =
                        hex::encode([42; 32]).into();
                }
                "fork" => {
                    fixture.tip = [0; 32];
                    fixture.add_scute(false);
                }
                "duplicate layer" => {
                    let layer = fixture.manifest["layers"][0].clone();
                    fixture.manifest["layers"]
                        .as_array_mut()
                        .unwrap()
                        .push(layer);
                }
                "bad blocks" => {
                    fixture.manifest["layers"][0]["annotations"]["dev.pichi.scute.verity.data-blocks"] =
                        "0".into()
                }
                "bad geometry" => {
                    fixture.manifest["annotations"]["dev.pichi.carapace.verity.data-block-size"] =
                        "3".into()
                }
                _ => {}
            }
            let digest = fixture.write(&output);
            let cow = output.join("blobs/sha256").join(
                Digest::Sha256(
                    <sha2::Sha256 as sha2::Digest>::digest(
                        fixture.blobs.first().map_or(&b""[..], Vec::as_slice),
                    )
                    .into(),
                )
                .hex(),
            );
            let mut index: Value =
                serde_json::from_slice(&std::fs::read(output.join("index.json")).unwrap()).unwrap();
            match case {
                "missing marker" => std::fs::remove_file(output.join("oci-layout")).unwrap(),
                "bad marker" => {
                    std::fs::write(output.join("oci-layout"), br#"{"imageLayoutVersion":"9"}"#)
                        .unwrap()
                }
                "empty index" => index["manifests"] = json!([]),
                "index schema" => index["schemaVersion"] = 1.into(),
                "index type" => index["mediaType"] = "text/plain".into(),
                "entry type" => index["manifests"][0]["mediaType"] = INDEX_TYPE.into(),
                "entry artifact" => index["manifests"][0]["artifactType"] = "text/plain".into(),
                "entry digest" => index["manifests"][0]["digest"] = "sha256:../../escape".into(),
                "entry size" => index["manifests"][0]["size"] = 1.into(),
                "missing manifest" => {
                    std::fs::remove_file(output.join("blobs/sha256").join(digest.hex())).unwrap()
                }
                "missing cow" => std::fs::remove_file(&cow).unwrap(),
                "tampered cow" => {
                    let mut bytes = fixture.blobs[0].clone();
                    bytes[100] ^= 1;
                    std::fs::write(&cow, bytes).unwrap();
                }
                "unexpected file" => std::fs::write(output.join("secret"), b"secret").unwrap(),
                "unreferenced blob" => std::fs::write(
                    output.join("blobs/sha256").join(
                        Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(b"extra").into())
                            .hex(),
                    ),
                    b"extra",
                )
                .unwrap(),

                "salt mismatch" => fixture.sidecars(&output),
                "guest verity" | "truncated verity" => {
                    fixture.sidecars(&output);
                    let mut bytes = fixture.trees[0].clone();
                    if case == "guest verity" {
                        *bytes.last_mut().unwrap() ^= 1;
                    } else {
                        bytes.pop();
                    }
                    std::fs::write(cow.verity_path(), bytes).unwrap();
                }
                "oversized blob" => File::create(&cow)
                    .unwrap()
                    .set_len(TEST_CAPACITY + 1)
                    .unwrap(),
                "oversized total" => {
                    for n in 0..5 {
                        File::create(output.join("blobs/sha256").join(format!("{n:064x}")))
                            .unwrap()
                            .set_len(TEST_CAPACITY / 4)
                            .unwrap();
                    }
                }
                "too many files" => {
                    for number in 0..FILE_CAP {
                        File::create(output.join("blobs/sha256").join(format!("{number:064x}")))
                            .unwrap();
                    }
                }
                "unexpected directory" => std::fs::create_dir(output.join("elsewhere")).unwrap(),
                "duplicate manifest" => {
                    let entry = index["manifests"][0].clone();
                    index["manifests"].as_array_mut().unwrap().push(entry);
                }
                _ => {}
            }
            std::fs::write(
                output.join("index.json"),
                serde_json::to_vec(&index).unwrap(),
            )
            .unwrap();
            if case == "bad index" {
                std::fs::write(output.join("index.json"), b"{").unwrap();
            }
            if case == "oversized metadata" {
                File::create(output.join("index.json"))
                    .unwrap()
                    .set_len(METADATA_CAP + 1)
                    .unwrap();
            }
            let result = match OciLayout::read_with_capacity(&output, &store, TEST_CAPACITY).await {
                Ok(layout) => layout.import_build(&store, &db, "built:v1").await,
                Err(error) => Err(error),
            };
            assert!(result.is_err(), "accepted {case}");
            if case == "oversized total" {
                assert!(
                    format!("{:#}", result.unwrap_err())
                        .contains("OCI layout exceeds size or file count cap"),
                    "missed total size cap"
                );
            }
            assert_eq!(
                db.resolve_tag(&key).await.unwrap(),
                Some(old),
                "changed tag for {case}"
            );
            assert!(
                !store.blob_path(&digest).exists(),
                "published invalid {case}"
            );
            assert_eq!(
                std::fs::read_dir(store.scratch_dir().await.unwrap())
                    .unwrap()
                    .count(),
                0,
                "leaked snapshot for {case}"
            );
        }
    }

    #[tokio::test]
    async fn build_rejects_several_distinct_manifests_without_publishing() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("output");
        let store = FilesystemBlobStore::new(dir.path().join("cache"));
        let db = FilesystemTagDb::open(dir.path().join("cache")).unwrap();
        let fixture = LayoutFixture::new();
        let digest = fixture.write(&output);
        let mut second = fixture.manifest.clone();
        second["annotations"]["org.opencontainers.image.created"] = "2026-10-28T00:00:00Z".into();
        let bytes = serde_json::to_vec(&second).unwrap();
        let other = Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&bytes).into());
        std::fs::write(output.join("blobs/sha256").join(other.hex()), &bytes).unwrap();
        let index_path = output.join("index.json");
        let mut index: Value =
            serde_json::from_slice(&std::fs::read(&index_path).unwrap()).unwrap();
        index["manifests"].as_array_mut().unwrap().push(json!({
            "mediaType": MANIFEST_TYPE, "digest": other.to_string(), "size": bytes.len(),
        }));
        std::fs::write(index_path, serde_json::to_vec(&index).unwrap()).unwrap();
        let layout = OciLayout::read(&output, &store).await.unwrap();
        assert!(layout.import_build(&store, &db, "built:v1").await.is_err());
        assert!(!store.blob_exists(&digest).await);
        assert!(!store.blob_exists(&other).await);
        assert!(
            db.resolve_tag(&"built:v1".parse::<Reference>().unwrap().to_string())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlinks_hardlinks_and_special_files() {
        use std::os::unix::fs::symlink;
        for case in [
            "root", "blobs", "sha256", "cow", "index", "marker", "yaml", "hardlink", "fifo",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let output = dir.path().join("output");
            let store = FilesystemBlobStore::new(dir.path().join("cache"));
            let fixture = LayoutFixture::new();
            fixture.write(&output);
            let cow = output.join("blobs/sha256").join(
                Digest::Sha256(
                    <sha2::Sha256 as sha2::Digest>::digest(
                        fixture.blobs.first().map_or(&b""[..], Vec::as_slice),
                    )
                    .into(),
                )
                .hex(),
            );
            let path = match case {
                "root" => output.clone(),
                "blobs" => output.join("blobs"),
                "sha256" => output.join("blobs/sha256"),
                "index" => output.join("index.json"),
                "marker" => output.join("oci-layout"),
                "yaml" => output.join("build.yaml"),
                _ => cow,
            };
            let external = dir.path().join("external");
            if case == "yaml" {
                std::fs::write(&external, b"pmi: /etc/passwd").unwrap();
            } else {
                std::fs::rename(&path, &external).unwrap();
            }
            match case {
                "hardlink" => std::fs::hard_link(&external, &path).unwrap(),
                "fifo" => nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRUSR).unwrap(),
                _ => symlink(&external, &path).unwrap(),
            }
            assert!(
                OciLayout::read(&output, &store).await.is_err(),
                "accepted {case}"
            );
            assert!(external.exists());
        }
    }
    #[cfg(unix)]
    #[test]
    fn scan_capacity_is_independent_of_the_old_image_limit() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = dir.path().join("blobs/sha256");
        std::fs::create_dir_all(&blobs).unwrap();
        let size = 65 * 1024 * 1024 * 1024;
        File::create(blobs.join("ab".repeat(32)))
            .unwrap()
            .set_len(size)
            .unwrap();
        let root = Dir::open_ambient_dir(dir.path(), cap_std::ambient_authority()).unwrap();
        let mut files = BTreeMap::new();
        let mut total = 0;
        OciLayout::scan(&root, Path::new(""), &mut files, &mut total, size).unwrap();
        assert_eq!(total, size);
        assert!(
            OciLayout::scan(&root, Path::new(""), &mut BTreeMap::new(), &mut 0, size - 1).is_err()
        );
    }
}
