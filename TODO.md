# dillo crate-split implementation plan

This plan implements `DILLO-CRATE-SPLIT.md` to completion. Each stage is meant
to be independently verifiable, committed, pushed, and checked by CI before the
next stage starts.

## Rules for every stage

1. Change only `dillo/` and dillo-owned dependencies unless the stage explicitly
   says otherwise.
2. Do not change `arma`, `tatu`, or PMI behavior unless the stage explicitly
   calls for an arma device-model discussion.
3. Derive all guest-visible hardware from the merged DTB. Every consumed node
   and property must have one owner; residual DTB facts are launch errors.
4. Do one stage at a time.
5. Run local verification before committing.
6. Commit only the stage change.
7. Push the commit so CI can verify the supported lanes.
8. Update this file when a stage is complete, including the commands that were
   run and the pushed commit.

Default local verification:

```sh
RUSTC_BOOTSTRAP=1 cargo fmt --all -- --check
git diff --check
RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo test --workspace --exclude vhost-backend --exclude snuffler
```

When a stage changes target-specific code, also run the relevant target checks:

```sh
RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target x86_64-unknown-linux-gnu
RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target x86_64-pc-windows-msvc
RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target aarch64-apple-darwin
```

After each push:

```sh
gh run list --branch main --limit 12
```

## Stage 0 - Temporarily quarantine macOS CI

Status: complete.

Goal: temporarily disable the self-hosted macOS/HVF CI lane while the runner is
offline, without weakening the target design or local macOS expectations.

Process:
- Remove the `macos-arm64` matrix entry from `.github/workflows/ci.yml`.
- Leave macOS-specific workflow steps intact so restoring the matrix entry is a
  small diff.
- Update workflow comments to say macOS is temporarily disabled, not unsupported.
- Keep local `aarch64-apple-darwin` checks in every stage that touches backend
  or target-specific code.

Success criteria:
- CI schedules Linux and Windows only.
- The workflow still documents the macOS/HVF lane as required once the
  self-hosted runner returns.
- `TODO.md` includes a later restore stage.
- Default local verification passes.

Completed changes:
- Removed the `macos-arm64` matrix entry from `.github/workflows/ci.yml`.
- Left macOS-only workflow steps in place behind `runner.os == 'macOS'` so the
  restore diff is small.
- Updated workflow comments and boot-test labeling to make the temporary
  Linux/Windows-only schedule explicit.
- Added Stage 15 to restore the macOS/HVF CI lane before final acceptance.

