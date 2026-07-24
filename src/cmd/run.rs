// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `pichi run <ref>` — boot an artifact by exec'ing the `dillo` launcher,
//! auto-pulling it first if it isn't cached (docker/podman `--pull=missing`).
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

use anyhow::{Context, Result, anyhow};

use pichi_artifact::{Digest, Manifest, Reference, ReferenceKind, Requirements, ScuteDescriptor};
use pichi_storage::{BlobSidecarExt, BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};

use crate::cli::{PullArgs, PullPolicy, RunArgs};
use crate::cmd::manifest_ext::ManifestExt;
use crate::cmd::requirements;
use crate::config::Config;

/// GPT type GUID for scute COW partitions (carapace spec §"GPT Deployment
/// Pattern", registered value). Not defined in a shared crate yet; pinned here.
const SCUTE_COW_TYPEGUID: &str = "11dd804a-e1bf-4ab3-98c1-f9f48ceedbf1";
/// GPT type GUID for scute verity partitions (carapace spec, registered value).
const SCUTE_VERITY_TYPEGUID: &str = "40bb9571-2972-4547-b580-6a8cb13fd7d1";

/// `pichi run <ref>` entry point.
pub async fn run(args: RunArgs, config: &Config) -> Result<()> {
    let layout = config.resolve_layout()?;
    let db = FilesystemTagDb::open(&layout.graphroot)?;
    let blob_store = FilesystemBlobStore::new(&layout.graphroot);

    // 1. Resolve ref -> manifest digest.
    let target_ref: Reference = args
        .reference
        .parse()
        .with_context(|| format!("invalid reference: {}", args.reference))?;
    let digest = match &target_ref.kind {
        ReferenceKind::Digest(d) => {
            // Auto-pull on a cache miss, mirroring `docker`/`podman run`'s
            // default `--pull=missing`.
            if !blob_store.blob_exists(d).await {
                auto_pull(&target_ref, config).await?;
            }
            d.clone()
        }
        ReferenceKind::Tag(_) => {
            let key = target_ref.to_string();
            match db.resolve_tag(&key).await? {
                Some(d) => d,
                None => {
                    auto_pull(&target_ref, config).await?;
                    db.resolve_tag(&key)
                        .await?
                        .ok_or_else(|| anyhow!("{key} not in cache after pull"))?
                }
            }
        }
    };

    // 2. Read + validate the manifest snapshot.
    let bytes = blob_store
        .get_blob(&digest)
        .await
        .with_context(|| format!("reading manifest blob {digest}"))?;
    let manifest = Manifest::from_reader_validated(bytes.as_slice())
        .with_context(|| format!("validating manifest {digest}"))?;

    // 3. Architecture-mismatch fail-closed check (accept if absent).
    manifest.check_architecture()?;

    // 4. Derive the dillo argv from the manifest + runtime resources, applying
    //    the artifact's requirements.yaml floors (BUILD.md §7) when present.
    let reqs = requirements::load_requirements(&manifest, &blob_store).await?;
    let cpus = requirements::resolve_sized(
        args.cpus.or(config.run.cpus),
        reqs.as_ref().and_then(Requirements::cpus_required),
        reqs.as_ref().and_then(Requirements::cpus_recommended),
        "cpus",
    )?;
    let memory = requirements::resolve_sized(
        args.memory.or(config.run.memory_mib),
        reqs.as_ref().and_then(Requirements::memory_required_mib),
        reqs.as_ref().and_then(Requirements::memory_recommended_mib),
        "memory (MiB)",
    )?;
    // One user-mode NIC per declared interface (BUILD.md §7); the guest brings
    // them up. v1 wires user-net only and does not yet act on ingress/slot.
    let interfaces = reqs.as_ref().map_or(0, |r| r.interfaces.len());
    let dillo_args = build_dillo_args(&manifest, &blob_store, cpus, memory, interfaces).await?;

    // 5. Locate dillo and hand off.
    let dillo = find_dillo();
    exec_dillo(&dillo, &dillo_args)
}

/// Fetch an artifact that isn't cached, mirroring `docker`/`podman run`'s
/// default `--pull=missing`: `pichi run` pulls on a cache miss rather than
/// erroring. Delegates to `cmd::pull` (which owns the registry, auth, and
/// verity-preparation pipeline) so there is one download path.
async fn auto_pull(reference: &Reference, config: &Config) -> Result<()> {
    eprintln!("Unable to find {reference} locally; pulling...");
    crate::cmd::pull::run(
        PullArgs {
            reference: reference.to_string(),
            pull: Some(PullPolicy::Missing),
            quiet: false,
        },
        config,
    )
    .await
    .with_context(|| format!("auto-pull {reference}"))
}

