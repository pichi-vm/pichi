// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `pichi build` — thin host launcher (BUILD.md).
//!
//! Specializes `pichi run`: same launch path, plus virtiofs wires and the
//! bidirectional named-console build-port protocol on `dev.pichi.build`
//! (guest requests build-input; host replies with CBOR). On guest exit 0 the
//! host takes the OCI layout from the output mount.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};

use conglobate::{BuildOutput, CarapaceRecipe, PmiRecipe};
use pichi_artifact::{
    CHAIN_ANNOTATION_VERITY_HASH, ConfigDescriptor, Digest, Layer, MEDIA_TYPE_PICHI_ARTIFACT_V1,
    Manifest, PmiDescriptor, Reference, ReferenceKind, Requirements, ScuteAnnotations,
    ScuteDescriptor,
};
use pichi_storage::sidecar::write_sidecar_atomic;
use pichi_storage::{BlobSidecarExt, BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};

use crate::cli::BuildArgs;
use crate::cmd::build_port::{
    self, BuildInput, clear_device_cloexec, console_arg, create_build_port_pipes, serve_build_port,
};
use crate::cmd::manifest_ext::ManifestExt;
use crate::cmd::requirements;
use crate::cmd::run::{build_gpt_spec, find_dillo, path_arg};
use crate::config::Config;

/// virtio-fs mount tags the host and Conglobate agree on (BUILD.md §2.3).
const REPO_TAG: &str = "repo";
const CACHE_TAG: &str = "cache";
const OUTPUT_TAG: &str = "output";