Local verification:
- `RUSTC_BOOTSTRAP=1 cargo fmt --all -- --check`
- `git diff --check`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo test --workspace --exclude vhost-backend --exclude snuffler`

Pushed commit:
- `498a2e5 docs: plan final dillo crate split`

## Stage 1 - Reconcile design docs and plan

Status: pending.

Goal: make `TODO.md`, `DILLO-CRATE-SPLIT.md`, and CI policy agree on the target
architecture and execution flow before code moves resume.

Process:
- Ensure `TODO.md` references the final crate/API design, not the old
  cfg-variable `BackendVm` shape.
- Keep `DILLO-CRATE-SPLIT.md` as the source of truth for trait contracts.
- Record any unresolved design decisions as explicit stage work, not hidden
  assumptions.

Success criteria:
- No plan stage requires a cfg-variable public trait.
- No plan stage adds a universal CPU-state type; CPU/memory inputs remain
  `Machine` associated types.
- The plan preserves native Windows MSVC support.
- Default local verification passes.

## Stage 2 - Extract `dillo-mmio`

Status: pending.

Goal: create the first stable trait crate: MMIO windows, MMIO device
requirements, interrupts, shared-memory capability traits, and `Attach<T>`.

Process:
- Create `dillo-mmio` or rename the current local MMIO module into that boundary.
- Move `MmioWindow`, `MmioDevice`, `Attach<T>`, interrupt requirement types,
  notify registration types, `SharedMemory`, `SharedRegion`, and
  `MmioAttachment` into the crate.
- Keep compatibility adapters in `dillo-vm` while call sites migrate.
- Preserve borrowed requirement access: windows, interrupts, and shared-memory
  requirements are slices or borrowed views, not owned allocations.

Success criteria:
- Device crates can depend on `dillo-mmio` without depending on backend crates.
- `MmioDevice` has no attach/init callback.
- Existing MMIO bus tests still pass.
- Default local verification and all three target checks pass.

## Stage 3 - Extract `dillo-pci`

Status: pending.

Goal: move PCI root and endpoint abstractions behind `dillo-pci`.

Process:
- Move `PciRoot` into `dillo-pci` as the concrete MMIO device for one DTB PCI
  host bridge.
- Move `PciDevice` into `dillo-pci`.
- Keep ECAM and BAR windows managed by the single `PciRoot` object.
- Keep PCI helper code small and rust-vmm-upstreamable where possible.
- Do not let machine backends depend on `dillo-pci`.

Success criteria:
- `PciRoot` implements `dillo-mmio::MmioDevice`.
- PCI endpoints attach to `PciRoot`, never directly to `Machine`.
- x86 CF8/CFC paths still decode onto the same `PciRoot` config accessor.
- ECAM, BAR, and absent-BDF tests pass.
- Default local verification and x86 Linux/Windows target checks pass.

## Stage 4 - Extract virtio traits and transports

Status: pending.

Goal: separate transport-neutral virtio devices from MMIO and PCI transports.

Process:
- Keep or rename the transport-neutral `virtio` crate as `dillo-virtio`.
- Move MMIO virtio transport into `dillo-mmio-virtio`.
- Move PCI virtio transport into `dillo-pci-virtio`.
- Make `VirtioActivate` carry queues, kicks, resolved interrupts, and
  attachment-scoped shared-memory capabilities.
- Remove whole-guest-memory handles from the target activation API.

Success criteria:
- Transport crates depend on `dillo-virtio` plus their transport crate only.
- Concrete virtio device crates do not depend on machine crates, PMI, or DTB.
- Existing virtio queue and transport tests pass.
- Default local verification and all target checks pass.

## Stage 5 - Move concrete devices behind final crate boundaries

Status: pending.

Goal: move concrete device implementations into their final leaf crates.

Process:
- Move UART into `dillo-mmio-uart`.
- Move virtio console, block, net, and vsock into `dillo-virtio-*` crates as
  applicable.
- Give each concrete device inherent constructors that take typed constructor
  inputs, not DTB nodes.
- Keep DTB consumption in `dillo`.

Success criteria:
- Device crates do not import `devtree`, PMI, KVM, HVF, WHP, or `dillo-machine`.
- `dillo` owns all `FromDevTree` implementations for concrete devices.
- UART and virtio tests pass after the move.
- Default local verification and all target checks pass.

## Stage 6 - Create `dillo-machine`

Status: pending.

Goal: introduce the final host-neutral machine trait crate.

Process:
- Create `dillo-machine` with `Machine`, `Vcpu`, `VcpuStop`, and target-neutral
  lifecycle types.
- Use associated types for `Error`, `Vcpu`, `Cpu`, and `Memory`.
- Define `Machine::request_vcpu_exit(&self) -> Result<(), Self::Error>`.
- Do not expose PCI, UART, MSI-X, IOAPIC, GIC, KVM, HVF, WHP, raw guest memory,
  or backend handles.
- Keep fatal vCPU failures in `Err(Self::Error)`, not `VcpuStop`.

Success criteria:
- There is one public `Machine` trait shape.
- `Machine` has no trait constructor and no `launch()`.
- `Machine` has no universal CPU-state type.
- Existing backend code can implement the trait through adapters.
- Default local verification and all target checks pass.

## Stage 7 - Split backend crates

Status: pending.

Goal: split backend implementations into `dillo-machine-kvm`,
`dillo-machine-whp`, and `dillo-machine-hvf`.

Process:
- Move KVM-specific code into `dillo-machine-kvm`.
- Move WHP-specific code into `dillo-machine-whp`.
- Move HVF-specific code into `dillo-machine-hvf`.
- Bind the selected backend in `dillo` through Cargo target dependencies and a
  generic dependency alias.
- Preserve native `x86_64-pc-windows-msvc` support.

Success criteria:
- `dillo` source imports no KVM, WHP, HVF, or raw backend crates except the
  generic backend alias.
- Backend crates do not depend on PCI, virtio transports, UART, or concrete
  devices.
- Linux/KVM, Windows/WHP, and local macOS/HVF builds pass.
- Default local verification and all target checks pass.

## Stage 8 - Move MMIO routing below `Machine`

Status: pending.

Goal: make `Vcpu::run()` callback-free and route MMIO through machine-owned
state populated by `Attach<Arc<dyn MmioDevice>>`.

Process:
- Move MMIO bus ownership into each backend machine implementation.
- Make machine attachment register MMIO windows, validate overlaps, realize
  interrupts, and return `MmioAttachment`.
- Route unresolved MMIO exits inside backend `Vcpu::run()`.
- Keep KVM ioeventfd as an optional notify acceleration path through
  `MmioAttachment::register_notify`.

Success criteria:
- `Vcpu::run()` has no MMIO or PIO callback parameters.
- Devices are reachable only after successful machine attachment.
- MMIO routing remains range-checked and overlap-checked.
- Existing MMIO, UART, PCI, virtio-mmio, and virtio-pci behavior is preserved.
- Default local verification and all target checks pass.

## Stage 9 - Move non-MMIO exits below `Machine`

Status: pending.

Goal: hide PIO, HVC, SMC, CPUID leaves, WFI/HLT, debug exits, and
backend-specific exits below `dillo-machine`.

Process:
- Move PSCI handling into backend or architecture substrate code.
- Preserve PSCI `CPU_ON` secondary bring-up with backend-owned vCPU
  parking/wakeup state.
- Keep x86 CF8/CFC PIO as backend/substrate decoding onto `PciRoot`.
- Keep WFI/HLT backend-internal.
- Decide and implement the target gdb/debug story: either an explicit
  debug-capable runner or removal from the final design.

Success criteria:
- `dillo` and device crates do not see backend exit enums.
- `VcpuStop` contains only guest/supervisor lifecycle outcomes.
- PSCI poweroff/reset and CPU_ON tests pass.
- Existing debug behavior is either preserved through an explicit API or
  intentionally removed with tests/docs updated.
- Default local verification and all target checks pass.

## Stage 10 - Implement vCPU stop control

Status: pending.

Goal: make guest poweroff reliably stop all vCPU worker threads on KVM, WHP, and
HVF.

Process:
- Implement `Machine::request_vcpu_exit` for KVM using backend-owned per-vCPU
  run records, `immediate_exit`, and thread-directed signal wakeups.
- Implement WHP cancellation using partition handle plus virtual processor
  indexes.
- Implement HVF cancellation using recorded vCPU handles and the existing
  `force_vcpus_exit` shape.
- Keep shutdown policy in the supervisor: stop vCPUs, join vCPUs, then stop
  device hosts.

Success criteria:
- A poweroff from one vCPU causes every running `Vcpu::run()` to return.
- Device hosts are not shut down until all vCPU workers are joined.
- KVM, WHP, and local HVF poweroff tests pass.
- Default local verification and all target checks pass.

## Stage 11 - Implement process/thread device host attachment

Status: pending.

Goal: make `MmioAttachment::spawn` the single backend-owned launch/connect point
for long-lived device hosts.

Process:
- Define the minimal `MmioDeviceHost` thread closure and process spec.
- Return `MmioDeviceHandle` with `shutdown` and `join`.
- Ensure dropping the handle does not silently detach guest-visible state.
- Migrate in-tree thread devices to the new launch path.
- Keep process-host support narrow and only as needed by existing vhost-user
  behavior.

Success criteria:
- Backend crates own the parallel model.
- Device wrappers do not know KVM, WHP, or HVF.
- VM shutdown stops device hosts after vCPU quiescence.
- Default local verification and all target checks pass.

## Stage 12 - Implement CC-first shared-memory capabilities

Status: pending.

Goal: replace whole-guest-memory exposure with attachment-scoped shared-memory
capabilities.

Process:
- Add backend-owned shared/private page tracking.
- Implement `SharedMemory::region()` as the dynamic runtime claim API used by
  virtio queues and device DMA.
- Ensure region claims succeed only inside the DTB-derived aperture and only for
  pages the backend currently tracks as shared.
- Route guest shared/private conversion exits or hypercalls inside the backend.
- On KVM, add guest-private memory support with the appropriate memory APIs.
- For standard VMs, implement the same API over ordinary mapped memory without
  exposing a whole-guest-memory accessor.

Success criteria:
- `VirtioActivate` no longer gives devices `GuestMemoryMmap`.
- Virtio descriptor, avail, used, and buffer access goes through
  `SharedMemory::region()`.
- A descriptor pointing outside the device aperture fails.
- A descriptor pointing to private memory fails in CC mode.
- Default local verification and Linux target checks pass.

## Stage 13 - Resolve restricted DMA aperture in DTB/device model

Status: pending.

Goal: ensure every shared-memory aperture used by dillo is derived from DTB
data.

Process:
- Audit whether current arma DTBs can describe virtio DMA/bounce-buffer
  apertures well enough for Stage 12.
- If not, write the arma device-model extension proposal before changing arma.
- Only after agreement, add the minimum DTB binding support needed.
- Update `FromDevTree` consumers to drain the new properties/nodes.

Success criteria:
- Dillo does not guess a DMA aperture.
- Every shared-memory capability has DTB provenance.
- All new DTB nodes/properties are consumed exactly once.
- Existing DTB-drain tests cover the new binding.
- Default local verification passes.

## Stage 14 - Remove compatibility adapters

Status: pending.

Goal: delete bridge code that allowed old and new APIs to coexist.

Process:
- Remove old monolithic `dillo-vm` APIs once crates have split.
- Remove closure MMIO dispatch paths.
- Remove whole-guest-memory virtio activation paths.
- Remove backend handle accessors above backend crates.
- Remove temporary module re-exports that hide the final crate graph.

Success criteria:
- The implemented crate graph matches `DILLO-CRATE-SPLIT.md`.
- Source search finds no KVM/HVF/WHP imports in `dillo` or device crates.
- Source search finds no whole-guest-memory device activation.
- Default local verification and all target checks pass.

## Stage 15 - Restore macOS CI

Status: pending.

Goal: restore required macOS/HVF CI once the self-hosted runner is online.

Process:
- Re-add the `macos-arm64` matrix entry using
  `["self-hosted","macOS","ARM64","bare-metal","m1"]`.
- Run local macOS verification on this workstation first.
- Push and confirm the macOS CI lane runs real HVF boot tests.

Success criteria:
- CI again schedules Linux/KVM, Windows/WHP, and macOS/HVF lanes.
- macOS workspace tests and signed HVF boot tests pass in CI.
- The temporary quarantine comments are removed.
- Default local verification passes.

## Stage 16 - Conformance loop 1

Status: pending.

Goal: perform the first full conformance pass over implementation,
`DILLO-CRATE-SPLIT.md`, and this plan.

Process:
- Read `DILLO-CRATE-SPLIT.md` from top to bottom.
- Read this `TODO.md` from top to bottom.
- Inspect the current crate graph, public traits, dependency edges, DTB
  consumption paths, backend APIs, device APIs, shared-memory APIs, and CI
  workflow.
- Patch the implementation or docs for any direct mismatch that can be resolved
  mechanically.
- Commit and push the fixes from this loop.

Success criteria:
- Every resolved mismatch has code or doc evidence.
- Any unresolved mismatch is written down for the next loop.
- Default local verification and all restored target checks pass.

## Stage 17 - Conformance loop 2

Status: pending.

Goal: repeat the full conformance pass after the first loop's fixes have landed
and CI has had a chance to run.

Process:
- Re-read `DILLO-CRATE-SPLIT.md`, this `TODO.md`, and the implementation.
- Treat the first loop's unresolved list as input, but do not trust it blindly.
- Patch additional mismatches found by the second pass.
- Commit and push the fixes from this loop.

Success criteria:
- No first-loop unresolved mismatch remains without either a fix or a concrete
  explanation.
- New mismatches found in the second loop are fixed or carried forward
  explicitly.
- Default local verification and all restored target checks pass.

## Stage 18 - Conformance loop 3

Status: pending.

Goal: perform the final automated conformance pass before human review.

Process:
- Re-read `DILLO-CRATE-SPLIT.md`, this `TODO.md`, and the implementation.
- Verify the dependency graph, public API shapes, DTB drain invariants,
  backend isolation, device isolation, shared-memory model, and CI coverage.
- Patch any remaining mismatch that can be fixed without changing the agreed
  target design.
- Commit and push the fixes from this loop.

Success criteria:
- The third loop finds no unrecorded implementation/spec/plan mismatch.
- All fixable mismatches have been fixed.
- Default local verification and all restored target checks pass.

## Stage 19 - Record remaining divergence for human review

Status: pending.

Goal: write down any remaining divergence after the third conformance loop.

Process:
- Create or update a human-review section in this file.
- For each remaining divergence, record:
  - the exact spec or plan requirement;
  - the current implementation behavior;
  - why the agent did not fix it;
  - the human decision needed;
  - the verification evidence collected.
- Do not hide unresolved target-design changes behind "complete" language.

Success criteria:
- Every known remaining divergence is explicitly listed.
- The list distinguishes implementation gaps, spec ambiguities, and intentional
  deferrals.
- Human reviewers can decide each item without reconstructing the audit.
- Default local verification passes.

## Stage 20 - Final acceptance audit

Status: pending.

Goal: prove the implementation satisfies `DILLO-CRATE-SPLIT.md` or that all
remaining divergence has been recorded for human review after three conformance
loops.

Process:
- Confirm Stages 16, 17, and 18 were completed in order.
- Confirm Stage 19 contains any remaining divergence, or explicitly says none
  remains.
- Audit the final crate graph against the target graph one last time.
- Audit source imports for forbidden backend, device, PMI, and DTB dependencies.
- Audit DTB consumption tests for all supported devices and transports.
- Audit shared-memory tests for aperture and private/shared behavior.
- Run Linux, Windows, and macOS local/CI verification.

Success criteria:
- `dillo` is the only composition point that knows PMI, devtree, concrete
  devices, transports, and the selected machine backend.
- Backend crates only create machines, attach MMIO devices, run vCPUs, and
  provide backend-owned interrupt/memory/notify plumbing.
- Device crates know no machine backend and no DTB/PMI.
- All DTB nodes/properties are consumed or launch fails.
- Three conformance loops have completed, and any remaining divergence is
  recorded for human review.
- CI passes all supported platform lanes, including real boot tests.
