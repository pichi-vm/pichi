// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `pichi manifest` — assemble and push a multi-arch OCI image index
//! (manifest list), mirroring `docker manifest create/annotate/push`.
//!
//! pichi artifacts are single-platform (`pichi push` == `docker push`), and
//! `pichi pull` already selects the matching entry from a remote image index
//! (see `pichi_registry::pick_pichi_entry_from_index`). This is the producer
//! half: a small three-verb flow that mirrors docker's, since pichi artifacts
//! carry no architecture of their own (like a docker image whose config lacks
//! a platform, the arch must be supplied via `annotate`).
//!
//! - `create <list> <src>...` fetches each already-pushed per-arch manifest
//!   from the registry (for its digest + size), records them in a local list
//!   under `<graphroot>/manifests/`, and verifies each is a pichi artifact.
//! - `annotate <list> <src> --os --arch` sets an entry's platform.
//! - `push <list>` validates every entry has a platform, strips the internal
//!   bookkeeping annotation, and PUTs the OCI image index to the registry.
//!   The per-arch manifests it references must already be in the target repo.

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use futures_util::StreamExt as _;
#[cfg(test)]
use serde_json::json;
use std::fs;
use std::path::PathBuf;

use conglobate::artifact::MEDIA_TYPE_PICHI_ARTIFACT_V1;
use oci_spec::distribution::Reference;
use oci_spec::image::{Descriptor, Digest, ImageIndex, MediaType, Platform};
use pichi_registry::Registry;
use pichi_storage::{BlobStore, FilesystemBlobStore, FilesystemTagDb};

use super::cache::ImageStore as _;
use crate::cmd::push::push_manifest_and_blobs;

use crate::cli::{ManifestAnnotateArgs, ManifestCreateArgs, ManifestPushArgs};
use crate::config::Config;

/// Internal, non-OCI annotation recording which `create`/`annotate` source ref
/// an entry came from. Stripped before the index is pushed.
const SOURCE_REF_ANNOTATION: &str = "dev.pichi.manifest.source-ref";

pub async fn create(args: ManifestCreateArgs, config: &Config) -> Result<()> {
    let list_ref: Reference = args
        .list
        .parse()
        .with_context(|| format!("invalid list reference: {}", args.list))?;

    // Sources are LOCAL images (podman-style): resolve each from the cache,
    // never the registry. `pichi manifest push` uploads them together.
    let layout = config.resolve_layout()?;
    let blob_store = FilesystemBlobStore::new(&layout.graphroot);
    let tag_db = FilesystemTagDb::open(&layout.graphroot)
        .with_context(|| format!("opening tag db at {}", layout.graphroot.display()))?;

    let mut entries = Vec::with_capacity(args.sources.len());
    for src in &args.sources {
        let src_ref: Reference = src
            .parse()
            .with_context(|| format!("invalid source reference: {src}"))?;
        let root = blob_store.resolve(&src_ref, &tag_db).await?;
        let mut metadata = blob_store.metadata(&root).await?;
        if metadata.is_index() {
            // Each source identifies one platform; assembly is independent
            // of the host that publishes the combined index.
            let [entry] = metadata.entries()? else {
                bail!("manifest source {src} must contain exactly one platform manifest");
            };
            (metadata, _) = blob_store.manifest_entry(entry).await?;
        } else {
            metadata.manifest()?;
        }
        let mut descriptor = Descriptor::new(
            MediaType::ImageManifest,
            metadata.bytes.len() as u64,
            metadata.digest,
        );
        descriptor.set_artifact_type(Some(MEDIA_TYPE_PICHI_ARTIFACT_V1.into()));
        descriptor.set_annotations(Some([(SOURCE_REF_ANNOTATION.into(), src.clone())].into()));
        entries.push(descriptor);
    }

    // Index-level provenance: carry caller annotations verbatim, and stamp
    // `created` (the multi-arch image's build time) unless the caller gave one.
    let mut annotations = crate::cmd::import::parse_annotations(&args.annotations)?;
    annotations
        .entry("org.opencontainers.image.created".to_string())
        .or_insert_with(|| chrono::Utc::now().to_rfc3339());

    let index = build_index(entries, &annotations);
    store_list(config, &list_ref, &index)?;
    println!(
        "created manifest list {list_ref} with {} manifest(s); annotate each with \
         `pichi manifest annotate {list_ref} <source> --os <os> --arch <arch>`",
        args.sources.len()
    );
    Ok(())
}

