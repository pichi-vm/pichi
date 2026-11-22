// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `pichi inspect <ref>...` (LOCAL-02 / D-20 image-index aware).
//!
//! Outputs an array of manifests and decoded launch configs with each resolved
//! content `digest` (the one datum a content-addressed manifest can't carry
//! about itself). `--format`
//! renders a minijinja (Jinja2) template — dotted OCI annotation keys are
//! reached with subscripts, e.g.
//! `{{ manifest.annotations["dev.pichi.carapace.verity.hash"] }}`. When the
//! resolved digest's blob is an OCI Image Index (D-20), drills into the
//! pichi-artifactType entry; if absent from the cache, lists the index
//! entries and points at `pichi pull`.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use super::cache::ImageStore as _;
use crate::cmd::manifest_ext::ManifestExt as _;
use anyhow::{Context, Result, anyhow};
use minijinja::Environment;
use serde::Serialize;
use serde_json::Value;

use conglobate::artifact::{Digest, Index, Manifest, Media as _, PichiArtifact, Reference};
use pichi_storage::{BlobStore, FilesystemBlobStore, FilesystemTagDb};

use crate::cli::InspectArgs;
use crate::config::Config;

/// `pichi inspect` output: the manifest verbatim (the artifact of record) plus
/// the resolved content digest — the one datum a content-addressed manifest
/// cannot carry about itself. Everything else callers might want (layer counts,
/// sizes, verity parameters) is already in the manifest; templating
/// reaches it directly (see [`InspectArgs::emit`]).
#[derive(Serialize)]
struct InspectOutput {
    digest: String,
    manifest: Value,
    config: conglobate::artifact::Config,
}

#[derive(Serialize)]
struct IndexInspectOutput {
    digest: String,
    media_type: String,
    entries: Vec<IndexEntryInfo>,
    note: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    manifest: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    manifest_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    config: Option<conglobate::artifact::Config>,
}

#[derive(Serialize)]
struct IndexEntryInfo {
    digest: String,
    artifact_type: Option<String>,
    media_type: String,
    platform: Value,
    is_pichi: bool,
}

/// Inspect cached images in order, retaining successful results after errors.
pub async fn run(args: InspectArgs, config: &Config) -> Result<()> {
    let layout = config.resolve_layout()?;
    let db = FilesystemTagDb::open(&layout.graphroot)?;
    let blob_store = FilesystemBlobStore::new(&layout.graphroot);

    let mut values = Vec::new();
    let mut failed = false;
    for reference in &args.references {
        match inspect_one(reference, &db, &blob_store).await {
            Ok(value) => values.push(value),
            Err(error) => {
                eprintln!("Error: {reference}: {error:#}");
                failed = true;
            }
        }
    }
    args.emit(&values)?;
    anyhow::ensure!(!failed, "one or more images could not be inspected");
    Ok(())
}

// Resolve one image without printing so a batch remains one JSON document.
async fn inspect_one(
    reference: &str,
    db: &FilesystemTagDb,
    blob_store: &FilesystemBlobStore,
) -> Result<Value> {
    let target_ref: Reference = super::build::canonicalize_reference(reference)
        .parse()
        .with_context(|| format!("invalid reference: {reference}"))?;

    let digest = blob_store.resolve(&target_ref, db).await?;
    let metadata = blob_store.metadata(&digest).await?;
    if metadata.is_index() {
        let selected = match metadata.selected_entry() {
            Ok(entry) => {
                let selected: Digest = entry["digest"]
                    .as_str()
                    .context("index entry has no digest")?
                    .parse()?;
                if blob_store.blob_exists(&selected).await {
                    let (selected, _) = blob_store.manifest(&digest).await?;
                    Some(selected)
                } else {
                    None
                }
            }
            Err(_) => None,
        };
        let config = match &selected {
            Some(image) => {
                Some(super::requirements::load_config(&image.manifest()?, blob_store).await?)
            }
            None => None,
        };
        return inspect_index(&digest, &metadata.value, selected, config);
    }
    let bytes = metadata.bytes;
    let value = metadata.value;

    // Bare manifest path. Validate defensively, then emit the manifest verbatim
    // plus its resolved digest.
    let manifest = Manifest::decode_json(bytes.as_slice())
        .with_context(|| format!("validating manifest {digest}"))?;
    let launch_config = super::requirements::load_config(&manifest, blob_store).await?;
    let output = InspectOutput {
        digest: digest.to_string(),
        manifest: value,
        config: launch_config,
    };
    Ok(serde_json::to_value(output)?)
}

fn inspect_index(
    digest: &Digest,
    value: &Value,
    selected: Option<super::cache::ImageMetadata>,
    config: Option<conglobate::artifact::Config>,
) -> Result<Value> {
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
        let is_pichi = artifact_type.as_deref() == Some(PichiArtifact::MEDIA_TYPE);
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
    let note = if selected.is_some() {
        "The cached platform manifest is included below; digest identifies the original index."
            .to_string()
    } else if pichi_count == 0 {
        "No pichi-artifactType entry found in this index. To pull a multi-arch tag's pichi entry: pichi pull <ref>".to_string()
    } else {
        format!(
            "Index contains {pichi_count} pichi entry/entries. To inspect one, pichi pull its specific manifest digest first."
        )
    };
    let output = IndexInspectOutput {
        digest: digest.to_string(),
        media_type: Index::MEDIA_TYPE.to_string(),
        entries,
        note,
        manifest_digest: selected.as_ref().map(|image| image.digest.to_string()),
        manifest: selected.map(|image| image.value),
        config,
    };
    Ok(serde_json::to_value(output)?)
}

impl InspectArgs {
    /// Emit a JSON array, or render the template once per inspected image.
    ///
    /// Templates are rendered with minijinja (Jinja2). Dotted OCI annotation
    /// keys are reached with subscript syntax — no data reshaping needed:
    /// `{{ manifest.annotations["dev.pichi.carapace.verity.hash"] }}`. For
    /// docker familiarity, a leading `{{.field}}` is accepted and normalised to
    /// `{{ field }}`.
    fn emit(&self, values: &[Value]) -> Result<()> {
        match &self.format {
            None => println!("{}", serde_json::to_string_pretty(values)?),
            Some(t) if t == "json" => println!("{}", serde_json::to_string_pretty(values)?),
            Some(template_in) => {
                let template = normalize_template_syntax(template_in);
                let env = Environment::new();
                let tmpl = env
                    .template_from_str(&template)
                    .with_context(|| format!("invalid --format template: {template_in:?}"))?;
                for value in values {
                    println!(
                        "{}",
                        tmpl.render(value).with_context(|| {
                            format!("rendering --format template: {template_in:?}")
                        })?
                    );
                }
            }
        }
        Ok(())
    }
}

/// Accept docker-style `{{.Field}}` by normalising the leading dot to Jinja's
/// `{{ Field }}`. Annotation subscripts (`["key"]`) are native minijinja and
/// pass through untouched.
fn normalize_template_syntax(user: &str) -> String {
    user.replace("{{.", "{{ ")
}