pub async fn run(args: BuildArgs, config: &Config) -> Result<()> {
    let build_dir = args.dir.join("pichi.build");
    if !build_dir.is_dir() {
        bail!(
            "no pichi.build/ directory under {} (run from a pichi project)",
            args.dir.display()
        );
    }

    let build_image_ref = resolve_build_image(args.build_image.as_deref())?;

    let layout = config.resolve_layout()?;
    let db = FilesystemTagDb::open(&layout.graphroot)?;
    let blob_store = FilesystemBlobStore::new(&layout.graphroot);

    // The build image is a PMI-only appliance: --pmi is its PMI blob.
    let (_, build_manifest) = resolve_manifest(&build_image_ref, &db, &blob_store)
        .await
        .with_context(|| format!("resolving build image {build_image_ref}"))?;
    let (pmi, _) = build_manifest
        .partition_layers()
        .with_context(|| format!("build image {build_image_ref} is not bootable"))?;
    let pmi_digest: Digest = pmi
        .digest
        .parse()
        .context("invalid build-image PMI digest")?;
    let pmi_path = blob_store.blob_path(&pmi_digest);

    // The carapace recipe's `from:` is the carapace we derive: its scutes
    // pass through unchanged into the output, and conglobate's per-directive
    // delta scutes chain onto its top root.
    let carapace_source = carapace_recipe_source(&build_dir)?;

    // Each source carapace (carapace.yaml / pmi.yaml `from:`) becomes its
    // own virtualized-GPT device.
    let mut gpt_specs = Vec::new();
    let mut source_scutes: Vec<ScuteDescriptor> = Vec::new();
    for reference in collect_source_refs(&build_dir)? {
        let (_, manifest) = resolve_manifest(&reference, &db, &blob_store)
            .await
            .with_context(|| format!("resolving source carapace {reference}"))?;
        let scutes = manifest
            .scute_layers()
            .with_context(|| format!("reading scutes of source carapace {reference}"))?;
        if scutes.is_empty() {
            bail!("source carapace {reference} has no scute layers");
        }
        if Some(&reference) == carapace_source.as_ref() {
            source_scutes = scutes.iter().map(|s| (*s).clone()).collect();
        }
        gpt_specs.push(
            build_gpt_spec(&manifest, &scutes, &blob_store)
                .await
                .with_context(|| format!("building vGPT for {reference}"))?,
        );
    }

    // Thin-launcher mounts (BUILD.md §2.2–§2.3): empty RW output, RO cache,
    // RO repo (git object store / work tree the guest resolves `--ref` against).
    let output_dir = tempfile::TempDir::new().context("creating build output dir")?;
    let repo_dir = std::fs::canonicalize(&args.dir)
        .with_context(|| format!("canonicalizing repo dir {}", args.dir.display()))?;
    let cache_dir = std::fs::canonicalize(&layout.graphroot)
        .with_context(|| format!("canonicalizing cache {}", layout.graphroot.display()))?;

    // Size the build VM from the build image's requirements, with the
    // operator's --memory/--cpus overriding (but not below required).
    let reqs = requirements::load_requirements(&build_manifest, &blob_store).await?;
    let cpus = requirements::resolve_sized(
        args.cpus,
        reqs.as_ref().and_then(Requirements::cpus_required),
        reqs.as_ref().and_then(Requirements::cpus_recommended),
        "cpus",
    )?;
    let memory = requirements::resolve_sized(
        args.memory,
        reqs.as_ref().and_then(Requirements::memory_required_mib),
        reqs.as_ref().and_then(Requirements::memory_recommended_mib),
        "memory (MiB)",
    )?;

    let build_input = BuildInput {
        git_ref: args
            .git_ref
            .clone()
            .ok_or_else(|| anyhow!("pichi build requires --ref (BUILD.md §2.4)"))?,
        recipe: args
            .recipe
            .clone()
            .ok_or_else(|| anyhow!("pichi build requires --recipe (BUILD.md §2.4)"))?,
        lock: args
            .lock
            .clone()
            .ok_or_else(|| anyhow!("pichi build requires --lock (BUILD.md §2.4)"))?,
        subdir: args.subdir.clone(),
        location: args.location.clone(),
    };
    let response = build_input
        .encode_success()
        .context("encoding build-input success response")?;

    let pipes = create_build_port_pipes().context("creating build-port pipes")?;
    let console = console_arg(pipes.device_rx_fd, pipes.device_tx_fd);

    let argv = build_dillo_args(
        &pmi_path,
        &gpt_specs,
        &repo_dir,
        &cache_dir,
        output_dir.path(),
        Some(&console),
        memory,
        cpus,
    );

    let tag = args
        .tag
        .as_deref()
        .ok_or_else(|| anyhow!("pichi build requires -t <tag> to name the result"))?;

    let dillo = find_dillo();
    boot_serve_and_wait(&dillo, &argv, pipes, response).await?;

    // The VM powered off; package what it wrote to the output sink.
    let created = chrono::Utc::now().to_rfc3339();
    let digest = package_artifact(
        output_dir.path(),
        &source_scutes,
        &blob_store,
        &db,
        tag,
        &created,
    )
    .await
    .context("packaging build output")?;
    println!("pichi build: packaged {tag} -> {digest}");
    Ok(())
}

/// Top-level chain annotation keys + locked values (carapace D-06), shared
/// with `pichi import`'s manifest builder.
fn chain_annotations(created_rfc3339: &str) -> BTreeMap<String, String> {
    let mut a = BTreeMap::new();
    a.insert(
        "dev.pichi.carapace.verity.algo".to_string(),
        "sha256".to_string(),
    );
    a.insert(
        "dev.pichi.carapace.verity.data-block-size".to_string(),
        "4096".to_string(),
    );
    a.insert(
        "dev.pichi.carapace.verity.hash-block-size".to_string(),
        "4096".to_string(),
    );
    a.insert(
        "dev.pichi.carapace.verity.version".to_string(),
        crate::cmd::verity::VERITY_FORMAT_VERSION.to_string(),
    );
    a.insert(
        "dev.pichi.carapace.verity.hash-type".to_string(),
        crate::cmd::verity::VERITY_HASH_TYPE_NORMAL.to_string(),
    );
    a.insert(
        "org.opencontainers.image.created".to_string(),
        created_rfc3339.to_string(),
    );
    a
}

