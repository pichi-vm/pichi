# pichi

The docker/podman-style front-end for the [pichi-vm](https://github.com/pichi-vm)
ecosystem: it manages VM images as OCI artifacts and boots them.

`pichi` owns image management — importing, pulling, pushing, and inspecting
artifacts in a local content-addressed cache — and delegates the actual VM
launch to the [`dillo`](https://github.com/pichi-vm/dillo) VMM. `pichi run`
derives the guest's device set from the cached artifact, then launches Dillo.

## The artifact model

A pichi artifact is an OCI image composed of:

- **scute layers** — the read-only block-device layers of a
  [carapace](https://github.com/pichi-vm/carapace) (the guest root filesystem),
- an optional **PMI** layer — the measured boot payload (kernel + initramfs)
  produced by [`arma`](https://github.com/pichi-vm/arma), described by the
  [`pmi`](https://github.com/pichi-vm/pmi) wire-format spec,
- optional native **bootconfig**, and
- a **config** blob — the launch contract (CPU / memory / storage / interface
  requirements).

An artifact is *bootable* exactly when it carries a PMI. A carapace with no PMI
is still a valid image — a base others derive from.

## Commands

| Command | Purpose |
|---|---|
| `pichi import` | Import a raw image (+ optional PMI / DTB / config) into the cache |
| `pichi images` | List cached artifacts |
| `pichi inspect` | Inspect a cached image and its platform manifest |
| `pichi tag` / `rmi` | Manage tags (refcount-aware blob GC) |
| `pichi pull` / `push` | Move artifacts to and from an OCI registry |
| `pichi run` | Boot a cached artifact with its declared resources |
| `pichi update` | Update a working-tree plan’s image lock |
| `pichi build` | Build via a VM appliance (thin launcher; see [BUILD.md](BUILD.md)) |
| `pichi system` | System-level inspection and maintenance |

Run `pichi <command> --help` for details.

## Launch requirements

The image config's recommended CPU, memory and storage sizes supply the run
defaults. Operator configuration (`run.cpus`, `run.memory_mib`, `run.storage_mib`)
overrides those defaults, and CLI values take precedence. Allocations below a
required minimum fail; manual allocations below a numeric recommendation warn.
`recommended: max` uses detected host availability when no operator value is
set: available CPU parallelism, allocatable guest memory, or free space on the
storage backing filesystem. Linux guest memory uses the available, unreserved
2-MiB hugepage pool; other hosts use available physical memory, rounded to 2 MiB.

Pichi attaches one unformatted writable block device alongside the read-only
image. The distribution decides how to format and mount it. Disposable storage
is removed after the VM exits. To retain it across runs, provide a raw file:

```sh
pichi run example:1 --cpus 4 --memory 4096 --storage 8192 \
  --storage-file /path/to/instance.raw
```

Sizes are in MiB. `run.storage_file` can configure the retained file. Pichi
locks retained storage against concurrent use, preserves existing contents,
and refuses to shrink it. A supplied file's existing capacity is an operator
allocation when no explicit size is set.

The image's `requirements.slot` defaults to `mmio`; interface and storage
slot declarations override it. Pichi supplies an explicit bus for every device.
Dillo allocates numeric slots within that bus and fails if the declared
platform cannot satisfy the request.

`pichi inspect` includes the decoded image config beside its manifest. Templates
can access it directly, for example
`pichi inspect example:1 --format '{{ config.requirements.storage.recommended }}'`.

Builds use the build appliance's requirements. `pichi build --storage <MiB>`
overrides its recommended storage allocation; output-image requirements remain
metadata for later launches.

## Build

`pichi build <dir> --ref <ref> -t <tag>` is a specialization of `pichi run`:
same launch path, plus virtiofs wires and a bidirectional build-port CBOR
exchange on named console `dev.pichi.build` (guest requests build-input; host
replies). The host is a thin launcher; Conglobate's pichi image performs the
build and writes an OCI layout to the output mount.

Plan and lock paths are relative to `--context` (default `.`). The defaults
are `pichi.yaml` and `pichi.lock`. `-p`/`--plan` and `-l`/`--lock` override
them. The build-port response carries those resolved relative paths as
`plan` and `lock`, alongside the selected `context`.

Update the lock from the plan currently on disk, then commit both files:

```sh
pichi update .
git add pichi.yaml pichi.lock
git commit -m "Update image inputs"
```

`pichi update [DIR]` defaults to the current directory. `-p`/`--plan` and
`-l`/`--lock` select paths within DIR, defaulting to `pichi.yaml` and
`pichi.lock`. It reads working-tree edits directly; Git, the Conglobate CLI,
and a VM are unnecessary. Stage commands are never executed during update. Plans require an explicit
`version: 1` field.

Update resolves only the selected image's remote dependencies, including
stage-copy and boot sources, and replaces the complete version-1 lock with
exact OCI index pins. Unused entries are removed. An error preserves the
existing lock. The summary reports added, changed, and removed pins; an
unreadable or malformed previous lock can be replaced, with comparison
reported as unavailable. Plan reads and lock writes stay within DIR.

The default `--pull=always` refreshes references from the registry. The
existing pull policies also apply: `missing` uses cached references and
fetches absent ones, `never` uses only the local cache, and `newer` checks
upstream and refreshes changed images. For locally imported inputs or
cached-only operation, use `pichi update --pull=never`. Inputs must resolve
to OCI indexes; bare manifest references are rejected.

The `conglobate` library supplies plan, artifact, and lock types;
`pichi-launch` supplies the build-port types. `pichi update` maintains locks
on the host; `pichi build` remains a thin launcher and consumes a committed
plan and lock in the appliance.

`-b`/`--build-image` selects the build appliance image. When it is omitted,
pichi uses the digest-pinned default. If that image is not already cached,
the first build pulls it and continues. Full contract: [BUILD.md](BUILD.md).

## Source checkout

Initialize the pinned Conglobate source before building:

```sh
git submodule update --init --recursive
```

## License

Apache-2.0. See [LICENSE](LICENSE).
