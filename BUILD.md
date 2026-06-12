# Dillo: Image Build System

## Design Document — Draft

---

## 1. Thesis

dillo's primary responsibility on the host is *launching VMs from registry artifacts*. Image construction is a secondary concern, and dillo solves it by making the build environment itself a dillo VM. Building is launching, with one specific appliance.

This collapses three problems into none:

1. **Bootstrap.** No host-side root requirement, no host-side mount, no host-side filesystem tooling beyond a tiny userspace converter. The user produces a raw GPT image any way they like (mkosi recommended); `dillo import` converts it to a base carapace via a pure-userspace transform; subsequent layering happens in the build VM with VM-root.
2. **Build kernel.** No need to bundle a kernel into dillo or interrogate the host kernel for required modules. The build image's IGVM carries the kernel.
3. **Build tools.** No need to bundle filesystem tools into dillo. They live in the build image's rootfs and are updated independently of dillo.

The host requirement reduces to `/dev/kvm`. dillo stays a small Rust binary with one job — pulling artifacts and launching VMs.

---

## 2. Object Model

A registry tag holds **one OCI artifact** containing two logical objects:

| Object | Required | Purpose |
|--------|----------|---------|
| Carapace | Yes | A stack of scutes (one or more layers). The bootable rootfs in composed form. Always read-only. |
| IGVM | Optional | Boot payload (kernel, initrd, cmdline, measured platform layout, measured manifest binding the carapace top-hash). Required to launch the artifact. |

`dillo run <tag>` requires the artifact to contain an IGVM. Without one, the artifact is a base — usable as a `from:` source but not bootable.

