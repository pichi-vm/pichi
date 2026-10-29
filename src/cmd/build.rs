// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `pichi build` — thin host launcher (BUILD.md).
//!
//! Specializes `pichi run`: same launch path, plus virtiofs wires and the
//! bidirectional named-console build-port protocol on `dev.pichi.build`
//! (guest requests build-input; host replies with CBOR). On guest exit 0 the
//! host requires finished output and validates the OCI (Open Container
//! Initiative) layout for import.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use conglobate::build::Lock as BuildLockFile;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use super::cache::ImageStore as _;
use conglobate::artifact::{Digest, Manifest, Reference, ReferenceKind, Requirements};
use pichi_storage::{FilesystemBlobStore, FilesystemTagDb};

use crate::cli::{BuildArgs, PullArgs, PullPolicy};
#[cfg(test)]
use crate::cli::{DEFAULT_LOCK, DEFAULT_PLAN};
#[cfg(unix)]
use crate::cmd::build_port::clear_device_cloexec;
use crate::cmd::build_port::{
    self, BuildInput, Response, ResponseExt as _, console_arg, create_build_port_pipes,
    serve_build_port,
};
use crate::cmd::manifest_ext::ManifestExt;
use crate::cmd::requirements;
use crate::cmd::run::{build_gpt_spec, find_dillo, path_arg};
use crate::config::Config;

mod output;
use output::{BuildOutput, ConsoleLog, OutputVerdict};

/// Bound guest execution, including the build-port exchange.
const BUILD_VM_TIMEOUT: Duration = Duration::from_mins(30);

/// virtio-fs mount tags the host and Conglobate agree on (BUILD.md §2.3).
const REPO_TAG: &str = "repo";
const CACHE_TAG: &str = "cache";
const OUTPUT_TAG: &str = "output";

