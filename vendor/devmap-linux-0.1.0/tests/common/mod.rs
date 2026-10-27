// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Shared mapping cleanup and sparse-file-backed loop devices for
//! real-kernel integration tests.
//!
//! Every test binary that `mod common;`s this file compiles the whole
//! thing but typically uses only part of it (e.g. `arm_poll.rs`
//! doesn't need `LoopDevice`) — `#![allow(dead_code)]` avoids per-binary
//! false-positive dead-code warnings from that, not from genuinely
//! unused code.
#![allow(dead_code)]

use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::Command;

use devmap_linux::{Control, DevId};

/// Test-only cleanup for a mapping; no device operations are wrapped here.
pub(crate) struct Owned {
    control: Control,
    id: Option<DevId>,
}

impl Owned {
    pub(crate) fn create(control: &Control, name: &str) -> io::Result<Self> {
        Ok(Self {
            control: control.clone(),
            id: Some(control.create(name)?),
        })
    }

    pub(crate) fn id(&self) -> DevId {
        self.id.expect("armed")
    }

    pub(crate) fn node_path(&self) -> PathBuf {
        PathBuf::from(format!("/dev/dm-{}", self.id().minor()))
    }

    pub(crate) fn remove(mut self, deferred: bool) -> io::Result<()> {
        self.control.remove(self.id(), deferred)?;
        self.id.take();
        Ok(())
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            let _ = self.control.remove(id, true);
        }
    }
}

/// A sparse backing file attached as a loop device via `losetup`. Detaches
/// the loop device and deletes the backing file on drop (best-effort).
pub(crate) struct LoopDevice {
    pub(crate) path: String,
    _directory: tempfile::TempDir,
}

impl LoopDevice {
    /// Creates a `size_bytes`-sized sparse file and attaches it.
    pub(crate) fn create(name: &str, size_bytes: u64) -> Self {
        let directory = tempfile::Builder::new()
            .prefix(&format!("devmap-test-{name}-"))
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .expect("create private backing directory");
        let file_path = directory.path().join("backing.img");
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&file_path)
            .expect("create backing file");
        file.set_len(size_bytes).expect("set_len backing file");
        drop(file);

        let output = Command::new("losetup")
            .args(["-f", "--show"])
            .arg(&file_path)
            .output()
            .expect("run losetup -f --show");
        assert!(
            output.status.success(),
            "losetup failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let path = String::from_utf8(output.stdout)
            .expect("losetup output is utf8")
            .trim()
            .to_string();

        Self {
            path,
            _directory: directory,
        }
    }
}

impl Drop for LoopDevice {
    fn drop(&mut self) {
        let _ = Command::new("losetup").args(["-d", &self.path]).status();
    }
}
