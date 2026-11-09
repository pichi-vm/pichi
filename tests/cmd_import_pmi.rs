// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `pichi import pmi` integration tests (BUILD.md §15).
//!
//! `import pmi` turns a pre-built PMI (+ optional boot configuration and launch config)
//! into a bootable artifact. Without `--carapace` the result is PMI-only
//! (bootable, no rootfs); with `--carapace <ref>` the referenced carapace's
//! scutes are combined in. The carapace reference is read-only — its tag is
//! never modified — and the bootable artifact is always its own `-t`.

const TEST_ARCH: &str = if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "amd64"
};

#[path = "support/index.rs"]
mod index;

use pichi::cmd::manifest_ext::ManifestExt as _;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use conglobate::artifact::{Layer, Manifest};
use pichi_storage::{BlobSidecarExt as _, BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};
use tempfile::TempDir;

fn graphroot(tmp: &TempDir) -> PathBuf {
    let p = tmp.path().join("pichi").join("storage");
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn write_raw(path: &Path) {
    const CHUNK: usize = 16 * 1024;
    let mut buf = vec![0u8; CHUNK * 64];
    buf[5 * CHUNK..6 * CHUNK].fill(0xA1);
    std::fs::write(path, &buf).unwrap();
}

fn pichi(tmp: &TempDir) -> Command {
    let mut c = Command::cargo_bin("pichi").unwrap();
    c.env("XDG_DATA_HOME", tmp.path());
    c
}

async fn resolve_manifest(g: &Path, key: &str) -> Manifest {
    let db = FilesystemTagDb::open(g).unwrap();
    let digest = db
        .resolve_tag(key)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("tag must resolve: {key}"));
    let blob_store = FilesystemBlobStore::new(g);
    let (_, bytes) = index::manifest(&blob_store, &digest).await;
    Manifest::decode_json(bytes.as_slice()).unwrap()
}

/// Fixture: a raw image, a PMI, and a boot configuration in `tmp`.
fn fixtures(tmp: &TempDir) -> (PathBuf, PathBuf, PathBuf) {
    let raw = tmp.path().join("input.raw");
    write_raw(&raw);
    let pmi = tmp.path().join("boot.pmi");
    std::fs::write(&pmi, b"opaque PMI").unwrap();
    let bootconfig = tmp.path().join("base.bootconfig");
    std::fs::write(&bootconfig, b"kernel.console = \"hvc0\";\n").unwrap();
    (raw, pmi, bootconfig)
}

/// Kind 2: `import pmi --carapace` produces a bootable rootfs appliance under
/// its own tag, and leaves the carapace tag untouched (never overwritten).
#[tokio::test]
async fn import_pmi_with_carapace_keeps_base_untouched() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let (raw, pmi, bootconfig) = fixtures(&tmp);

    pichi(&tmp)
        .args([
            "import",
            "raw",
            "--arch",
            TEST_ARCH,
            raw.to_str().unwrap(),
            "-t",
            "app:base",
        ])
        .assert()
        .success();
    pichi(&tmp)
        .args([
            "import",
            "pmi",
            "--arch",
            TEST_ARCH,
            pmi.to_str().unwrap(),
            "--bootconfig",
            bootconfig.to_str().unwrap(),
            "--carapace",
            "app:base",
            "-t",
            "app:1",
            "--annotation",
            "dev.pichi.carapace.verity.hash=caller-cannot-replace-root",
        ])
        .assert()
        .success();

    // The carapace tag is unchanged — still a non-bootable carapace.
    let base = resolve_manifest(&g, "docker.io/library/app:base").await;
    assert!(
        !base.layers.iter().any(|l| matches!(l, Layer::Pmi(_))),
        "--carapace must not modify the carapace tag"
    );
    assert!(base.layers.iter().any(|l| matches!(l, Layer::Scute(_))));

    // The appliance carries scute + pmi + bootconfig.
    let app = resolve_manifest(&g, "docker.io/library/app:1").await;
    assert_eq!(app.layers.len(), 3, "scute + pmi + bootconfig");
    assert!(app.layers.iter().any(|l| matches!(l, Layer::Scute(_))));
    assert!(app.layers.iter().any(|l| matches!(l, Layer::Pmi(_))));
    assert!(app.layers.iter().any(|l| matches!(l, Layer::BootConfig(_))));
    let root = base
        .annotations
        .get("dev.pichi.carapace.verity.hash")
        .unwrap();
    assert_eq!(root.len(), 71);
    assert!(root.starts_with("sha256:"));
    assert_eq!(
        app.annotations.get("dev.pichi.carapace.verity.hash"),
        Some(root)
    );

    // A new composition must authenticate inherited bytes even when its
    // source image and prepared verity are already cached.
    let scute = base.scute_layers()[0];
    let store = FilesystemBlobStore::new(&g);
    std::fs::write(
        store.blob_path(&scute.digest_str().parse().unwrap()),
        vec![b'x'; scute.size() as usize],
    )
    .unwrap();
    let failed = pichi(&tmp)
        .args([
            "import",
            "pmi",
            "--arch",
            TEST_ARCH,
            pmi.to_str().unwrap(),
            "--carapace",
            "app:base",
            "-t",
            "app:bad",
        ])
        .assert()
        .failure();
    assert!(String::from_utf8_lossy(&failed.get_output().stderr).contains("digest mismatch"));
    assert!(
        FilesystemTagDb::open(&g)
            .unwrap()
            .resolve_tag("docker.io/library/app:bad")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        resolve_manifest(&g, "docker.io/library/app:base").await,
        base
    );
    assert_eq!(resolve_manifest(&g, "docker.io/library/app:1").await, app);
}