/// Read the build VM's output sink (`build.yaml` + scute/PMI blobs), insert
/// the blobs into the cache, build + validate the OCI artifact manifest, and
/// tag it. Returns the manifest digest.
///
/// The output carapace is `source_scutes` (the carapace recipe's `from:`,
/// passed through unchanged — their blobs already live in the cache) followed
/// by the delta scutes conglobate emitted (the build's [`BuildOutput`]
/// contract), each salt-chained onto the source's top root.
async fn package_artifact(
    output_dir: &Path,
    source_scutes: &[ScuteDescriptor],
    blob_store: &FilesystemBlobStore,
    db: &FilesystemTagDb,
    tag: &str,
    created_rfc3339: &str,
) -> Result<Digest> {
    let manifest_yaml = std::fs::read_to_string(output_dir.join("build.yaml"))
        .context("reading output/build.yaml (build produced no output manifest)")?;
    let out = BuildOutput::parse(&manifest_yaml).context("parsing output/build.yaml")?;
    if source_scutes.is_empty() && out.scutes.is_empty() {
        bail!("build produced no scutes");
    }

    let scratch = blob_store
        .scratch_dir()
        .await
        .context("preparing scratch dir")?;
    let mut layers: Vec<Layer> = Vec::with_capacity(source_scutes.len() + out.scutes.len() + 1);

    // Source scutes pass through verbatim — their cow blobs + verity sidecars
    // are already cached (they were imported / pulled), so we only re-reference
    // their descriptors.
    for s in source_scutes {
        layers.push(Layer::Scute(s.clone()));
    }

    for s in &out.scutes {
        let cow_bytes = std::fs::read(output_dir.join(&s.cow))
            .with_context(|| format!("reading scute cow {}", s.cow))?;
        let verity_bytes = std::fs::read(output_dir.join(&s.verity))
            .with_context(|| format!("reading scute verity {}", s.verity))?;
        hex::decode(&s.salt).with_context(|| format!("scute salt is not hex: {}", s.salt))?;

        let cow_digest = Digest::from_bytes_sha256(&cow_bytes);
        blob_store
            .put_blob(&cow_digest, &cow_bytes)
            .await
            .with_context(|| format!("put cow blob {cow_digest}"))?;
        write_sidecar_atomic(
            &scratch,
            &blob_store.blob_path(&cow_digest).verity_path(),
            &verity_bytes,
        )
        .await
        .with_context(|| format!("write verity sidecar for {cow_digest}"))?;

        layers.push(Layer::Scute(ScuteDescriptor {
            digest: cow_digest.to_string(),
            size: cow_bytes.len() as u64,
            annotations: ScuteAnnotations {
                data_blocks: Some((cow_bytes.len() as u64).div_ceil(4096)),
                salt: s.salt.clone(),
            },
        }));
    }

    if let Some(pmi_name) = &out.pmi {
        let pmi_bytes = std::fs::read(output_dir.join(pmi_name))
            .with_context(|| format!("reading pmi {pmi_name}"))?;
        let pmi_digest = Digest::from_bytes_sha256(&pmi_bytes);
        blob_store
            .put_blob(&pmi_digest, &pmi_bytes)
            .await
            .with_context(|| format!("put pmi blob {pmi_digest}"))?;
        layers.push(Layer::Pmi(PmiDescriptor {
            digest: pmi_digest.to_string(),
            size: pmi_bytes.len() as u64,
        }));
    }

    let mut manifest = Manifest {
        schema_version: 2,
        media_type: "application/vnd.oci.image.manifest.v1+json".to_string(),
        artifact_type: MEDIA_TYPE_PICHI_ARTIFACT_V1.to_string(),
        config: ConfigDescriptor::canonical(),
        layers,
        annotations: chain_annotations(created_rfc3339),
    };
    // Stamp the carapace trust anchor (rootₙ₋₁): recompute the top root from the
    // now-cached scute cows (BUILD.md §7.1). All cow blobs are present above.
    let root = manifest
        .carapace_top_root(blob_store)
        .await
        .context("computing carapace verity root for packaged manifest")?;
    manifest
        .annotations
        .insert(CHAIN_ANNOTATION_VERITY_HASH.to_string(), hex::encode(root));
    manifest
        .validate()
        .context("packaged manifest failed self-validation")?;
    let bytes = manifest.to_bytes().context("manifest to_bytes")?;
    let digest = Digest::from_bytes_sha256(&bytes);
    blob_store
        .put_blob(&digest, &bytes)
        .await
        .with_context(|| format!("put manifest blob {digest}"))?;

    let tag_ref: Reference = tag.parse().with_context(|| format!("invalid tag: {tag}"))?;
    db.set_tag(&tag_ref.to_string(), &digest)
        .await
        .with_context(|| format!("set tag {tag}"))?;

    Ok(digest)
}