pub async fn annotate(args: ManifestAnnotateArgs, config: &Config) -> Result<()> {
    let list_ref: Reference = args
        .list
        .parse()
        .with_context(|| format!("invalid list reference: {}", args.list))?;
    let mut index = load_list(config, &list_ref)?;
    set_platform(&mut index, &args.source, &args.os, &args.arch)?;
    store_list(config, &list_ref, &index)?;
    println!(
        "annotated {} in {list_ref} as {}/{}",
        args.source, args.os, args.arch
    );
    Ok(())
}

pub async fn push(args: ManifestPushArgs, config: &Config) -> Result<()> {
    let list_ref: Reference = args
        .list
        .parse()
        .with_context(|| format!("invalid list reference: {}", args.list))?;
    let dest: Reference = args
        .dest
        .parse()
        .with_context(|| format!("invalid destination reference: {}", args.dest))?;
    let ready = prepare_for_push(load_list(config, &list_ref)?, &list_ref)?;

    let layout = config.resolve_layout()?;
    let blob_store = FilesystemBlobStore::new(&layout.graphroot);
    let registry = config.http_registry();

    // podman `--all` semantics: push every referenced local image (blobs +
    // manifest, by digest into the dest repo), then the index last, so the
    // dest tag only resolves once all content is present. The per-arch images
    // are independent, so push them concurrently; the index push waits for all.
    let mut arches: Vec<(Digest, Vec<u8>)> = Vec::with_capacity(ready.manifests().len());
    for descriptor in ready.manifests() {
        let digest = descriptor.digest().clone();
        let raw = blob_store
            .get_blob(&digest)
            .await
            .with_context(|| format!("read local manifest {digest}"))?;
        arches.push((digest, raw));
    }
    let registry = &registry;
    let blob_store = &blob_store;
    let mut pushes = futures_util::stream::iter(arches.iter().map(|(digest, raw)| {
        let arch_ref = dest.clone_with_digest(digest.to_string());
        async move {
            push_manifest_and_blobs(
                registry,
                blob_store,
                &arch_ref,
                raw,
                MediaType::ImageManifest.as_ref(),
            )
            .await
            .with_context(|| format!("push arch image {digest}"))
        }
    }))
    .buffer_unordered(3);
    while let Some(result) = futures_util::StreamExt::next(&mut pushes).await {
        result?;
    }
    drop(pushes);

    let mut value = serde_json::to_value(&ready)?;
    value.sort_all_objects();
    let index_bytes = serde_json::to_vec(&value).context("serialise OCI image index")?;
    registry
        .push_manifest(
            &dest,
            MediaType::ImageIndex.as_ref(),
            Bytes::from(index_bytes),
        )
        .await
        .map_err(|e| anyhow!("push image index {dest}: {e}"))?;

    println!("pushed manifest list {list_ref} -> {dest}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested; no I/O).
// ---------------------------------------------------------------------------

/// Wrap per-arch descriptor entries in an OCI image index, with optional
/// index-level annotations (omitted from the JSON when empty).
fn build_index(
    entries: Vec<Descriptor>,
    annotations: &std::collections::BTreeMap<String, String>,
) -> ImageIndex {
    let mut index = ImageIndex::default();
    index.set_media_type(Some(MediaType::ImageIndex));
    index.set_manifests(entries);
    if !annotations.is_empty() {
        index.set_annotations(Some(annotations.clone().into_iter().collect()));
    }
    index
}

/// Set the platform on the entry recorded for `source`.
fn set_platform(index: &mut ImageIndex, source: &str, os: &str, arch: &str) -> Result<()> {
    let mut manifests = index.manifests().clone();
    let mut matched = false;
    for descriptor in &mut manifests {
        if descriptor
            .annotations()
            .as_ref()
            .and_then(|a| a.get(SOURCE_REF_ANNOTATION))
            .map(String::as_str)
            == Some(source)
        {
            let mut platform = Platform::default();
            platform.set_os(os.into());
            platform.set_architecture(arch.into());
            descriptor.set_platform(Some(platform));
            matched = true;
        }
    }
    if !matched {
        bail!("source {source} is not in this manifest list (run `create` first)");
    }
    index.set_manifests(manifests);
    Ok(())
}

/// Require a platform and strip the internal source annotation before publication.
fn prepare_for_push(mut index: ImageIndex, list_ref: &Reference) -> Result<ImageIndex> {
    if index.manifests().is_empty() {
        bail!("manifest list {list_ref} is empty");
    }
    let mut manifests = index.manifests().clone();
    for descriptor in &mut manifests {
        if descriptor.platform().is_none() {
            bail!(
                "entry {} has no platform; run `pichi manifest annotate {list_ref} <source> --os <os> --arch <arch>` before pushing",
                descriptor.digest()
            );
        }
        if let Some(mut annotations) = descriptor.annotations().clone() {
            annotations.remove(SOURCE_REF_ANNOTATION);
            descriptor.set_annotations((!annotations.is_empty()).then_some(annotations));
        }
    }
    index.set_manifests(manifests);
    Ok(index)
}

// ---------------------------------------------------------------------------
// Local list storage: `<graphroot>/manifests/<encoded-ref>.json`.
// ---------------------------------------------------------------------------

fn store_list(config: &Config, list_ref: &Reference, index: &ImageIndex) -> Result<()> {
    let path = list_path(config, list_ref)?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)
            .with_context(|| format!("create manifests dir {}", dir.display()))?;
    }
    let mut value = serde_json::to_value(index)?;
    value.sort_all_objects();
    let bytes = serde_json::to_vec_pretty(&value).context("serialise manifest list")?;
    fs::write(&path, bytes).with_context(|| format!("write manifest list {}", path.display()))?;
    Ok(())
}

