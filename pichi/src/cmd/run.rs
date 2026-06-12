// SPDX-License-Identifier: Apache-2.0

//! `pichi run <ref>` — boot a cached artifact by exec'ing the `dillo` launcher.
//!
//! pichi *derives* the boot-critical device set from the image manifest; it
//! does not choose it. The PMI layer becomes `--pmi`; the scute layers become
//! one virtualized-GPT (`--gpt`) carapace disk whose partition PARTUUIDs and
//! labels are a pure function of each scute's verity root; a stdio console is
//! dillo's default. The guest verifies itself against exactly those PARTUUIDs
//! (the carapace trust contract), so the values must be reproduced precisely
//! — there is no freedom here. The only runtime knobs are cpus/memory, taken
//! from the CLI, then pichi config, then dillo's built-in defaults.
//!
//! dillo's `--gpt` derives the disk device-id/disk-guid from the partition
//! PARTUUIDs (`sha256(concat(partuuids))[..20]`/`[..16]`), which is exactly
//! the carapace formula, so pichi omits them and lets dillo derive.
//!
//! The assembled invocation is handed to `dillo` via `exec()` (process
//! replacement on Unix; spawn + wait + exit-code propagation elsewhere).

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

use pichi_artifact::{
    Digest, Layer, Manifest, PmiDescriptor, Reference, ReferenceKind, ScuteDescriptor,
};
use pichi_storage::{
    BlobStore, CacheLayout, FilesystemBlobStore, FilesystemTagDb, TagDb, sidecar::verity_path,
};

use crate::cli::RunArgs;
use crate::config::Config;

/// GPT type GUID for scute COW partitions (carapace spec §"GPT Deployment
/// Pattern", registered value). Not defined in a shared crate yet; pinned here.
const SCUTE_COW_TYPEGUID: &str = "11dd804a-e1bf-4ab3-98c1-f9f48ceedbf1";
/// GPT type GUID for scute verity partitions (carapace spec, registered value).
const SCUTE_VERITY_TYPEGUID: &str = "40bb9571-2972-4547-b580-6a8cb13fd7d1";

/// Chain-wide verity block-size annotation keys (mirror pichi-artifact's
/// private consts). carapace D-06 locks both to 4096; we read them anyway and
/// fall back to the lock if absent.
const ANN_DATA_BLOCK_SIZE: &str = "dev.pichi.carapace.verity.data-block-size";
const ANN_HASH_BLOCK_SIZE: &str = "dev.pichi.carapace.verity.hash-block-size";
const DEFAULT_BLOCK_SIZE: u32 = 4096;

/// Standard OCI image-architecture annotation key (SC#3 fail-closed check).
const ARCH_ANNOTATION: &str = "org.opencontainers.image.architecture";

/// `pichi run <ref>` entry point.
pub fn run(args: RunArgs, config: &Config) -> Result<()> {
    let layout = resolve_layout(config)?;
    let db = FilesystemTagDb::open(&layout.graphroot)?;
    let blob_store = FilesystemBlobStore::new(&layout.graphroot);

    // 1. Resolve ref -> manifest digest.
    let target_ref: Reference = args
        .reference
        .parse()
        .with_context(|| format!("invalid reference: {}", args.reference))?;
    let digest = match &target_ref.kind {
        ReferenceKind::Digest(d) => d.clone(),
        ReferenceKind::Tag(_) => {
            let key = target_ref.to_string();
            db.resolve_tag(&key)?
                .ok_or_else(|| anyhow!("ref not in cache: {key}\n  hint: pichi pull {key}"))?
        }
    };

    // 2. Read + validate the manifest snapshot.
    let bytes = blob_store
        .get_blob(&digest)
        .with_context(|| format!("reading manifest blob {digest}"))?;
    let manifest = Manifest::from_reader_validated(bytes.as_slice())
        .with_context(|| format!("validating manifest {digest}"))?;

    // 3. Architecture-mismatch fail-closed check (accept if absent).
    check_architecture(&manifest)?;

    // 4. Derive the dillo argv from the manifest + runtime resources.
    let cpus = args.cpus.or(config.run.cpus);
    let memory = args.memory.or(config.run.memory_mib);
    let dillo_args = build_dillo_args(&manifest, &blob_store, cpus, memory)?;

    // 5. Locate dillo and hand off.
    let dillo = find_dillo();
    exec_dillo(&dillo, &dillo_args)
}

