// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! The MVP gate: `pichi build` (through conglobate) produces a carapace, and
//! `pichi run` boots an application artifact whose root carapace **corium
//! assembles and mounts in the initramfs**.
//!
//! Flow: import a marked ext4 → `pichi build` a from:-only carapace (the
//! source scutes pass through) → seal a corium PMI whose cmdline carries
//! `root.carapace=<the carapace's top root>` → package the built carapace's
//! scutes + that PMI into one application artifact → `pichi run` it. corium
//! reads the root off the cmdline, assembles the carapace, mounts it, and
//! prints its root listing. x86_64 + KVM only.

#![cfg(all(feature = "vm-tests", target_os = "linux", target_arch = "x86_64"))]

mod common;

use std::path::{Path, PathBuf};

use common::{
    CORIUM_BIN, DILLO_BIN, assemble_initrd_with, assert_pichi_ok, kvm_available, make_ext4, pichi,
    pull_build_image, seal,
};
use corium::CORIUM_ROOT_OK;
use pichi_artifact::{Digest, Layer, Manifest, PmiDescriptor, Reference};
use pichi_storage::{BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};

const MARKER: &str = "corium-root-marker";

/// The graphroot the `pichi` binary uses under `XDG_DATA_HOME=<xdg>`.
fn storage(xdg: &Path) -> PathBuf {
    xdg.join("pichi/storage")
}

/// Read `carapace: sha256:<hex>` for `base:1` from a build project's refs.lock.
fn carapace_root(refs_lock: &Path) -> String {
    let body = std::fs::read_to_string(refs_lock).unwrap();
    body.lines()
        .find_map(|l| l.trim().strip_prefix("carapace: sha256:"))
        .expect("refs.lock has a carapace root")
        .to_string()
}

/// Package the built carapace `cara_tag`'s scutes + the corium PMI into one
/// application artifact tagged `app_tag`.
fn package_app(xdg: &Path, cara_tag: &str, app_tag: &str, pmi: &Path) {
    let graphroot = storage(xdg);
    let blob_store = FilesystemBlobStore::new(&graphroot);
    let db = FilesystemTagDb::open(&graphroot).unwrap();

    let cara_key = cara_tag.parse::<Reference>().unwrap().to_string();
    let cara_digest = db
        .resolve_tag(&cara_key)
        .await
        .unwrap()
        .expect("carapace tag resolves");
    let mut manifest = Manifest::from_reader_validated(
        blob_store.get_blob(&cara_digest).await.unwrap().as_slice(),
    )
    .unwrap();

    let pmi_bytes = std::fs::read(pmi).unwrap();
    let pmi_digest = Digest::from_bytes_sha256(&pmi_bytes);
    blob_store.put_blob(&pmi_digest, &pmi_bytes).await.unwrap();
    manifest.layers.push(Layer::Pmi(PmiDescriptor {
        digest: pmi_digest.to_string(),
        size: pmi_bytes.len() as u64,
    }));

    manifest.validate().unwrap();
    let bytes = manifest.to_bytes().unwrap();
    let digest = Digest::from_bytes_sha256(&bytes);
    blob_store.put_blob(&digest, &bytes).await.unwrap();
    db.set_tag(
        &app_tag.parse::<Reference>().await.unwrap().to_string(),
        &digest,
    )
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "pichi build deferred; re-enable with build work"]
#[cfg(target_arch = "x86_64")]
async fn pichi_build_then_run_mounts_root_carapace() {
    if !kvm_available() {
        eprintln!("skip: no usable /dev/kvm");
        return;
    }
    let tmp = tempfile::TempDir::new().unwrap();
    let xdg = tmp.path().join("xdg");
    std::fs::create_dir_all(&xdg).unwrap();
    let g = &xdg;

    // --- Build image (conglobate): the real official image from GHCR. ---
    pull_build_image(g);

    // --- Source carapace: a marked ext4. ---
    let src = tmp.path().join("source.ext4");
    make_ext4(&src, &[(MARKER, "hello from the root carapace\n")]);
    assert_pichi_ok(
        "import source",
        &pichi(
            g,
            &[],
            &["import", "raw", src.to_str().unwrap(), "-t", "base:1"],
        ),
    );

    // --- pichi build the carapace (from:-only → source passes through). ---
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(proj.join("pichi.build")).unwrap();
    std::fs::write(proj.join("pichi.build/carapace.yaml"), "from: base:1\n").unwrap();
    assert_pichi_ok(
        "update",
        &pichi(g, &[], &["update", proj.to_str().unwrap()]),
    );
    assert_pichi_ok(
        "build",
        &pichi(
            g,
            &[],
            &[
                "build",
                proj.to_str().unwrap(),
                "-t",
                "appcara:1",
                "--memory",
                "2048",
                "--cpus",
                "1",
            ],
        ),
    );

    // --- Seal a corium PMI carrying root.carapace=<carapace top root>. ---
    let root = carapace_root(&proj.join("pichi.build/refs.lock"));
    let cor = tmp.path().join("cor");
    std::fs::create_dir_all(&cor).unwrap();
    let cor_initrd = assemble_initrd_with(&cor, CORIUM_BIN, &["dm_verity"]);
    let cor_pmi = seal(
        &cor,
        &cor_initrd,
        &format!("console=hvc0 root.carapace={root}"),
    );

    // --- Package the built carapace + corium PMI into an app artifact. ---
    package_app(&xdg, "appcara:1", "app:1", &cor_pmi);

    // --- pichi run: corium must assemble + mount the root carapace. ---
    let out_path = tmp.path().join("run.out");
    let err_path = tmp.path().join("run.err");
    let mut child = std::process::Command::new(common::PICHI_BIN)
        .env("XDG_DATA_HOME", &xdg)
        .env("PICHI_DILLO", DILLO_BIN)
        .args(["run", "app:1", "--memory", "1024", "--cpus", "1"])
        .stdout(std::fs::File::create(&out_path).await.unwrap())
        .stderr(std::fs::File::create(&err_path).await.unwrap())
        .spawn()
        .expect("spawn pichi run");
    let status = {
        use wait_timeout::ChildExt;
        let Some(s) = child
            .wait_timeout(std::time::Duration::from_secs(180))
            .unwrap()
        else {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "pichi run timed out:\n{}",
                common::read_combined(&out_path, &err_path)
            );
        };
        s
    };
    let combined = common::read_combined(&out_path, &err_path);
    assert!(
        status.success(),
        "pichi run exited non-zero ({status}):\n{combined}"
    );
    assert!(
        combined.contains(CORIUM_ROOT_OK),
        "corium did not mount the root carapace:\n{combined}"
    );
    assert!(
        combined.contains(MARKER),
        "mounted root carapace did not show the marker file {MARKER:?}:\n{combined}"
    );
}
