// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `pichi import <verb>` — bring external bytes into the local cache. Two
//! subcommands map onto the artifact's two axes (BUILD.md §15):
//!
//! - `import raw <raw.img>` — a raw disk image becomes a base **carapace**
//!   (rootfs; no PMI). `-t` is optional: omit it to cache an ephemeral carapace
//!   (the index digest is printed).
//! - `import pmi <boot.pmi> --dtb <d>` — a pre-built, detached PMI (+ base DTB,
//!   + optional launch config) becomes a **bootable** artifact. With
//!   `--carapace <ref>` the referenced carapace's scutes are combined in (a
//!   rootfs appliance); without it the result is PMI-only (bootable, no rootfs).
//!   The `--carapace` reference is READ-ONLY — its tag is never modified; the
//!   bootable artifact is always its own `-t`.
//!
//! The chicken-and-egg (a bootable PMI must bake the carapace's dm-verity root
//! into its measured cmdline, but that root only exists after import) is why
//! these are two steps: `import raw` prints the root, the producer builds the
//! PMI/DTB against it, then `import pmi --carapace` binds them on.

#![cfg_attr(test, allow(clippy::unwrap_used))]

pub(crate) mod raw;

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};

use conglobate::artifact::{
    Config, ConfigDescriptor, Digest, DtbDescriptor, Layer, MEDIA_TYPE_PICHI_ARTIFACT_V1, Manifest,
    PmiDescriptor, Reference,
};
use pichi_storage::{BlobStore, FilesystemBlobStore, FilesystemTagDb};

use super::cache::ImageStore as _;
use crate::cli::{ImportPmiArgs, ImportRawArgs};
use crate::cmd::manifest_ext::ManifestExt as _;
use crate::config::Config as PichiConfig;

/// `pichi import raw` — import a raw image as a base carapace (IMPORT-01..07).
pub async fn run_raw(args: ImportRawArgs, config: &PichiConfig) -> Result<()> {
    // T-43-02: parse the tag (if any) through the path-traversal-safe parser
    // BEFORE any I/O.
    if let Some(t) = &args.tag {
        let _tag_ref: Reference = t
            .parse()
            .with_context(|| format!("invalid tag reference: {t}"))?;
    }

    let layout = config.resolve_layout()?;
    let mut lib_args: raw::ImportArgs = args.try_into()?;
    lib_args.created_rfc3339 = chrono::Utc::now().to_rfc3339();
    lib_args.run(&layout.graphroot).await
}

/// `pichi import pmi` — import a pre-built PMI (+ DTB, + optional config) as a
/// bootable artifact, optionally combining a cached carapace's scutes.
pub async fn run_pmi(args: ImportPmiArgs, config: &PichiConfig) -> Result<()> {
    let layout = config.resolve_layout()?;
    let blob_store = FilesystemBlobStore::new(&layout.graphroot);
    let db = FilesystemTagDb::open(&layout.graphroot)
        .with_context(|| format!("opening tag db at {}", layout.graphroot.display()))?;

    let dest_key = args
        .tag
        .as_deref()
        .map(|t| {
            t.parse::<Reference>()
                .map(|r| r.to_string())
                .with_context(|| format!("invalid tag: {t}"))
        })
        .transpose()?;

    // Read every input up front (fail-fast: a missing/invalid file aborts
    // before any blob is written, so a failure leaves no partial state).
    let pmi_bytes = tokio::fs::read(&args.pmi)
        .await
        .with_context(|| format!("reading PMI file: {}", args.pmi.display()))?;
    let pmi_digest = Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&pmi_bytes).into());

    let dtb_bytes = tokio::fs::read(&args.dtb)
        .await
        .with_context(|| format!("reading DTB file: {}", args.dtb.display()))?;
    let dtb_digest = Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&dtb_bytes).into());

    let config_blob = match &args.config {
        Some(p) => Some(read_launch_contract(p).await?),
        None => None,
    };

    // Protect inherited cache inputs and new blobs until the tag is published.
    let _guard = db.lock_index().await?;

    // Assemble the manifest. With `--carapace`, start from the cached carapace
    // (reuse its scutes and verity parameter annotations) and keep only its
    // scute layers and the committed tip hash annotation. Without
    // `--carapace`, start from an empty scute-less manifest (a PMI-only
    // bootable artifact).
    let mut manifest = match &args.carapace {
        Some(carapace) => carapace_scutes_manifest(&blob_store, &db, carapace).await?,
        None => scuteless_manifest(),
    };
    // Merge caller-supplied provenance annotations. Structural verity-chain keys
    // are never overridable (they describe the carapace, not the operator's
    // metadata); `created` and everything else the caller may set.
    for (k, v) in parse_annotations(&args.annotations)? {
        if k.starts_with("dev.pichi.carapace.verity.") {
            continue;
        }
        manifest.annotations.insert(k, v);
    }
    manifest.layers.push(Layer::Pmi(PmiDescriptor {
        digest: pmi_digest.to_string(),
        size: pmi_bytes.len() as u64,
    }));
    manifest.layers.push(Layer::Dtb(DtbDescriptor {
        digest: dtb_digest.to_string(),
        size: dtb_bytes.len() as u64,
    }));
    if let Some((digest, bytes)) = &config_blob {
        manifest.config = ConfigDescriptor::for_config(digest.to_string(), bytes.len() as u64);
    }
    manifest
        .validate()
        .context("assembled bootable manifest failed self-validation (bug)")?;
    let manifest_bytes = serde_json::to_vec(&manifest).context("serialising bootable manifest")?;
    let manifest_digest =
        Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&manifest_bytes).into());

    // Commit blobs (PMI → DTB → config → manifest), then tag. Scute blobs (if
    // any) are already present — content-addressed no-op if re-put.
    blob_store
        .put_blob(&pmi_digest, &pmi_bytes)
        .await
        .with_context(|| format!("put_blob pmi {pmi_digest}"))?;
    blob_store
        .put_blob(&dtb_digest, &dtb_bytes)
        .await
        .with_context(|| format!("put_blob dtb {dtb_digest}"))?;
    if let Some((digest, bytes)) = &config_blob {
        blob_store
            .put_blob(digest, bytes)
            .await
            .with_context(|| format!("put_blob config {digest}"))?;
    }
    blob_store
        .put_blob(&manifest_digest, &manifest_bytes)
        .await
        .with_context(|| format!("put_blob manifest {manifest_digest}"))?;
    let index = blob_store.local_index(&manifest_digest, None).await?;

    if let Some(dest_key) = &dest_key {
        db.set_tag_locked(dest_key, &index)
            .await
            .with_context(|| format!("set_tag {dest_key}"))?;
    }

    if !args.quiet {
        log::info!(
            "pichi import pmi: cached bootable image {} (tag: {})",
            index,
            dest_key.as_deref().unwrap_or("<none>"),
        );
    }
    // Print the same index identity used by tags, locks and publication.
    println!("{index}");
    Ok(())
}

