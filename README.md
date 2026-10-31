# pichi

The docker/podman-style front-end for the [pichi-vm](https://github.com/pichi-vm)
ecosystem: it manages VM images as OCI artifacts and boots them.

`pichi` owns image management — importing, pulling, pushing, and inspecting
artifacts in a local content-addressed cache — and delegates the actual VM
launch to the [`dillo`](https://github.com/pichi-vm/dillo) VMM. `pichi run`
derives the guest's device set from the cached artifact, then `exec()`s dillo.

## Dillo location

`pichi run` and `pichi build` look for the dillo VMM in this order:

1. `PICHI_DILLO`, when set, is used verbatim. A failed override does not fall back.
2. The packaged default: `/usr/libexec/pichi/dillo` on Fedora and Ubuntu.
3. `dillo` beside the running pichi executable (`dillo.exe` on Windows).
4. `dillo` on `PATH` (`dillo.exe` on Windows), using the native process search.

The `dillo-vm` package provides the VMM separately from Fedora's unrelated
`dillo` web browser. Pichi is the VMM's only user, so its packaged binary lives
under libexec. Developers can keep using a sibling binary or `PATH`, or select
one explicitly with `PICHI_DILLO=/path/to/dillo`. The packaged path has one
source constant, `Dillo::LIBEXEC_PATH` in `src/cmd/dillo/mod.rs`. The `dillo-vm`
package owns the confirmed default path on both Fedora and Ubuntu.

Packagers can set another default at compile time:

```sh
PICHI_DILLO_DEFAULT=/custom/libexec/pichi/dillo cargo build --release
```

`PICHI_DILLO_DEFAULT` is read at compile time with `option_env!`; setting it at
runtime has no effect. `PICHI_DILLO` still takes priority at runtime, followed
by the compiled default, the executable sibling, and `PATH`.

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
| `pichi update` | Resolve committed plan inputs into a build lock |
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

Create a lock with the matching Conglobate CLI installed:

```sh
pichi update . --ref HEAD --conglobate /path/to/conglobate
```

Conglobate reads the committed plan and reports its selected remote inputs.
Pichi resolves those references from the local cache into `pichi.lock`; pull
missing inputs first. Bare imported manifests receive a cached index wrapper
without changing their tags. Existing index inputs retain their exact digest.
Commit the generated lock before selecting that commit for `pichi build`.
Working-tree plan edits do not affect inspection. `--context`, `--plan`, and
`--lock` select alternative paths; the legacy `pichi.build` layout is retired.

The `conglobate` library supplies shared artifact, lock, and build-port types.
Its default feature set has no builder runtime dependencies. Build the matching
executable with `cargo build --features cli --bin conglobate` in that repository.

`-b`/`--build-image` selects the build appliance image. When it is omitted,
pichi uses the digest-pinned default. If that image is not already cached,
the first build pulls it and continues. Full contract: [BUILD.md](BUILD.md).

## License

Apache-2.0. See [LICENSE](LICENSE).
