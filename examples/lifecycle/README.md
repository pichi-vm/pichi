# A VM lifecycle in four commands

Build a small workload on Fedora, inspect its layers and provenance, then boot
it. The workload prints two lines and asks systemd to power off immediately.
No account, browser connection, network service, or interactive guest login is
needed. The guest uses 2 CPUs and 1 GiB RAM by default.

## Prepare

Use Pichi with the readable inspection/build output changes and Dillo with the
console-diagnostic fixes. Linux needs KVM access and sufficient reserved
hugepages for the 4 GiB builder and the 1 GiB workload. Keep `PICHI_DILLO` pointed
at the matching Dillo executable if it is not beside Pichi or on `PATH`.

Create a standalone Git repository from this directory, outside the Pichi
checkout, so the build can access its complete Git object store:

```sh
cp -a examples/lifecycle /path/to/lifecycle-demo
cd /path/to/lifecycle-demo
git init
git add .
git commit -m "Add the lifecycle workload"
pichi pull ghcr.io/pichi-vm/fedora:43@sha256:6fcb43a770f32a6fdfcac210d139aad1c768e0631c937f35602a23d655700747
```

The committed lock pins the rehearsed Fedora image. To deliberately refresh
it, run `pichi update .` and commit the new lock. Prepare and rehearse once
before presenting so the Fedora input and pinned build appliance are cached.
The workload build itself needs no network; a cold cache can require downloads.

## Present

From the committed demo project:

```sh
pichi build . --ref HEAD -t lifecycle:demo --memory 4096 --cpus 2
pichi inspect lifecycle:demo --format layers
pichi inspect lifecycle:demo --format annotations
pichi run lifecycle:demo
```

Or use `bash demo.sh` to advance through those commands one at a time.
`bash demo.sh --auto` runs the same commands without prompts and stops on any
failure. Its completion message appears only after the real VM process exits
successfully.

The story is visible in the artifact: the first filesystem layer is reused
from Fedora, the second contains the workload, and the boot payload is
inherited. Build annotations identify the exact source commit, context, plan,
and lock. `pichi inspect lifecycle:demo` retains the full JSON representation;
the layer table explicitly abbreviates digests for readability.

The image keeps boot diagnostics on the guest's virtual console (`tty0`) and
routes the workload to the host-visible virtio console (`hvc0`). The application
console uses a fixed terminal type to avoid capability queries. This is image
configuration; the presenter does not filter or rewrite VM output.

## Improve and rehearse

Run the loop from the demo project with the intended Pichi and Dillo binaries:

```sh
python3 rehearse.py . --output /path/to/rehearsal-evidence --iterations 2
```

Each pass builds a real image, verifies Fedora layer reuse and source
provenance, checks readable layer/annotation output, and boots a real VM. The
run must exit within 30 seconds with exactly the expected three output lines.
Build output has an eight-line budget; the layer and annotation views have
twelve- and thirty-line budgets. Captured output must contain no terminal
control sequences or VMM info/warning chatter. On Unix, injected launcher
failures check that both ordinary and verbose build modes preserve diagnostics
and exit unsuccessfully.

The loop saves raw command output, exit codes, durations, and line counts.
Review a failure, fix the responsible component, commit workload changes, and
repeat until consecutive full passes succeed. Use a real terminal as well to
check spacing, color, and terminal restoration. Color accents are disabled for
redirected output, `TERM=dumb`, and `NO_COLOR`.

For build troubleshooting, use `pichi build ... --verbose`, or open the log
path reported on failure. For VMM diagnostics, use `RUST_LOG=debug pichi run ...`.
To see guest boot diagnostics, change `kernel.console` to `hvc0`, commit, and
rebuild the image. The demo's quiet boot-console configuration does not resolve
the upstream Fedora boot/shutdown warnings tracked in
[Fedora #36](https://github.com/pichi-vm/fedora/issues/36).
