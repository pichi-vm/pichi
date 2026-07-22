// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `pichi images` (LOCAL-01). Lists cached artifacts.
//!
//! Per D-12..D-19:
//! - Default columns: REPOSITORY | TAG | BOOTABLE | DIGEST(12) | CREATED | SIZE
//! - `--quiet` (`-q`): full sha256:... digests, one per line (D-18)
//! - `--digests`: full DIGEST column (D-14)
//! - `--format <tpl>`: minijinja (Jinja2) render; docker-style `{{.Field}}` is
//!   normalised to `{{ Field }}` before render; minijinja does not auto-escape
//!   an unnamed template, so CLI output is emitted verbatim
//! - Sort: most-recently-created first; alphabetical fallback (D-12)

#![cfg_attr(test, allow(clippy::unwrap_used))]

use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};
use humansize::{BINARY, format_size};
use minijinja::Environment;
use serde::Serialize;

use pichi_artifact::{Layer, Manifest};
use pichi_storage::{
    BlobStore, CacheLayout, FilesystemBlobStore, FilesystemTagDb, TagDb, TagEntry,
};

use crate::cli::ImagesArgs;
use crate::config::Config;

const DIGEST_PREFIX_LEN: usize = 19; // "sha256:" (7) + 12 hex chars
const BOOTABLE_GLYPH_TRUE: &str = "✓";
const BOOTABLE_GLYPH_FALSE: &str = "—"; // em-dash per D-13

#[derive(Serialize, Debug, Clone)]
struct Row {
    #[serde(rename = "Repository")]
    repository: String,
    #[serde(rename = "Tag")]
    tag: String,
    #[serde(rename = "Bootable")]
    bootable: bool,
    #[serde(rename = "ID")]
    id: String, // 12-char prefix (sha256:abc...)
    #[serde(rename = "Digest")]
    digest: String, // full sha256:...
    #[serde(rename = "Created")]
    created: String, // raw RFC3339 (or "?" if unknown)
    #[serde(rename = "Size")]
    size: u64, // bytes
    #[serde(rename = "ScuteCount")]
    scute_count: usize,
    // Internal sort key — not serialised to template render.
    #[serde(skip)]
    created_dt: Option<DateTime<Utc>>,
}

/// `pichi images` entry point — list cached artifacts (LOCAL-01).
pub async fn run(args: ImagesArgs, config: &Config) -> Result<()> {
    let layout = config.resolve_layout()?;
    let db = FilesystemTagDb::open(&layout.graphroot)
        .with_context(|| format!("opening tag db at {}", layout.graphroot.display()))?;
    let blob_store = FilesystemBlobStore::new(&layout.graphroot);

    let entries = db.list_tags().await?;
    // Read every tag's manifest concurrently (one blob read each).
    let mut rows: Vec<Row> = futures_util::future::join_all(
        entries
            .iter()
            .map(|e| Row::from_tag_entry(e, &blob_store, &layout)),
    )
    .await;

    // Default sort (D-12): most-recently-created first, then repo, then tag.
    rows.sort_by(|a, b| {
        b.created_dt
            .cmp(&a.created_dt)
            .then_with(|| a.repository.cmp(&b.repository))
            .then_with(|| a.tag.cmp(&b.tag))
    });

    if args.quiet {
        for r in &rows {
            println!("{}", r.digest); // FULL digest per D-18
        }
        return Ok(());
    }

    if let Some(template_in) = &args.format {
        let template = translate_template_syntax(template_in);
        let env = Environment::new();
        let tmpl = env
            .template_from_str(&template)
            .with_context(|| format!("invalid --format template: {template_in:?}"))?;
        for r in &rows {
            match tmpl.render(r) {
                Ok(out) => println!("{out}"),
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "failed to render row for {}: {e}",
                        r.digest
                    ));
                }
            }
        }
        return Ok(());
    }

    Row::print_table(&rows, args.digests);
    Ok(())
}

impl Row {
    async fn from_tag_entry(
        entry: &TagEntry,
        blob_store: &FilesystemBlobStore,
        layout: &CacheLayout,
    ) -> Self {
        // Try to parse the manifest. On any failure, list the row with placeholder
        // values rather than dropping it (presentation-layer resilience — Phase 42
        // doesn't validate; that's `pichi inspect`'s job).
        let manifest_bytes = blob_store.get_blob(&entry.digest).await.ok();
        let manifest = manifest_bytes
            .as_ref()
            .and_then(|b| Manifest::from_reader(b.as_slice()).ok());

        let (bootable, scute_count, size) = manifest
            .as_ref()
            .map(|m| {
                let bootable = m.layers.iter().any(|l| matches!(l, Layer::Pmi(_)));
                let scute_count = m
                    .layers
                    .iter()
                    .filter(|l| !matches!(l, Layer::Pmi(_)))
                    .count();
                // Sum of all layer sizes (per D-16). Excludes inline empty config + manifest.
                let size: u64 = m.layers.iter().map(Layer::size).sum();
                (bootable, scute_count, size)
            })
            .unwrap_or((false, 0, 0));

        let (repository, tag_name) = split_repo_tag(&entry.tag);

        let created_dt = manifest
            .as_ref()
            .and_then(|m| m.annotations.get("org.opencontainers.image.created"))
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc))
            .or_else(|| {
                // Fallback: manifest blob mtime (per D-15).
                std::fs::metadata(layout.blob_path(&entry.digest))
                    .and_then(|m| m.modified())
                    .ok()
                    .map(DateTime::<Utc>::from)
            });

        let created = created_dt
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_else(|| "?".to_string());

        let digest_full = entry.digest.to_string();
        let digest_short: String = digest_full.chars().take(DIGEST_PREFIX_LEN).collect();

        Row {
            repository,
            tag: tag_name,
            bootable,
            id: digest_short,
            digest: digest_full,
            created,
            size,
            scute_count,
            created_dt,
        }
    }
}

