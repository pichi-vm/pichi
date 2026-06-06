# dillo DESIGN.md migration

Rules for every stage:

1. Change only `dillo/` and dillo-owned deps unless the stage explicitly says otherwise.
2. Do not change `arma`, `tatu`, or PMI behavior; treat PMI and `arma/docs/device-model.md` as contracts.
3. Do one stage at a time.
4. Run local verification before committing.
5. Commit only that stage.
6. Push the commit and let CI independently verify supported platforms before starting the next stage.
7. After a stage is complete, update this file in the same commit or a follow-up bookkeeping commit.

Default local verification:

```sh
RUSTC_BOOTSTRAP=1 cargo test -p dillo-platform -p dillo-vm --all-targets
```

Run `RUSTC_BOOTSTRAP=1 cargo test -p arma --all-targets` only when root workspace or shared dependency wiring changes.

## Stage 0 - Upstream PMI crate

Status: complete.

Goal: replace the local `deps/pmi` crate with the upstream PMI repo crate.

Success criteria:
- Workspace `pmi` dependency resolves from `https://github.com/pichi-vm/pmi`.
- Local `deps/pmi` source is removed from the workspace.
- `dillo-platform`, `dillo-vm`, and `arma` tests pass locally.

Completed changes:
- `Cargo.toml` uses `pmi = { git = "https://github.com/pichi-vm/pmi" }`.
- `Cargo.lock` pins `pmi` to `pichi-vm/pmi#d068c50a`.
- `arma/Cargo.toml` uses `pmi.workspace = true`.
- `deps/pmi` source files were deleted.

## Stage 1 - Make `Machine::survey` authoritative in dillo

Status: complete.

Goal: dillo launch paths consume the total-coverage `Machine::survey` result instead of partial `extract -> Platform` state.

Process:
- Replace dillo run-path `dillo_platform::extract` usage with `Machine::survey` where possible.
- Drive placement and load-vs-device validation from `ResourcePlan` / claimed regions.
- Keep compatibility helpers only as temporary adapters around surveyed data.

Success criteria:
- Linux, macOS, and Windows dillo launch paths have a coverage gate before realization.
- New code does not use `Platform.device_regions` as authoritative placement input.
- Tests prove unknown DTB nodes/properties fail closed.
- Default local verification passes.

Completed changes:
- Windows and Linux launch paths now run `Machine::survey` before realization.
- Windows and Linux load validation uses surveyed `ResourcePlan` coverage.
- Windows and Linux memory placement uses surveyed placement regions.
- Existing `Platform` extraction remains only as a temporary realization adapter.

Local verification:
- `RUSTC_BOOTSTRAP=1 cargo test -p dillo-platform -p dillo-vm --all-targets`

## Stage 2 - Preserve DTB relationship provenance

Status: complete.

Goal: dillo retains enough DTB relationship data to resolve interrupts and MSI from declared controller links, not positional heuristics.

Process:
- Record `interrupt-parent`, `msi-parent`, and controller phandles during survey consumption.
- Represent wired interrupt sources as DTB-derived sources resolved against the claimed controller.
- Represent MSI parentage for the PCIe bridge from DTB data.

Success criteria:
- Device interrupt wiring is derived from node relationship data and controller `#interrupt-cells`.
- Tests cover aarch64 GIC interrupt cells and x86 IOAPIC interrupt cells.
- Tests fail when required interrupt/MSI parentage is missing or inconsistent.
- Default local verification passes.

Completed changes:
- `Machine::survey` records interrupt controller phandles, kinds, and `#interrupt-cells`.
- Serial and virtio-mmio interrupts are resolved through `interrupt-parent`.
- PCIe MSI parentage is resolved through `msi-parent`.
- Missing, unknown, and malformed interrupt/MSI relationships fail during survey.

