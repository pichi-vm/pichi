# pichi

The docker/podman-style front-end for the [pichi-vm](https://github.com/pichi-vm)
ecosystem: it manages VM images as OCI artifacts and boots them.

`pichi` owns image management — importing, pulling, pushing, and inspecting
artifacts in a local content-addressed cache — and delegates the actual VM
launch to the [`dillo`](https://github.com/pichi-vm/dillo) VMM. `pichi run`
derives the guest's device set from the cached artifact, then launches dillo
through the separate [`madrillo`](https://github.com/pichi-vm/madrillo) monitor.

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
| `pichi run` | Boot an artifact under madrillo; `-d` prints a durable workload ID, `-i` forwards stdin |
| `pichi attach` | Attach to live console output; `-i` also acquires input |
| `pichi logs` | Print a snapshot of retained stdout/stderr for a workload ID |
| `pichi wait` | Wait for a workload ID and print its native exit status |
| `pichi stop` | Request console shutdown; force cleanup after `--time` seconds (default 10) |
| `pichi ps` | List retained workload IDs and running/exited/unknown state |
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

Madrillo retains the storage reservation across detach and caller exit. Disposable
backing lives in the private workload record. A completion callback removes it
only after confirmed workload cleanup; `wait` and `stop` retry that cleanup.
Missing or incomplete results preserve the backing for reconciliation.

Sizes are in MiB. `run.storage_file` can configure the retained file. Pichi
locks retained storage against concurrent use, preserves existing contents,
and refuses to shrink it. A supplied file's existing capacity is an operator
allocation when no explicit size is set.

The global `requirements.slot` supplies a preferred PCI or virtio-MMIO
transport; interface and storage preferences override it. Dillo assigns actual
slots within the platform's declared capacity and can fall back to another
transport when a preference cannot be met.

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


Madrillo integration is under development on this branch. Set `PICHI_MADRILLO`
to its executable (otherwise a sibling executable or PATH is used), and use a
dillo build whose console closure requests shutdown. Foreground run prints its
ID on stderr; detached run prints it on stdout. Host terminals use raw input with
`-i`; Ctrl-P Ctrl-Q detaches. Piped input remains binary. Input EOF releases
ownership while madrillo keeps dillo's liveness pipe open. Attachment is live
only; completed workloads retain their result and configured CRI console log.
An interactive terminal sends its initial dimensions and subsequent changes
directly to Dillo's private resize endpoint. Reattachment supplies the new
terminal's dimensions, including after a detached launch. Only the input owner
can resize; output-only attachments and piped input do not change guest size.
Madrillo does not interpret these updates. Guest resize requires Linux's
[virtio-console cols/rows fix](https://github.com/torvalds/linux/commit/5326ab737a47278dbd16ed3ee7380b26c7056ddd)
or a backport; older kernels interpret the standard dimensions in reverse.
Confirmed workload cleanup also removes a Unix resize socket left by an abrupt
Dillo exit. Unknown or incomplete cleanup preserves runtime files.
`pichi logs ID` prints a snapshot of that log to the original stdout/stderr
streams, preserving binary bytes and partial-line fragments. It includes only
complete CRI records present when the file is opened; a record still being
written appears on a later invocation. Logs remain available after monitor exit
or loss, independently of the live attachment endpoint.
`pichi stop ID` closes Dillo's console input to request clean shutdown, even
while another client owns input. After 10 seconds it asks madrillo to force
workload cleanup; `--time SECONDS` changes that grace period and `--time 0`
forces immediately. It prints the ID after confirmed exit. Stopping an exited
workload succeeds using its retained result. The original workload exit status
remains available through `pichi wait ID`.
Missing results remain unknown and retain image pins. `rmi` may remove a tag
without deleting images still needed by such workloads.

Build uses the same monitor launch and retains its output outside the cache.
Its named guest API pipes pass directly through to dillo: the guest requests
input before the host responds, and successful guest exit precedes OCI import.
Output is removed only after import and confirmed complete workload cleanup.
An active build retains the image cache because the guest selects its inputs.
Native caller qualification passes on Linux and macOS, and on Windows when
launched outside the runner/Cargo Jobs that forbid breakaway. Full native VM build
qualification remains pending.

Windows extra-handle inheritance, caller-managed process containment,
and full native signal/resize qualification are not complete yet.
Windows launch requires permission to break
away from the caller's Job Object; the current hosted runner denies that
operation. Pichi does not fall back to a monitor whose lifetime is tied to
that Job. Current external
termination signals end the foreground attachment; they are not proxied to
dillo. Workload state lives beside the canonical image cache, in a directory
whose name appends `.workloads` (for example `storage.workloads`). This keeps
console data outside the build guest's cache mount. No full conformance pass
is claimed.
