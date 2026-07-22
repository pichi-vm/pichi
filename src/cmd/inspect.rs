// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `pichi inspect <ref>` (LOCAL-02 / D-20 image-index aware).
//!
//! Outputs the manifest verbatim as pretty-printed JSON plus the resolved
//! content `digest` (the one datum a content-addressed manifest can't carry
//! about itself); everything else is derivable from the manifest. `--format`
//! renders a minijinja (Jinja2) template — dotted OCI annotation keys are
//! reached with subscripts, e.g.
//! `{{ manifest.annotations["dev.pichi.carapace.verity.hash"] }}`. When the
//! resolved digest's blob is an OCI Image Index (D-20), drills into the
//! pichi-artifactType entry; if absent from the cache, lists the index
//! entries and points at `pichi pull`.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use anyhow::{Context, Result, anyhow};
use minijinja::Environment;
use serde::Serialize;
use serde_json::Value;

use pichi_artifact::{Digest, MEDIA_TYPE_PICHI_ARTIFACT_V1, Manifest, Reference, ReferenceKind};
use pichi_storage::{BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};

use crate::cli::InspectArgs;
use crate::config::Config;

const OCI_IMAGE_INDEX_MEDIA_TYPE: &str = "application/vnd.oci.image.index.v1+json";

/// `pichi inspect` output: the manifest verbatim (the artifact of record) plus
/// the resolved content digest — the one datum a content-addressed manifest
/// cannot carry about itself. Everything else callers might want (layer counts,
/// sizes, the carapace verity hash) is already in the manifest; templating
/// reaches it directly (see [`InspectArgs::emit`]).
#[derive(Serialize)]
struct InspectOutput {
    digest: String,
    manifest: Value,
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
    let layout = config.resolve_layout()?;
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

    // Bare manifest path. Validate defensively, then emit the manifest verbatim
    // plus its resolved digest.
    Manifest::from_reader_validated(bytes.as_slice())
        .with_context(|| format!("validating manifest {digest}"))?;
    let output = InspectOutput {
        digest: digest.to_string(),
        manifest: value,
    };
    args.emit(&output)
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
    args.emit(&output)
}

impl InspectArgs {
    /// Emit `value` per the `--format` selection (default: pretty JSON).
    ///
    /// Templates are rendered with minijinja (Jinja2). Dotted OCI annotation
    /// keys are reached with subscript syntax — no data reshaping needed:
    /// `{{ manifest.annotations["dev.pichi.carapace.verity.hash"] }}`. For
    /// docker familiarity, a leading `{{.field}}` is accepted and normalised to
    /// `{{ field }}`.
    fn emit<T: Serialize>(&self, value: &T) -> Result<()> {
        match &self.format {
            None => println!("{}", serde_json::to_string_pretty(value)?),
            Some(t) if t == "json" => println!("{}", serde_json::to_string_pretty(value)?),
            Some(template_in) => {
                let template = normalize_template_syntax(template_in);
                let env = Environment::new();
                let tmpl = env
                    .template_from_str(&template)
                    .with_context(|| format!("invalid --format template: {template_in:?}"))?;
                println!(
                    "{}",
                    tmpl.render(value)
                        .with_context(|| format!("rendering --format template: {template_in:?}"))?
                );
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
