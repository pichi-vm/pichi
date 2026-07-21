// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Tag database. STORAGE-05, STORAGE-10.
//!
//! `FilesystemTagDb` stores tag → manifest-digest mappings as an
//! [OCI Image Layout][1] `index.json` file at the cache root, with one
//! `manifests` entry per tag. Each entry's
//! `annotations["org.opencontainers.image.ref.name"]` carries the canonical
//! tag string (per CONTEXT D-02: `pichi_artifact::Reference::Display` form).
//!
//! Concurrency model: `set_tag` and `delete_tag` perform read-modify-write
//! under an advisory `flock(2)` on a sibling lockfile (`index.json.lock`).
//! The rewrite is atomic: temp file in the same dir, `fsync`, `rename(2)`.
//! Reads are lock-free (each call re-parses index.json from disk; index.json
//! is only ever replaced by atomic rename, so readers see either the old or
//! new file in full — never a torn write).
//!
//! On a brand-new cache where index.json does not yet exist, `resolve_tag`
//! and `list_tags` return `Ok(None)` / `Ok(vec![])` rather than erroring.
//!
//! On first write, an `oci-layout` marker file containing
//! `{"imageLayoutVersion":"1.0.0"}` is created next to index.json (idempotent).
//!
//! [1]: https://github.com/opencontainers/image-spec/blob/main/image-layout.md

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use pichi_artifact::{Digest, MEDIA_TYPE_PICHI_ARTIFACT_V1};

use crate::lock::with_advisory_lock;

/// OCI image-index media type per the image-spec.
const OCI_IMAGE_INDEX_MEDIA_TYPE: &str = "application/vnd.oci.image.index.v1+json";

/// OCI annotation key for the human-readable image reference (tag).
const REF_NAME_ANNOTATION: &str = "org.opencontainers.image.ref.name";

/// Contents of the `oci-layout` marker file at the cache root.
const OCI_LAYOUT_MARKER: &str = r#"{"imageLayoutVersion":"1.0.0"}"#;

/// Tag-to-digest mapping trait. Implementations MUST be `Send + Sync`.
pub trait TagDb: Send + Sync {
    /// Insert or overwrite the tag → digest mapping.
    fn set_tag(&self, tag: &str, digest: &Digest) -> Result<()>;
    /// Look up the digest for `tag`, returning `Ok(None)` if absent.
    fn resolve_tag(&self, tag: &str) -> Result<Option<Digest>>;
    /// Enumerate all tag entries. Order is the on-disk order in `index.json`
    /// (insertion order with set_tag overwrites moving entries to the back).
    fn list_tags(&self) -> Result<Vec<TagEntry>>;
    /// Remove the tag. `Ok(true)` if it existed; `Ok(false)` otherwise.
    fn delete_tag(&self, tag: &str) -> Result<bool>;
}

/// One row of the tag table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagEntry {
    /// Canonical `Reference::Display` form (per D-02).
    pub tag: String,
    /// The manifest digest this tag points at.
    pub digest: Digest,
}

// --- On-disk schema (OCI Image Index v1) ------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct ImageIndex {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    #[serde(rename = "mediaType")]
    media_type: String,
    #[serde(default)]
    manifests: Vec<ManifestDescriptor>,
}

