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
| `pichi inspect` | Inspect a cached manifest |
| `pichi tag` / `rmi` | Manage tags (refcount-aware blob GC) |
| `pichi pull` / `push` | Move artifacts to and from an OCI registry |
| `pichi run` | Boot a cached artifact (prepares the environment, execs dillo) |
| `pichi build` | Build via a VM appliance (thin launcher; see [BUILD.md](BUILD.md)) |
| `pichi system` | System-level inspection and maintenance |

Run `pichi <command> --help` for details.

## Publishing a carapace

Import a raw root filesystem and capture its computed top root hash:

```sh
hash="$(pichi import raw root.raw -t myapp:base --quiet --print-root)"
```

The output is exactly 64 lowercase hexadecimal characters followed by a
newline. This is the top dm-verity root: dm-verity checks disk blocks against
a tree of hashes. Put `corium=$hash` on the boot command line when building
the PMI, then bind it to the same carapace:

```sh
pichi import pmi boot.pmi --dtb base.dtb --carapace myapp:base -t myapp:1
```

For an already imported, pulled, or loaded carapace, compute it from the
cached layer data with `pichi inspect myapp:base --print-root`. This fails
for artifacts without a carapace, including PMI-only images. The root is
never saved as an annotation or separate metadata file.

Without `--print-root`, `import raw` prints the manifest digest, including
with `--quiet` or without a tag. Progress messages go to stderr.

## Build

`pichi build <dir> --ref <ref> -t <tag>` is a specialization of `pichi run`:
same launch path, plus virtiofs wires and a bidirectional build-port CBOR
exchange on named console `dev.pichi.build` (guest requests build-input; host
replies). The host is a thin launcher; Conglobate's pichi image performs the
build and writes an OCI layout to the output mount.

Recipe and lock paths are relative to `--subdir` (default `.`). The defaults
are `pichi.yaml` and `pichi.lock`. `-f`/`--file` and `-l`/`--lock` override
them. The build-port response carries those resolved relative paths as
`recipe` and `lock`.

`-b`/`--build-image` selects the build appliance image. When it is omitted,
pichi uses the digest-pinned default. If that image is not already cached,
the first build pulls it and continues. Full contract: [BUILD.md](BUILD.md).

## License

Apache-2.0. See [LICENSE](LICENSE).
