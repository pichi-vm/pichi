// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `pichi inspect <ref>` (LOCAL-02 / D-20 image-index aware).
//!
//! Outputs the manifest as pretty-printed JSON augmented with a `_pichi`
//! sidecar (blob list with sizes, scute count, PMI presence boolean). When
//! the resolved digest's blob is an OCI Image Index (D-20), drills into the
//! pichi-artifactType entry; if absent from the cache, lists the index
//! entries and points at `pichi pull`.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde_json::Value;
use tinytemplate::{TinyTemplate, format_unescaped};

use pichi_artifact::{
    Digest, Layer, MEDIA_TYPE_PICHI_ARTIFACT_V1, Manifest, Reference, ReferenceKind,
};
use pichi_storage::{BlobStore, CacheLayout, FilesystemBlobStore, FilesystemTagDb, TagDb};

use crate::cli::InspectArgs;
use crate::config::Config;

const OCI_IMAGE_INDEX_MEDIA_TYPE: &str = "application/vnd.oci.image.index.v1+json";

#[derive(Serialize)]
struct InspectOutput {
    manifest: Value,
    _pichi: PichiSidecar,
}

#[derive(Serialize)]
struct PichiSidecar {
    digest: String,
    artifact_type: String,
    blob_count: usize,
    scute_count: usize,
    pmi_present: bool,
    total_layer_size: u64,
    blobs: Vec<BlobEntry>,
}

#[derive(Serialize)]
struct BlobEntry {
    digest: String,
    media_type: String,
    size: u64,
}

#[derive(Serialize)]
struct IndexInspectOutput {
    digest: String,
    media_type: String,
    entries: Vec<IndexEntryInfo>,
    note: String,
}

#[derive(Serialize)]
struct IndexEntryInfo {
    digest: String,
    artifact_type: Option<String>,
    media_type: String,
    platform: Value,
    is_pichi: bool,
}

/// `pichi inspect <ref>` entry point — print the cached manifest as
/// pretty-printed JSON augmented with a `_pichi` sidecar (LOCAL-02).
pub async fn run(args: InspectArgs, config: &Config) -> Result<()> {
    let layout = resolve_layout(config)?;
    let db = FilesystemTagDb::open(&layout.graphroot)?;
    let blob_store = FilesystemBlobStore::new(&layout.graphroot);

    let target_ref: Reference = args
        .reference
        .parse()
        .with_context(|| format!("invalid reference: {}", args.reference))?;

    // Resolve to a digest. If the ref is digest-form, use directly.
    let digest = match &target_ref.kind {
        ReferenceKind::Digest(d) => d.clone(),
        ReferenceKind::Tag(_) => {
            let key = target_ref.to_string();
            db.resolve_tag(&key)
                .await?
                .ok_or_else(|| anyhow!("ref not found in cache: {key}"))?
        }
    };

    let bytes = blob_store
        .get_blob(&digest)
        .await
        .with_context(|| format!("reading manifest blob {digest}"))?;
    let value: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing manifest JSON for {digest}"))?;

    // D-20: branch on the blob's mediaType.
    let media_type = value
        .get("mediaType")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if media_type == OCI_IMAGE_INDEX_MEDIA_TYPE {
        return inspect_index(args, &digest, &value);
    }

    // Bare manifest path.
    let manifest = Manifest::from_reader_validated(bytes.as_slice())
        .with_context(|| format!("validating manifest {digest}"))?;
    let sidecar = build_sidecar(&digest, &manifest);
    let output = InspectOutput {
        manifest: value,
        _pichi: sidecar,
    };
    emit(&args, &output)
}

fn inspect_index(args: InspectArgs, digest: &Digest, value: &Value) -> Result<()> {
    let mut entries = Vec::new();
    let manifests = value
        .get("manifests")
        .and_then(|m| m.as_array())
        .ok_or_else(|| anyhow!("OCI image index missing `manifests` array"))?;
    let mut pichi_count = 0usize;
    for m in manifests {
        let artifact_type = m
            .get("artifactType")
            .and_then(|v| v.as_str())
            .map(String::from);
        let is_pichi = artifact_type.as_deref() == Some(MEDIA_TYPE_PICHI_ARTIFACT_V1);
        if is_pichi {
            pichi_count += 1;
        }
        entries.push(IndexEntryInfo {
            digest: m
                .get("digest")
                .and_then(|d| d.as_str())
                .unwrap_or("?")
                .to_string(),
            artifact_type,
            media_type: m
                .get("mediaType")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string(),
            platform: m.get("platform").cloned().unwrap_or(Value::Null),
            is_pichi,
        });
    }
    let note = if pichi_count == 0 {
        "No pichi-artifactType entry found in this index. To pull a multi-arch tag's pichi entry: pichi pull <ref>".to_string()
    } else {
        format!(
            "Index contains {pichi_count} pichi entry/entries. To inspect one, pichi pull its specific manifest digest first."
        )
    };
    let output = IndexInspectOutput {
        digest: digest.to_string(),
        media_type: OCI_IMAGE_INDEX_MEDIA_TYPE.to_string(),
        entries,
        note,
    };
    emit(&args, &output)
}

fn build_sidecar(digest: &Digest, m: &Manifest) -> PichiSidecar {
    let mut blobs = Vec::with_capacity(m.layers.len());
    let mut pmi_present = false;
    let mut scute_count = 0usize;
    let mut total = 0u64;
    for layer in &m.layers {
        match layer {
            Layer::Pmi(_) => pmi_present = true,
            Layer::Scute(_) | Layer::ScuteZstd(_) => scute_count += 1,
            Layer::Dtb(_) => {}
        }
        total += layer.size();
        blobs.push(BlobEntry {
            digest: layer.digest_str().to_string(),
            media_type: layer.media_type_str().to_string(),
            size: layer.size(),
        });
    }
    PichiSidecar {
        digest: digest.to_string(),
        artifact_type: m.artifact_type.clone(),
        blob_count: m.layers.len(),
        scute_count,
        pmi_present,
        total_layer_size: total,
        blobs,
    }
}

fn emit<T: Serialize>(args: &InspectArgs, value: &T) -> Result<()> {
    if let Some(template_in) = &args.format {
        if template_in == "json" {
            let s = serde_json::to_string_pretty(value)?;
            println!("{s}");
        } else {
            // tinytemplate render. D-17 syntax translation.
            let template = template_in.replace("{{.", "{").replace("}}", "}");
            let mut tt = TinyTemplate::new();
            tt.set_default_formatter(&format_unescaped);
            tt.add_template("inspect", &template)
                .with_context(|| format!("invalid --format template: {template_in:?}"))?;
            let out = tt.render("inspect", value)?;
            println!("{out}");
        }
    } else {
        // Default: pretty-printed JSON.
        let s = serde_json::to_string_pretty(value)?;
        println!("{s}");
    }
    Ok(())
}

fn resolve_layout(config: &Config) -> Result<CacheLayout> {
    let mut layout = CacheLayout::resolve()?;
    if let Some(p) = &config.storage.graphroot {
        layout.graphroot.clone_from(p);
    }
    if let Some(p) = &config.storage.runroot {
        layout.runroot.clone_from(p);
    }
    Ok(layout)
}
