// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use pichi_artifact::Digest;
use pichi_storage::{BlobStore, FilesystemBlobStore};

/// Read a local import's single manifest while asserting its index descriptor.
pub(super) async fn manifest(store: &FilesystemBlobStore, index: &Digest) -> (Digest, Vec<u8>) {
    let bytes = store.get_blob(index).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        value["mediaType"],
        "application/vnd.oci.image.index.v1+json"
    );
    let entries = value["manifests"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    let digest: Digest = entries[0]["digest"].as_str().unwrap().parse().unwrap();
    let bytes = store.get_blob(&digest).await.unwrap();
    assert_eq!(entries[0]["size"], bytes.len());
    (digest, bytes)
}