The dillo ↔ carapace relationship is defined separately in the [carapace specification](https://github.com/ShelbyADillo/carapace). Briefly:

- A **scute** is a layer: one cow file (dm-snapshot persistent COW format) and one verity file (dm-verity hash tree over the cow).
- A **carapace** is N scutes composed via salt-chain binding. The top scute's verity root (`rootₙ₋₁`) is the trust anchor.
- The IGVM's measured manifest binds the expected `rootₙ₋₁`. The guest verifies what it mounts against this measurement.

### 2.1 GPT Inside the Carapace

The composed carapace block device contains a **GPT** with partitions following systemd's [Discoverable Partitions Specification](https://uapi-group.org/specifications/specs/discoverable_partitions_specification/). This is a hard requirement, not a convention.

This means there are two GPTs in the runtime stack, serving completely different purposes:

| GPT | Where | What it identifies | Consumer |
|-----|-------|-------------------|----------|
| Outer | Synthesized by the host's carapace device | Individual scutes (DDI PARTUUIDs from the carapace spec) | Guest's carapace-assembly code |
| Inner | Inside the composed carapace block device | Filesystem partitions (PARTUUIDs from systemd's Discoverable Partitions spec) | systemd-gpt-auto-generator at boot |

Consequences:

- Appliance authors do not write fstab or mount units. `systemd-gpt-auto-generator` discovers root, `/usr`, `/var`, etc. by well-known PARTUUID.
- The kernel cmdline does not need an explicit `root=`. Systemd finds it by architecture-specific PARTUUID.
- The model aligns with mkosi / systemd-sysext / image-based Linux. Standard tooling, standard idioms, no dillo-specific guest agent for filesystem assembly.
- Writable scratch (`/var`, `/tmp`, `/run`) uses standard tmpfs overlays configured by the appliance's systemd. Carapaces themselves stay read-only (per §2.3).

### 2.2 Multi-Carapace and Launch Config — v2

v1 supports exactly **one carapace per artifact**, bound implicitly to the IGVM in the same tag. No external launch config blob.

When the time comes to support attached carapaces (model weights, supplementary data, etc.) and the configuration that names them, this is v2 work and gets its own design pass.

### 2.3 Mutable Storage — Out of Scope

Carapaces are always read-only. Mutable storage is a different object type and is not designed in this document.

---

## 3. CLI Surface

dillo's CLI mirrors podman/docker for image management:

| Command | Purpose |
|---------|---------|
| `dillo import <raw-image> <tag>` | Convert a raw GPT image into a base carapace. Pure host-side userspace. Tags the result in the local cache. |
| `dillo build [-t <tag>] <dir>` | Build an artifact from `<dir>/dillo.yaml`. Always derives from a `from:` carapace. Uses the build VM. |
| `dillo run <tag> [flags]` | Launch a VM from a tag. Errors if not cached. Requires the artifact to contain an IGVM. |
| `dillo pull <tag>` | Pull a tag's artifact into the local cache. |
| `dillo push <tag>` | Push a cached tag to its registry. |
| `dillo images` | List locally cached artifacts. |
| `dillo inspect <tag>` | Show artifact contents (carapace metadata, IGVM presence, manifest details). |
| `dillo rmi <tag>` | Remove a cached artifact. |

There is no `dillo bootstrap`. The role it would have served is split: raw-image production is the user's concern (recommended: mkosi), and `dillo import` performs the userspace conversion to a base carapace.

---

## 4. `dillo import`

Equivalent in spirit to `podman import`: takes raw bytes, produces a base image.

**Input:** A raw disk image with a GPT inside, partitioned per the systemd Discoverable Partitions Specification. How the user produces this image is outside dillo's scope. Recommended toolchain: mkosi. Other valid choices: hand-rolled `parted` + `mkfs` + `mount` + `tar`, `systemd-repart`, custom build pipelines, dumps from real disks. dillo does not validate the *contents* of the image beyond requiring a parseable GPT.

**Operation:** Pure host-side userspace. No root, no kernel modules, no mounts, no ioctls.

1. Open raw image; verify GPT header is parseable.
2. For each chunk in the raw image (chunk size = dm-snapshot default):
   - If the chunk is all zero, skip (origin matches dm-zero, no exception needed).
   - Otherwise, append an exception entry to the COW file: `(chunk_index_in_raw → chunk_index_in_cow_data_area)`, and append the chunk data.
3. Compute the dm-verity hash tree over the produced COW file.
4. Write `(cow, verity)` as a one-scute carapace artifact in the local image cache, tagged per the CLI argument.

**Output:** A base carapace. Single scute. No IGVM. Usable as a `from:` source for `dillo build`.

**Implementation:** A small Rust crate in this repo (probably `tools/import/` or as a library used by `dillo import`). Estimated ~200-500 LOC. The dm-snapshot persistent COW format is documented; the dm-verity tree computation has existing crate options.

---

## 5. The Build Image

The build image is itself a dillo artifact, hosted in a registry. `dillo build` invokes `dillo run` (internally) on it with extra wiring:

- The user's project directory is mounted into the build VM via virtio-fs (read-only) as the build context.
- An output virtio-fs export receives the produced scute and IGVM artifacts.
- A vsock control plane carries commands from host to the in-VM **build agent** (running as init).
- Network access is granted (the build needs `dnf`, `pip`, etc. to fetch packages during user `RUN` steps).

### 5.1 Default Reference and Override

The default build image reference is a tag, not a digest:

```
registry.dillo.dev/build-image:stable    # default, baked into dillo source
```

Override via, in precedence order:

1. `--build-image <ref>` CLI flag
2. `DILLO_BUILD_IMAGE` environment variable
3. `~/.config/dillo/config.yaml`'s `build_image` field
4. The default

Pinning is the user's choice, not dillo's. Anyone who wants a reproducible build pins a digest in their override.

### 5.2 Trust Model

| Phase | Trust anchor |
|-------|--------------|
| Today | Registry + TLS (with optional cosign signatures as a transitional middle step) |
| When CVM lands | The build VM runs as an SNP/TDX guest. The launch measurement covers `(build IGVM, build carapace top-hash)`. The agent reports build-output hashes via attestation. Built artifacts can carry attestation reports as hardware-rooted provenance. |

The attestation story is what makes "no digest pin in dillo source" the right choice — once CVM is online, trust is hardware-rooted and ephemeral, not source-rooted and static.

### 5.3 Build Image Contents

The build image's rootfs carries:

- A static **build agent** binary as init (PID 1)
- Layer-execution tooling: `mount`, `losetup`, `dmsetup`, `veritysetup`
- The IGVM-construction tool (`arma`, possibly renamed; see §6.4)
- Initramfs builders for IGVM production: `dracut` (and/or `mkinitcpio`)
- Whatever package-management tooling user `RUN` steps need: `dnf`, `apt-get`, `apk`, etc.

Notably **not** in the build image:
- mkosi (lives on the host or wherever the user produces raw images)
- The raw-to-COW converter (lives in the dillo binary, host-side)
- A kernel (the IGVM carries it)

The build image is large (hundreds of MB compressed) and that's fine — pulled once per host, cached locally, updated rarely.

### 5.4 Self-Hosting

A chicken-and-egg: building the build image requires a build image. Resolved by the standard compiler-bootstrap pattern:

- **v0:** A one-off bootstrap tool in the dillo repo (`tools/bootstrap-build-image/`) constructs a single-layer build image by hand from raw inputs (kernel, minimal rootfs raw image, agent binary). Used exactly once. Pushed to `registry.dillo.dev/build-image:0.1`.
- **v1:** Use the v0 build image to rebuild the build image with a proper multi-layer `dillo.yaml`. Push as `:0.2`.
- **v2+:** Each release of dillo's build image is built using the previous release's build image. Self-hosted from here on.

The one-off bootstrap tool is a CI-only artifact, never invoked by users.

---

## 6. `dillo build`

### 6.1 Flow

1. Host reads `dillo.yaml` and the build context.
2. Host ensures the build image is in the local cache (pulls if not).
3. Host launches the build VM via the normal `dillo run` path with extra wiring (virtio-fs in/out, vsock control plane).
4. Host walks the `dillo.yaml` and sends per-step commands to the build agent over vsock. Each command produces exactly one output: a scute, an ephemeral scute (discarded), or an IGVM.
5. Agent executes inside the build VM. Each step:
   - Stack a new dm-snapshot on the parent scute
   - Mount the resulting block device
   - Run the directive (`run`/`copy`)
   - Unmount, tear down dm-snapshot
   - Compute dm-verity tree over the COW
   - Write `(cow, verity)` pair to the output virtio-fs (or skip writing for ephemeral steps)
6. Host packages the produced scutes and the IGVM (if any) into a single OCI artifact and stores in the local cache, tagged per `-t`.
7. Build VM shuts down.

The build VM **persists across all steps** of one build. One boot per `dillo build` invocation.

### 6.2 Layer Caching

Cache key per scute:

```
hash(parent_rootₙ₋₁ || directive_kind || directive_content)
```

For `copy:`, `directive_content` includes the content hash of every file under `src`. For `run:`, it's the literal command string. Cache hits cause the host to short-circuit — it tells the agent "skip this step, reuse scute X" — and the agent jumps the dm-snapshot stack ahead. Misses invalidate the layer and every layer above it.

Cache lives in the local image cache. Pushing to a registry warms remote consumers; pulling warms the local cache.

### 6.3 `dillo.yaml`

```yaml
from:
  scute: registry.example.com/base/fedora:43

layer:                        # zero or more retained layers
  - run: dnf install -y python3 torch
  - copy:
      src: ./app
      dst: /opt/app
  - run: chown -R appuser:appuser /opt/app

igvm:                         # optional; produces a bootable artifact when present
  layer:                      # zero or more ephemeral layers (discarded after IGVM extraction)
    - run: dracut --add-drivers "virtio_blk virtio_console" /tmp/initramfs.img
  build: arma -k /boot/vmlinuz-* -i /tmp/initramfs.img -c "$CMDLINE" -o /out/boot.igvm
```

#### `from:`

Exactly one variant in v1: `scute: <ref>`. Derive from an existing carapace.

There is no `tarball:` or `raw:` or `oci:` variant. To start from a freshly-produced raw image, run `dillo import <raw> <tag>` first, then reference `<tag>` in `from: scute:`.

#### `layer:`

A list of directives, each producing one retained scute:

- `run: <command>` — execute a shell command inside the build VM, working directory `/`. Whatever shell and tools exist in the parent scute are available.
- `copy: { src, dst }` — copy files from the build context (mounted via virtio-fs) into the guest filesystem.

`env:` and `workdir:` are deferred; inline them in `run` commands for now.

#### `igvm:`

Optional. If present, the artifact is bootable.

- `igvm.layer:` — ephemeral layers stacked on top of the retained chain. Used to install/configure things needed for IGVM production (e.g., `dracut`) without retaining them as scutes.
- `igvm.build:` — the command that produces the IGVM file at `/out/boot.igvm` (or wherever, as long as it ends up in the output virtio-fs).

The IGVM-construction tool is the user's choice. dillo ships `arma` as a convenience preinstalled in the official build image, but users may install and invoke any IGVM builder they like.

### 6.4 Cmdline

The cmdline lives inside the IGVM and is the IGVM author's domain. dillo never injects or modifies cmdline parameters. arma may warn about missing recommended options but never appends them.

### 6.5 arma

arma is the current IGVM-construction tool (`tools/arma` in this repo). It is a convenience, not a requirement. Its role is to take `(kernel, initrd, cmdline, manifest, carapace top-hash)` and produce a valid IGVM file.

In the build image model, arma lives inside the build image's rootfs. Users who want a different IGVM builder can install one in their `igvm.layer:` ephemeral steps.

---

## 7. Carapace Device

At runtime, all of a VM's scutes are exposed to the guest as **one virtio-blk** — the carapace device.

The carapace device is a VMM-side synthetic block device. The VMM:

1. Reads the artifact's IGVM-internal manifest (CBOR ACPI OEM table per `DESIGN.md` §6) to know the expected scute count, order, and partition layout.
2. Lays out a GPT image whose partitions are the scutes, using the carapace specification's GPT deployment pattern (DDI PARTUUIDs + base-flag attribute on the bottom scute).
3. Exposes the synthesized image as one virtio-blk to the guest. Reads are translated to the underlying scute files; the GPT header and partition table are synthesized in memory.

The guest's initrd:

1. Enumerates partitions on the carapace device via standard GPT parsing.
2. Identifies scutes by DDI PARTUUID.
3. Stacks dm-verity over dm-snapshot per the salt-chain rules.
4. Verifies the top-hash (`rootₙ₋₁`) against the value in the IGVM-measured manifest.
5. Exposes the composed carapace block device.

The composed device contains the inner GPT (per §2.1). systemd-gpt-auto-generator takes over from there: discovers root, `/usr`, `/var`, etc. by Discoverable PARTUUIDs, generates mount units, mounts everything. No appliance-specific filesystem assembly code beyond the carapace stacking step.

### 7.1 Why One Outer Device

PCIe slot pressure. In v2 with multi-carapace support, a single VM might have a dozen scutes across multiple carapaces. One virtio-blk per scute would exhaust slots fast. One virtio-blk for the whole set is cheap, matches carapace's GPT deployment pattern, and keeps slot accounting simple.

---

## 8. Privilege Model

| Operation | Where | Privilege required |
|-----------|-------|-------------------|
| `dillo import` | Host | None (pure userspace) |
| `dillo build` | Host | `/dev/kvm` |
| `dillo run` | Host | `/dev/kvm` |
| `dillo pull` / `push` | Host | Network + cache write |
| Layer execution | Build VM guest | Real root inside the guest |
| Carapace assembly | Production VM guest | Real root inside the guest |
| Raw image production (mkosi etc.) | Outside dillo | User's concern |

The host never:
- Mounts a scute filesystem
- Loads kernel modules to support a build
- Runs any dillo operation as root
- Parses untrusted bytes in a privileged code path

If the build VM crashes or misbehaves, the blast radius is the VM itself.

---

## 9. Things This Document Deliberately Does Not Do

- **`dillo build` never injects kernel cmdline parameters.** The cmdline is the author's domain.
- **No host-side mount of scutes.** Even with `--privileged`. The build VM exists specifically to keep this off the host.
- **No FUSE for layer mounting.** FUSE blocks `security.*` xattrs, breaking SELinux.
- **No bundled tools/world in dillo.** The build image carries everything.
- **No raw-image production tooling in dillo.** mkosi (or whatever) lives outside; dillo only consumes the raw image via `dillo import`.
- **No digest pin for the build image in dillo source.** Update flow is via registry pull. Hardware attestation is the long-term trust anchor.
- **No `entrypoint` directive.** Guest init is determined by the rootfs contents, not by image metadata.
- **No fstab in appliances.** The inner GPT + systemd-gpt-auto-generator handles mounting.

---

## 10. Deferred to Future Versions

| Item | Reason |
|------|--------|
| Multi-carapace per artifact | v2. Requires explicit launch config and measured manifest extension. |
| Mutable storage / volumes | Separate object type. Different design pass. |
| Explicit launch config | v2, alongside multi-carapace. Today: implicit single-carapace binding. |
| Attestation-based provenance for build outputs | Lands when CVM ships. |
| `env:` and `workdir:` directives | Inline in `run` commands. |
| Multi-stage builds (`copy --from=<stage>`) | The `igvm.layer:` ephemeral pattern covers the most common use; broader multi-stage is v2+. |
| Cross-host build cache | Local cache + registry push is sufficient for v1. |
| Reproducibility tooling (faked time, frozen package indices) | Author's choice today; revisit if it becomes a pain point. |

---

## 11. Open Questions

- **Build agent name.** "corium" was previously used in design memory but conflated with a runtime concept. The build agent is build-time only. A name needs to be chosen and any other usages retired.
- **Build agent ↔ host wire protocol.** vsock + JSON-RPC mirroring `DESIGN.md` §15.2 is the obvious choice. Method set: `begin_layer`, `run`, `copy`, `end_layer`, `produce_igvm`, `cache_hit`. To be specified.
- **Outer GPT layout determinism.** PARTUUIDs derived deterministically from carapace ID + scute hash so the synthesized GPT image is reproducible? Or random per launch? Determinism is friendlier for caching the synthesized GPT header.
- **OCI artifact layout details.** Media types (`application/vnd.dillo.scute.v1`, `application/vnd.dillo.igvm.v1`, plus the artifactType for the wrapper), how scutes are ordered in the manifest's layer list, annotation conventions for tooling discoverability.
- **The first build image's distro base.** Fedora, Alpine, Debian, something custom? Affects long-term maintenance and the userspace tool versions available to user builds.
- **Raw-to-COW chunk size.** The dm-snapshot persistent COW format has a configurable chunk size (default 32 sectors = 16 KiB). Does `dillo import` always use the default, or expose a flag?

---

*Status: design draft. No code yet. This document defines the next milestone's scope.*