/// Load the cached carapace referenced by `carapace` (tag or digest; read-only)
/// and return a manifest carrying only its scute layers and chain annotations,
/// ready for PMI/DTB layers to be appended.
async fn carapace_scutes_manifest(
    blob_store: &FilesystemBlobStore,
    db: &FilesystemTagDb,
    carapace: &str,
) -> Result<Manifest> {
    // Accept a bare `sha256:<hex>` image digest directly — that is exactly
    // what `pichi import raw` prints, so an untagged carapace is referenceable
    // without inventing a repo. Otherwise parse as a tag / `repo@sha256:…` ref.
    let src_digest = if let Ok(digest) = carapace.parse::<Digest>() {
        digest
    } else {
        let carapace_ref: Reference = carapace
            .parse()
            .with_context(|| format!("invalid carapace reference: {carapace}"))?;
        blob_store.resolve(&carapace_ref, db).await?
    };
    let (_, mut manifest) = blob_store.manifest(&src_digest).await?;
    if manifest.scute_layers()?.is_empty() {
        bail!("{carapace} is not a carapace (no scute layers)");
    }
    // Keep only the carapace's scute layers (drop any prior PMI/DTB so a re-run
    // replaces cleanly); its scute cows + `.verity` sidecars are already cached.
    manifest
        .layers
        .retain(|l| matches!(l, Layer::Scute(_) | Layer::ScuteZstd(_)));
    Ok(manifest)
}

/// A fresh scute-less manifest (a PMI-only bootable artifact): no carapace, so
/// no chain-verity annotations — only the creation timestamp.
fn scuteless_manifest() -> Manifest {
    let mut annotations = BTreeMap::new();
    annotations.insert(
        "org.opencontainers.image.created".to_string(),
        chrono::Utc::now().to_rfc3339(),
    );
    Manifest {
        schema_version: 2,
        media_type: conglobate::artifact::MEDIA_TYPE_OCI_MANIFEST_V1.into(),
        artifact_type: MEDIA_TYPE_PICHI_ARTIFACT_V1.to_string(),
        config: ConfigDescriptor::canonical(),
        layers: Vec::new(),
        annotations,
    }
}

/// Parse repeatable `KEY=VALUE` annotation flags into a map. The key is
/// everything before the first `=`; the value may itself contain `=`.
pub(crate) fn parse_annotations(pairs: &[String]) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    for p in pairs {
        let (k, v) = p
            .split_once('=')
            .ok_or_else(|| anyhow!("annotation must be KEY=VALUE, got {p:?}"))?;
        if k.is_empty() {
            bail!("annotation key is empty in {p:?}");
        }
        map.insert(k.to_string(), v.to_string());
    }
    Ok(map)
}

/// Read a launch-contract config file (JSON or YAML — `serde_yaml` reads both),
/// validate its requirements, and re-serialise to canonical config-blob JSON.
/// Returns the blob's digest and bytes for storage as the manifest config.
async fn read_launch_contract(path: &Path) -> Result<(Digest, Vec<u8>)> {
    let text = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("read --config: {}", path.display()))?;
    let cfg: Config = serde_yaml::from_str(&text)
        .with_context(|| format!("parse --config: {}", path.display()))?;
    cfg.validate().context("invalid --config")?;
    let bytes = serde_json::to_vec(&cfg).context("serialise config blob")?;
    Ok((
        Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&bytes).into()),
        bytes,
    ))
}