pub async fn run(args: BuildArgs, config: &Config) -> Result<()> {
    let build_image_ref = resolve_build_image(args.build_image.as_deref());

    // Thin-launcher build-input (BUILD.md §2.4): `ref` is required; plan
    // and lock default to `pichi.yaml` / `pichi.lock` (`-p`/`--plan`,
    // `-l`/`--lock` override). CBOR carries the resolved relative paths.
    let git_ref = args.git_ref.clone();
    let plan = args.plan.clone();
    let lock = args.lock.clone();
    let context = if args.context.is_empty() {
        ".".to_string()
    } else {
        args.context.clone()
    };

    let layout = config.resolve_layout()?;
    let db = FilesystemTagDb::open(&layout.graphroot)?;
    let blob_store = FilesystemBlobStore::new(&layout.graphroot);

    ensure_build_image(&build_image_ref, config, &db, &blob_store).await?;

    // The build image launches like `pichi run` (BUILD.md §2.6): --pmi,
    // optional detached --dtb, and a vGPT for the appliance carapace scutes.
    let (_, build_manifest) = resolve_manifest(&build_image_ref, &db, &blob_store)
        .await
        .with_context(|| format!("resolving build image {build_image_ref}"))?;
    let (pmi, build_scutes) = build_manifest
        .partition_layers()
        .with_context(|| format!("build image {build_image_ref} is not bootable"))?;
    let pmi_digest: Digest = pmi
        .digest
        .parse()
        .context("invalid build-image PMI digest")?;
    let pmi_path = blob_store.blob_path(&pmi_digest);
    let dtb_path = if let Some(dtb) = build_manifest.dtb_layer() {
        let dtb_digest: Digest = dtb
            .digest
            .parse()
            .with_context(|| format!("invalid build-image DTB digest: {}", dtb.digest))?;
        Some(blob_store.blob_path(&dtb_digest))
    } else {
        None
    };

    // Source preparation follows the current lock contract. Plans are parsed
    // only by Conglobate; legacy host plan parsing is no longer supported.
    let build_lock = load_build_lock(&args.dir, &context, &lock)?;

    let mut gpt_specs = Vec::new();
    // Appliance root carapace first (corium discovers it by PARTUUID), then
    // transitional source-carapace vGPTs for lock-pinned inputs.
    if !build_scutes.is_empty() {
        gpt_specs.push(
            build_gpt_spec(&build_manifest, &build_scutes, &blob_store)
                .await
                .with_context(|| format!("building vGPT for build image {build_image_ref}"))?,
        );
    }
    for (reference, pin) in &build_lock.images {
        let mut target: Reference = canonicalize_reference(reference).parse()?;
        target.kind = ReferenceKind::Digest(pin.index.clone());
        let (digest, manifest) = resolve_manifest(&target.to_string(), &db, &blob_store)
            .await
            .with_context(|| format!("resolving pinned source {reference}"))?;
        if digest != pin.index {
            bail!("cached {reference} does not match lock pin {}", pin.index);
        }
        let scutes = manifest
            .scute_layers()
            .with_context(|| format!("reading scutes of source carapace {reference}"))?;
        if scutes.is_empty() {
            bail!("source carapace {reference} has no scute layers");
        }
        gpt_specs.push(
            build_gpt_spec(&manifest, &scutes, &blob_store)
                .await
                .with_context(|| format!("building vGPT for {reference}"))?,
        );
    }

    // Thin-launcher mounts (BUILD.md §2.2–§2.3): empty RW output, RO cache,
    // RO repo (git object store / work tree the guest resolves `--ref` against).
    let output = BuildOutput::new()?;
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
        git_ref,
        plan,
        lock,
        context: Some(context),
        location: args.location.clone(),
    };
    let response = Response::Success(build_input)
        .encode_cbor()
        .context("encoding build-input success response")?;

    let pipes = create_build_port_pipes().context("creating build-port pipes")?;
    let console = console_arg(pipes.device_rx_fd, pipes.device_tx_fd);

    let argv = build_dillo_args(
        &pmi_path,
        dtb_path.as_deref(),
        &gpt_specs,
        [&repo_dir, &cache_dir, output.as_ref()],
        Some(&console),
        memory,
        cpus,
    );

    let tag = args
        .tag
        .as_deref()
        .ok_or_else(|| anyhow!("pichi build requires -t <tag> to name the result"))?;

    let dillo = find_dillo();
    let mut console_log = ConsoleLog::new()?;
    output.check_empty()?;
    let result = async {
        let vm_result = boot_serve_and_wait(
            &dillo,
            &argv,
            pipes,
            response,
            &console_log,
            BUILD_VM_TIMEOUT,
        )
        .await;
        let verdict = output.verdict().context("checking build output")?;
        vm_result.with_context(|| format!("build VM failed; {verdict}"))?;
        if verdict != OutputVerdict::Complete {
            bail!("{verdict}");
        }
        // Marker presence alone is not success: use the normal fail-closed
        // importer before publishing anything under the requested tag.
        let imported = crate::cmd::load::OciLayout::read(output.as_ref(), &blob_store).await?;
        imported
            .import_build(&blob_store, &db, tag)
            .await
            .context("importing build output")
    }
    .await;
    let digest = result.map_err(|error| console_log.failure(error))?;
    println!("pichi build: imported {tag} -> {digest}");
    Ok(())
}