/// Assemble the `dillo` argument vector for the build VM (program name
/// excluded). The pure core, split out for testing without a boot.
pub(crate) fn build_dillo_args(
    pmi_path: &Path,
    gpt_specs: &[String],
    repo_dir: &Path,
    cache_dir: &Path,
    output_dir: &Path,
    console: Option<&str>,
    memory_mib: Option<u32>,
    cpus: Option<u32>,
) -> Vec<String> {
    let mut argv: Vec<String> = vec!["--pmi".to_string(), path_arg(pmi_path)];
    if let Some(m) = memory_mib {
        argv.push("--memory".to_string());
        argv.push(m.to_string());
    }
    if let Some(c) = cpus {
        argv.push("--cpus".to_string());
        argv.push(c.to_string());
    }
    // Virtiofs tags by convention (BUILD.md §2.3).
    argv.push("--fs".to_string());
    argv.push(format!(
        "tag={REPO_TAG},source={},readonly",
        path_arg(repo_dir)
    ));
    argv.push("--fs".to_string());
    argv.push(format!(
        "tag={CACHE_TAG},source={},readonly",
        path_arg(cache_dir)
    ));
    argv.push("--fs".to_string());
    argv.push(format!("tag={OUTPUT_TAG},source={}", path_arg(output_dir)));
    if let Some(console) = console {
        argv.push("--console".to_string());
        argv.push(console.to_string());
    }
    // Transitional: source-carapace vGPTs (retired as the product API; kept
    // while packaging still consumes them).
    for spec in gpt_specs {
        argv.push("--gpt".to_string());
        argv.push(spec.clone());
    }
    // User-mode NIC for stages that opt in with `network: true`.
    argv.push("--net".to_string());
    argv.push(USER_NET_SPEC.to_string());
    argv
}

/// dillo `--net` value for a user-mode (NAT + DHCP) interface — the only kind
/// pichi wires. `NetSpec` defaults `backend` to user; we state it explicitly.
pub(crate) const USER_NET_SPEC: &str = "backend=user";

/// The official build image, published multi-arch by the conglobate repo's CI.
/// Used by default when neither `--build-image` nor `PICHI_BUILD_IMAGE` is set.
pub const DEFAULT_BUILD_IMAGE: &str = "ghcr.io/pichi-vm/conglobate:latest";

/// Resolve the build-image reference: `--build-image`, then
/// `PICHI_BUILD_IMAGE`, then the [`DEFAULT_BUILD_IMAGE`].
fn resolve_build_image(flag: Option<&str>) -> Result<String> {
    if let Some(r) = flag {
        return Ok(r.to_string());
    }
    if let Some(env) = std::env::var_os("PICHI_BUILD_IMAGE") {
        return env
            .into_string()
            .map_err(|_| anyhow!("PICHI_BUILD_IMAGE is not valid UTF-8"));
    }
    Ok(DEFAULT_BUILD_IMAGE.to_string())
}

/// The carapace recipe's `from:` reference, if a `carapace.yaml` is present —
/// the carapace whose scutes pass through into the output artifact.
fn carapace_recipe_source(build_dir: &Path) -> Result<Option<String>> {
    let carapace_yaml = build_dir.join("carapace.yaml");
    if !carapace_yaml.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&carapace_yaml)
        .with_context(|| format!("reading {}", carapace_yaml.display()))?;
    let recipe = CarapaceRecipe::parse(&text)
        .with_context(|| format!("parsing {}", carapace_yaml.display()))?;
    Ok(Some(recipe.from))
}

