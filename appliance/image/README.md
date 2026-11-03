# Pichi build appliance

Pichi owns this Fedora 43 image, its guest adapter and its BPF LSM policy.
The host library and the packaged Conglobate engine come from the same
`deps/conglobate` submodule revision. Conglobate owns plan execution and
annotated OCI generation; [BUILD.md](../../BUILD.md) defines the host contract.

From a recursive source checkout, build the host CLI and put it on PATH,
then run `appliance/image/build.sh`. Prerequisites are mkosi, Arma, Cargo,
Rustup, bpf-linker, cpio, e2fsprogs and Python. `ARMA` selects the Arma
executable. The script builds the pinned engine, `appliance/guest` adapter,
`appliance/lsm` loader, and static Corium before assembling the image.
The existing toolchain and Corium overrides support local qualification.

The script imports `pichi-build:<pichi-version>` into the local cache.
Publication is a separate CI step on main, to `ghcr.io/pichi-vm/pichi-build`.
PRs produce OCI layouts for qualification. A new default in `pichi build`
requires a published digest; the previous immutable default remains usable.

## Guest environment

- Corium pivots into the partitioned root; systemd is PID 1.
- Static virtiofs units mount read-only `repo` and `cache` and writable
  `output` at `/run/pichi/{repo,cache,output}`.
- `pichi-build.service` loads the LSM policy, then starts
  `/usr/libexec/pichi-build`. The adapter exchanges the versioned build
  command on `dev.pichi.build` and executes `/usr/bin/conglobate build`.
- UID/GID mappings are fixed at `100000:65536`. Commands with networking
  enabled inherit the guest network and resolver. Offline commands receive
  a disconnected namespace, enforced by Conglobate.
- The environment supplies the fixed BPF LSM exception for `security.selinux`.
  The program and loader live in `../lsm`; the attachment lasts until VM
  disposal. SELinux is disabled in the build guest. Startup fails if the
  required kernel hooks are unavailable.
- Image-resident ext4 and FAT tools perform formatting. The rootfs retains
  virtiofs, FUSE, loop, FAT and network modules needed after pivot.
- The service powers off on success or failure. The host imports only a
  complete, verified OCI layout from the output mount.

## Boot and output

The complete GPT disk becomes the root Carapace. Arma produces an immutable
PMI with embedded DTB; native bootconfig carries the selected chain root
and kernel settings. The engine emits `oci-layout`, `index.json` and all
referenced blobs, including inherited content and build identity annotations.
The host uses the ordinary OCI importer; it does not rebuild the image graph.

`appliance/image/.build/` and `appliance/image/mkosi.extra/` are temporary build outputs.
To qualify a rebuilt OCI layout with the existing VM tests, set
`PICHI_TEST_BUILD_IMAGE_LAYOUT` to its absolute path.