#[tokio::test]
async fn legacy_carapace_composition_emits_only_a_qualified_root() {
    use conglobate::artifact::Digest;
    use sha2::{Digest as _, Sha256};
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let (raw, pmi, _) = fixtures(&tmp);
    pichi(&tmp)
        .args([
            "import",
            "raw",
            "--arch",
            TEST_ARCH,
            raw.to_str().unwrap(),
            "-t",
            "base:1",
        ])
        .assert()
        .success();
    let mut legacy = resolve_manifest(&g, "docker.io/library/base:1").await;
    let root = legacy.carapace_root().unwrap().unwrap();
    legacy
        .annotations
        .insert("dev.pichi.carapace.verity.hash".into(), hex::encode(root));
    legacy
        .annotations
        .insert("dev.pichi.carapace.verity.algo".into(), "sha256".into());
    let store = FilesystemBlobStore::new(&g);
    let bytes = serde_json::to_vec(&legacy).unwrap();
    let digest = Digest::Sha256(Sha256::digest(&bytes).into());
    store.put_blob(&digest, &bytes).await.unwrap();
    let db = FilesystemTagDb::open(&g).unwrap();
    let original = db
        .resolve_tag("docker.io/library/base:1")
        .await
        .unwrap()
        .unwrap();
    let mut index: serde_json::Value =
        serde_json::from_slice(&store.get_blob(&original).await.unwrap()).unwrap();
    index["manifests"][0]["digest"] = digest.to_string().into();
    index["manifests"][0]["size"] = bytes.len().into();
    let index = serde_json::to_vec(&index).unwrap();
    let digest = Digest::Sha256(Sha256::digest(&index).into());
    store.put_blob(&digest, &index).await.unwrap();
    FilesystemTagDb::open(&g)
        .unwrap()
        .set_tag("docker.io/library/legacy:1", &digest)
        .await
        .unwrap();
    pichi(&tmp)
        .args([
            "import",
            "pmi",
            "--arch",
            TEST_ARCH,
            pmi.to_str().unwrap(),
            "--carapace",
            "legacy:1",
            "-t",
            "app:1",
        ])
        .assert()
        .success();
    let app = resolve_manifest(&g, "docker.io/library/app:1").await;
    assert_eq!(app.carapace_root().unwrap(), Some(root));
    assert_eq!(
        app.annotations["dev.pichi.carapace.verity.hash"],
        Digest::Sha256(root).to_string()
    );
    assert!(
        !app.annotations
            .contains_key("dev.pichi.carapace.verity.algo")
    );
    assert_eq!(
        resolve_manifest(&g, "docker.io/library/legacy:1").await,
        legacy
    );
}