/// Read `carapace.yaml` (`from:`) and `pmi.yaml` (`from:`, if present),
/// returning the unique source-carapace references, sorted.
fn collect_source_refs(build_dir: &Path) -> Result<Vec<String>> {
    let mut refs: BTreeSet<String> = BTreeSet::new();

    let carapace_yaml = build_dir.join("carapace.yaml");
    if carapace_yaml.is_file() {
        let text = std::fs::read_to_string(&carapace_yaml)
            .with_context(|| format!("reading {}", carapace_yaml.display()))?;
        let recipe = CarapaceRecipe::parse(&text)
            .with_context(|| format!("parsing {}", carapace_yaml.display()))?;
        refs.insert(recipe.from);
    }

    let pmi_yaml = build_dir.join("pmi.yaml");
    if pmi_yaml.is_file() {
        let text = std::fs::read_to_string(&pmi_yaml)
            .with_context(|| format!("reading {}", pmi_yaml.display()))?;
        let recipe =
            PmiRecipe::parse(&text).with_context(|| format!("parsing {}", pmi_yaml.display()))?;
        if let Some(from) = recipe.from {
            refs.insert(from);
        }
    }

    Ok(refs.into_iter().collect())
}

/// Resolve a cached reference to its manifest digest + parsed manifest.
async fn resolve_manifest(
    reference: &str,
    db: &FilesystemTagDb,
    blob_store: &FilesystemBlobStore,
) -> Result<(Digest, Manifest)> {
    let target: Reference = reference
        .parse()
        .with_context(|| format!("invalid reference: {reference}"))?;
    let digest = match &target.kind {
        ReferenceKind::Digest(d) => d.clone(),
        ReferenceKind::Tag(_) => {
            let key = target.to_string();
            db.resolve_tag(&key)
                .await?
                .ok_or_else(|| anyhow!("ref not in cache: {key}\n  hint: pichi pull {key}"))?
        }
    };
    let bytes = blob_store
        .get_blob(&digest)
        .await
        .with_context(|| format!("reading manifest blob {digest}"))?;
    let manifest = Manifest::from_reader_validated(bytes.as_slice())
        .with_context(|| format!("validating manifest {digest}"))?;
    Ok((digest, manifest))
}

