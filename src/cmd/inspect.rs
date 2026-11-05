// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `pichi inspect <ref>` (LOCAL-02 / D-20 image-index aware).
//!
//! Outputs the manifest verbatim as pretty-printed JSON plus the resolved
//! content `digest` (the one datum a content-addressed manifest can't carry
//! about itself); everything else is derivable from the manifest. `--format`
//! renders a minijinja (Jinja2) template — dotted OCI annotation keys are
//! reached with subscripts, e.g.
//! `{{ manifest.annotations["dev.pichi.carapace.verity.algo"] }}`. When the
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
use std::fmt::Write as _;

use conglobate::artifact::{Digest, MEDIA_TYPE_PICHI_ARTIFACT_V1, Manifest, Reference};
use pichi_storage::{BlobStore, FilesystemBlobStore, FilesystemTagDb};

use crate::cli::InspectArgs;
use crate::config::Config;

use conglobate::artifact::MEDIA_TYPE_OCI_INDEX_V1 as OCI_IMAGE_INDEX_MEDIA_TYPE;

/// `pichi inspect` output: the manifest verbatim (the artifact of record) plus
/// the resolved content digest — the one datum a content-addressed manifest
/// cannot carry about itself. Everything else callers might want (layer counts,
/// sizes, verity parameters) is already in the manifest; templating
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
    #[serde(skip_serializing_if = "Option::is_none")]
    annotations: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    manifest: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    manifest_digest: Option<String>,
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

    let target_ref: Reference = super::build::canonicalize_reference(&args.reference)
        .parse()
        .with_context(|| format!("invalid reference: {}", args.reference))?;

    let digest = blob_store.resolve(&target_ref, &db).await?;
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
        return inspect_index(args, &digest, &metadata.value, selected);
    }
    let bytes = metadata.bytes;
    let value = metadata.value;

    // Bare manifest path. Validate defensively, then emit the manifest verbatim
    // plus its resolved digest.
    Manifest::decode_json(bytes.as_slice())
        .with_context(|| format!("validating manifest {digest}"))?;
    let output = InspectOutput {
        digest: digest.to_string(),
        manifest: value,
    };
    args.emit(&output)
}

fn inspect_index(
    args: InspectArgs,
    digest: &Digest,
    value: &Value,
    selected: Option<super::cache::ImageMetadata>,
) -> Result<()> {
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
        media_type: OCI_IMAGE_INDEX_MEDIA_TYPE.to_string(),
        entries,
        note,
        annotations: value.get("annotations").cloned(),
        manifest_digest: selected.as_ref().map(|image| image.digest.to_string()),
        manifest: selected.map(|image| image.value),
    };
    args.emit(&output)
}