impl Default for ImageIndex {
    fn default() -> Self {
        Self {
            schema_version: 2,
            media_type: OCI_IMAGE_INDEX_MEDIA_TYPE.to_string(),
            manifests: Vec::new(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ManifestDescriptor {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String, // "sha256:<hex>"
    size: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    annotations: BTreeMap<String, String>,
}

// --- FilesystemTagDb --------------------------------------------------------

/// Filesystem-backed tag database storing OCI Image Layout `index.json`.
///
/// `path` passed to `open` is the **cache root directory** (the parent of
/// `index.json` and `oci-layout`). For backward compatibility with the prior
/// redb-based call sites, if `path` ends with any `.redb` extension we treat
/// its parent as the root. (This compatibility shim covers any internal call
/// site that was not yet migrated; the canonical public usage from Task 2
/// onward is to pass the cache root directly.)
#[derive(Debug, Clone)]
pub struct FilesystemTagDb {
    root: PathBuf,
}

impl FilesystemTagDb {
    /// Open or initialise a tag database rooted at `path`.
    ///
    /// `path` is interpreted as the cache root directory. Parents are created
    /// as needed; the directory itself is created if missing. `index.json` is
    /// NOT created eagerly — it appears on the first `set_tag`. The
    /// `oci-layout` marker is also written lazily on the first `set_tag`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let mut root = PathBuf::from(path.as_ref());

        // Backward-compat shim: if the caller passed a *.redb file path
        // (legacy), treat its parent as the root.
        if root.extension().is_some_and(|e| e == "redb") {
            if let Some(parent) = root.parent() {
                root = parent.to_path_buf();
            }
        }

        fs::create_dir_all(&root)
            .with_context(|| format!("failed to create tag db root: {}", root.display()))?;
        Ok(Self { root })
    }

    fn index_path(&self) -> PathBuf {
        self.root.join("index.json")
    }

    fn lock_path(&self) -> PathBuf {
        self.root.join("index.json.lock")
    }

    fn oci_layout_marker_path(&self) -> PathBuf {
        self.root.join("oci-layout")
    }

    /// Read index.json from disk. Returns a default (empty) index if the file
    /// does not yet exist. Returns Err only on parse/IO failures other than
    /// NotFound.
    fn read_index(&self) -> Result<ImageIndex> {
        let path = self.index_path();
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<ImageIndex>(&bytes)
                .with_context(|| format!("failed to parse index.json at {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ImageIndex::default()),
            Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    /// Atomically write `index` to disk: tempfile in same dir → fsync →
    /// rename(2). Caller MUST hold the advisory flock.
    fn write_index_atomic(&self, index: &ImageIndex) -> Result<()> {
        let path = self.index_path();
        let parent = path
            .parent()
            .expect("index_path always has a parent (cache root)");

        let tmp = NamedTempFile::new_in(parent).with_context(|| {
            format!("failed to create temp index.json in: {}", parent.display())
        })?;

        // Pretty-print for human readability and skopeo/oras compatibility.
        let bytes = serde_json::to_vec_pretty(index).context("failed to serialise index.json")?;

        // Write + fsync the temp file before renaming.
        {
            let mut file = tmp.as_file();
            file.write_all(&bytes)
                .context("failed to write temp index.json")?;
            file.sync_all().context("failed to fsync temp index.json")?;
        }

        // persist() = atomic rename(2) on Linux.
        tmp.persist(&path).map_err(|e| {
            anyhow::anyhow!(
                "failed to persist index.json to {}: {}",
                path.display(),
                e.error
            )
        })?;
        Ok(())
    }

    /// Idempotently emit the `oci-layout` marker file. Caller MUST hold the
    /// advisory flock (call from inside set_tag / delete_tag write paths).
    fn ensure_oci_layout_marker(&self) -> Result<()> {
        let path = self.oci_layout_marker_path();
        if path.exists() {
            return Ok(());
        }
        let parent = path.parent().expect("oci-layout path has a parent");
        let tmp = NamedTempFile::new_in(parent).with_context(|| {
            format!("failed to create temp oci-layout in: {}", parent.display())
        })?;
        {
            let mut file = tmp.as_file();
            file.write_all(OCI_LAYOUT_MARKER.as_bytes())
                .context("failed to write temp oci-layout")?;
            file.sync_all().context("failed to fsync temp oci-layout")?;
        }
        // If a concurrent writer beat us, that's fine — the content is identical.
        match tmp.persist(&path) {
            Ok(_) => Ok(()),
            Err(e) if path.exists() => {
                drop(e);
                Ok(())
            }
            Err(e) => Err(anyhow::anyhow!(
                "failed to persist oci-layout to {}: {}",
                path.display(),
                e.error
            )),
        }
    }
}

impl TagDb for FilesystemTagDb {
    fn set_tag(&self, tag: &str, digest: &Digest) -> Result<()> {
        let lock_path = self.lock_path();
        with_advisory_lock(&lock_path, || {
            self.ensure_oci_layout_marker()?;
            let mut index = self.read_index()?;

            // Remove any existing entry for this tag (overwrite semantics).
            index.manifests.retain(|m| {
                m.annotations.get(REF_NAME_ANNOTATION).map(String::as_str) != Some(tag)
            });

            // Compute size: for v0.8 we don't have the manifest bytes here, so
            // record 0 as a placeholder (Phase 41 has no consumer reading
            // `size` yet; future work that resolves the manifest blob will
            // populate it). The OCI spec requires the field but does not
            // mandate its accuracy for an in-memory cache view used only by
            // pichi internally. External consumers (skopeo) read size for
            // pull validation, which is out of scope here.
            let descriptor = ManifestDescriptor {
                media_type: MEDIA_TYPE_PICHI_ARTIFACT_V1.to_string(),
                digest: digest.to_string(),
                size: 0,
                annotations: {
                    let mut a = BTreeMap::new();
                    a.insert(REF_NAME_ANNOTATION.to_string(), tag.to_string());
                    a
                },
            };
            index.manifests.push(descriptor);
            self.write_index_atomic(&index)
        })
    }

    fn resolve_tag(&self, tag: &str) -> Result<Option<Digest>> {
        // Lock-free read: index.json is replaced atomically by writers, so a
        // concurrent reader sees either the old or the new file in full.
        let index = self.read_index()?;
        for m in &index.manifests {
            if m.annotations.get(REF_NAME_ANNOTATION).map(String::as_str) == Some(tag) {
                let d = Digest::from_str(&m.digest).with_context(|| {
                    format!("corrupt digest for tag {tag} in index.json: {}", m.digest)
                })?;
                return Ok(Some(d));
            }
        }
        Ok(None)
    }

    fn list_tags(&self) -> Result<Vec<TagEntry>> {
        let index = self.read_index()?;
        let mut out = Vec::with_capacity(index.manifests.len());
        for m in &index.manifests {
            let Some(tag) = m.annotations.get(REF_NAME_ANNOTATION) else {
                continue; // skip manifests without a ref name annotation
            };
            let digest = Digest::from_str(&m.digest).with_context(|| {
                format!("corrupt digest for tag {tag} in index.json: {}", m.digest)
            })?;
            out.push(TagEntry {
                tag: tag.clone(),
                digest,
            });
        }
        Ok(out)
    }

    fn delete_tag(&self, tag: &str) -> Result<bool> {
        let lock_path = self.lock_path();
        with_advisory_lock(&lock_path, || self.delete_tag_locked(tag))
    }
}

impl FilesystemTagDb {
    /// Lock-free variant of [`TagDb::delete_tag`] for use inside a
    /// [`crate::with_index_lock`] window.
    ///
    /// **Caller MUST hold the advisory flock on `<root>/index.json.lock`** —
    /// either via `with_index_lock(layout, ...)` (the canonical path) or via
    /// any other holder of the same flock. Calling this without the flock is
    /// a refcount-races bug (T-42-02): two concurrent `rmi` processes could
    /// each compute orphan sets against the same pre-state and both unlink
    /// blobs the other still considers referenced.
    ///
    /// Calling [`TagDb::delete_tag`] (the locked entry point) from inside
    /// `with_index_lock` would self-deadlock under `flock(2)` (separate FDs
    /// to the same file from one process block each other). Use this method
    /// instead in that context.
    ///
    /// Returns `Ok(true)` if the tag existed and was removed, `Ok(false)`
    /// otherwise. Same semantics as `delete_tag` but with no flock acquisition.
    pub fn delete_tag_locked(&self, tag: &str) -> Result<bool> {
        let mut index = self.read_index()?;
        let before = index.manifests.len();
        index
            .manifests
            .retain(|m| m.annotations.get(REF_NAME_ANNOTATION).map(String::as_str) != Some(tag));
        let existed = index.manifests.len() != before;
        if existed {
            self.ensure_oci_layout_marker()?;
            self.write_index_atomic(&index)?;
        }
        Ok(existed)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn db() -> (tempfile::TempDir, FilesystemTagDb) {
        let dir = tempfile::TempDir::new().unwrap();
        // Pass the ROOT directory (new convention).
        let db = FilesystemTagDb::open(dir.path()).unwrap();
        (dir, db)
    }

    #[test]
    fn set_and_resolve_round_trip() {
        let (_dir, db) = db();
        let d = Digest::from_bytes_sha256(b"manifest-bytes");
        db.set_tag("docker.io/library/alpine:latest", &d).unwrap();
        let got = db.resolve_tag("docker.io/library/alpine:latest").unwrap();
        assert_eq!(got, Some(d));
    }

    #[test]
    fn resolve_absent_returns_none() {
        let (_dir, db) = db();
        assert_eq!(db.resolve_tag("never-set").unwrap(), None);
    }

    #[test]
    fn list_tags_returns_all() {
        use std::collections::HashSet;
        let (_dir, db) = db();
        let d1 = Digest::from_bytes_sha256(b"one");
        let d2 = Digest::from_bytes_sha256(b"two");
        let d3 = Digest::from_bytes_sha256(b"three");
        db.set_tag("a:latest", &d1).unwrap();
        db.set_tag("b:latest", &d2).unwrap();
        db.set_tag("c:latest", &d3).unwrap();
        let entries = db.list_tags().unwrap();
        assert_eq!(entries.len(), 3);
        let want: HashSet<_> = [
            ("a:latest".to_string(), d1),
            ("b:latest".to_string(), d2),
            ("c:latest".to_string(), d3),
        ]
        .into_iter()
        .collect();
        let got: HashSet<_> = entries.into_iter().map(|e| (e.tag, e.digest)).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn delete_tag_returns_true_then_false() {
        let (_dir, db) = db();
        let d = Digest::from_bytes_sha256(b"toremove");
        db.set_tag("foo:latest", &d).unwrap();
        assert!(db.delete_tag("foo:latest").unwrap());
        assert_eq!(db.resolve_tag("foo:latest").unwrap(), None);
        assert!(!db.delete_tag("foo:latest").unwrap());
    }

    #[test]
    fn persists_across_drop_and_reopen() {
        let dir = tempfile::TempDir::new().unwrap();
        let d = Digest::from_bytes_sha256(b"persistent");
        {
            let db = FilesystemTagDb::open(dir.path()).unwrap();
            db.set_tag("persist:tag", &d).unwrap();
        }
        let db2 = FilesystemTagDb::open(dir.path()).unwrap();
        assert_eq!(db2.resolve_tag("persist:tag").unwrap(), Some(d));
    }

    #[test]
    fn box_dyn_tagdb_compiles() {
        let dir = tempfile::TempDir::new().unwrap();
        let _b: Box<dyn TagDb> = Box::new(FilesystemTagDb::open(dir.path()).unwrap());
    }

    #[test]
    fn concurrent_set_tag_different_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Arc::new(FilesystemTagDb::open(dir.path()).unwrap());

        let d1 = Digest::from_bytes_sha256(b"data-one");
        let d2 = Digest::from_bytes_sha256(b"data-two");

        let h1 = {
            let db = Arc::clone(&db);
            let d = d1.clone();
            std::thread::spawn(move || db.set_tag("myimage:latest", &d))
        };
        let h2 = {
            let db = Arc::clone(&db);
            let d = d2.clone();
            std::thread::spawn(move || db.set_tag("myimage:v1", &d))
        };

        h1.join().unwrap().expect("set_tag latest must succeed");
        h2.join().unwrap().expect("set_tag v1 must succeed");

        // CRITICAL: both writes must survive — no lost-update under flock.
        assert_eq!(db.resolve_tag("myimage:latest").unwrap(), Some(d1));
        assert_eq!(db.resolve_tag("myimage:v1").unwrap(), Some(d2));
        assert_eq!(db.list_tags().unwrap().len(), 2);
    }

    #[test]
    fn index_json_schema_is_oci_compliant() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = FilesystemTagDb::open(dir.path()).unwrap();
        let d = Digest::from_bytes_sha256(b"oci-shape-check");
        db.set_tag("registry.example/app:v1", &d).unwrap();

        let index_path = dir.path().join("index.json");
        let bytes = std::fs::read(&index_path).expect("index.json must exist");
        let parsed: serde_json::Value =
            serde_json::from_slice(&bytes).expect("index.json must parse");

        assert_eq!(parsed["schemaVersion"].as_u64(), Some(2));
        assert_eq!(
            parsed["mediaType"].as_str(),
            Some("application/vnd.oci.image.index.v1+json")
        );
        let manifests = parsed["manifests"].as_array().expect("manifests array");
        assert_eq!(manifests.len(), 1);
        assert_eq!(
            manifests[0]["digest"].as_str(),
            Some(d.to_string().as_str())
        );
        assert!(manifests[0]["size"].as_u64().is_some());
        assert_eq!(
            manifests[0]["annotations"]["org.opencontainers.image.ref.name"].as_str(),
            Some("registry.example/app:v1")
        );

        // oci-layout marker file present with the canonical contents.
        let marker = std::fs::read_to_string(dir.path().join("oci-layout")).unwrap();
        assert_eq!(marker.trim(), r#"{"imageLayoutVersion":"1.0.0"}"#);
    }

    #[test]
    fn setting_same_tag_twice_overwrites() {
        let (_dir, db) = db();
        let d1 = Digest::from_bytes_sha256(b"v1");
        let d2 = Digest::from_bytes_sha256(b"v2");
        db.set_tag("x:latest", &d1).unwrap();
        db.set_tag("x:latest", &d2).unwrap();
        assert_eq!(db.resolve_tag("x:latest").unwrap(), Some(d2));
        assert_eq!(db.list_tags().unwrap().len(), 1);
    }
}
