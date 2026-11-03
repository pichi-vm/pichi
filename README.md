# pichi

The docker/podman-style front-end for the [pichi-vm](https://github.com/pichi-vm)
ecosystem: it manages VM images as OCI artifacts and boots them.

`pichi` owns image management — importing, pulling, pushing, and inspecting
artifacts in a local content-addressed cache — and delegates the actual VM
launch to the [`dillo`](https://github.com/pichi-vm/dillo) VMM. `pichi run`
derives the guest's device set from the cached artifact, then `exec()`s dillo.

## The artifact model

A pichi artifact is an OCI image composed of:

- **scute layers** — the read-only block-device layers of a
  [carapace](https://github.com/pichi-vm/carapace) (the guest root filesystem),
- an optional **PMI** layer — the measured boot payload (kernel + initramfs)
  produced by [`arma`](https://github.com/pichi-vm/arma), described by the
  [`pmi`](https://github.com/pichi-vm/pmi) wire-format spec,
- an optional detached **base DTB** layer, and
- a **config** blob — the launch contract (CPU / memory / interface
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
| `pichi run` | Boot a cached artifact (prepares the environment, execs dillo) |
| `pichi update` | Update a working-tree plan’s image lock |
| `pichi build` | Build via a VM appliance (thin launcher; see [BUILD.md](BUILD.md)) |
| `pichi system` | System-level inspection and maintenance |

Run `pichi <command> --help` for details.

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

The `conglobate` library supplies shared plan, artifact, lock, and build-port
types. `pichi update` maintains locks on the host; `pichi build` remains a
thin launcher and consumes a committed plan and lock in the appliance.

`-b`/`--build-image` selects the build appliance image. When it is omitted,
pichi uses the digest-pinned default. If that image is not already cached,
the first build pulls it and continues. Full contract: [BUILD.md](BUILD.md).

## License

Apache-2.0. See [LICENSE](LICENSE).
