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
5. Before starting a stage, confirm the latest pushed commit has passing CI; if
   CI failed, fix that prior stage first.
6. Run local verification before committing.
7. Commit only the stage change.
8. Push the commit so CI can verify the supported lanes.
9. Wait for that pushed commit's CI to pass before starting the next stage.
10. Update this file when a stage is complete, including the commands that were
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
- `05d0ae9 docs: mark macos ci quarantine complete`
- `1de220c fix: wake vcpus during shutdown`

## Stage 1 - Reconcile design docs and plan

Status: complete.

Goal: make `TODO.md`, `DILLO-CRATE-SPLIT.md`, and CI policy agree on the target
architecture and execution flow before code moves resume.

Process:
- Ensure `TODO.md` references the final crate/API design, not the old
  cfg-variable `BackendVm` shape.
- Keep `DILLO-CRATE-SPLIT.md` as the source of truth for trait contracts.
- Record any unresolved design decisions as explicit stage work, not hidden
  assumptions.
- Confirm the workspace already uses the upstream PMI crate through a git
  dependency; later stages may move or remove dillo-local PMI parsing, but must
  not replace the PMI spec crate with a local fork.

Success criteria:
- No plan stage requires a cfg-variable public trait.
- No plan stage adds a universal CPU-state type; CPU/memory inputs remain
  `Machine` associated types.
- The plan preserves native Windows MSVC support.
- The plan records the CI-before-next-stage gate.
- `DILLO-CRATE-SPLIT.md` records that `BackendVm` is current-state evidence,
  not the target API.
- The root workspace keeps `pmi = { git = "https://github.com/pichi-vm/pmi" }`.
- Default local verification passes.

Completed changes:
- Added the CI-before-next-stage gate to the plan-wide rules.
- Recorded the upstream PMI git dependency invariant in the plan and design.
- Updated Stage 0's record with the follow-up CI-fix commits that made the
  quarantine stage green.