/// Assemble the `dillo` argument vector (program name excluded).
fn build_dillo_args(
    manifest: &Manifest,
    blob_store: &FilesystemBlobStore,
    cpus: Option<u32>,
    memory_mib: Option<u32>,
) -> Result<Vec<String>> {
    let (pmi, scutes) = partition_layers(manifest)?;
    let pmi_digest: Digest = pmi
        .digest
        .parse()
        .with_context(|| format!("invalid PMI digest: {}", pmi.digest))?;
    let pmi_path = blob_store.blob_path(&pmi_digest);

    let mut argv: Vec<String> = vec!["--pmi".to_string(), path_arg(&pmi_path)];
    if let Some(c) = cpus {
        argv.push("--cpus".to_string());
        argv.push(c.to_string());
    }
    if let Some(m) = memory_mib {
        argv.push("--memory".to_string());
        argv.push(m.to_string());
    }
    if !scutes.is_empty() {
        argv.push("--gpt".to_string());
        argv.push(build_gpt_spec(manifest, &scutes, blob_store)?);
    }
    Ok(argv)
}

/// Partition the manifest's layers into the single PMI descriptor and the
/// ordered scute list. `+zstd` scutes are not yet supported by `run`.
fn partition_layers(manifest: &Manifest) -> Result<(&PmiDescriptor, Vec<&ScuteDescriptor>)> {
    let mut pmi: Option<&PmiDescriptor> = None;
    let mut scutes: Vec<&ScuteDescriptor> = Vec::new();
    for layer in &manifest.layers {
        match layer {
            Layer::Pmi(d) => pmi = Some(d),
            Layer::Scute(d) => scutes.push(d),
            Layer::ScuteZstd(_) => bail!(
                "this artifact has zstd-compressed scute layers; `pichi run` does not yet \
                 support them (decompressed-COW handling is deferred)"
            ),
        }
    }
    let pmi = pmi.ok_or_else(|| {
        anyhow!("artifact is not bootable (no PMI layer); usable as a `from:` source only")
    })?;
    Ok((pmi, scutes))
}

/// Build the `--gpt` value: a chain-ordered partition list `[cow0, verity0,
/// cow1, verity1, ...]`. device-id/disk-guid are omitted so dillo derives
/// them from the PARTUUIDs (matching the carapace formula).
fn build_gpt_spec(
    manifest: &Manifest,
    scutes: &[&ScuteDescriptor],
    blob_store: &FilesystemBlobStore,
) -> Result<String> {
    let data_block_size = block_size(manifest, ANN_DATA_BLOCK_SIZE);
    let hash_block_size = block_size(manifest, ANN_HASH_BLOCK_SIZE);

    let mut parts: Vec<String> = Vec::with_capacity(scutes.len() * 2);
    for scute in scutes {
        let cow_digest: Digest = scute
            .digest
            .parse()
            .with_context(|| format!("invalid scute digest: {}", scute.digest))?;
        let cow_path = blob_store.blob_path(&cow_digest);
        let v_path = verity_path(&cow_path);

        let salt = hex::decode(&scute.annotations.salt)
            .with_context(|| format!("scute salt is not valid hex: {}", scute.annotations.salt))?;
        let cow_bytes = blob_store
            .get_blob(&cow_digest)
            .with_context(|| format!("reading scute cow blob {cow_digest}"))?;

        let params = pichi_import::verity::VerityParams {
            data_block_size,
            hash_block_size,
            salt,
            // Cosmetic for the root-hash computation (inputs are salt + data).
            uuid: [0u8; 16],
        };
        let root = pichi_import::verity::compute(&cow_bytes, &params)
            .context("dm-verity root computation failed")?
            .root_hash;

        // D-run-01: cow PARTUUID = root[0..16], verity PARTUUID = root[16..32].
        let mut cow_b = [0u8; 16];
        cow_b.copy_from_slice(&root[0..16]);
        let mut ver_b = [0u8; 16];
        ver_b.copy_from_slice(&root[16..32]);
        let cow_partuuid = uuid::Uuid::from_bytes(cow_b);
        let verity_partuuid = uuid::Uuid::from_bytes(ver_b);

        // D-run-03: labels = "c:"/"v:" + first 34 hex chars of root.
        let label_hex = &hex::encode(root)[..34];

        parts.push(format!(
            "[path={},partuuid={},typeguid={},label=c:{}]",
            path_arg(&cow_path),
            cow_partuuid,
            SCUTE_COW_TYPEGUID,
            label_hex
        ));
        parts.push(format!(
            "[path={},partuuid={},typeguid={},label=v:{}]",
            path_arg(&v_path),
            verity_partuuid,
            SCUTE_VERITY_TYPEGUID,
            label_hex
        ));
    }
    Ok(format!("partitions=[{}]", parts.join(",")))
}