Local verification:
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo test -p dillo-platform -p dillo-vm --all-targets`

## Stage 3 - Add `MmioDevice` beside `MmioBus`

Status: complete.

Goal: introduce the universal MMIO attach trait without breaking existing closure-based bus wiring.

Process:
- Add `MmioWindow`.
- Add `MmioDevice: Send + Sync` with `window`, `read`, and `write`.
- Add `MmioBus::register_device(...)`.
- Keep existing closure registration until all users migrate.

Success criteria:
- Existing closure registrations still work.
- Trait-device registration routes offsets identically to closure registration.
- Overlap detection works for trait devices.
- Default local verification passes.

Completed changes:
- Added `MmioWindow`.
- Added `MmioDevice: Send + Sync` with `window`, `read`, and `write`.
- Added `MmioBus::register_device(...)`.
- Kept existing closure registration intact.

Local verification:
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --lib`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo test -p dillo-platform -p dillo-vm --all-targets`

## Stage 4 - Convert UART to an owned MMIO device

Status: complete.

Goal: replace global UART state and per-OS init functions with an owned `Ns16550` `MmioDevice`.

Process:
- Remove `OnceLock<Mutex<Ns16550>>` as the runtime device model.
- Give the UART object its DTB-derived window, `reg-shift`, output sink, and injected interrupt.
- Keep serial as a plugged external platform device, not VM substrate.

Success criteria:
- UART is attachable as an `MmioDevice`.
- No per-OS UART init signature remains in the device model.
- Existing THR-empty behavior tests still pass.
- Default local verification passes.

Completed changes:
- Replaced process-global UART state with owned `Ns16550` devices.
- `Ns16550` now carries its DTB-derived `MmioWindow` and attaches through `MmioBus::register_device`.
- Backend launch paths construct the UART with their host interrupt trigger and output sink, then plug it into the MMIO bus.
- Removed the global init/read/write UART callback API.

Local verification:
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --lib`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo test -p dillo-platform -p dillo-vm --all-targets`

## Stage 5 - Model substrate MMIO explicitly

Status: complete.

Goal: make VM-owned substrate devices explicit when they are realized through MMIO.

Process:
- Convert userspace IOAPIC register model to an attached VM-owned MMIO device.
- Convert x86 syscon poweroff/reset to VM-owned MMIO device(s).
- Keep substrate ownership distinct from plugged device ownership.

Success criteria:
- IOAPIC and syscon are attached through the same MMIO mechanism as other MMIO devices.
- Syscon paths return structured shutdown/reboot state instead of directly exiting where practical for the stage.
- Default local verification passes.

Completed changes:
- Added typed x86 syscon MMIO devices that record structured poweroff/reboot actions.
- Linux and Windows x86 run loops now observe syscon action state instead of exiting from the MMIO write handler.
- Windows IOAPIC now owns its DTB-derived MMIO window and attaches through `MmioBus::register_device`.
- Updated stale supervisor comments that described the removed direct-exit behavior.

Local verification:
- `RUSTC_BOOTSTRAP=1 cargo fmt --all -- --check`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --lib`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo test -p dillo-platform -p dillo-vm --all-targets`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo --all-targets`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --lib --target x86_64-unknown-linux-gnu`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target x86_64-unknown-linux-gnu`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --lib --target x86_64-pc-windows-msvc`
- `RUSTC_BOOTSTRAP=1 CARGO_BUILD_RUSTFLAGS='-D warnings' cargo check -p dillo-vm --tests --target x86_64-pc-windows-msvc`

## Stage 6 - Convert PCI root into an MMIO device

Status: pending.

Goal: make the declared PCIe host bridge a `PciRoot` with an ECAM `MmioDevice` face.

Process:
- Reshape `PciBus`/host bridge into `PciRoot`.
- Move ECAM decoding out of per-backend run-path closures.
- Keep x86 CF8/CFC as a backend/supervisor decoder onto the same config accessor.

Success criteria:
- ECAM config reads/writes route through `PciRoot`.
- BAR dispatch remains correct.
- x86 legacy config ports and ECAM return identical base config bytes.
- Default local verification passes.

## Stage 7 - Hide KVM handles from virtio-pci

Status: pending.

Goal: remove KVM `VmFd` leakage from the virtio-pci transport.

Process:
- Remove `set_vm_fd` from the transport-facing API.
- Move ioeventfd registration behind a backend notify hook.
- Keep `MsixNotifier` as the MSI-X routing abstraction.

Success criteria:
- `virtio-pci` no longer stores or exposes `VmFd`.
- KVM ioeventfd behavior is preserved.
- non-Linux direct kick behavior is preserved.
- Default local verification passes.

## Stage 8 - Introduce compile-time `Vm` trait

Status: pending.

Goal: add the backend abstraction boundary from `DESIGN.md` after attach surfaces are uniform.

Process:
- Add `VmOptions` with DTB-derived memory, substrate, vCPU count, and address-space watermark.
- Add `guest_memory`, `attach_mmio`, `wired_irq`, `msi_notifier`, and vCPU seed/factory APIs.
- Keep dispatch static; do not add `dyn Vm` in the vCPU hot path.

Success criteria:
- Backend-specific construction ordering is hidden behind each implementation.
- No KVM/HVF/WHP handle leaks above the trait.
- Existing launch behavior is preserved.
- Default local verification passes.

## Stage 9 - Unify supervisor run outcome

Status: pending.

Goal: supervisor owns vCPU threads and returns uniform `RunOutcome`.

Process:
- Move direct shutdown/reboot exits toward `RunOutcome::{Exit, Reboot}`.
- Preserve HVF warm reboot.
- Bring Linux and WHP shutdown paths into the same shape.

Success criteria:
- Guest poweroff and reboot are represented structurally.
- vCPU loops remain backend-correct.
- Default local verification passes.

## Stage 10 - Remove temporary compatibility paths

Status: pending.

Goal: finish the migration by deleting bridge code that kept old and new attach paths alive together.

Process:
- Remove closure-only MMIO registration paths once no longer used.
- Remove stale `extract -> Platform` adapters from dillo.
- Remove obsolete TODOs/comments created by earlier stages.

Success criteria:
- dillo enables no guest-visible hardware unless derived from the PMI DTB.
- The trait stack in `DESIGN.md` matches implementation.
- Default local verification passes.