Local verification:
- `RUSTC_BOOTSTRAP=1 cargo fmt --all -- --check`
- `git diff --check`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo test --workspace --exclude vhost-backend --exclude snuffler`

CI verification:
- `27135940650` passed on `cargo fmt`, `ubuntu-24.04`, and `windows-2025`.

Pushed commit:
- `714ab6e docs: reconcile plan with design target`

## Stage 2 - Extract `dillo-mmio`

Status: complete.

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

Completed changes:
- Added `dillo/deps/dillo-mmio` as a workspace crate.
- Moved `MmioWindow`, `MmioDevice`, `MmioBus`, `Attach<T>`, interrupt
  requirement types, shared-memory requirement/capability types, and basic
  interrupt delivery traits into `dillo-mmio`.
- Switched `MmioDevice::windows()` to borrowed slice access.
- Updated `dillo-vm` to depend on `dillo-mmio` and removed its private
  `mmio_bus` module.
- Updated `PciRoot` to cache ECAM plus BAR windows so the single root object
  still owns all PCI MMIO addresses while exposing borrowed windows.

Local verification:
- `RUSTC_BOOTSTRAP=1 cargo fmt --all -- --check`
- `git diff --check`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target x86_64-unknown-linux-gnu`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target x86_64-pc-windows-msvc`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target aarch64-apple-darwin`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo test --workspace --exclude vhost-backend --exclude snuffler`

CI verification:
- `27136612124` passed on `cargo fmt`, `ubuntu-24.04`, and `windows-2025`.

Pushed commit:
- `fd33516 refactor: extract dillo mmio crate`

## Stage 3 - Extract `dillo-pci`

Status: complete.
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

Completed changes:
- Added `dillo/deps/dillo-pci` as a workspace crate.
- Moved `PciRoot`, `PciBus`, `PciDevice`, `BarRegion`, `HostBridge`, and PCI
  root tests into `dillo-pci`.
- Kept `VirtioPciAdapter` in `dillo-vm` as compatibility glue until
  `dillo-pci-virtio` exists.
- Updated `PciDevice` to use shared-reference config/BAR write methods, with
  mutable endpoint state behind locks.
- Updated x86 CF8/CFC PIO decoding to use `dillo_pci::PciRoot`.

Local verification:
- `RUSTC_BOOTSTRAP=1 cargo fmt --all -- --check`
- `git diff --check`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target x86_64-unknown-linux-gnu`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target x86_64-pc-windows-msvc`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target aarch64-apple-darwin`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo test --workspace --exclude vhost-backend --exclude snuffler`

CI verification:
- `27137010919` passed on `cargo fmt`, `ubuntu-24.04`, and `windows-2025`.

Pushed commit:
- `aa8a4c5 refactor: extract dillo pci crate`

## Stage 4 - Extract virtio traits and transports

Status: complete.

Goal: separate transport-neutral virtio devices from MMIO and PCI transports.

Process:
- Rename the transport-neutral `virtio` package to `dillo-virtio`.
- Move MMIO virtio transport into `dillo-mmio-virtio`.
- Move PCI virtio transport into `dillo-pci-virtio`.
- Make `VirtioActivate` carry queues and kicks through one transport-resolved
  activation value.
- Record the remaining activation divergence: existing queue walking still
  requires `GuestMemoryMmap`; Stage 12 must replace that compatibility field
  with attachment-scoped shared-memory capabilities.

Success criteria:
- Transport crates depend on `dillo-virtio` plus their transport crate only.
- Concrete virtio device crates do not depend on machine crates, PMI, or DTB.
- Existing virtio queue and transport tests pass.
- Default local verification and all target checks pass.

Completed changes:
- Renamed the transport-neutral package at `dillo/deps/virtio` to
  `dillo-virtio`.
- Added `dillo-mmio-virtio` and moved the virtio-mmio transport out of
  `dillo-vm`.
- Added `dillo-pci-virtio` and moved the virtio PCI transport plus
  `PciDevice` adapter out of `dillo-vm`.
- Updated `dillo-vm` to consume `dillo-mmio-virtio` and `dillo-pci-virtio`
  instead of local transport modules.
- Added `VirtioActivate` as the single activation handoff type.

Remaining divergence:
- `VirtioActivate` still carries `GuestMemoryMmap` because the current queue
  and vhost-user code require whole-guest-memory access. Stage 12 owns the
  replacement with attachment-scoped shared-memory capabilities.

Local verification:
- `RUSTC_BOOTSTRAP=1 cargo fmt --all -- --check`
- `git diff --check`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target x86_64-unknown-linux-gnu`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target x86_64-pc-windows-msvc`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target aarch64-apple-darwin`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo test --workspace --exclude vhost-backend --exclude snuffler`

CI verification:
- `27138094866` passed on `cargo fmt`, `ubuntu-24.04`, and `windows-2025`.

Pushed commit:
- `0463e2a refactor: extract dillo virtio transports`

## Stage 5 - Move concrete devices behind final crate boundaries

Status: complete.

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

Completed changes:
- Added `dillo-mmio-uart` as the backend-neutral ns16550a MMIO device crate.
- Moved UART register emulation and tests out of `dillo-vm`.
- Made `Ns16550<T>` generic over its interrupt trigger so backend-specific
  IRQ plumbing stays in `dillo-vm`.
- Kept the Windows/WHP IOAPIC trigger in `dillo-vm`; the UART crate imports no
  KVM, HVF, WHP, PMI, DTB, or machine backend code.
- Confirmed `dillo-virtio-console` was already the concrete virtio console
  device crate.

Local verification:
- `RUSTC_BOOTSTRAP=1 cargo fmt --all -- --check`
- `git diff --check`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target x86_64-unknown-linux-gnu`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target x86_64-pc-windows-msvc`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target aarch64-apple-darwin`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo test --workspace --exclude vhost-backend --exclude snuffler`

CI verification:
- `27138666519` failed on Linux because the Linux-only UART test harness still
  named `Ns16550State` without its trigger type parameter.
- `27138891882` passed on `cargo fmt`, `ubuntu-24.04`, and `windows-2025` after
  the follow-up fix.

Pushed commit:
- `af1750b refactor: extract dillo uart device`
- `5bc46d2 fix: compile uart tests on linux`

## Stage 6 - Create `dillo-machine`

Status: complete.

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

Completed changes:
- Added `dillo-machine` as the host-neutral machine trait crate.
- Defined `DeviceModel`, `Machine`, `Vcpu`, and `VcpuStop`.
- Kept constructors out of the traits; backend crates will expose inherent
  constructors on concrete machine/input types.
- Used `Machine` associated types for `Error`, `Vcpu`, `Cpu`, and `Memory`.
- Added a compile-time test adapter that assembles memory and CPU inputs through
  `dillo_mmio::Attach` using fully qualified associated-type bounds.

Local verification:
- `RUSTC_BOOTSTRAP=1 cargo fmt --all -- --check`
- `git diff --check`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo test -p dillo-machine`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target x86_64-unknown-linux-gnu`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target x86_64-pc-windows-msvc`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target aarch64-apple-darwin`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo test --workspace --exclude vhost-backend --exclude snuffler`

