// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Publisher-facing root output, computed from the cached layer graph.

use assert_cmd::Command;
use pichi::cmd::manifest_ext::ManifestExt as _;
use pichi_artifact::{Digest, Layer, Manifest};
use pichi_storage::{BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};
use tempfile::TempDir;

struct Fixture {
    tmp: TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            tmp: TempDir::new().unwrap(),
        }
    }

    fn pichi(&self) -> Command {
        let mut command = Command::cargo_bin("pichi").unwrap();
        command.env("XDG_DATA_HOME", self.tmp.path());
        command.env("RUST_LOG", "info");
        command
    }

    fn store(&self) -> FilesystemBlobStore {
        FilesystemBlobStore::new(self.tmp.path().join("pichi/storage"))
    }

    async fn manifest(&self, tag: &str) -> Manifest {
        let db = FilesystemTagDb::open(self.tmp.path().join("pichi/storage")).unwrap();
        let digest = db.resolve_tag(tag).await.unwrap().unwrap();
        let bytes = self.store().get_blob(&digest).await.unwrap();
        Manifest::from_reader_validated(bytes.as_slice()).unwrap()
    }
}

#[tokio::test]
async fn import_print_root_is_one_hex_line_with_or_without_quiet() {
    for quiet in [false, true] {
        let fixture = Fixture::new();
        let raw = fixture.tmp.path().join("root.raw");
        std::fs::write(&raw, vec![0x5a; 8193]).unwrap();
        let mut command = fixture.pichi();
        command.args([
            "import",
            "raw",
            raw.to_str().unwrap(),
            "-t",
            "app:base",
            "--salt",
            "deadbeef",
            "--print-root",
            "-a",
            "dev.pichi.carapace.verity.hash=ignored",
        ]);
        if quiet {
            command.arg("--quiet");
        }
        let output = command.assert().success().get_output().clone();
        let root = std::str::from_utf8(&output.stdout).unwrap();
        assert_eq!(root.len(), 65);
        assert_eq!(&root[64..], "\n");
        assert!(
            root[..64]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
        assert_eq!(output.stderr.is_empty(), quiet);

        assert_eq!(
            std::fs::read_dir(fixture.tmp.path().join("pichi/storage/blobs/sha256"))
                .unwrap()
                .count(),
            3
        );
        let manifest = fixture.manifest("docker.io/library/app:base").await;
        assert!(
            !manifest
                .annotations
                .contains_key("dev.pichi.carapace.verity.hash")
        );
        let expected = manifest.carapace_top_root(&fixture.store()).await.unwrap();
        assert_eq!(root, format!("{}\n", hex::encode(expected)));
        fixture
            .pichi()
            .args(["inspect", "app:base", "--print-root"])
            .assert()
            .success()
            .stdout(root.to_owned())
            .stderr("");
    }
}

#[tokio::test]
async fn inspect_print_root_uses_graph_tip_even_when_layers_are_reversed() {
    let fixture = Fixture::new();
    for (tag, byte) in [("app:base", 0x5a), ("app:child", 0xc3)] {
        let raw = fixture.tmp.path().join("root.raw");
        std::fs::write(&raw, vec![byte; 8193]).unwrap();
        fixture
            .pichi()
            .args(["import", "raw", raw.to_str().unwrap(), "-t", tag, "--quiet"])
            .assert()
            .success();
    }
    let store = fixture.store();
    let mut base = fixture.manifest("docker.io/library/app:base").await;
    let base_root = base.carapace_top_root(&store).await.unwrap();
    let mut child = fixture.manifest("docker.io/library/app:child").await;
    let Layer::Scute(scute) = &mut child.layers[0] else {
        panic!("expected scute")
    };
    scute.annotations.salt = hex::encode(base_root);
    let expected = child
        .cached_scute_root(&child.layers[0], &store)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(expected, base_root);
    // Tip first: choosing the first/last layer's salt or the base root is wrong.
    base.layers.insert(0, child.layers.remove(0));
    assert_eq!(base.carapace_top_root(&store).await.unwrap(), expected);
    let bytes = base.to_bytes().unwrap();
    let digest = Digest::from_bytes_sha256(&bytes);
    store.put_blob(&digest, &bytes).await.unwrap();
    fixture
        .pichi()
        .args(["inspect", &format!("app@{digest}"), "--print-root"])
        .assert()
        .success()
        .stdout(format!("{}\n", hex::encode(expected)))
        .stderr("");
}

#[test]
fn inspect_print_root_rejects_pmi_only_artifact() {
    let fixture = Fixture::new();
    let pmi = fixture.tmp.path().join("boot.pmi");
    let dtb = fixture.tmp.path().join("base.dtb");
    std::fs::write(&pmi, [0xc3; 4096]).unwrap();
    std::fs::write(&dtb, [0xd7; 4096]).unwrap();
    fixture
        .pichi()
        .args([
            "import",
            "pmi",
            pmi.to_str().unwrap(),
            "--dtb",
            dtb.to_str().unwrap(),
            "-t",
            "boot:1",
        ])
        .assert()
        .success();
    let output = fixture
        .pichi()
        .args(["inspect", "boot:1", "--print-root"])
        .assert()
        .failure()
        .get_output()
        .clone();
    assert!(output.stdout.is_empty());
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    assert!(
        stderr.contains("requires cached carapace layers"),
        "{stderr}"
    );
    assert!(stderr.contains("no top scute"), "{stderr}");
}
