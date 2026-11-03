// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `pichi import <verb>` — bring external bytes into the local cache. Two
//! subcommands map onto the artifact's two axes (BUILD.md §15):
//!
//! - `import raw <raw.img>` — a raw disk image becomes a base **carapace**
//!   (rootfs; no PMI). `-t` is optional: omit it to cache an ephemeral carapace
//!   (the index digest is printed).
//! - `import pmi <boot.pmi> [--bootconfig <file>]` — immutable firmware with
//!   optional native Linux configuration and launch requirements. With
//!   `--carapace <ref>`, include that image's scutes without changing its tag.
//!
//! The producer can import its disk first and use the resulting root hash in
//! bootconfig. Composing a new image does not rewrite the PMI command line or
//! its embedded platform device tree.

#![cfg_attr(test, allow(clippy::unwrap_used))]

pub(crate) mod raw;

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};

use conglobate::artifact::{MEDIA_TYPE_PICHI_ARTIFACT_V1, Manifest};
use conglobate::resources::Config;
use oci_spec::distribution::Reference;
use oci_spec::image::Digest;
use pichi_storage::{BlobStore, FilesystemBlobStore, FilesystemTagDb};

use super::{boot_config::BootConfigFile, cache::ImageStore as _};
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

/// `pichi import pmi` — import PMI with optional native boot configuration as a
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
    let pmi_digest = format!(
        "sha256:{}",
        hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&pmi_bytes))
    )
    .parse::<Digest>()
    .expect("SHA-256 digest");

    let bootconfig = match &args.bootconfig {
        Some(path) => Some(BootConfigFile::read(path).await?),
        None => None,
    };

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
    manifest.layers.push(oci_spec::image::Descriptor::new(
        conglobate::artifact::MEDIA_TYPE_PICHI_PMI_V1.into(),
        pmi_bytes.len() as u64,
        pmi_digest.clone(),
    ));
    if let Some(config) = &bootconfig
        && let Some(descriptor) = config.descriptor()
    {
        manifest.layers.push(descriptor);
    }
    if let Some((digest, bytes)) = &config_blob {
        manifest.config = oci_spec::image::Descriptor::new(
            conglobate::resources::MEDIA_TYPE_PICHI_CONFIG_V1.into(),
            bytes.len() as u64,
            digest.clone(),
        );
    }
    manifest
        .validate()
        .context("assembled bootable manifest failed self-validation (bug)")?;
    let mut value = serde_json::to_value(&manifest)?;
    value.sort_all_objects();
    let manifest_bytes = serde_json::to_vec(&value).context("serialising bootable manifest")?;
    let manifest_digest = format!(
        "sha256:{}",
        hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&manifest_bytes))
    )
    .parse::<Digest>()
    .expect("SHA-256 digest");

    // Commit blobs (PMI → bootconfig → launch config → manifest), then tag. Scute blobs (if
    // any) are already present — content-addressed no-op if re-put.
    blob_store
        .put_blob(&pmi_digest, &pmi_bytes)
        .await
        .with_context(|| format!("put_blob pmi {pmi_digest}"))?;
    let pmi_layer = manifest
        .layers
        .iter()
        .find(|layer| {
            matches!(
                layer.media_type().as_ref(),
                conglobate::artifact::MEDIA_TYPE_PICHI_PMI_V1
            )
        })
        .context("PMI descriptor missing")?;
    super::layer::PreparedLayer::prepare_cached(&manifest, pmi_layer, &blob_store).await?;
    if let Some(config) = &bootconfig {
        config.store(&blob_store).await?;
    }
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
    BootConfigFile::validate_image(&manifest, &blob_store).await?;
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
/// ready for PMI/bootconfig layers to be appended.
async fn carapace_scutes_manifest(
    blob_store: &FilesystemBlobStore,
    db: &FilesystemTagDb,
    carapace: &str,
) -> Result<Manifest> {
    // Accept a bare `sha256:<hex>` image digest directly — that is exactly
    // what `pichi import raw` prints, so an untagged carapace is referenceable
    // without inventing a repo. Otherwise parse as a tag / `repo@sha256:…` ref.
    let src_digest = if carapace.starts_with("sha256:") {
        carapace.parse::<Digest>()?
    } else {
        let carapace_ref: Reference = carapace
            .parse()
            .with_context(|| format!("invalid carapace reference: {carapace}"))?;
        blob_store.resolve(&carapace_ref, db).await?
    };
    let (_, mut manifest) = blob_store.manifest(&src_digest).await?;
    if manifest.scute_layers().is_empty() {
        bail!("{carapace} is not a carapace (no scute layers)");
    }
    // Keep only the carapace's scute layers (drop any prior PMI/bootconfig so a re-run
    // replaces cleanly); its scute cows + `.verity` sidecars are already cached.
    manifest.layers.retain(|l| {
        matches!(
            l.media_type().as_ref(),
            conglobate::artifact::MEDIA_TYPE_PICHI_SCUTE_V1
                | conglobate::artifact::MEDIA_TYPE_PICHI_SCUTE_V1_ZSTD
        )
    });
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
        media_type: oci_spec::image::MediaType::ImageManifest.to_string(),
        artifact_type: MEDIA_TYPE_PICHI_ARTIFACT_V1.to_string(),
        config: oci_spec::image::DescriptorBuilder::default()
            .media_type(oci_spec::image::MediaType::EmptyJSON)
            .digest(
                "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
                    .parse::<oci_spec::image::Digest>()
                    .expect("empty config digest"),
            )
            .size(2u64)
            .data("e30=")
            .build()
            .expect("empty config descriptor"),
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

/// Read a launch-contract config file (JSON or YAML — `serde-saphyr` reads both),
/// validate its requirements, and re-serialise to canonical config-blob JSON.
/// Returns the blob's digest and bytes for storage as the manifest config.
async fn read_launch_contract(path: &Path) -> Result<(Digest, Vec<u8>)> {
    let text = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("read --config: {}", path.display()))?;
    let cfg: Config = serde_saphyr::from_str_with_options(
        &text,
        serde_saphyr::options! {
            // Match YAML 1.2 scalars and leave merge keys to the schema.
            strict_booleans: true,
            merge_keys: serde_saphyr::MergeKeyPolicy::AsOrdinary,
        },
    )
    .with_context(|| format!("parse --config: {}", path.display()))?;
    cfg.validate().context("invalid --config")?;
    let bytes = serde_json::to_vec(&cfg).context("serialise config blob")?;
    Ok((
        format!(
            "sha256:{}",
            hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bytes))
        )
        .parse::<Digest>()
        .expect("SHA-256 digest"),
        bytes,
    ))
}