fn load_list(config: &Config, list_ref: &Reference) -> Result<ImageIndex> {
    let path = list_path(config, list_ref)?;
    let bytes = fs::read(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow!("no manifest list {list_ref}; run `pichi manifest create` first")
        } else {
            anyhow!("read manifest list {}: {e}", path.display())
        }
    })?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse manifest list {list_ref}"))
}

fn list_path(config: &Config, list_ref: &Reference) -> Result<PathBuf> {
    let layout = config.resolve_layout()?;
    Ok(layout
        .graphroot
        .join("manifests")
        .join(format!("{}.json", encode_ref(&list_ref.to_string()))))
}

/// Bijective, filesystem-safe encoding of a reference string: safe chars pass
/// through, everything else becomes `%XX`. Avoids the collisions a naive
/// `/`/`:` → `_` substitution would cause (e.g. `img:43` vs `img/43`).
fn encode_ref(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' => out.push(b as char),
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0f) as usize] as char);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty() -> std::collections::BTreeMap<String, String> {
        std::collections::BTreeMap::new()
    }

    fn entry(source: &str, digest: &str) -> Descriptor {
        let mut descriptor = Descriptor::new(
            MediaType::ImageManifest,
            123,
            digest.parse::<Digest>().unwrap(),
        );
        descriptor.set_artifact_type(Some(MEDIA_TYPE_PICHI_ARTIFACT_V1.into()));
        descriptor.set_annotations(Some([(SOURCE_REF_ANNOTATION.into(), source.into())].into()));
        descriptor
    }

    fn list_ref() -> Reference {
        "ghcr.io/pichi-vm/fedora:43".parse().unwrap()
    }

    #[tokio::test]
    async fn build_index_has_index_media_type() {
        let idx = build_index(
            vec![entry(
                "img:43-amd64",
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )],
            &empty(),
        );
        assert_eq!(
            json!(idx)["mediaType"],
            MediaType::ImageIndex.as_ref(),
            "index media type"
        );
        assert_eq!(json!(idx)["schemaVersion"], 2);
        assert_eq!(json!(idx)["manifests"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn index_annotations_are_carried_and_survive_prepare() {
        let mut ann = std::collections::BTreeMap::new();
        ann.insert(
            "org.opencontainers.image.source".to_string(),
            "https://example/repo".to_string(),
        );
        let mut idx = build_index(
            vec![entry(
                "img:43-amd64",
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )],
            &ann,
        );
        assert_eq!(
            json!(idx)["annotations"]["org.opencontainers.image.source"],
            "https://example/repo"
        );
        // prepare_for_push only touches per-entry annotations, so index-level
        // annotations must remain on the pushed index.
        set_platform(&mut idx, "img:43-amd64", "pichi", "amd64").unwrap();
        let ready = prepare_for_push(idx, &list_ref()).unwrap();
        assert_eq!(
            json!(ready)["annotations"]["org.opencontainers.image.source"],
            "https://example/repo"
        );
    }

    #[tokio::test]
    async fn empty_index_annotations_omit_the_field() {
        let idx = build_index(
            vec![entry(
                "img:43-amd64",
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )],
            &empty(),
        );
        assert!(idx.annotations().is_none());
    }

    #[tokio::test]
    async fn annotate_sets_platform_on_matching_source() {
        let mut idx = build_index(
            vec![
                entry(
                    "img:43-amd64",
                    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                ),
                entry(
                    "img:43-arm64",
                    "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                ),
            ],
            &empty(),
        );
        set_platform(&mut idx, "img:43-arm64", "pichi", "arm64").unwrap();
        let arm = &json!(idx)["manifests"][1];
        assert_eq!(arm["platform"]["os"], "pichi");
        assert_eq!(arm["platform"]["architecture"], "arm64");
        // the other entry is untouched
        assert!(json!(idx)["manifests"][0].get("platform").is_none());
    }

    #[tokio::test]
    async fn annotate_unknown_source_errors() {
        let mut idx = build_index(
            vec![entry(
                "img:43-amd64",
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )],
            &empty(),
        );
        assert!(set_platform(&mut idx, "img:43-ppc64le", "pichi", "ppc64le").is_err());
    }

    #[tokio::test]
    async fn push_requires_platform_on_every_entry() {
        let mut idx = build_index(
            vec![
                entry(
                    "img:43-amd64",
                    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                ),
                entry(
                    "img:43-arm64",
                    "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                ),
            ],
            &empty(),
        );
        set_platform(&mut idx, "img:43-amd64", "pichi", "amd64").unwrap();
        // arm64 left un-annotated → push must refuse.
        let err = prepare_for_push(idx, &list_ref()).unwrap_err();
        assert!(err.to_string().contains("no platform"), "{err}");
    }

    #[tokio::test]
    async fn prepare_strips_internal_annotation_and_keeps_platform() {
        let mut idx = build_index(
            vec![entry(
                "img:43-amd64",
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )],
            &empty(),
        );
        set_platform(&mut idx, "img:43-amd64", "pichi", "amd64").unwrap();
        let ready = prepare_for_push(idx, &list_ref()).unwrap();
        let m = &json!(ready)["manifests"][0];
        // internal annotation gone; empty annotations object removed entirely
        assert!(
            m.get("annotations").is_none(),
            "annotations should be stripped"
        );
        assert_eq!(m["platform"]["architecture"], "amd64");
        assert_eq!(m["artifactType"], MEDIA_TYPE_PICHI_ARTIFACT_V1);
    }

    #[tokio::test]
    async fn encode_ref_is_collision_free() {
        assert_ne!(encode_ref("img:43"), encode_ref("img/43"));
        assert_eq!(encode_ref("ghcr.io/org/img:43"), "ghcr.io%2Forg%2Fimg%3A43");
    }
}