#[tokio::test]
async fn composition_omits_inherited_default_parameters() {
    use conglobate::artifact::Digest;
    use sha2::{Digest as _, Sha256};
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let (raw, pmi, _) = fixtures(&tmp);
    pichi(&tmp)
        .args([
            "import",
            "raw",
            "--arch",
            TEST_ARCH,
            raw.to_str().unwrap(),
            "-t",
            "base:1",
        ])
        .assert()
        .success();
    let mut source = resolve_manifest(&g, "docker.io/library/base:1").await;
    for (key, value) in [
        ("data-block-size", "4096"),
        ("hash-block-size", "4096"),
        ("version", "1"),
        ("hash-type", "1"),
    ] {
        source
            .annotations
            .insert(format!("dev.pichi.carapace.verity.{key}"), value.into());
    }
    let store = FilesystemBlobStore::new(&g);
    let bytes = serde_json::to_vec(&source).unwrap();
    let digest = Digest::Sha256(Sha256::digest(&bytes).into());
    store.put_blob(&digest, &bytes).await.unwrap();
    pichi(&tmp)
        .args([
            "import",
            "pmi",
            "--arch",
            TEST_ARCH,
            pmi.to_str().unwrap(),
            "--carapace",
            &digest.to_string(),
            "-t",
            "app:1",
        ])
        .assert()
        .success();
    let app = resolve_manifest(&g, "docker.io/library/app:1").await;
    assert_eq!(
        app.carapace_root().unwrap(),
        source.carapace_root().unwrap()
    );
    for key in ["data-block-size", "hash-block-size", "version", "hash-type"] {
        assert!(
            !app.annotations
                .contains_key(&format!("dev.pichi.carapace.verity.{key}"))
        );
    }
    assert_eq!(store.get_blob(&digest).await.unwrap(), bytes);
}

/// Kind 3: `import pmi` without `--carapace` produces a PMI-only bootable
/// artifact — no scute layers, no carapace verity annotations.
#[tokio::test]
async fn import_pmi_without_carapace_keeps_only_boot_components() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let (_raw, pmi, bootconfig) = fixtures(&tmp);

    pichi(&tmp)
        .args([
            "import",
            "pmi",
            "--arch",
            TEST_ARCH,
            pmi.to_str().unwrap(),
            "--bootconfig",
            bootconfig.to_str().unwrap(),
            "-t",
            "boot:1",
        ])
        .assert()
        .success();

    let m = resolve_manifest(&g, "docker.io/library/boot:1").await;
    assert_eq!(m.layers.len(), 2, "pmi + bootconfig only");
    assert!(m.layers.iter().any(|l| matches!(l, Layer::Pmi(_))));
    assert!(m.layers.iter().any(|l| matches!(l, Layer::BootConfig(_))));
    assert!(
        !m.layers.iter().any(|l| matches!(l, Layer::Scute(_))),
        "PMI-only artifact has no scutes"
    );
    assert!(
        !m.annotations.contains_key("dev.pichi.carapace.verity.hash"),
        "PMI-only artifact declares no carapace verity hash"
    );
}

/// `import raw` without `-t` caches an ephemeral carapace (no tag) and prints
/// the index digest — which is directly usable as a `--carapace` reference.
#[tokio::test]
async fn import_raw_without_tag_prints_digest_usable_as_carapace() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let raw = tmp.path().join("input.raw");
    write_raw(&raw);
    let pmi = tmp.path().join("boot.pmi");
    std::fs::write(&pmi, b"opaque PMI").unwrap();
    let bootconfig = tmp.path().join("base.bootconfig");
    std::fs::write(&bootconfig, b"kernel.console = \"hvc0\";\n").unwrap();

    let out = pichi(&tmp)
        .args(["import", "raw", "--arch", TEST_ARCH, raw.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let digest = String::from_utf8(out).unwrap();
    let digest = digest.trim();
    assert!(
        digest.starts_with("sha256:") && digest.len() == "sha256:".len() + 64,
        "import prints the index digest: {digest:?}"
    );

    // No tags were created.
    let db = FilesystemTagDb::open(&g).unwrap();
    assert!(
        db.list_tags().await.unwrap().is_empty(),
        "untagged import must not create a tag"
    );

    // The printed digest works directly as a --carapace reference.
    pichi(&tmp)
        .args([
            "import",
            "pmi",
            "--arch",
            TEST_ARCH,
            pmi.to_str().unwrap(),
            "--bootconfig",
            bootconfig.to_str().unwrap(),
            "--carapace",
            digest,
            "-t",
            "app:1",
        ])
        .assert()
        .success();
    let app = resolve_manifest(&g, "docker.io/library/app:1").await;
    assert!(app.layers.iter().any(|l| matches!(l, Layer::Scute(_))));
    assert!(app.layers.iter().any(|l| matches!(l, Layer::Pmi(_))));
}

/// `--config` stores the launch contract as the manifest config blob
/// (`vnd.pichi.config.v1+json`) instead of the OCI empty config.
#[tokio::test]
async fn import_pmi_with_config_sets_config_blob() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let (_raw, pmi, bootconfig) = fixtures(&tmp);
    let cfg = tmp.path().join("config.yaml");
    let mut configs = Vec::new();
    for text in [
        "version: 1\nrequirements:\n  memory:\n    required: 512\n  cpus:\n    required: 2\n",
        r#"{"version":1,"requirements":{"memory":{"required":512},"cpus":{"required":2}}}"#,
        "version: 1\nrequirements:\n  memory: &minimum\n    required: 512\n  cpus: *minimum\n",
    ] {
        std::fs::write(&cfg, text).unwrap();

        pichi(&tmp)
            .args([
                "import",
                "pmi",
                "--arch",
                TEST_ARCH,
                pmi.to_str().unwrap(),
                "--bootconfig",
                bootconfig.to_str().unwrap(),
                "--config",
                cfg.to_str().unwrap(),
                "-t",
                "boot:1",
            ])
            .assert()
            .success();

        let m = resolve_manifest(&g, "docker.io/library/boot:1").await;
        assert!(
            !m.config.is_empty(),
            "config blob must be a real vnd.pichi.config.v1+json, not the empty config"
        );
        assert_eq!(m.config.media_type, "application/vnd.pichi.config.v1+json");
        let bytes = FilesystemBlobStore::new(&g)
            .get_blob(&m.config.digest.parse().unwrap())
            .await
            .unwrap();
        let config: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(config["requirements"]["memory"]["required"], 512);
        configs.push(bytes);
    }
    assert_eq!(configs[0], configs[1]);
    let alias: serde_json::Value = serde_json::from_slice(&configs[2]).unwrap();
    assert_eq!(alias["requirements"]["cpus"]["required"], 512);
}