/// Read a u32 chain annotation, falling back to the carapace-locked default.
fn block_size(manifest: &Manifest, key: &str) -> u32 {
    manifest
        .annotations
        .get(key)
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(DEFAULT_BLOCK_SIZE)
}

/// Fail closed when the artifact declares an architecture that doesn't match
/// the host. Absent annotation ⇒ accept (publisher's choice).
fn check_architecture(manifest: &Manifest) -> Result<()> {
    let Some(arch) = manifest.annotations.get(ARCH_ANNOTATION) else {
        return Ok(());
    };
    let host = std::env::consts::ARCH;
    let host_norm = match host {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    if arch != host && arch != host_norm {
        bail!(
            "artifact architecture {arch:?} does not match host {host:?} \
             (host normalised as {host_norm:?}; supported synonyms: \
             x86_64=amd64, aarch64=arm64)"
        );
    }
    Ok(())
}

/// Locate the `dillo` binary: `PICHI_DILLO` env override, then a sibling of
/// the running `pichi` binary, then bare `dillo` (resolved via `PATH`).
fn find_dillo() -> PathBuf {
    resolve_dillo(
        std::env::var_os("PICHI_DILLO"),
        std::env::current_exe().ok(),
    )
}

/// Pure core of [`find_dillo`], split out so tests don't touch process state.
fn resolve_dillo(override_path: Option<OsString>, current_exe: Option<PathBuf>) -> PathBuf {
    if let Some(p) = override_path {
        return PathBuf::from(p);
    }
    let exe_name = if cfg!(windows) { "dillo.exe" } else { "dillo" };
    if let Some(exe) = current_exe {
        if let Some(dir) = exe.parent() {
            let cand = dir.join(exe_name);
            if cand.is_file() {
                return cand;
            }
        }
    }
    PathBuf::from(exe_name)
}

/// Hand the assembled invocation to `dillo`. On Unix this replaces the
/// process image (clean signal/tty/exit-code semantics); elsewhere it spawns,
/// waits, and propagates the child's exit code.
fn exec_dillo(dillo: &Path, args: &[String]) -> Result<()> {
    log::info!("exec dillo: {} {}", dillo.display(), args.join(" "));
    let mut cmd = Command::new(dillo);
    cmd.args(args);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // `exec` only returns if the replacement failed.
        let err = cmd.exec();
        Err(err).with_context(|| format!("failed to exec dillo at {}", dillo.display()))
    }
    #[cfg(not(unix))]
    {
        let status = cmd
            .status()
            .with_context(|| format!("failed to spawn dillo at {}", dillo.display()))?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

/// Render a path as a CLI argument string (lossy is acceptable — blob paths
/// are content-addressed hex under the graphroot).
fn path_arg(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Resolve the on-disk cache layout, applying config overrides. Mirrors
/// `cmd::inspect::resolve_layout` verbatim (project convention: per-cmd
/// duplication, do not factor into `cmd/mod.rs`).
fn resolve_layout(config: &Config) -> Result<CacheLayout> {
    let mut layout = CacheLayout::resolve()?;
    if let Some(p) = &config.storage.graphroot {
        layout.graphroot.clone_from(p);
    }
    if let Some(p) = &config.storage.runroot {
        layout.runroot.clone_from(p);
    }
    Ok(layout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use pichi_artifact::{
        EmptyConfigDescriptor, Layer, MEDIA_TYPE_PICHI_ARTIFACT_V1, Manifest, PmiDescriptor,
        ScuteAnnotations, ScuteDescriptor,
    };
    use pichi_storage::{BlobStore, FilesystemBlobStore};
    use tempfile::TempDir;

    fn chain_annotations() -> BTreeMap<String, String> {
        [
            ("dev.pichi.carapace.verity.algo", "sha256"),
            ("dev.pichi.carapace.verity.data-block-size", "4096"),
            ("dev.pichi.carapace.verity.hash-block-size", "4096"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    fn sha256_digest(bytes: &[u8]) -> Digest {
        use sha2::{Digest as _, Sha256};
        let h = Sha256::digest(bytes);
        format!("sha256:{}", hex::encode(h)).parse().unwrap()
    }

    fn pmi_layer() -> Layer {
        Layer::Pmi(PmiDescriptor {
            digest: "sha256:2222222222222222222222222222222222222222222222222222222222222222"
                .into(),
            size: 8192,
        })
    }

    fn manifest_with(layers: Vec<Layer>) -> Manifest {
        Manifest {
            schema_version: 2,
            media_type: "application/vnd.oci.image.manifest.v1+json".into(),
            artifact_type: MEDIA_TYPE_PICHI_ARTIFACT_V1.into(),
            config: EmptyConfigDescriptor::canonical(),
            layers,
            annotations: chain_annotations(),
        }
    }

    #[test]
    fn partition_layers_splits_pmi_and_scutes() {
        let scute = ScuteDescriptor {
            digest: "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                .into(),
            size: 4096,
            annotations: ScuteAnnotations { salt: "00".into() },
        };
        let m = manifest_with(vec![pmi_layer(), Layer::Scute(scute)]);
        let (_pmi, scutes) = partition_layers(&m).unwrap();
        assert_eq!(scutes.len(), 1);
    }

    #[test]
    fn partition_layers_errors_without_pmi() {
        let scute = ScuteDescriptor {
            digest: "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                .into(),
            size: 4096,
            annotations: ScuteAnnotations { salt: "00".into() },
        };
        let m = manifest_with(vec![Layer::Scute(scute)]);
        let err = partition_layers(&m).unwrap_err();
        assert!(err.to_string().contains("not bootable"), "got: {err}");
    }

    #[test]
    fn partition_layers_rejects_zstd_scutes() {
        let scute = ScuteDescriptor {
            digest: "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                .into(),
            size: 4096,
            annotations: ScuteAnnotations { salt: "00".into() },
        };
        let m = manifest_with(vec![pmi_layer(), Layer::ScuteZstd(scute)]);
        let err = partition_layers(&m).unwrap_err();
        assert!(err.to_string().contains("zstd"), "got: {err}");
    }

    #[test]
    fn build_dillo_args_pmi_only_passes_resources_and_no_gpt() {
        let tmp = TempDir::new().unwrap();
        let blob_store = FilesystemBlobStore::new(tmp.path());
        let m = manifest_with(vec![pmi_layer()]);

        let argv = build_dillo_args(&m, &blob_store, Some(2), None).unwrap();
        assert_eq!(argv[0], "--pmi");
        assert!(argv.contains(&"--cpus".to_string()));
        assert!(argv.contains(&"2".to_string()));
        assert!(!argv.contains(&"--memory".to_string()));
        assert!(!argv.contains(&"--gpt".to_string()));
    }

    #[test]
    fn build_dillo_args_omits_resource_flags_when_unset() {
        let tmp = TempDir::new().unwrap();
        let blob_store = FilesystemBlobStore::new(tmp.path());
        let m = manifest_with(vec![pmi_layer()]);

        let argv = build_dillo_args(&m, &blob_store, None, None).unwrap();
        assert!(!argv.contains(&"--cpus".to_string()));
        assert!(!argv.contains(&"--memory".to_string()));
    }

    /// The `--gpt` kv string we emit must parse back through dillo's own
    /// device schema into the partitions we intended (guards format drift).
    #[test]
    fn gpt_spec_round_trips_through_dillo_config() {
        let tmp = TempDir::new().unwrap();
        let blob_store = FilesystemBlobStore::new(tmp.path());

        let cow_bytes = vec![0u8; 8192];
        let cow_digest = sha256_digest(&cow_bytes);
        blob_store.put_blob(&cow_digest, &cow_bytes).unwrap();

        let scute = ScuteDescriptor {
            digest: cow_digest.to_string(),
            size: cow_bytes.len() as u64,
            annotations: ScuteAnnotations { salt: "00".into() },
        };
        let m = manifest_with(vec![pmi_layer(), Layer::Scute(scute.clone())]);

        let spec = build_gpt_spec(&m, &[&scute], &blob_store).unwrap();
        // Must parse back through dillo's own device schema (same kv parser
        // dillo's `--gpt` uses).
        let gpt: dillo_config::GptSpec = serde_keyvalue::from_key_values(&spec).unwrap();

        assert_eq!(gpt.partitions.len(), 2, "cow + verity");
        assert!(gpt.device_id.is_none(), "dillo derives device-id");
        assert!(gpt.disk_guid.is_none(), "dillo derives disk-guid");

        let cow_tg = uuid::Uuid::parse_str(SCUTE_COW_TYPEGUID).unwrap();
        let ver_tg = uuid::Uuid::parse_str(SCUTE_VERITY_TYPEGUID).unwrap();
        assert_eq!(gpt.partitions[0].typeguid, cow_tg);
        assert_eq!(gpt.partitions[1].typeguid, ver_tg);
        assert!(gpt.partitions[0].label.starts_with("c:"));
        assert!(gpt.partitions[1].label.starts_with("v:"));
        assert_eq!(gpt.partitions[0].path, blob_store.blob_path(&cow_digest));
        assert_eq!(
            gpt.partitions[1].path,
            verity_path(&blob_store.blob_path(&cow_digest))
        );
        // Paired cow/verity share the same 34-hex label body.
        assert_eq!(gpt.partitions[0].label[2..], gpt.partitions[1].label[2..]);
    }

    #[test]
    fn check_architecture_accepts_absent_and_matching() {
        let m = manifest_with(vec![pmi_layer()]);
        check_architecture(&m).unwrap(); // absent

        let mut m2 = manifest_with(vec![pmi_layer()]);
        let host = std::env::consts::ARCH;
        m2.annotations
            .insert(ARCH_ANNOTATION.to_string(), host.to_string());
        check_architecture(&m2).unwrap(); // matching
    }

    #[test]
    fn check_architecture_rejects_mismatch() {
        let mut m = manifest_with(vec![pmi_layer()]);
        m.annotations.insert(
            ARCH_ANNOTATION.to_string(),
            "totally-not-this-host".to_string(),
        );
        let err = check_architecture(&m).unwrap_err();
        assert!(
            err.to_string().contains("does not match host"),
            "got: {err}"
        );
    }

    #[test]
    fn resolve_dillo_prefers_override() {
        let got = resolve_dillo(Some(OsString::from("/opt/dillo")), None);
        assert_eq!(got, PathBuf::from("/opt/dillo"));
    }

    #[test]
    fn resolve_dillo_finds_sibling() {
        let tmp = TempDir::new().unwrap();
        let name = if cfg!(windows) { "dillo.exe" } else { "dillo" };
        std::fs::write(tmp.path().join(name), b"#!/bin/sh\n").unwrap();
        let pichi_exe = tmp.path().join("pichi");
        let got = resolve_dillo(None, Some(pichi_exe));
        assert_eq!(got, tmp.path().join(name));
    }

    #[test]
    fn resolve_dillo_falls_back_to_path() {
        let got = resolve_dillo(None, None);
        let name = if cfg!(windows) { "dillo.exe" } else { "dillo" };
        assert_eq!(got, PathBuf::from(name));
    }
}