/// Split a canonical `Reference::Display` string into (repository, tag).
/// Examples:
///   `docker.io/library/alpine:3` → (`docker.io/library/alpine`, `3`)
///   `registry.io/foo@sha256:...` → (`registry.io/foo`, `<digest>`)
fn split_repo_tag(canonical: &str) -> (String, String) {
    if let Some(at) = canonical.rfind('@') {
        return (canonical[..at].to_string(), canonical[at + 1..].to_string());
    }
    // Find last `:` that comes after the last `/` (avoid host:port).
    let last_slash = canonical.rfind('/').map_or(0, |p| p + 1);
    if let Some(rel_colon) = canonical[last_slash..].rfind(':') {
        let abs_colon = last_slash + rel_colon;
        return (
            canonical[..abs_colon].to_string(),
            canonical[abs_colon + 1..].to_string(),
        );
    }
    (canonical.to_string(), "<none>".to_string())
}

fn humanize_age(dt: DateTime<Utc>) -> String {
    let now: DateTime<Utc> = Local::now().with_timezone(&Utc);
    let d = now.signed_duration_since(dt);
    let secs = d.num_seconds().max(0);
    if secs < 60 {
        return format!("{secs}s ago");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    if days < 7 {
        return format!("{days}d ago");
    }
    if days < 30 {
        return format!("{}w ago", days / 7);
    }
    if days < 365 {
        return format!("{}mo ago", days / 30);
    }
    format!("{}y ago", days / 365)
}

fn translate_template_syntax(user: &str) -> String {
    // Accept docker-style `{{.Field}}` by normalising the leading dot to Jinja's
    // `{{ Field }}` (minijinja render). D-17 field names are unchanged.
    user.replace("{{.", "{{ ")
}

impl Row {
    fn print_table(rows: &[Row], full_digests: bool) {
        let headers = ["REPOSITORY", "TAG", "BOOTABLE", "DIGEST", "CREATED", "SIZE"];
        let display_rows: Vec<[String; 6]> = rows
            .iter()
            .map(|r| {
                let glyph = if r.bootable {
                    BOOTABLE_GLYPH_TRUE
                } else {
                    BOOTABLE_GLYPH_FALSE
                };
                let digest_col = if full_digests {
                    r.digest.clone()
                } else {
                    r.id.clone()
                };
                let created_col = match &r.created_dt {
                    Some(dt) => humanize_age(*dt),
                    None => "?".to_string(),
                };
                let size_col = if r.size == 0 {
                    "0 B".to_string()
                } else {
                    format_size(r.size, BINARY)
                };
                [
                    r.repository.clone(),
                    r.tag.clone(),
                    glyph.to_string(),
                    digest_col,
                    created_col,
                    size_col,
                ]
            })
            .collect();

        // Column widths: max of header + all cells, +2 padding.
        let mut widths = [0usize; 6];
        for (i, h) in headers.iter().enumerate() {
            widths[i] = h.chars().count();
        }
        for row in &display_rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }

        // Print header
        let mut header_line = String::new();
        for (i, h) in headers.iter().enumerate() {
            if i > 0 {
                header_line.push_str("  ");
            }
            let w = widths[i];
            use std::fmt::Write as _;
            write!(header_line, "{h:<w$}").ok();
        }
        println!("{}", header_line.trim_end());

        // Print rows
        for row in &display_rows {
            let mut line = String::new();
            for (i, cell) in row.iter().enumerate() {
                if i > 0 {
                    line.push_str("  ");
                }
                let w = widths[i];
                use std::fmt::Write as _;
                write!(line, "{cell:<w$}").ok();
            }
            println!("{}", line.trim_end());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn translate_template_syntax_d17() {
        // docker-style leading dot is normalised to Jinja `{{ Field }}`.
        assert_eq!(
            translate_template_syntax("{{.Repository}}:{{.Tag}}"),
            "{{ Repository}}:{{ Tag}}"
        );
        // Native Jinja (no leading dot) passes through unchanged.
        assert_eq!(translate_template_syntax("{{ Foo }}"), "{{ Foo }}");
    }

    #[tokio::test]
    async fn split_repo_tag_canonical() {
        assert_eq!(
            split_repo_tag("docker.io/library/alpine:3"),
            ("docker.io/library/alpine".to_string(), "3".to_string())
        );
        assert_eq!(
            split_repo_tag("registry.io/foo/bar:v1"),
            ("registry.io/foo/bar".to_string(), "v1".to_string())
        );
        assert_eq!(
            split_repo_tag("docker.io/library/alpine@sha256:abc"),
            (
                "docker.io/library/alpine".to_string(),
                "sha256:abc".to_string()
            )
        );
    }

    #[tokio::test]
    async fn humanize_age_branches() {
        let now: DateTime<Utc> = Local::now().with_timezone(&Utc);
        assert!(humanize_age(now - chrono::Duration::seconds(30)).ends_with("s ago"));
        assert!(humanize_age(now - chrono::Duration::seconds(120)).ends_with("m ago"));
        assert!(humanize_age(now - chrono::Duration::hours(3)).ends_with("h ago"));
        assert!(humanize_age(now - chrono::Duration::days(2)).ends_with("d ago"));
        assert!(humanize_age(now - chrono::Duration::days(20)).ends_with("w ago"));
        assert!(humanize_age(now - chrono::Duration::days(60)).ends_with("mo ago"));
        assert!(humanize_age(now - chrono::Duration::days(800)).ends_with("y ago"));
    }
}
