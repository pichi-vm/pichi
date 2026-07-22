// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Async atomic-write primitive shared by the blob store, tag db, and sidecar
//! writers. Create a temp file on the SAME filesystem as the target, write it,
//! `fsync` it, then `rename(2)` into place — the rename is the atomic commit,
//! so a reader (or a second `pichi`) never observes a partially-written file
//! at its final (often content-addressed) path. Pure `tokio::fs`: no blocking
//! syscall on a runtime worker and no `spawn_blocking`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, Result};
use tokio::io::AsyncWriteExt as _;

/// Process-local counter making temp names unique without a random-number dep
/// (combined with the pid it is unique across concurrent writers and runs).
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_path(dir: &Path) -> PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(".pichi-tmp-{}-{n}", std::process::id()))
}

/// Atomically write `bytes` to `final_path`. `temp_dir` MUST live on the same
/// filesystem as `final_path` (else `rename(2)` fails with `EXDEV`); callers
/// pass the target's own directory or a same-fs scratch dir. On any error the
/// temp file is best-effort removed so no litter is left behind.
pub(crate) async fn write_atomic(temp_dir: &Path, final_path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = temp_path(temp_dir);
    let result = async {
        let mut f = tokio::fs::File::create(&tmp)
            .await
            .with_context(|| format!("create temp file {}", tmp.display()))?;
        f.write_all(bytes)
            .await
            .with_context(|| format!("write temp file {}", tmp.display()))?;
        f.sync_all()
            .await
            .with_context(|| format!("fsync temp file {}", tmp.display()))?;
        drop(f);
        // rename(2) atomically replaces any existing target; for a
        // content-addressed target a concurrent writer's bytes are identical,
        // so last-writer-wins is safe.
        tokio::fs::rename(&tmp, final_path)
            .await
            .with_context(|| format!("rename {} -> {}", tmp.display(), final_path.display()))?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    result
}
