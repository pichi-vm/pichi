# VM lifecycle example

Build a Fedora-based workload, inspect its provenance, and run it to completion.
The workload prints two lines and asks systemd to power off. It needs no guest
login or network service. The lock file pins the Fedora input; the example does
not modify its inherited kernel or boot payload.

Use the Pichi, Dillo, and Madrillo executables described in the
[project README](../../README.md). On Linux, the builder needs KVM access
and 4 GiB of available hugepage-backed memory. The workload recommends four
vCPUs, 1 GiB RAM, and 64 MiB writable storage.

From the repository root, with the example committed:

```sh
pichi build . --context examples/lifecycle --ref HEAD -t lifecycle:example \
  --memory 4096 --cpus 2 \
  --build-image ghcr.io/pichi-vm/pichi-build@sha256:92f564699cda5c301b6f9e487af5083aafd8200c54f62b83d0813073299aa53d
pichi inspect lifecycle:example
pichi run lifecycle:example
```

A cold cache may need network access to fetch the pinned inputs. The build reads
the selected Git commit, so commit any example edits before rebuilding.

The workload output is:

```text
Hello from Pichi.
Workload complete. Shutting down.
```

The example starts `lifecycle.service` directly, retaining its normal systemd
initialization and shutdown dependencies. The service waits for
`dracut-shutdown.service` so immediate poweroff cannot race preparation of the
shutdown restoration hook. Console type and dimensions are explicit to avoid
terminal queries; kernel and systemd warnings and errors remain visible.

## Disposable storage on Linux

To run this example with volatile storage in `/dev/shm` instead of normal
writable storage, use this explicit invocation. The contents are lost
on reboot; this is suitable only for the disposable example. The command
replaces any configured `run.storage_file` for this run.

```sh
storage_dir=$(mktemp -d /dev/shm/pichi-lifecycle-XXXXXXXX) &&
  printf 'Storage retained at %s\n' "$storage_dir" &&
  pichi run lifecycle:example --storage 32 --storage-file "$storage_dir/storage.raw"
```

The directory remains after the command returns, including after an error or
interruption. A zero exit status alone does not confirm workload cleanup.
Remove the directory manually only after confirming the workload has stopped
and released its storage. Each invocation allocates another private directory;
retained files consume shared-memory capacity until removed or the host reboots.