#[test]
fn import_pmi_config_errors_include_path_and_location() {
    let tmp = TempDir::new().unwrap();
    let (_raw, pmi, bootconfig) = fixtures(&tmp);
    let cfg = tmp.path().join("invalid.yaml");
    for text in [
        "version: 1\nrequirements: [\n",
        "version: 1\nrequirements:\n  memory:\n    required: invalid\n",
        "version: 1\n<<: {requirements: {}}\n",
    ] {
        std::fs::write(&cfg, text).unwrap();
        let output = pichi(&tmp)
            .args(["import", "pmi", "--arch", TEST_ARCH])
            .arg(&pmi)
            .arg("--bootconfig")
            .arg(&bootconfig)
            .arg("--config")
            .arg(&cfg)
            .assert()
            .failure()
            .get_output()
            .stderr
            .clone();
        let diagnostic = String::from_utf8(output).unwrap();
        assert!(diagnostic.contains(cfg.to_str().unwrap()), "{diagnostic}");
        assert!(diagnostic.contains("line"), "{diagnostic}");
        assert!(diagnostic.contains("column"), "{diagnostic}");
    }
}

/// `import pmi` without `-t` caches the bootable artifact untagged and prints
/// its index digest (consistent with `import raw`).
#[tokio::test]
async fn import_pmi_without_tag_prints_digest_and_leaves_no_tag() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let (_raw, pmi, bootconfig) = fixtures(&tmp);

    let out = pichi(&tmp)
        .args([
            "import",
            "pmi",
            "--arch",
            TEST_ARCH,
            pmi.to_str().unwrap(),
            "--bootconfig",
            bootconfig.to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let digest = String::from_utf8(out).unwrap();
    assert!(
        digest.trim().starts_with("sha256:"),
        "prints the index digest: {digest:?}"
    );

    let db = FilesystemTagDb::open(&g).unwrap();
    assert!(
        db.list_tags().await.unwrap().is_empty(),
        "untagged import pmi must not create a tag"
    );
}

/// A missing PMI file aborts before any tag is created (no partial state).
#[tokio::test]
async fn import_pmi_missing_file_errors() {
    let tmp = TempDir::new().unwrap();
    let g = graphroot(&tmp);
    let (_raw, _pmi, bootconfig) = fixtures(&tmp);

    pichi(&tmp)
        .args([
            "import",
            "pmi",
            "--arch",
            TEST_ARCH,
            "/nonexistent/boot.pmi",
            "--bootconfig",
            bootconfig.to_str().unwrap(),
            "-t",
            "boot:1",
        ])
        .assert()
        .failure();

    let db = FilesystemTagDb::open(&g).unwrap();
    assert!(
        db.list_tags().await.unwrap().is_empty(),
        "no tag on failure"
    );
}

/// `--carapace` pointing at a missing reference fails cleanly.
#[tokio::test]
async fn import_pmi_unknown_carapace_errors() {
    let tmp = TempDir::new().unwrap();
    let _g = graphroot(&tmp);
    let (_raw, pmi, bootconfig) = fixtures(&tmp);

    pichi(&tmp)
        .args([
            "import",
            "pmi",
            "--arch",
            TEST_ARCH,
            pmi.to_str().unwrap(),
            "--bootconfig",
            bootconfig.to_str().unwrap(),
            "--carapace",
            "nope:1",
            "-t",
            "app:1",
        ])
        .assert()
        .failure();
}

#[tokio::test]
async fn import_omits_absent_and_empty_bootconfig() {
    for empty in [false, true] {
        let tmp = TempDir::new().unwrap();
        let g = graphroot(&tmp);
        let (_, pmi, bootconfig) = fixtures(&tmp);
        std::fs::write(&bootconfig, b"").unwrap();
        let mut command = pichi(&tmp);
        command.args([
            "import",
            "pmi",
            "--arch",
            TEST_ARCH,
            pmi.to_str().unwrap(),
            "-t",
            "boot:1",
        ]);
        if empty {
            command.args(["--bootconfig", bootconfig.to_str().unwrap()]);
        }
        command.assert().success();
        let image = resolve_manifest(&g, "docker.io/library/boot:1").await;
        assert!(matches!(image.layers.as_slice(), [Layer::Pmi(_)]));
    }
}

#[tokio::test]
async fn native_boot_components_survive_save_and_load_exactly() {
    let source = TempDir::new().unwrap();
    let source_root = graphroot(&source);
    let (_, pmi, bootconfig) = fixtures(&source);
    pichi(&source)
        .args([
            "import",
            "pmi",
            "--arch",
            TEST_ARCH,
            pmi.to_str().unwrap(),
            "--bootconfig",
            bootconfig.to_str().unwrap(),
            "-t",
            "boot:1",
        ])
        .assert()
        .success();
    // Native text must be ready when import returns, before any launch or
    // export can prepare it as a side effect.
    let before = resolve_manifest(&source_root, "docker.io/library/boot:1").await;
    let config = before
        .layers
        .iter()
        .find(|layer| matches!(layer, Layer::BootConfig(_)))
        .unwrap();
    let native = FilesystemBlobStore::new(&source_root)
        .blob_path(&config.digest_str().parse().unwrap())
        .prepared_dir()
        .join("bootconfig");
    assert_eq!(
        std::fs::read(native).unwrap(),
        std::fs::read(&bootconfig).unwrap()
    );
    let layout = source.path().join("saved");
    pichi(&source)
        .args(["save", "boot:1", "-o", layout.to_str().unwrap()])
        .assert()
        .success();
    let destination = TempDir::new().unwrap();
    let destination_root = graphroot(&destination);
    pichi(&destination)
        .args(["load", layout.to_str().unwrap(), "--tag", "boot:copy"])
        .assert()
        .success();
    let after = resolve_manifest(&destination_root, "docker.io/library/boot:copy").await;
    assert_eq!(before, after);
    for layer in &after.layers {
        let digest = layer.digest_str().parse().unwrap();
        assert_eq!(
            std::fs::read(FilesystemBlobStore::new(&source_root).blob_path(&digest)).unwrap(),
            std::fs::read(FilesystemBlobStore::new(&destination_root).blob_path(&digest)).unwrap()
        );
    }
}

#[tokio::test]
async fn publication_rechecks_cached_bootconfig_bytes() {
    let source = TempDir::new().unwrap();
    let g = graphroot(&source);
    let (_, pmi, bootconfig) = fixtures(&source);
    pichi(&source)
        .args([
            "import",
            "pmi",
            "--arch",
            TEST_ARCH,
            pmi.to_str().unwrap(),
            "--bootconfig",
            bootconfig.to_str().unwrap(),
            "-t",
            "boot:1",
        ])
        .assert()
        .success();
    let manifest = resolve_manifest(&g, "docker.io/library/boot:1").await;
    let layer = manifest
        .layers
        .iter()
        .find(|layer| matches!(layer, Layer::BootConfig(_)))
        .unwrap();
    std::fs::write(
        FilesystemBlobStore::new(&g).blob_path(&layer.digest_str().parse().unwrap()),
        vec![b'x'; layer.size() as usize],
    )
    .unwrap();
    let failed = pichi(&source)
        .args([
            "import",
            "pmi",
            "--arch",
            TEST_ARCH,
            pmi.to_str().unwrap(),
            "--bootconfig",
            bootconfig.to_str().unwrap(),
            "-t",
            "boot:bad",
        ])
        .assert()
        .failure();
    assert!(String::from_utf8_lossy(&failed.get_output().stderr).contains("digest mismatch"));
    assert!(
        FilesystemTagDb::open(&g)
            .unwrap()
            .resolve_tag("docker.io/library/boot:bad")
            .await
            .unwrap()
            .is_none()
    );
    let output = source.path().join("invalid-export");
    let failed = pichi(&source)
        .args(["save", "boot:1", "-o", output.to_str().unwrap()])
        .assert()
        .failure();
    assert!(String::from_utf8_lossy(&failed.get_output().stderr).contains("digest mismatch"));
    assert!(!output.exists());
}

#[tokio::test]
async fn empty_bootconfig_request_rechecks_the_cached_pmi() {
    let source = TempDir::new().unwrap();
    let root = graphroot(&source);
    let (_, pmi, config) = fixtures(&source);
    pichi(&source)
        .args([
            "import",
            "pmi",
            "--arch",
            TEST_ARCH,
            pmi.to_str().unwrap(),
            "-t",
            "boot:1",
        ])
        .assert()
        .success();
    let manifest = resolve_manifest(&root, "docker.io/library/boot:1").await;
    let layer = &manifest.layers[0];
    std::fs::write(
        FilesystemBlobStore::new(&root).blob_path(&layer.digest_str().parse().unwrap()),
        vec![b'x'; layer.size() as usize],
    )
    .unwrap();
    std::fs::write(&config, b"").unwrap();
    let failed = pichi(&source)
        .args([
            "import",
            "pmi",
            "--arch",
            TEST_ARCH,
            pmi.to_str().unwrap(),
            "--bootconfig",
            config.to_str().unwrap(),
            "-t",
            "boot:bad",
        ])
        .assert()
        .failure();
    assert!(String::from_utf8_lossy(&failed.get_output().stderr).contains("digest mismatch"));
    assert!(
        FilesystemTagDb::open(&root)
            .unwrap()
            .resolve_tag("docker.io/library/boot:bad")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn imports_target_architecture_independently_of_host() {
    for (machine, arch) in [(0x8664u16, "amd64"), (0xaa64, "arm64")] {
        let tmp = TempDir::new().unwrap();
        let g = graphroot(&tmp);
        let (raw, pmi, _) = fixtures(&tmp);
        // Minimal PE32+ header: enough to identify the target, not a bootable PMI.
        let mut pe = vec![0; 512];
        pe[..2].copy_from_slice(b"MZ");
        pe[60..64].copy_from_slice(&128u32.to_le_bytes());
        pe[128..132].copy_from_slice(b"PE\0\0");
        pe[132..134].copy_from_slice(&machine.to_le_bytes());
        pe[148..150].copy_from_slice(&240u16.to_le_bytes());
        pe[152..154].copy_from_slice(&0x20bu16.to_le_bytes());
        std::fs::write(&pmi, pe).unwrap();
        pichi(&tmp)
            .args([
                "import",
                "raw",
                raw.to_str().unwrap(),
                "--arch",
                arch,
                "-t",
                "base:1",
            ])
            .assert()
            .success();
        pichi(&tmp)
            .args([
                "import",
                "pmi",
                pmi.to_str().unwrap(),
                "--carapace",
                "base:1",
                "-t",
                "boot:1",
            ])
            .assert()
            .success();
        let db = FilesystemTagDb::open(&g).unwrap();
        let digest = db
            .resolve_tag("docker.io/library/boot:1")
            .await
            .unwrap()
            .unwrap();
        let bytes = FilesystemBlobStore::new(&g)
            .get_blob(&digest)
            .await
            .unwrap();
        let index: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(index["manifests"][0]["platform"]["architecture"], arch);
        let wrong = if arch == "amd64" { "arm64" } else { "amd64" };
        pichi(&tmp)
            .args(["import", "pmi", pmi.to_str().unwrap(), "--arch", wrong])
            .assert()
            .failure();
    }
}

#[test]
fn unknown_import_architecture_requires_explicit_choice() {
    let tmp = TempDir::new().unwrap();
    let (raw, pmi, _) = fixtures(&tmp);
    for (kind, path) in [("raw", raw), ("pmi", pmi)] {
        let output = pichi(&tmp)
            .args(["import", kind])
            .arg(path)
            .assert()
            .failure();
        assert!(String::from_utf8_lossy(&output.get_output().stderr).contains("--arch"));
    }
}
