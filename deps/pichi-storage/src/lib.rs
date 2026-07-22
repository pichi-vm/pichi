// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `pichi-storage`: cache I/O for pichi OCI artifacts.
//!
//! This crate is the only path through which the pichi binary touches
//! the on-disk cache. It exposes:
//!
//! - [`CacheLayout`]: rootless/rootful path resolution per podman convention
//!   (STORAGE-03/04).
//! - [`BlobStore`] / [`FilesystemBlobStore`]: async content-addressed blob
//!   storage with atomic-rename writes (STORAGE-02, STORAGE-06, STORAGE-10).
//!   `open_blob` yields an async streaming reader ([`BlobReader`]).
//! - [`with_advisory_lock`] / [`lock_exclusive`] / [`with_index_lock`]:
//!   cross-platform advisory file-lock helpers (std `File::lock`, i.e.
//!   `flock(2)` on Unix / `LockFileEx` on Windows) for inter-process
//!   safety. `with_index_lock` wraps
//!   the canonical `<graphroot>/index.json.lock` path for use by callers
//!   that need to serialise multi-step cache mutations (e.g., `pichi rmi`).
//!
//! - [`TagDb`] / [`FilesystemTagDb`]: OCI Image Layout `index.json`-backed
//!   tag-to-digest database with persistence and concurrent-write safety
//!   (STORAGE-05, STORAGE-10).
//! - [`sidecar`]: per-source-blob sidecar path resolvers + atomic write +
//!   ENOENT-tolerant unlink (Phase 46 D-01, D-03, D-08).

mod blob_store;
mod layout;
mod lock;
pub mod sidecar;
mod tag_db;

pub use blob_store::{BlobReader, BlobStore, FilesystemBlobStore};
pub use layout::{CacheLayout, EnvSnapshot, Mode};
pub use lock::{lock_exclusive, with_advisory_lock, with_index_lock};
pub use sidecar::{deflated_path, unlink_blob_with_sidecars, verity_path, write_sidecar_atomic};
pub use tag_db::{FilesystemTagDb, TagDb, TagEntry};
