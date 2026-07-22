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
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;

use pichi_artifact::{Digest, MEDIA_TYPE_PICHI_ARTIFACT_V1, Reference, ReferenceKind};
use pichi_registry::{OCI_IMAGE_INDEX_MEDIA_TYPE, Registry};
use pichi_storage::{BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};

use crate::cmd::push::push_manifest_and_blobs;

use crate::cli::{ManifestAnnotateArgs, ManifestCreateArgs, ManifestPushArgs};
use crate::config::Config;

/// OCI image manifest media type (the per-arch entries' `mediaType`).
const OCI_MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

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
        let digest = tag_db
            .resolve_tag(&src_ref.to_string())
            .await?
            .ok_or_else(|| anyhow!("source not in local cache: {src}"))?;
        let raw = blob_store
            .get_blob(&digest)
            .await
            .with_context(|| format!("read local manifest {digest}"))?;
        let value: Value =
            serde_json::from_slice(&raw).with_context(|| format!("parse manifest {src}"))?;
        let artifact_type = value.get("artifactType").and_then(|v| v.as_str());
        if artifact_type != Some(MEDIA_TYPE_PICHI_ARTIFACT_V1) {
            bail!(
                "source {src} is not a pichi artifact (artifactType={artifact_type:?}); \
                 refusing to add it to the list"
            );
        }
        entries.push(json!({
            "mediaType": OCI_MANIFEST_MEDIA_TYPE,
            "artifactType": MEDIA_TYPE_PICHI_ARTIFACT_V1,
            "digest": digest.to_string(),
            "size": raw.len(),
            "annotations": { SOURCE_REF_ANNOTATION: src },
        }));
    }

    let index = build_index(entries);
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
    let manifests = ready
        .get("manifests")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("manifest list is malformed (no `manifests` array)"))?;
    let mut arches: Vec<(Digest, Vec<u8>)> = Vec::with_capacity(manifests.len());
    for m in manifests {
        let digest: Digest = m
            .get("digest")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("index entry missing digest"))?
            .parse()?;
        let raw = blob_store
            .get_blob(&digest)
            .await
            .with_context(|| format!("read local manifest {digest}"))?;
        arches.push((digest, raw));
    }
    let registry = &registry;
    let blob_store = &blob_store;
    let mut pushes = futures_util::stream::iter(arches.iter().map(|(digest, raw)| {
        let arch_ref = Reference {
            registry: dest.registry.clone(),
            repo: dest.repo.clone(),
            kind: ReferenceKind::Digest(digest.clone()),
        };
        async move {
            push_manifest_and_blobs(
                registry,
                blob_store,
                &arch_ref,
                raw,
                OCI_MANIFEST_MEDIA_TYPE,
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

    let index_bytes = serde_json::to_vec(&ready).context("serialise OCI image index")?;
    registry
        .push_manifest(&dest, OCI_IMAGE_INDEX_MEDIA_TYPE, Bytes::from(index_bytes))
        .await
        .map_err(|e| anyhow!("push image index {dest}: {e}"))?;

    println!("pushed manifest list {list_ref} -> {dest}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested; no I/O).
// ---------------------------------------------------------------------------

/// Wrap per-arch descriptor entries in an OCI image index.
fn build_index(entries: Vec<Value>) -> Value {
    json!({
        "schemaVersion": 2,
        "mediaType": OCI_IMAGE_INDEX_MEDIA_TYPE,
        "manifests": entries,
    })
}

/// Set `platform.os`/`platform.architecture` on the entry recorded for
/// `source`. Errors if no entry matches.
fn set_platform(index: &mut Value, source: &str, os: &str, arch: &str) -> Result<()> {
    let manifests = index
        .get_mut("manifests")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| anyhow!("manifest list is malformed (no `manifests` array)"))?;
    let mut matched = false;
    for m in manifests.iter_mut() {
        let is_match = m
            .get("annotations")
            .and_then(|a| a.get(SOURCE_REF_ANNOTATION))
            .and_then(Value::as_str)
            == Some(source);
        if is_match {
            m["platform"] = json!({ "os": os, "architecture": arch });
            matched = true;
        }
    }
    if !matched {
        bail!("source {source} is not in this manifest list (run `create` first)");
    }
    Ok(())
}

/// Validate every entry has a platform and strip the internal source-ref
/// annotation, producing the exact index to PUT.
fn prepare_for_push(mut index: Value, list_ref: &Reference) -> Result<Value> {
    let manifests = index
        .get_mut("manifests")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| anyhow!("manifest list is malformed (no `manifests` array)"))?;
    if manifests.is_empty() {
        bail!("manifest list {list_ref} is empty");
    }
    for m in manifests.iter_mut() {
        let has_platform = m.pointer("/platform/os").and_then(Value::as_str).is_some()
            && m.pointer("/platform/architecture")
                .and_then(Value::as_str)
                .is_some();
        if !has_platform {
            let digest = m.get("digest").and_then(Value::as_str).unwrap_or("?");
            bail!(
                "entry {digest} has no platform; run `pichi manifest annotate {list_ref} \
                 <source> --os <os> --arch <arch>` before pushing"
            );
        }
        if let Some(obj) = m.get_mut("annotations").and_then(Value::as_object_mut) {
            obj.remove(SOURCE_REF_ANNOTATION);
            if obj.is_empty() {
                m.as_object_mut()
                    .expect("entry is an object")
                    .remove("annotations");
            }
        }
    }
    Ok(index)
}

// ---------------------------------------------------------------------------
// Local list storage: `<graphroot>/manifests/<encoded-ref>.json`.
// ---------------------------------------------------------------------------

fn store_list(config: &Config, list_ref: &Reference, index: &Value) -> Result<()> {
    let path = list_path(config, list_ref)?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)
            .with_context(|| format!("create manifests dir {}", dir.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(index).context("serialise manifest list")?;
    fs::write(&path, bytes).with_context(|| format!("write manifest list {}", path.display()))?;
    Ok(())
}

fn load_list(config: &Config, list_ref: &Reference) -> Result<Value> {
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

    fn entry(source: &str, digest: &str) -> Value {
        json!({
            "mediaType": OCI_MANIFEST_MEDIA_TYPE,
            "artifactType": MEDIA_TYPE_PICHI_ARTIFACT_V1,
            "digest": digest,
            "size": 123,
            "annotations": { SOURCE_REF_ANNOTATION: source },
        })
    }

    fn list_ref() -> Reference {
        "ghcr.io/pichi-vm/fedora:43".parse().unwrap()
    }

    #[tokio::test]
    async fn build_index_has_index_media_type() {
        let idx = build_index(vec![entry("img:43-amd64", "sha256:aa")]);
        assert_eq!(
            idx["mediaType"], OCI_IMAGE_INDEX_MEDIA_TYPE,
            "index media type"
        );
        assert_eq!(idx["schemaVersion"], 2);
        assert_eq!(idx["manifests"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn annotate_sets_platform_on_matching_source() {
        let mut idx = build_index(vec![
            entry("img:43-amd64", "sha256:aa"),
            entry("img:43-arm64", "sha256:bb"),
        ]);
        set_platform(&mut idx, "img:43-arm64", "pichi", "arm64").unwrap();
        let arm = &idx["manifests"][1];
        assert_eq!(arm["platform"]["os"], "pichi");
        assert_eq!(arm["platform"]["architecture"], "arm64");
        // the other entry is untouched
        assert!(idx["manifests"][0].get("platform").is_none());
    }

    #[tokio::test]
    async fn annotate_unknown_source_errors() {
        let mut idx = build_index(vec![entry("img:43-amd64", "sha256:aa")]);
        assert!(set_platform(&mut idx, "img:43-ppc64le", "pichi", "ppc64le").is_err());
    }

    #[tokio::test]
    async fn push_requires_platform_on_every_entry() {
        let mut idx = build_index(vec![
            entry("img:43-amd64", "sha256:aa"),
            entry("img:43-arm64", "sha256:bb"),
        ]);
        set_platform(&mut idx, "img:43-amd64", "pichi", "amd64").unwrap();
        // arm64 left un-annotated → push must refuse.
        let err = prepare_for_push(idx, &list_ref()).unwrap_err();
        assert!(err.to_string().contains("no platform"), "{err}");
    }

    #[tokio::test]
    async fn prepare_strips_internal_annotation_and_keeps_platform() {
        let mut idx = build_index(vec![entry("img:43-amd64", "sha256:aa")]);
        set_platform(&mut idx, "img:43-amd64", "pichi", "amd64").unwrap();
        let ready = prepare_for_push(idx, &list_ref()).unwrap();
        let m = &ready["manifests"][0];
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
