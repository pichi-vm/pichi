// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! An instance owns its frozen configuration, image pins and private disk.

use super::record::Records;
use crate::cmd::writable_storage::InstanceDisk;
use anyhow::{Context, Result, ensure};
use cap_std::fs::OpenOptions;
use conglobate::artifact::{Digest, Memory};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const VERSION: u16 = 2;
const DISK: &str = "storage.qcow2";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    version: u16,
    id: Uuid,
    root: Digest,
    manifest: Digest,
    cpus: u32,
    memory_mib: Memory,
    storage_bytes: u64,
}

#[derive(Debug)]
pub(crate) struct Spec {
    pub(crate) root: Digest,
    pub(crate) manifest: Digest,
    pub(crate) cpus: u32,
    pub(crate) memory_mib: Memory,
    pub(crate) storage_bytes: u64,
}

#[derive(Debug)]
pub(crate) struct Instance {
    identity: Identity,
}

impl Instance {
    /// Caller holds the image-index lock through disk creation and publication.
    pub(crate) fn create(records: &Records, spec: Spec) -> Result<Self> {
        ensure!(
            records.path().to_str().is_some(),
            "instance storage path must be UTF-8"
        );
        let id = Uuid::new_v4();
        let identity = Identity {
            version: VERSION,
            id,
            root: spec.root,
            manifest: spec.manifest,
            cpus: spec.cpus,
            memory_mib: spec.memory_mib,
            storage_bytes: spec.storage_bytes,
        };
        records.publish(id, &identity, |directory| {
            let disk = directory
                .open_with(
                    DISK,
                    OpenOptions::new().read(true).write(true).create_new(true),
                )?
                .into_std();
            // Keep synchronous disk preparation inside publication ownership.
            InstanceDisk::create(disk, identity.storage_bytes).context("creating instance disk")
        })?;
        Ok(Self { identity })
    }

    pub(super) fn read(bytes: &[u8], id: Uuid) -> Result<Self> {
        let identity: Identity =
            serde_json::from_slice(bytes).context("invalid instance record")?;
        ensure!(
            identity.version == VERSION,
            "unsupported instance record version"
        );
        ensure!(
            identity.id == id && identity.cpus != 0 && identity.storage_bytes != 0,
            "invalid instance record"
        );
        Ok(Self { identity })
    }

    pub(crate) fn id(&self) -> Uuid {
        self.identity.id
    }

    pub(super) fn images(&self) -> Vec<Digest> {
        let mut images = vec![self.identity.root.clone()];
        if self.identity.manifest != self.identity.root {
            images.push(self.identity.manifest.clone());
        }
        images
    }
}