/// Assemble the `dillo` argument vector (program name excluded).
async fn build_dillo_args(
    manifest: &Manifest,
    blob_store: &FilesystemBlobStore,
    cpus: Option<u32>,
    memory_mib: Option<u32>,
    interfaces: usize,
) -> Result<Vec<String>> {
    let (pmi, scutes) = manifest.partition_layers()?;
    let pmi_digest: Digest = pmi
        .digest
        .parse()
        .with_context(|| format!("invalid PMI digest: {}", pmi.digest))?;
    let pmi_path = blob_store.blob_path(&pmi_digest);

    let mut argv: Vec<String> = vec!["--pmi".to_string(), path_arg(&pmi_path)];
    // A detached-mode PMI carries its measured base DTB as a separate layer;
    // hand it to the VMM out-of-band. The operator never specifies this — it
    // rides in the artifact, like the PMI and scutes.
    if let Some(dtb) = manifest.dtb_layer() {
        let dtb_digest: Digest = dtb
            .digest
            .parse()
            .with_context(|| format!("invalid DTB digest: {}", dtb.digest))?;
        argv.push("--dtb".to_string());
        argv.push(path_arg(&blob_store.blob_path(&dtb_digest)));
    }
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
        argv.push(build_gpt_spec(manifest, &scutes, blob_store).await?);
    }
    for _ in 0..interfaces {
        argv.push("--net".to_string());
        argv.push(crate::cmd::build::USER_NET_SPEC.to_string());
    }
    Ok(argv)
}