impl InspectArgs {
    /// Emit `value` per the `--format` selection (default: pretty JSON).
    ///
    /// Templates are rendered with minijinja (Jinja2). Dotted OCI annotation
    /// keys are reached with subscript syntax — no data reshaping needed:
    /// `{{ manifest.annotations["dev.pichi.carapace.verity.algo"] }}`. For
    /// docker familiarity, a leading `{{.field}}` is accepted and normalised to
    /// `{{ field }}`.
    fn emit<T: Serialize>(&self, value: &T) -> Result<()> {
        match &self.format {
            None => println!("{}", serde_json::to_string_pretty(value)?),
            Some(t) if t == "json" => println!("{}", serde_json::to_string_pretty(value)?),
            Some(t) if t == "layers" || t == "annotations" => {
                let document = serde_json::to_value(value)?;
                let view = InspectionView {
                    reference: &self.reference,
                    document: &document,
                };
                print!(
                    "{}",
                    if t == "layers" {
                        view.layers()?
                    } else {
                        view.annotations()?
                    }
                );
            }
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

#[derive(Debug)]
struct InspectionView<'a> {
    reference: &'a str,
    document: &'a Value,
}

impl InspectionView<'_> {
    fn manifest(&self) -> Result<&Value> {
        self.document.get("manifest").context(
            "platform manifest is not cached; use pichi pull on a platform digest from pichi inspect",
        )
    }

    fn layers(&self) -> Result<String> {
        let layers = self.manifest()?["layers"]
            .as_array()
            .context("manifest has no layers")?;
        let mut output = format!(
            "{}\n\n  {:<3} {:<19} {:>12}  DIGEST\n",
            super::presentation::Presentation::stdout()
                .heading(&format!("Layers · {}", self.reference)),
            "#",
            "LAYER",
            "SIZE"
        );
        let mut filesystem = 0;
        let mut total = 0u64;
        for (index, layer) in layers.iter().enumerate() {
            let kind = match layer["mediaType"].as_str().unwrap_or("") {
                "application/vnd.pichi.scute.v1" => {
                    filesystem += 1;
                    format!("Filesystem {filesystem}")
                }
                "application/vnd.pichi.pmi.v1" => "Boot payload".into(),
                "application/vnd.pichi.bootconfig.v1+zstd" => "Boot configuration".into(),
                "application/vnd.pichi.dtb.v1" => "Device tree".into(),
                other => other.chars().flat_map(char::escape_debug).collect(),
            };
            let size = layer["size"].as_u64().context("layer has no size")?;
            total = total
                .checked_add(size)
                .context("combined layer size exceeds u64")?;
            let digest = layer["digest"].as_str().context("layer has no digest")?;
            let short: String = digest.chars().take(19).collect();
            writeln!(
                output,
                "  {:<3} {kind:<19} {:>12}  {short}",
                index + 1,
                humansize::format_size(size, humansize::BINARY)
            )?;
        }
        writeln!(
            output,
            "\n{} layers · {}\nDigests abbreviated; use --format json for full values.",
            layers.len(),
            humansize::format_size(total, humansize::BINARY)
        )?;
        Ok(output)
    }

    fn annotations(&self) -> Result<String> {
        let mut output = format!(
            "{}\n",
            super::presentation::Presentation::stdout()
                .heading(&format!("Annotations · {}", self.reference))
        );
        if let Some(index) = self.document["annotations"]
            .as_object()
            .filter(|a| !a.is_empty())
        {
            writeln!(output, "\nIndex")?;
            Self::annotation_rows(&mut output, index.iter(), "")?;
        }
        let annotations = self.manifest()?["annotations"]
            .as_object()
            .context("manifest has no annotations")?;
        for (heading, prefix) in [
            ("Build provenance", "dev.pichi.build."),
            ("Image metadata", "org.opencontainers.image."),
            ("Filesystem integrity", "dev.pichi.carapace.verity."),
        ] {
            let selected: Vec<_> = annotations
                .iter()
                .filter(|(key, _)| key.starts_with(prefix))
                .collect();
            if !selected.is_empty() {
                writeln!(output, "\n{heading} ({prefix}*)")?;
                Self::annotation_rows(&mut output, selected.into_iter(), prefix)?;
            }
        }
        let other: Vec<_> = annotations
            .iter()
            .filter(|(key, _)| {
                ![
                    "dev.pichi.build.",
                    "org.opencontainers.image.",
                    "dev.pichi.carapace.verity.",
                ]
                .iter()
                .any(|p| key.starts_with(p))
            })
            .collect();
        if !other.is_empty() {
            writeln!(output, "\nOther annotations")?;
            Self::annotation_rows(&mut output, other.into_iter(), "")?;
        }
        if annotations.is_empty() {
            writeln!(output, "\nNo manifest annotations.")?;
        }
        Ok(output)
    }

    fn annotation_rows<'a>(
        output: &mut String,
        rows: impl Iterator<Item = (&'a String, &'a Value)>,
        prefix: &str,
    ) -> Result<()> {
        for (key, value) in rows {
            let name: String = key
                .strip_prefix(prefix)
                .unwrap_or(key)
                .chars()
                .flat_map(char::escape_debug)
                .collect();
            let value: String = value
                .as_str()
                .context("annotation value is not a string")?
                .chars()
                .flat_map(char::escape_debug)
                .collect();
            if name.chars().count().max(17) + value.chars().count() + 4 > 80 {
                writeln!(output, "  {name}\n    {value}")?;
            } else {
                writeln!(output, "  {name:<17}  {value}")?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_size_overflow_is_an_error() {
        let document = serde_json::json!({"manifest": {"layers": [
            {"mediaType": "application/vnd.pichi.pmi.v1", "digest": "sha256:aaaa", "size": u64::MAX},
            {"mediaType": "application/vnd.pichi.pmi.v1", "digest": "sha256:bbbb", "size": 1}
        ]}});
        let view = InspectionView {
            reference: "oversized:demo",
            document: &document,
        };
        assert!(
            view.layers()
                .unwrap_err()
                .to_string()
                .contains("combined layer size")
        );
    }
}