/// Assemble the `dillo` argument vector for the build VM (program name
/// excluded). The pure core, split out for testing without a boot.
pub(crate) fn build_dillo_args(
    pmi_path: &Path,
    dtb_path: Option<&Path>,
    gpt_specs: &[String],
    [repo_dir, cache_dir, output_dir]: [&Path; 3],
    console: Option<&str>,
    memory_mib: Option<u32>,
    cpus: Option<u32>,
) -> Vec<String> {
    let mut argv: Vec<String> = vec!["--pmi".to_string(), path_arg(pmi_path)];
    // Detached-mode PMI (Conglobate 0.2.0+): measured base DTB rides as its
    // own layer; pass it out-of-band like `pichi run` does.
    if let Some(dtb) = dtb_path {
        argv.push("--dtb".to_string());
        argv.push(path_arg(dtb));
    }
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
    // Source-carapace vGPTs remain guest build inputs. The output OCI
    // manifest owns inherited layers; host import never prepends them.
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

/// Conglobate appliance pinned by content digest.
/// Index digest identifies Conglobate f88a7215 (plan/context inputs, OCI-only output).
/// Official-image smoke verifies guest output, poweroff, and host import.
pub const DEFAULT_BUILD_IMAGE: &str = "ghcr.io/pichi-vm/conglobate@sha256:b9e26efd9d81ecb5b1a950e3f4891e9a04fbc7d9d222444621bccc95fad3babd";

/// Resolve the build appliance: `-b`/`--build-image`, else
/// [`DEFAULT_BUILD_IMAGE`].
fn resolve_build_image(flag: Option<&str>) -> String {
    flag.map_or_else(|| DEFAULT_BUILD_IMAGE.to_owned(), str::to_owned)
}

/// Pull the resolved appliance when it is not already cached.
///
/// Same download path as `pichi pull` (`--pull=missing`). A tag-db hit, or
/// a stored digest blob, skips the network.
async fn ensure_build_image(
    reference: &str,
    config: &Config,
    db: &FilesystemTagDb,
    blob_store: &FilesystemBlobStore,
) -> Result<()> {
    if image_cached(reference, db, blob_store).await? {
        return Ok(());
    }
    eprintln!("Unable to find build image {reference} locally; pulling...");
    crate::cmd::pull::run(
        PullArgs {
            reference: reference.to_string(),
            pull: Some(PullPolicy::Missing),
            quiet: false,
        },
        config,
    )
    .await
    .with_context(|| format!("auto-pull build image {reference}"))
}

/// True when `resolve_manifest` would find `reference` without a pull.
pub(crate) async fn image_cached(
    reference: &str,
    db: &FilesystemTagDb,
    blob_store: &FilesystemBlobStore,
) -> Result<bool> {
    let canonical = canonicalize_reference(reference);
    let target: Reference = canonical
        .parse()
        .with_context(|| format!("invalid image reference: {reference}"))?;
    blob_store.cached(&target, db).await
}

/// Map OCI `name:tag@digest` → `name@digest` before [`Reference`] parse.
///
/// Conglobate locks pins as `tag@digest`. `conglobate::artifact::Reference` currently
/// folds `:tag` into `repo` when `@digest` is present, so GHCR blob paths 404.
/// Digest wins; the tag is informational. No-op without `@digest` or without a
/// `:tag` after the last `/`.
pub(crate) fn canonicalize_reference(s: &str) -> std::borrow::Cow<'_, str> {
    let Some(at) = s.find('@') else {
        return std::borrow::Cow::Borrowed(s);
    };
    let name = &s[..at];
    let digest = &s[at..]; // includes '@'
    let search_from = name.rfind('/').map_or(0, |i| i + 1);
    let Some(rel_colon) = name[search_from..].find(':') else {
        return std::borrow::Cow::Borrowed(s);
    };
    let abs = search_from + rel_colon;
    std::borrow::Cow::Owned(format!("{}{}", &name[..abs], digest))
}

/// Resolve `rel` under `--context` inside the repo mount.
fn path_under_context(repo: &Path, context: &str, rel: &str) -> PathBuf {
    repo.join(context).join(rel)
}

/// Load the build lock (`pichi.lock` by default; `-l`/`--lock` overrides)
/// without interpreting any plan on the host.
fn load_build_lock(repo: &Path, context: &str, lock_rel: &str) -> Result<BuildLockFile> {
    let path = path_under_context(repo, context, lock_rel);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading build lock {}", path.display()))?;
    let parsed: BuildLockFile = serde_yaml::from_str(&text)
        .with_context(|| format!("parsing build lock {}", path.display()))?;
    Ok(parsed)
}