Pushed commit:
- `refactor: add dillo machine traits`; final pushed hash and CI run to be
  recorded with the next implementation commit.

CI verification:
- `27139795918` passed on `cargo fmt`, `ubuntu-24.04`, and `windows-2025`.

## Stage 7 - Split backend crates

Status: complete.

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

Completed changes:
- Added `dillo-machine-kvm`, `dillo-machine-hvf`, and `dillo-machine-whp`.
- Added `dillo-machine-backend` as the single target-selected backend facade
  consumed by `dillo-vm`.
- Replaced direct `dillo_hypervisor` imports in `dillo-vm` with
  `dillo_machine_backend`.
- Kept backend crates free of PCI, virtio transports, UART, concrete devices,
  PMI, and DTB dependencies.

Remaining divergence:
- The new backend crates are facade crates over the existing
  `dillo-hypervisor` wrapper. Stages 8-10 own moving MMIO routing, non-MMIO
  exits, and vCPU stop control below the machine boundary.

Local verification:
- `RUSTC_BOOTSTRAP=1 cargo fmt --all -- --check`
- `git diff --check`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target x86_64-unknown-linux-gnu`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target x86_64-pc-windows-msvc`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target aarch64-apple-darwin`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo test --workspace --exclude vhost-backend --exclude snuffler`

Pushed commit:
- `55326b9 refactor: split dillo machine backend crates`

CI verification:
- `27140379136` passed on `cargo fmt`, `ubuntu-24.04`, and `windows-2025`.

## Stage 8 - Move MMIO routing below `Machine`

Status: complete; CI pending for the implementation commit.

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

Completed changes:
- Added backend-resolved `MmioAttachment` and `MmioInterrupt` types to
  `dillo-mmio`.
- Made the KVM, WHP, and HVF machine facade crates own their MMIO buses.
- Implemented `Attach<Arc<D>>` for backend machine facades so MMIO devices are
  registered through the machine.
- Made the facade-level KVM, WHP, and HVF vCPU `run()` methods callback-free;
  backend vCPUs now route MMIO through machine-owned bus state internally.
- Returned to the supervisor after dispatched MMIO writes so existing shutdown
  checks still observe syscon poweroff until Stage 10 installs backend-owned
  vCPU stop control.
- Updated `dillo-vm` to attach UART, syscon, PCI root, and virtio-mmio devices
  to the machine instead of to supervisor-owned MMIO buses.
- Restored the Linux PCI root machine attachment so DTB-declared ECAM/BAR
  windows route before vCPUs run.
- Kept x86 PIO reads as constructor-time vCPU input until Stage 9 moves
  non-MMIO exits fully below the machine boundary.

Remaining divergence:
- `MmioAttachment` currently exposes empty interrupt/shared-memory slices. Later
  stages wire backend-resolved interrupts, notify registration, spawn handles,
  and shared-memory capabilities.