/// Boot the build VM, serve the v0 build-port request/response on
/// `dev.pichi.build`, and wait for power-off. Unlike `pichi run` (which
/// `exec`s dillo), build spawns and waits so it can take the OCI output.
async fn boot_serve_and_wait(
    dillo: &Path,
    args: &[String],
    pipes: build_port::BuildPortPipes,
    response: Vec<u8>,
) -> Result<()> {
    log::info!("boot build VM: {} {}", dillo.display(), args.join(" "));

    let build_port::BuildPortPipes {
        host_tx,
        host_rx,
        device_rx_fd,
        device_tx_fd,
        device_rx,
        device_tx,
    } = pipes;

    let mut cmd = tokio::process::Command::new(dillo);
    cmd.args(args);

    #[cfg(unix)]
    {
        // Clear CLOEXEC in the child so `pipe:N` fds survive exec under the
        // numbers named in `--console`.
        #[allow(unsafe_code)]
        // SAFETY: the closure only fcntl-clears FD_CLOEXEC on descriptors this
        // process created and still owns (`device_rx` / `device_tx` below).
        unsafe {
            cmd.pre_exec(move || {
                clear_device_cloexec(device_rx_fd, device_tx_fd)
                    .map_err(|err| std::io::Error::other(err.to_string()))
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.inherit_handles(true);
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning dillo at {}", dillo.display()))?;
    // Child has inherited the device ends; drop our copies.
    drop(device_rx);
    drop(device_tx);

    let mut serve =
        tokio::task::spawn_blocking(move || serve_build_port(host_rx, host_tx, &response));

    let status = tokio::select! {
        serve_result = &mut serve => {
            serve_result
                .context("build-port task join")?
                .context("build-port exchange")?;
            child
                .wait()
                .await
                .context("waiting for build VM")?
        }
        status = child.wait() => {
            let status = status.context("waiting for build VM")?;
            serve.abort();
            match serve.await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    bail!(
                        "guest exited before completing build-port exchange ({status}): {err}"
                    );
                }
                Err(join) if join.is_cancelled() => {
                    bail!("guest exited before build-input request ({status})");
                }
                Err(join) => {
                    return Err(join).context("build-port task join");
                }
            }
            status
        }
    };

    if !status.success() {
        bail!("build VM exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dillo_config::FsSpec;

    fn parse_fs(value: &str) -> FsSpec {
        serde_keyvalue::from_key_values(value).expect("--fs value parses as dillo FsSpec")
    }

    #[tokio::test]
    async fn argv_has_pmi_repo_cache_output_console_and_one_gpt() {
        let console = console_arg(3, 4);
        let argv = build_dillo_args(
            Path::new("/cache/blobs/sha256/deadbeef"),
            &["partitions=[[path=/c,partuuid=u,typeguid=t,label=c:x]]".to_string()],
            Path::new("/proj"),
            Path::new("/var/cache/pichi"),
            Path::new("/run/out"),
            Some(&console),
            Some(4096),
            Some(8),
        );

        // --pmi <path>
        let pmi_i = argv.iter().position(|a| a == "--pmi").unwrap();
        assert_eq!(argv[pmi_i + 1], "/cache/blobs/sha256/deadbeef");

        // resources forwarded verbatim
        let mem_i = argv.iter().position(|a| a == "--memory").unwrap();
        assert_eq!(argv[mem_i + 1], "4096");
        let cpu_i = argv.iter().position(|a| a == "--cpus").unwrap();
        assert_eq!(argv[cpu_i + 1], "8");

        // exactly one --gpt for the single source carapace
        assert_eq!(argv.iter().filter(|a| *a == "--gpt").count(), 1);

        // build always wires exactly one user-mode NIC, and its value parses
        // back as a user-mode dillo NetSpec (guards format drift).
        let net_values: Vec<&String> = argv
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "--net")
            .map(|(i, _)| &argv[i + 1])
            .collect();
        assert_eq!(net_values.len(), 1);
        let net: dillo_config::NetSpec =
            serde_keyvalue::from_key_values(net_values[0]).expect("--net parses as NetSpec");
        assert_eq!(net.backend, dillo_config::NetBackendKind::User);

        // three --fs mounts: ro repo + ro cache + rw output
        let fs_values: Vec<&String> = argv
            .iter()
            .enumerate()
            .filter(|(i, a)| *a == "--fs" && *i + 1 < argv.len())
            .map(|(i, _)| &argv[i + 1])
            .collect();
        assert_eq!(fs_values.len(), 3);

        let repo = parse_fs(fs_values[0]);
        assert_eq!(repo.tag, REPO_TAG);
        assert_eq!(repo.source, Path::new("/proj"));
        assert!(repo.readonly, "repo mount must be read-only");

        let cache = parse_fs(fs_values[1]);
        assert_eq!(cache.tag, CACHE_TAG);
        assert_eq!(cache.source, Path::new("/var/cache/pichi"));
        assert!(cache.readonly, "cache mount must be read-only");

        let out = parse_fs(fs_values[2]);
        assert_eq!(out.tag, OUTPUT_TAG);
        assert_eq!(out.source, Path::new("/run/out"));
        assert!(!out.readonly, "output sink must be writable");

        // bidirectional named build port
        let console_i = argv.iter().position(|a| a == "--console").unwrap();
        let console_val = &argv[console_i + 1];
        let spec: dillo_config::ConsoleSpec =
            serde_keyvalue::from_key_values(console_val).expect("--console parses");
        assert_eq!(spec.endpoint.as_deref(), Some("stdio"));
        assert_eq!(spec.ports.len(), 1);
        assert_eq!(spec.ports[0].name, build_port::BUILD_PORT_NAME);
        assert!(spec.ports[0].rx.is_some());
        assert!(spec.ports[0].tx.is_some());
    }

    #[tokio::test]
    async fn argv_omits_resource_flags_when_unset() {
        let argv = build_dillo_args(
            Path::new("/p.pmi"),
            &[],
            Path::new("/proj"),
            Path::new("/cache"),
            Path::new("/out"),
            None,
            None,
            None,
        );
        assert!(!argv.iter().any(|a| a == "--memory"));
        assert!(!argv.iter().any(|a| a == "--cpus"));
        assert!(!argv.iter().any(|a| a == "--gpt"));
        assert!(!argv.iter().any(|a| a == "--console"));
    }

    #[tokio::test]
    async fn build_image_flag_beats_env_and_falls_back_to_default() {
        assert_eq!(resolve_build_image(Some("reg/bi:1")).unwrap(), "reg/bi:1");
        // Neither flag nor env -> the default official build image. (Env is
        // process-global; assert the default only when it is unset.)
        if std::env::var_os("PICHI_BUILD_IMAGE").is_none() {
            assert_eq!(resolve_build_image(None).unwrap(), DEFAULT_BUILD_IMAGE);
        }
    }

    #[tokio::test]
    async fn collect_source_refs_reads_from_keys() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bd = tmp.path().join("pichi.build");
        std::fs::create_dir_all(&bd).unwrap();
        std::fs::write(bd.join("carapace.yaml"), "from: reg/base:1\n").unwrap();
        std::fs::write(
            bd.join("pmi.yaml"),
            "from: reg/kbuilder:2\ninto: /tmp/x.pmi\n",
        )
        .unwrap();
        let refs = collect_source_refs(&bd).unwrap();
        assert_eq!(
            refs,
            vec!["reg/base:1".to_string(), "reg/kbuilder:2".to_string()]
        );
    }

    #[tokio::test]
    async fn package_artifact_builds_and_tags_a_carapace() {
        use crate::cmd::{test_support::ScuteFixture as _, verity};

        let tmp = tempfile::TempDir::new().unwrap();
        let out = tmp.path().join("out");
        std::fs::create_dir_all(&out).unwrap();

        // One base scute: cow + verity blob + 32-byte zero salt.
        let mut raw = vec![0u8; 4096 * 3];
        raw[100] = 7;
        let cow_bytes = raw.scute();
        std::fs::write(out.join("0000.cow"), &cow_bytes).unwrap();
        let salt = vec![0u8; 32];
        let params = verity::VerityParams {
            data_block_size: 4096,
            hash_block_size: 4096,
            salt: salt.clone(),
            uuid: [0u8; 16],
        };
        let vout = params.compute(&cow_bytes).unwrap();
        std::fs::write(out.join("0000.verity"), &vout.blob).unwrap();
        std::fs::write(
            out.join("build.yaml"),
            format!(
                "scutes:\n- cow: 0000.cow\n  verity: 0000.verity\n  salt: {}\n",
                hex::encode(&salt)
            ),
        )
        .unwrap();

        let graphroot = tmp.path().join("storage");
        std::fs::create_dir_all(&graphroot).unwrap();
        let blob_store = FilesystemBlobStore::new(&graphroot);
        let db = FilesystemTagDb::open(&graphroot).unwrap();

        let digest = package_artifact(
            &out,
            &[],
            &blob_store,
            &db,
            "myapp:v1",
            "2026-06-22T00:00:00Z",
        )
        .await
        .unwrap();

        let key = "myapp:v1".parse::<Reference>().unwrap().to_string();
        assert_eq!(db.resolve_tag(&key).await.unwrap(), Some(digest.clone()));
        let mbytes = blob_store.get_blob(&digest).await.unwrap();
        let m = Manifest::from_reader_validated(mbytes.as_slice()).unwrap();
        assert_eq!(m.layers.len(), 1, "one scute layer");
    }

    #[tokio::test]
    async fn package_artifact_prepends_source_scutes() {
        use crate::cmd::{test_support::ScuteFixture as _, verity};

        let tmp = tempfile::TempDir::new().unwrap();
        let out = tmp.path().join("out");
        std::fs::create_dir_all(&out).unwrap();

        let graphroot = tmp.path().join("storage");
        std::fs::create_dir_all(&graphroot).unwrap();
        let blob_store = FilesystemBlobStore::new(&graphroot);
        let db = FilesystemTagDb::open(&graphroot).unwrap();

        // Stamping the carapace root recomputes the whole chain, so the scutes
        // must form a real salt chain: each non-base scute's salt equals the
        // previous scute's verity root. Build a genuine 2-source + 1-delta chain.
        let verity_root = |cow_bytes: &[u8], salt: &[u8]| -> [u8; 32] {
            verity::VerityParams {
                data_block_size: 4096,
                hash_block_size: 4096,
                salt: salt.to_vec(),
                uuid: [0u8; 16],
            }
            .compute(cow_bytes)
            .unwrap()
            .root_hash
        };
        let make_cow = |seed: u8| {
            let mut v = vec![0u8; 4096 * 2];
            v[10] = seed;
            v.scute()
        };

        // Two pass-through source scutes; their cow blobs are cached (as in a
        // real build) so the root recompute can read them.
        let cow0 = make_cow(1);
        let salt0 = vec![0u8; 32]; // base scute
        let root0 = verity_root(&cow0, &salt0);
        let d0 = Digest::from_bytes_sha256(&cow0);
        blob_store.put_blob(&d0, &cow0).await.unwrap();

        let cow1 = make_cow(2);
        let salt1 = root0.to_vec(); // chains onto scute 0
        let root1 = verity_root(&cow1, &salt1);
        let d1 = Digest::from_bytes_sha256(&cow1);
        blob_store.put_blob(&d1, &cow1).await.unwrap();

        let source_scutes = vec![
            ScuteDescriptor {
                digest: d0.to_string(),
                size: cow0.len() as u64,
                annotations: ScuteAnnotations {
                    data_blocks: None,
                    salt: hex::encode(&salt0),
                },
            },
            ScuteDescriptor {
                digest: d1.to_string(),
                size: cow1.len() as u64,
                annotations: ScuteAnnotations {
                    data_blocks: None,
                    salt: hex::encode(&salt1),
                },
            },
        ];

        // One delta scute conglobate emitted, chaining onto source scute 1.
        let cow_delta = make_cow(9);
        let salt_delta = root1.to_vec();
        let root_delta = verity_root(&cow_delta, &salt_delta);
        std::fs::write(out.join("0000.cow"), &cow_delta).unwrap();
        let vout = verity::VerityParams {
            data_block_size: 4096,
            hash_block_size: 4096,
            salt: salt_delta.clone(),
            uuid: [0u8; 16],
        }
        .compute(&cow_delta)
        .unwrap();
        std::fs::write(out.join("0000.verity"), &vout.blob).unwrap();
        std::fs::write(
            out.join("build.yaml"),
            format!(
                "scutes:\n- cow: 0000.cow\n  verity: 0000.verity\n  salt: {}\n",
                hex::encode(&salt_delta)
            ),
        )
        .unwrap();

        let digest = package_artifact(
            &out,
            &source_scutes,
            &blob_store,
            &db,
            "app:v1",
            "2026-06-22T00:00:00Z",
        )
        .await
        .unwrap();

        let mbytes = blob_store.get_blob(&digest).await.unwrap();
        let m = Manifest::from_reader_validated(mbytes.as_slice()).unwrap();
        let scute_layers = m
            .layers
            .iter()
            .filter(|l| matches!(l, Layer::Scute(_)))
            .count();
        assert_eq!(scute_layers, 3, "2 source scutes + 1 delta");
        // The stamped root is the top of the chain — the delta scute's root.
        assert_eq!(
            m.carapace_verity_hash(),
            Some(root_delta),
            "packaged manifest must declare the delta (top) scute's verity root"
        );
    }
}