/// Resolve a cached reference to its parsed pichi manifest.
///
/// Returns `(pin_digest, manifest)` where `pin_digest` is the digest the tag
/// (or digest-ref) resolved to — the OCI index digest when the cache entry is
/// an image index (Conglobate Follow-up A lock pins), else the bare manifest
/// digest. Index entries are walked with the same `pichi`/`GOARCH` selector
/// as `pichi pull` (D-02 / D-20).
pub(crate) async fn resolve_manifest(
    reference: &str,
    db: &FilesystemTagDb,
    blob_store: &FilesystemBlobStore,
) -> Result<(Digest, Manifest)> {
    let canonical = canonicalize_reference(reference);
    let target: Reference = canonical
        .parse()
        .with_context(|| format!("invalid reference: {reference}"))?;
    let digest = blob_store.resolve(&target, db).await?;
    let (_, manifest) = blob_store
        .manifest(&digest)
        .await
        .with_context(|| format!("reading image {reference}; try pichi pull {reference}"))?;
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
    console_log: &ConsoleLog,
    time_limit: Duration,
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
    console_log.capture(&mut cmd)?;
    // Tear down dillo if the host drops the child (exchange/error paths).
    cmd.kill_on_drop(true);

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
        // tokio::process::Command does not wrap inherit_handles; apply it on
        // the inner std::process::Command (nightly feature gated in lib.rs).
        use std::os::windows::process::CommandExt as _;
        // FDs are only consumed by the unix pre_exec path; silence -D warnings.
        let _ = (device_rx_fd, device_tx_fd);
        cmd.as_std_mut().inherit_handles(true);
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning dillo at {}", dillo.display()))?;
    // Child has inherited the device ends; drop our copies.
    drop(device_rx);
    drop(device_tx);

    let mut serve =
        tokio::task::spawn_blocking(move || serve_build_port(host_rx, host_tx, &response));

    let result = tokio::time::timeout(time_limit, async {
        let status = tokio::select! {
            serve_result = &mut serve => {
                match serve_result.context("build-port task join")? {
                    Ok(()) => child.wait().await.context("waiting for build VM")?,
                    Err(err) => {
                        // dillo often exits first (e.g. missing detached --dtb). The
                        // failure-response write then hits Broken pipe — secondary.
                        // Prefer the guest/dillo exit so CI surfaces the real cause.
                        match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
                            Ok(Ok(st)) => {
                                bail!(
                                    "build-port exchange failed after guest/dillo exit ({st}): {err}"
                                );
                            }
                            Ok(Err(wait_err)) => {
                                return Err(err).context(format!(
                                    "build-port exchange failed (also waiting for build VM: {wait_err})"
                                ));
                            }
                            Err(_elapsed) => {
                                let _ = child.start_kill();
                                return Err(err).context(
                                    "build-port exchange failed (dillo still running; killed)",
                                );
                            }
                        }
                    }
                }
            }
            status = child.wait() => {
                let status = status.context("waiting for build VM")?;
                serve.abort();
                match (&mut serve).await {
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
    }).await;
    if !matches!(result, Ok(Ok(()))) {
        // Reap the child before reading the console log or output directory.
        let _ = child.kill().await;
        serve.abort();
    }
    result.with_context(|| format!("build VM timed out after {} seconds", time_limit.as_secs()))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use conglobate::artifact::ReferenceKind;
    use dillo_config::FsSpec;
    use pichi_storage::TagDb as _;

    #[cfg(unix)]
    #[tokio::test]
    async fn hung_build_is_killed_with_console_tail() {
        for exchange in [false, true] {
            let pipes = create_build_port_pipes().unwrap();
            let mut log = ConsoleLog::new().unwrap();
            let request = if exchange {
                format!(
                    "printf '\\xa2\\x61v\\x01\\x62op\\x6bbuild-input' >&{}; cat <&{} >/dev/null; ",
                    pipes.device_tx_fd, pipes.device_rx_fd
                )
            } else {
                String::new()
            };
            let script = format!("{request}echo 'conglobate: hung build'; exec sleep 60");
            let started = std::time::Instant::now();
            let error = boot_serve_and_wait(
                // Nix-based runners provide bash through PATH.
                Path::new("bash"),
                &["-c".into(), script],
                pipes,
                vec![0xa0],
                &log,
                Duration::from_secs(1),
            )
            .await
            .unwrap_err();
            assert!(started.elapsed() < Duration::from_secs(5));
            assert!(
                error.to_string().contains("timed out after 1 seconds"),
                "exchange={exchange}: {:#}",
                log.failure(error)
            );
            assert_eq!(log.failure(error).to_string(), "conglobate: hung build");
        }
    }

    fn parse_fs(value: &str) -> FsSpec {
        serde_keyvalue::from_key_values(value).expect("--fs value parses as dillo FsSpec")
    }

    #[tokio::test]
    async fn argv_has_pmi_repo_cache_output_console_and_one_gpt() {
        let console = console_arg(3, 4);
        let argv = build_dillo_args(
            Path::new("/cache/blobs/sha256/deadbeef"),
            None,
            &["partitions=[[path=/c,partuuid=u,typeguid=t,label=c:x]]".to_string()],
            [
                Path::new("/proj"),
                Path::new("/var/cache/pichi"),
                Path::new("/run/out"),
            ],
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
            None,
            &[],
            [Path::new("/proj"), Path::new("/cache"), Path::new("/out")],
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
    async fn argv_passes_dtb_when_present() {
        let argv = build_dillo_args(
            Path::new("/p.pmi"),
            Some(Path::new("/cache/blobs/sha256/dtbdead")),
            &[],
            [Path::new("/proj"), Path::new("/cache"), Path::new("/out")],
            None,
            None,
            None,
        );
        let i = argv.iter().position(|a| a == "--dtb").expect("--dtb");
        assert_eq!(argv[i + 1], "/cache/blobs/sha256/dtbdead");
    }

    #[test]
    fn build_image_flag_or_digest_pinned_default() {
        assert_eq!(resolve_build_image(Some("reg/bi:1")), "reg/bi:1");
        assert_eq!(resolve_build_image(None), DEFAULT_BUILD_IMAGE);
    }

    #[test]
    fn default_plan_and_lock_names() {
        assert_eq!(DEFAULT_PLAN, "pichi.yaml");
        assert_eq!(DEFAULT_LOCK, "pichi.lock");
    }

    #[tokio::test]
    async fn build_image_cached_follows_tag_or_digest_blob() {
        let tmp = tempfile::TempDir::new().unwrap();
        let graphroot = tmp.path().join("storage");
        std::fs::create_dir_all(&graphroot).unwrap();
        let db = FilesystemTagDb::open(&graphroot).unwrap();
        let blob_store = FilesystemBlobStore::new(&graphroot);

        let tag_ref = "ghcr.io/example/appliance:1";
        assert!(!image_cached(tag_ref, &db, &blob_store).await.unwrap());

        let digest = crate::cmd::load::tests::LayoutFixture::new().write(&graphroot);
        db.set_tag(tag_ref, &digest).await.unwrap();
        assert!(image_cached(tag_ref, &db, &blob_store).await.unwrap());

        // A legacy mapping to another digest cannot satisfy the pin.
        let pin = "ghcr.io/pichi-vm/conglobate:0.2.0@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        assert!(!image_cached(pin, &db, &blob_store).await.unwrap());
        let canon = canonicalize_reference(pin);
        db.set_tag(canon.as_ref(), &digest).await.unwrap();
        assert!(!image_cached(pin, &db, &blob_store).await.unwrap());
    }

    #[test]
    fn canonicalize_reference_strips_tag_before_digest() {
        let pinned = format!(
            "ghcr.io/pichi-vm/conglobate:0.2.0@{}",
            DEFAULT_BUILD_IMAGE.split_once('@').unwrap().1
        );
        let canon = canonicalize_reference(&pinned);
        assert_eq!(
            canon.as_ref(),
            "ghcr.io/pichi-vm/conglobate@sha256:b9e26efd9d81ecb5b1a950e3f4891e9a04fbc7d9d222444621bccc95fad3babd"
        );
        let parsed: Reference = canon.parse().unwrap();
        assert_eq!(parsed.registry, "ghcr.io");
        assert_eq!(parsed.repo, "pichi-vm/conglobate");
        match parsed.kind {
            ReferenceKind::Digest(d) => {
                assert_eq!(
                    d.to_string(),
                    "sha256:b9e26efd9d81ecb5b1a950e3f4891e9a04fbc7d9d222444621bccc95fad3babd"
                );
            }
            ReferenceKind::Tag(t) => panic!("expected digest kind, got tag {t}"),
        }
    }

    #[test]
    fn canonicalize_reference_noop_without_digest_or_tag() {
        assert_eq!(
            canonicalize_reference("ghcr.io/pichi-vm/conglobate:0.2.0").as_ref(),
            "ghcr.io/pichi-vm/conglobate:0.2.0"
        );
        assert_eq!(
            canonicalize_reference(
                "ghcr.io/pichi-vm/conglobate@sha256:b9e26efd9d81ecb5b1a950e3f4891e9a04fbc7d9d222444621bccc95fad3babd"
            )
            .as_ref(),
            "ghcr.io/pichi-vm/conglobate@sha256:b9e26efd9d81ecb5b1a950e3f4891e9a04fbc7d9d222444621bccc95fad3babd"
        );
    }

    /// Host-side smoke: mounts, build-port exchange, then guest OCI import.
    #[tokio::test]
    async fn smoke_port_output_import() {
        use crate::cmd::load::tests::LayoutFixture;
        use std::io::{Read, Write};
        use std::thread;
        use std::time::Duration;

        let console = console_arg(5, 6);
        let argv = build_dillo_args(
            Path::new("/cache/blobs/sha256/deadbeef"),
            None,
            &[],
            [
                Path::new("/proj"),
                Path::new("/var/cache/pichi"),
                Path::new("/run/out"),
            ],
            Some(&console),
            None,
            None,
        );
        let fs_values: Vec<&String> = argv
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "--fs")
            .map(|(i, _)| &argv[i + 1])
            .collect();
        assert_eq!(fs_values.len(), 3);
        assert!(fs_values[0].contains("tag=repo") && fs_values[0].contains("readonly"));
        assert!(fs_values[1].contains("tag=cache") && fs_values[1].contains("readonly"));
        assert!(
            fs_values[2].contains("tag=output") && !fs_values[2].contains("readonly"),
            "output sink must be writable: {}",
            fs_values[2]
        );
        assert!(
            argv.iter().any(|a| a.contains(build_port::BUILD_PORT_NAME)),
            "missing named console {}",
            build_port::BUILD_PORT_NAME
        );

        let tmp = tempfile::TempDir::new().unwrap();
        let output_dir = tmp.path().join("output");
        std::fs::create_dir_all(&output_dir).unwrap();

        let fixture = LayoutFixture::new();

        let input = BuildInput {
            git_ref: "main".into(),
            plan: DEFAULT_PLAN.into(),
            lock: DEFAULT_LOCK.into(),
            context: Some("tests/distro/context".into()),
            location: None,
        };
        let response = Response::Success(input).encode_cbor().unwrap();

        let pipes = create_build_port_pipes().unwrap();
        let mut device_tx = pipes.device_tx;
        let mut device_rx = pipes.device_rx;
        let host_rx = pipes.host_rx;
        let host_tx = pipes.host_tx;

        let guest = thread::spawn(move || {
            // Hold-until-open: guest request arrives after host is waiting.
            thread::sleep(Duration::from_millis(50));
            let mut req = Vec::new();
            ciborium::into_writer(&build_port::GuestRequest::default(), &mut req).unwrap();
            device_tx.write_all(&req).unwrap();
            device_tx.flush().unwrap();
            let mut reply = Vec::new();
            device_rx.read_to_end(&mut reply).unwrap();
            assert!(!reply.is_empty(), "host wrote no build-input response");

            fixture.write(&output_dir);
            (reply, output_dir)
        });

        serve_build_port(host_rx, host_tx, &response).unwrap();
        let (reply, output_dir) = guest.join().unwrap();
        assert_eq!(reply, response, "guest must receive prepared build-input");

        let graphroot = tmp.path().join("storage");
        std::fs::create_dir_all(&graphroot).unwrap();
        let blob_store = FilesystemBlobStore::new(&graphroot);
        let db = FilesystemTagDb::open(&graphroot).unwrap();
        let digest = crate::cmd::load::OciLayout::read(&output_dir, &blob_store)
            .await
            .unwrap()
            .import_build(&blob_store, &db, "smoke:oci")
            .await
            .unwrap();
        let key = "smoke:oci".parse::<Reference>().unwrap().to_string();
        assert_eq!(db.resolve_tag(&key).await.unwrap(), Some(digest));
    }

    #[test]
    fn load_build_lock_reads_conglobate_shape() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = tmp.path().join("tests/distro/context");
        std::fs::create_dir_all(&ctx).unwrap();
        std::fs::write(
            ctx.join("pichi.lock"),
            "version: 1\nimages:\n  qualification.local/fedora:43:\n    index: sha256:abababababababababababababababababababababababababababababababab\n",
        )
        .unwrap();
        let parsed = load_build_lock(tmp.path(), "tests/distro/context", "pichi.lock").unwrap();
        assert_eq!(parsed.version, 1);
        assert_eq!(
            parsed.images["qualification.local/fedora:43"]
                .index
                .to_string(),
            "sha256:abababababababababababababababababababababababababababababababab"
        );
    }
    /// A pin must never resolve to different metadata through a tag mapping.
    #[tokio::test]
    async fn resolve_manifest_rejects_missing_pin_despite_tag_mapping() {
        use pichi_storage::{BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};

        let tmp = tempfile::TempDir::new().unwrap();
        let graphroot = tmp.path().join("storage");
        std::fs::create_dir_all(&graphroot).unwrap();
        let db = FilesystemTagDb::open(&graphroot).unwrap();
        let blob_store = FilesystemBlobStore::new(&graphroot);

        // Minimal D-07-valid PMI-only appliance manifest (empty config).
        let platform_bytes = br#"{
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "artifactType": "application/vnd.pichi.artifact.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.empty.v1+json",
                "digest": "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a",
                "size": 2,
                "data": "e30="
            },
            "layers": [{
                "mediaType": "application/vnd.pichi.pmi.v1",
                "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "size": 1
            }],
            "annotations": {}
        }"#;
        // Compact JSON so digest is stable-ish; recompute after parse round-trip.
        let platform_val: serde_json::Value = serde_json::from_slice(platform_bytes).unwrap();
        let platform = serde_json::to_vec(&platform_val).unwrap();
        let platform_digest =
            Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(&platform).into());
        blob_store
            .put_blob(&platform_digest, &platform)
            .await
            .unwrap();

        // Fake index digest — deliberately NOT stored as a blob.
        let index_digest: Digest =
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                .parse()
                .unwrap();
        assert!(!blob_store.blob_exists(&index_digest).await);

        let tag_key = format!("ghcr.io/pichi-vm/conglobate@{index_digest}");
        db.set_tag(&tag_key, &platform_digest).await.unwrap();

        let pinned = format!("ghcr.io/pichi-vm/conglobate:0.2.0@{index_digest}");
        let error = resolve_manifest(&pinned, &db, &blob_store)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("pichi pull"));
    }
    #[test]
    fn build_lock_is_required_but_may_have_no_remote_inputs() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load_build_lock(tmp.path(), ".", "pichi.lock").is_err());
        std::fs::write(tmp.path().join("pichi.lock"), "version: 1\nimages: {}\n").unwrap();
        assert!(
            load_build_lock(tmp.path(), ".", "pichi.lock")
                .unwrap()
                .images
                .is_empty()
        );
    }
}