- PIO writes and other non-MMIO exits can still surface above the backend
  facade. Stage 9 owns that boundary cleanup.
- The lower `dillo-hypervisor` wrappers still use callback-style run methods
  internally. The public backend facade no longer exposes those callbacks.

Local verification:
- `RUSTC_BOOTSTRAP=1 cargo fmt --all -- --check`
- `git diff --check`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target x86_64-unknown-linux-gnu`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target x86_64-pc-windows-msvc`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target aarch64-apple-darwin`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo test --workspace --exclude vhost-backend --exclude snuffler`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo test -p dillo --features vm-tests -- --test-threads=1 --nocapture`
  was attempted locally but this machine lacks the required HVF entitlement, so
  platform boot validation is delegated to CI.

Pushed commit:
- `23f892f refactor: route mmio inside machine backends`
- `0982ec1 fix: return after backend mmio writes`
- `0b2abc9 fix: attach linux pci root to machine`

CI verification:
- `27142608685` passed on `cargo fmt`, `ubuntu-24.04`, and `windows-2025`.

## Stage 9 - Move non-MMIO exits below `Machine`

Status: complete.

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

Completed changes:
- Moved x86 PIO write handling behind the KVM/WHP backend facades by passing a
  constructor-time PIO write function alongside the existing PIO read function.
- Kept PCI CF8/CFC decoding in `dillo-vm` for now, but `Vcpu::run()` no longer
  returns those PIO writes to the supervisor loop on KVM/WHP.
- Added KVM/WHP `VcpuExit` facade enums so normal `dillo-vm` and Linux gdb
  callers no longer see raw PIO read/write exits from `dillo-hypervisor`.
- Moved x86 HLT and unexpected HVC/SMC handling inside the KVM/WHP facades
  instead of exposing those raw exits to `dillo-vm`.
- Split Linux gdb onto an explicit KVM `DebugExit` runner so normal KVM/WHP
  supervisor execution no longer sees debug exits.
- Mapped unknown KVM/WHP exits to backend errors instead of normal supervisor
  `VcpuExit` variants.
- Moved normal x86 KVM/WHP supervisor execution to `VcpuStop` via backend
  `run_until_stop` methods; `dillo-vm` no longer matches x86 `VcpuExit`.
- Moved HVF AArch64 PSCI decode, secondary CPU parking, and raw `VmExit`
  handling into `dillo-machine-hvf::run_smp`; `dillo-vm` now consumes only
  `VcpuStop` for the normal macOS supervisor path.
- Moved the PSCI decoder tests and CPU-slot wakeup tests into
  `dillo-machine-hvf`.

Remaining divergence:
- Linux gdb intentionally imports the explicit KVM `DebugExit` runner. Normal
  supervisor paths no longer import or match backend vCPU exit enums.

Local verification:
- `RUSTC_BOOTSTRAP=1 cargo fmt --all -- --check`
- `git diff --check`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target x86_64-unknown-linux-gnu`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target x86_64-pc-windows-msvc`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target aarch64-apple-darwin`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo test --workspace --exclude vhost-backend --exclude snuffler`

Pushed commit:
- `189e9c9 refactor: handle x86 pio writes in backends`
- `a6577cb refactor: hide pio exits behind backend facades`
- `cdd6f5b refactor: keep x86 halt exits in backends`
- `915e649 refactor: split kvm debug exits from normal run`
- `d715295 refactor: report unknown vcpu exits as errors`
- `ca1ba18 refactor: run x86 vcpus until lifecycle stop`
- `223c36c refactor: move hvf psci exits behind backend`

CI verification:
- `27143278195` passed on `cargo fmt`, `ubuntu-24.04`, and `windows-2025`.
- `27143928716` passed on `cargo fmt`, `ubuntu-24.04`, and `windows-2025`.
- `27144366150` passed on `cargo fmt`, `ubuntu-24.04`, and `windows-2025`.
- `27144798819` passed on `cargo fmt`, `ubuntu-24.04`, and `windows-2025`.
- `27145113148` passed on `cargo fmt`, `ubuntu-24.04`, and `windows-2025`.
- `27145571028` passed on `cargo fmt`, `ubuntu-24.04`, and `windows-2025`.
- `27147561569` passed on `cargo fmt`, `ubuntu-24.04`, and `windows-2025`.
- `27149322531` passed on `cargo fmt`, `ubuntu-24.04`, and `windows-2025`.
- `27149884240` passed on `cargo fmt`, `ubuntu-24.04`, and `windows-2025`.
- `27150535316` passed on `cargo fmt`, `ubuntu-24.04`, and `windows-2025`.

## Stage 10 - Implement vCPU stop control

Status: in progress.

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

Completed changes:
- Commit `a72a4ff` fixed WHP two-vCPU shutdown by canceling peer vCPUs from
  the returning vCPU thread; CI run `27148630715` passed on fmt, Ubuntu, and
  Windows/WHP.
- Moved WHP vCPU cancel-handle ownership into `dillo-machine-whp::Vm`.
- Added backend-owned `Vm::request_vcpu_exit()` on WHP and updated the Windows
  supervisor loop to request vCPU exit through the machine backend instead of
  iterating cancel handles in `dillo-vm`.
- Added a cloneable WHP backend exit requester and gave each Windows vCPU
  thread its own requester so the first returning vCPU cancels the rest without
  waiting on join order.
- Moved KVM vCPU thread signaling into `dillo-machine-kvm::Vm` through a
  backend-owned `VcpuExitRequester`; the Linux supervisor loop now asks the KVM
  backend to wake blocked vCPU runs instead of owning pthread IDs directly.
- Commit `c54d1e4` moved KVM wakeup ownership into the backend; CI run
  `27149322531` passed on fmt, Ubuntu/KVM boot tests, and Windows/WHP boot
  tests.

Remaining divergence:
- KVM still uses the existing signal wake path; no `immediate_exit` wrapper
  exists in the current local KVM abstraction.
- HVF uses backend-owned `force_vcpus_exit` inside `dillo-machine-hvf::run_smp`,
  but the `Machine::request_vcpu_exit` trait is not wired for the backend
  crates yet.
- CI run `27147948508` failed the WHP two-vCPU boot tests after cancellation
  ownership moved into the backend; commit `a72a4ff` corrected that regression
  and CI run `27148630715` confirmed the fix.

## Stage 11 - Implement process/thread device host attachment

Status: in progress.

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

Completed changes:
- Moved `DeviceModel` ownership into `dillo-mmio` and kept
  `dillo_machine::DeviceModel` as a re-export to avoid a dependency cycle.
- Added `MmioDeviceHost`, `MmioRunToken`, `MmioDeviceHandle`, and
  `MmioAttachment::spawn` as the backend-neutral launch/connect API.
- Implemented thread-host spawning for KVM, WHP, and HVF attachment objects;
  process-host requests currently fail closed as unsupported.
- Commit `18d0b7d` added the attachment API; CI run `27149884240` passed on
  fmt, Ubuntu/KVM boot tests, and Windows/WHP boot tests.
- Changed virtio activation to return a retained `VirtioDeviceHandle` and
  updated PCI/MMIO virtio transports to drop activation handles on reset/drop.
- Updated virtio-console TX/RX workers to observe shutdown and join through the
  retained activation handle.
- Commit `785d099` retained virtio activation worker handles; CI run
  `27150535316` passed on fmt, Ubuntu/KVM boot tests, and Windows/WHP boot
  tests.
- Moved current vhost-user child-process ownership into the retained virtio
  activation handle, with a frontend drop fallback when activation never occurs.

Remaining divergence:
- Existing devices are still activated by the compatibility path; no device
  wrapper calls `MmioAttachment::spawn` yet.
- Process-host support is represented in the API but not wired to current
  vhost-user behavior yet.

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