/// Build the `--gpt` value: a chain-ordered partition list `[cow0, verity0,
/// cow1, verity1, ...]`. device-id/disk-guid are omitted so dillo derives
/// them from the PARTUUIDs (matching the carapace formula).
pub(crate) async fn build_gpt_spec(
    manifest: &Manifest,
    scutes: &[&ScuteDescriptor],
    blob_store: &FilesystemBlobStore,
) -> Result<String> {
    let data_block_size = manifest.data_block_size();
    let hash_block_size = manifest.hash_block_size();

    let mut parts: Vec<String> = Vec::with_capacity(scutes.len() * 2);
    for scute in scutes {
        let cow_digest: Digest = scute
            .digest
            .parse()
            .with_context(|| format!("invalid scute digest: {}", scute.digest))?;
        let cow_path = blob_store.blob_path(&cow_digest);
        let v_path = cow_path.verity_path();

        // Fast path: for a single-scute carapace the chain verity root recorded
        // in the manifest annotation IS this scute's root, so reuse it instead
        // of reading the whole cow blob (hundreds of MB) and re-hashing it every
        // launch. Falls back to recomputation for multi-scute stacks (where each
        // scute has a distinct root not individually stored).
        let root: [u8; 32] = match (scutes.len() == 1)
            .then(|| manifest.carapace_verity_hash())
            .flatten()
        {
            Some(h) => h,
            None => {
                let salt = hex::decode(&scute.annotations.salt).with_context(|| {
                    format!("scute salt is not valid hex: {}", scute.annotations.salt)
                })?;
                let cow_bytes = blob_store
                    .get_blob(&cow_digest)
                    .await
                    .with_context(|| format!("reading scute cow blob {cow_digest}"))?;
                // dm-verity root computation is CPU-bound — run it off the runtime.
                let params = pichi_import::verity::VerityParams {
                    data_block_size,
                    hash_block_size,
                    salt,
                    // Cosmetic for the root-hash computation (inputs are salt + data).
                    uuid: [0u8; 16],
                };
                tokio::task::spawn_blocking(move || params.compute(&cow_bytes).map(|o| o.root_hash))
                    .await
                    .context("verity task panicked")?
                    .context("dm-verity root computation failed")?
            }
        };

        // D-run-01: cow PARTUUID = root[0..16], verity PARTUUID = root[16..32].
        //
        // These raw bytes are the GPT *on-disk* GUID bytes (carapace's read
        // convention: `partition::raw_partuuid_to_text` treats the raw root
        // halves as the mixed-endian on-disk form). The `gpt` crate dillo
        // uses is spec-compliant and stores GUIDs mixed-endian (it writes
        // `Uuid::to_bytes_le`), so we must build the `Uuid` with
        // `from_bytes_le` for its on-disk bytes to equal the raw root halves
        // verbatim. Using `from_bytes` here byte-swaps fields 1-3 on disk and
        // the guest's carapace chain walk then fails to find the partition.
        let mut cow_b = [0u8; 16];
        cow_b.copy_from_slice(&root[0..16]);
        let mut ver_b = [0u8; 16];
        ver_b.copy_from_slice(&root[16..32]);
        let cow_partuuid = uuid::Uuid::from_bytes_le(cow_b);
        let verity_partuuid = uuid::Uuid::from_bytes_le(ver_b);

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

/// Locate the `dillo` binary: `PICHI_DILLO` env override, then a sibling of
/// the running `pichi` binary, then bare `dillo` (resolved via `PATH`).
pub(crate) fn find_dillo() -> PathBuf {
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
pub(crate) fn path_arg(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const ARCH_ANNOTATION: &str = "org.opencontainers.image.architecture";

    use pichi_artifact::{
        ConfigDescriptor, Layer, MEDIA_TYPE_PICHI_ARTIFACT_V1, Manifest, PmiDescriptor,
        ScuteAnnotations, ScuteDescriptor,
    };
    use pichi_storage::{BlobStore, FilesystemBlobStore};
    use tempfile::TempDir;

    fn chain_annotations() -> BTreeMap<String, String> {
        [
            ("dev.pichi.carapace.verity.algo", "sha256"),
            ("dev.pichi.carapace.verity.data-block-size", "4096"),
            ("dev.pichi.carapace.verity.hash-block-size", "4096"),
            ("dev.pichi.carapace.verity.version", "1"),
            ("dev.pichi.carapace.verity.hash-type", "1"),
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
            config: ConfigDescriptor::canonical(),
            layers,
            annotations: chain_annotations(),
        }
    }

    #[tokio::test]
    async fn partition_layers_splits_pmi_and_scutes() {
        let scute = ScuteDescriptor {
            digest: "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                .into(),
            size: 4096,
            annotations: ScuteAnnotations { salt: "00".into() },
        };
        let m = manifest_with(vec![pmi_layer(), Layer::Scute(scute)]);
        let (_pmi, scutes) = m.partition_layers().unwrap();
        assert_eq!(scutes.len(), 1);
    }

    #[tokio::test]
    async fn partition_layers_errors_without_pmi() {
        let scute = ScuteDescriptor {
            digest: "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                .into(),
            size: 4096,
            annotations: ScuteAnnotations { salt: "00".into() },
        };
        let m = manifest_with(vec![Layer::Scute(scute)]);
        let err = m.partition_layers().unwrap_err();
        assert!(err.to_string().contains("not bootable"), "got: {err}");
    }

    #[tokio::test]
    async fn partition_layers_rejects_zstd_scutes() {
        let scute = ScuteDescriptor {
            digest: "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                .into(),
            size: 4096,
            annotations: ScuteAnnotations { salt: "00".into() },
        };
        let m = manifest_with(vec![pmi_layer(), Layer::ScuteZstd(scute)]);
        let err = m.partition_layers().unwrap_err();
        assert!(err.to_string().contains("zstd"), "got: {err}");
    }

    #[tokio::test]
    async fn build_dillo_args_pmi_only_passes_resources_and_no_gpt() {
        let tmp = TempDir::new().unwrap();
        let blob_store = FilesystemBlobStore::new(tmp.path());
        let m = manifest_with(vec![pmi_layer()]);

        let argv = build_dillo_args(&m, &blob_store, Some(2), None, 1)
            .await
            .unwrap();
        assert_eq!(argv[0], "--pmi");
        assert!(argv.contains(&"--cpus".to_string()));
        assert!(argv.contains(&"2".to_string()));
        assert!(!argv.contains(&"--memory".to_string()));
        assert!(!argv.contains(&"--gpt".to_string()));
        // One declared interface -> one user-net device.
        assert_eq!(argv.iter().filter(|a| *a == "--net").count(), 1);
    }

    #[tokio::test]
    async fn build_dillo_args_omits_resource_flags_when_unset() {
        let tmp = TempDir::new().unwrap();
        let blob_store = FilesystemBlobStore::new(tmp.path());
        let m = manifest_with(vec![pmi_layer()]);

        let argv = build_dillo_args(&m, &blob_store, None, None, 0)
            .await
            .unwrap();
        assert!(!argv.contains(&"--cpus".to_string()));
        assert!(!argv.contains(&"--memory".to_string()));
        // No declared interfaces -> no --net.
        assert!(!argv.contains(&"--net".to_string()));
    }

    fn dtb_layer_fixture() -> Layer {
        Layer::Dtb(pichi_artifact::DtbDescriptor {
            digest: "sha256:4444444444444444444444444444444444444444444444444444444444444444"
                .into(),
            size: 8192,
        })
    }

    #[tokio::test]
    async fn build_dillo_args_passes_dtb_when_present() {
        let tmp = TempDir::new().unwrap();
        let blob_store = FilesystemBlobStore::new(tmp.path());
        let m = manifest_with(vec![pmi_layer(), dtb_layer_fixture()]);
        let argv = build_dillo_args(&m, &blob_store, None, None, 0)
            .await
            .unwrap();
        assert!(argv.contains(&"--dtb".to_string()), "argv: {argv:?}");
    }

    #[tokio::test]
    async fn build_dillo_args_no_dtb_when_absent() {
        let tmp = TempDir::new().unwrap();
        let blob_store = FilesystemBlobStore::new(tmp.path());
        let m = manifest_with(vec![pmi_layer()]);
        let argv = build_dillo_args(&m, &blob_store, None, None, 0)
            .await
            .unwrap();
        assert!(!argv.contains(&"--dtb".to_string()));
    }

    /// The user-net `--net` value we emit must parse back through dillo's own
    /// device schema as a user-mode interface (guards format drift).
    #[tokio::test]
    async fn net_spec_round_trips_through_dillo_config() {
        let spec: dillo_config::NetSpec =
            serde_keyvalue::from_key_values(crate::cmd::build::USER_NET_SPEC)
                .expect("user-net spec parses as dillo NetSpec");
        assert_eq!(spec.backend, dillo_config::NetBackendKind::User);
    }

    /// The `--gpt` kv string we emit must parse back through dillo's own
    /// device schema into the partitions we intended (guards format drift).
    #[tokio::test]
    async fn gpt_spec_round_trips_through_dillo_config() {
        let tmp = TempDir::new().unwrap();
        let blob_store = FilesystemBlobStore::new(tmp.path());

        let cow_bytes = vec![0u8; 8192];
        let cow_digest = sha256_digest(&cow_bytes);
        blob_store.put_blob(&cow_digest, &cow_bytes).await.unwrap();

        let scute = ScuteDescriptor {
            digest: cow_digest.to_string(),
            size: cow_bytes.len() as u64,
            annotations: ScuteAnnotations { salt: "00".into() },
        };
        let m = manifest_with(vec![pmi_layer(), Layer::Scute(scute.clone())]);

        let spec = build_gpt_spec(&m, &[&scute], &blob_store).await.unwrap();
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
            blob_store.blob_path(&cow_digest).verity_path()
        );
        // Paired cow/verity share the same 34-hex label body.
        assert_eq!(gpt.partitions[0].label[2..], gpt.partitions[1].label[2..]);
    }

    #[tokio::test]
    async fn check_architecture_accepts_absent_and_matching() {
        let m = manifest_with(vec![pmi_layer()]);
        m.check_architecture().unwrap(); // absent

        let mut m2 = manifest_with(vec![pmi_layer()]);
        let host = std::env::consts::ARCH;
        m2.annotations
            .insert(ARCH_ANNOTATION.to_string(), host.to_string());
        m2.check_architecture().unwrap(); // matching
    }

    #[tokio::test]
    async fn check_architecture_rejects_mismatch() {
        let mut m = manifest_with(vec![pmi_layer()]);
        m.annotations.insert(
            ARCH_ANNOTATION.to_string(),
            "totally-not-this-host".to_string(),
        );
        let err = m.check_architecture().unwrap_err();
        assert!(
            err.to_string().contains("does not match host"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn resolve_dillo_prefers_override() {
        let got = resolve_dillo(Some(OsString::from("/opt/dillo")), None);
        assert_eq!(got, PathBuf::from("/opt/dillo"));
    }

    #[tokio::test]
    async fn resolve_dillo_finds_sibling() {
        let tmp = TempDir::new().unwrap();
        let name = if cfg!(windows) { "dillo.exe" } else { "dillo" };
        std::fs::write(tmp.path().join(name), b"#!/bin/sh\n").unwrap();
        let pichi_exe = tmp.path().join("pichi");
        let got = resolve_dillo(None, Some(pichi_exe));
        assert_eq!(got, tmp.path().join(name));
    }

    #[tokio::test]
    async fn resolve_dillo_falls_back_to_path() {
        let got = resolve_dillo(None, None);
        let name = if cfg!(windows) { "dillo.exe" } else { "dillo" };
        assert_eq!(got, PathBuf::from(name));
    }
}
