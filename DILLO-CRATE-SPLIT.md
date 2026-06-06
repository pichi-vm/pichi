# dillo crate split design

Status: design target. This is not an implementation plan. It defines the crate
boundaries and trait contracts needed for the `dillo` binary and device crates
to have no knowledge of KVM, HVF, or WHP APIs.

## Empirical inputs

This design is derived from current code and primary specs, not from desired
shape alone.

| Fact | Evidence |
| --- | --- |
| PMI `.pmi.vm` launch order is read target, initialize hypervisor state, process actions, initialize boot vCPU, start guest. | `pichi-vm/pmi` `spec/vm.md:16` |
| PMI requires `vm:vcpu` and `cpu:profile`; both must match `PE.FileHeader.Machine`. | `pichi-vm/pmi` `spec/vm.md:26`, `spec/cpu.md:14` |
| PMI `merged` base DTB is platform definition; overlay may contribute only CPUs, memory, distance-map, and `numa-node-id`. | `pichi-vm/pmi` `spec/merged.md:31`, `spec/merged.md:53`, `spec/merged.md:98` |
| Arma defines platform as motherboard/slots; dillo plugs CPUs, memory, and runtime-discovered devices into those slots. | `arma/docs/device-model.md:21`, `arma/docs/device-model.md:51` |
| Arma forbids hidden guest hardware; device addresses and interrupts come from DTB. | `arma/docs/device-model.md:91` |
| Current `dillo-vm` has one `BackendVm` trait but it still exposes backend-shaped associated state and lives inside the monolith. | `dillo/deps/dillo-vm/src/backend.rs:58` |
| Current `MmioDevice` already supports multiple windows, which is required for `PciRoot` ECAM plus BAR windows. | `dillo/deps/dillo-vm/src/mmio_bus.rs:23` |
| Current `PciRoot` already owns ECAM plus BAR windows and implements `MmioDevice`. | `dillo/deps/dillo-vm/src/pci.rs:188`, `dillo/deps/dillo-vm/src/pci.rs:265` |
| Current `PciDevice`, `VirtioDevice`, `QueueNotifier`, and `MsixNotifier` are already separable traits, but some live in the wrong crates. | `dillo/deps/dillo-vm/src/pci.rs:23`, `dillo/deps/virtio/src/device.rs:27`, `dillo/deps/dillo-vm/deps/virtio-pci/src/transport.rs:63`, `dillo/deps/dillo-vm/deps/vm-pci/src/msix.rs:99` |
| CI's supported platform matrix is Linux x86-64/KVM, Windows x86-64/WHP, and macOS arm64/HVF, with warnings denied and real boot tests. | `.github/workflows/ci.yml:13`, `.github/workflows/ci.yml:92`, `.github/workflows/ci.yml:100`, `.github/workflows/ci.yml:108` |

## Target dependency graph

The final graph is acyclic and keeps backend APIs behind backend crates.

```text
dillo
  -> dillo-runtime
  -> dillo-backend     (Cargo alias; package is dillo-kvm, dillo-hvf, or dillo-whp)
  -> dillo-virtio-console / blk / net / vsock ...

dillo-runtime
  -> pmi, dillo-platform, dillo-core, dillo-pci, dillo-virtio, device crates

dillo-core
  -> pmi, dillo-platform, vm-memory, vm-pci

dillo-pci
  -> dillo-core, vm-pci

dillo-virtio
  -> virtio, dillo-core
  -> dillo-pci           (only with feature = "pci")

dillo-kvm
  -> dillo-core, dillo-pci, dillo-virtio, kvm-ioctls, kvm-bindings, vhost backend support

dillo-hvf
  -> dillo-core, dillo-pci, dillo-virtio, applevisor/HVF bindings

dillo-whp
  -> dillo-core, dillo-pci, dillo-virtio, WHP bindings
```

Cargo, not source cfg, selects the backend:

```toml
[target.'cfg(target_os = "linux")'.dependencies]
dillo-backend = { package = "dillo-kvm", path = "deps/dillo-kvm" }

[target.'cfg(target_os = "macos")'.dependencies]
dillo-backend = { package = "dillo-hvf", path = "deps/dillo-hvf" }

[target.'cfg(target_os = "windows")'.dependencies]
dillo-backend = { package = "dillo-whp", path = "deps/dillo-whp" }
```

`dillo` source imports only `dillo_backend::Backend`. It never imports
`dillo_kvm`, `dillo_hvf`, `dillo_whp`, `kvm_ioctls`, `applevisor`, or WHP
bindings.

## Crate roles

`dillo` is the binary crate. It parses CLI, initializes logging, chooses device
occupancy policy, and calls `dillo_runtime::run::<dillo_backend::Backend>(...)`.
It contains no OS cfg except non-backend CLI affordances that are inherently
host-specific, and no hypervisor API imports.

`dillo-runtime` is the generic launcher. It parses PMI, surveys the DTB,
validates ownership coverage, builds memory/overlay plans, plugs devices into
DTB-declared slots, and runs the supervisor against a generic `Vm`. It may know
PMI, DTB, dillo traits, and device crates. It must not know KVM, HVF, WHP, or
their handle types.

`dillo-core` owns the cross-crate contracts: `Vm`, `Vcpu`, `IoDevice`,
interrupt handles, memory plans, I/O windows, run outcomes, and errors. It
contains no backend implementation.

`dillo-platform` remains the DTB survey and resource-planning crate. It consumes
base DTB and overlay rules and returns typed platform facts with provenance. It
does not know backend crates or device implementations.

`dillo-pci` owns `PciRoot`, `PciDevice`, PCI config/BAR routing, and PCI slot
attachment. It is independent of backend crates. `PciRoot` is the single object
that owns every guest address range related to a declared PCI host bridge:
ECAM, BAR windows, MSI-X table/PBA BARs, and any x86 legacy config-port decoder
needed to access the same config space.

`dillo-virtio` owns transport adapters. Base feature exposes `VirtioMmio`;
feature `pci` adds `dillo-pci` and exposes `VirtioPci`. Virtio device logic is
in device crates and implements `virtio::VirtioDevice` once.

`dillo-kvm`, `dillo-hvf`, and `dillo-whp` implement `dillo_core::Vm` and
backend-owned support traits. They are the only crates allowed to depend on
OS hypervisor APIs.

## Trait inventory

Every trait that crosses a crate boundary is listed here. Traits not listed are
private implementation detail.

### `Vm`

Owned by `dillo-core`. Implemented by `dillo-kvm`, `dillo-hvf`, and `dillo-whp`.
Consumed by `dillo-runtime`.

```rust
pub enum DeviceModel {
    Process,
    Thread,
}

pub trait Vm: Sized + Send + Sync + 'static {
    const DEVICE_MODEL: DeviceModel;

    type Error: std::error::Error + Send + Sync + 'static;
    type Vcpu: Vcpu<Error = Self::Error>;
    type VcpuSeed: Send + 'static;
    type QueueNotifier: QueueNotifier;
    type MsiNotifier: vm_pci::MsixNotifier + 'static;

    fn new(opts: VmOptions) -> Result<Self, Self::Error>;
    fn guest_memory(&self) -> GuestMemory;
    fn attach_io(&mut self, dev: Arc<dyn IoDevice>) -> Result<(), Self::Error>;
    fn queue_notifier(&self) -> Self::QueueNotifier;
    fn wired_irq(&self, source: InterruptSource) -> Result<Interrupt, Self::Error>;
    fn msi_notifier(&self, source: MsiSource, vectors: u16)
        -> Result<Arc<Self::MsiNotifier>, Self::Error>;
    fn vcpu_seeds(&self, boot: BootState) -> Result<Vec<Self::VcpuSeed>, Self::Error>;
    fn create_vcpu(&self, seed: Self::VcpuSeed) -> Result<Self::Vcpu, Self::Error>;
}
```

`VmOptions` is backend-neutral input: PMI action plan, DTB-derived `Machine`,
memory plan, vCPU count, CPU profile, and launch sections. It does not contain
KVM fd, HVF GIC handles, WHP partition handles, raw GSIs, or raw SPIs.

`DEVICE_MODEL` is the stable process/thread policy. Linux/KVM returns
`Process`; HVF/WHP return `Thread`. Runtime may use the const for logging and
policy checks, but concrete spawning behavior is provided by a backend-selected
`DeviceHost` type so non-Linux builds do not depend on vhost crates.

### `Vcpu`

Owned by `dillo-core`. Implemented by backend crates.

```rust
pub trait Vcpu: Send + 'static {
    type Error: std::error::Error + Send + Sync + 'static;

    fn run(&mut self, exits: &dyn ExitHandler) -> Result<VcpuStop, Self::Error>;
}
```

`Vcpu::run` returns backend-neutral stops: exit code, reboot, halted secondary,
or fatal unknown exit. MMIO and PIO exits are dispatched through `ExitHandler`
inside `run` when the host API requires in-call emulation, and after `run`
otherwise. This hides KVM `VcpuExit`, HVF syndrome decoding, and WHP emulator
callbacks from runtime and device crates.

### `ExitHandler`

Owned by `dillo-core`. Implemented by `dillo-runtime`.

```rust
pub trait ExitHandler: Send + Sync {
    fn mmio_read(&self, addr: u64, data: &mut [u8]) -> bool;
    fn mmio_write(&self, addr: u64, data: &[u8]) -> bool;
    fn pio_read(&self, port: u16, data: &mut [u8]) -> bool;
    fn pio_write(&self, port: u16, data: &[u8]) -> bool;
    fn hypercall(&self, call: Hypercall) -> HypercallResult;
}
```

PIO is present here because some host APIs expose PIO exits. It is not a device
model feature. A PIO handler is installed only when a DTB-declared device
exposes an architectural PIO alias, currently the x86 PCI config-port alias for
the declared PCI host bridge.

### `IoDevice`

Owned by `dillo-core`. This replaces the current narrow name `MmioDevice`
because a single registered object may own multiple address spaces.

```rust
pub enum AddressSpace {
    Mmio,
    Pio,
}

pub struct IoWindow {
    pub space: AddressSpace,
    pub name: &'static str,
    pub base: u64,
    pub size: u64,
}

pub trait IoDevice: Send + Sync + std::fmt::Debug {
    fn windows(&self) -> Vec<IoWindow>;
    fn read(&self, window: IoWindow, offset: u64, data: &mut [u8]) -> bool;
    fn write(&self, window: IoWindow, offset: u64, data: &[u8]) -> bool;
}
```

All guest-visible address registration goes through `Vm::attach_io`. Ordinary
MMIO devices return only `AddressSpace::Mmio` windows. `PciRoot` returns ECAM
and BAR MMIO windows, and on x86 may also return CF8/CFC PIO windows as aliases
onto the same config accessor. Those PIO windows are enabled only because the
DTB declared the PCI host bridge; they are not a separate hardware decision.

### `PciDevice`

Owned by `dillo-pci`. Implemented by PCI transports and future PCI devices.

```rust
pub trait PciDevice: Send + std::fmt::Debug {
    fn config_read(&self, reg_idx: usize) -> u32;
    fn config_write(&mut self, reg_idx: usize, offset: u64, data: &[u8]);
    fn name(&self) -> &str;
    fn bar_regions(&self) -> Vec<BarRegion>;
    fn bar_read(&self, bar_idx: u8, offset: u64, data: &mut [u8]) -> bool;
    fn bar_write(&mut self, bar_idx: u8, offset: u64, data: &[u8]) -> bool;
}
```

### `PciRoot`

`PciRoot` is a type, not a trait. It is owned by `dillo-pci` and implements
`IoDevice`. Construction consumes DTB-derived PCI facts:

```rust
pub struct PciRootOptions {
    pub ecam: IoWindow,
    pub slots: u8,
    pub bar_window: AddressRange,
    pub msi: MsiSource,
    pub legacy_config_io: Option<LegacyConfigIo>,
}
```

Runtime asks for one `PciRoot`, registers devices into slots, then attaches that
single object to the VM. The VM receives no separate BAR registrations.

### `VirtioDevice`

Owned by `virtio`. Implemented by device crates (`dillo-virtio-console`,
`dillo-virtio-blk`, `dillo-virtio-net`, `dillo-virtio-vsock`, etc.).

The existing trait shape is mostly right: device type, queue count, queue sizes,
features, config read/write, and `activate`. The required change is that
`activate` receives resolved queue interrupts instead of making devices know
how to look up backend interrupt state.

```rust
pub trait VirtioDevice: Send {
    fn device_type(&self) -> u32;
    fn num_queues(&self) -> usize;
    fn queue_max_sizes(&self) -> &[u16];
    fn features(&self) -> u64;
    fn activate(&mut self, ctx: VirtioActivate) -> Result<(), ActivateError>;
    fn read_config(&self, offset: u64, data: &mut [u8]);
    fn write_config(&mut self, offset: u64, data: &[u8]);
}
```

`VirtioActivate` contains `GuestMemory`, queues, kicks, and resolved interrupt
handles. It contains no KVM fd, WHP interrupt controller, or HVF GIC handle.

### `QueueNotifier`

Owned by `dillo-core` or `dillo-virtio` and implemented by backend crates.

```rust
pub trait QueueNotifier: Send {
    fn register(&mut self, queue_index: usize, addr: u64, kick: &virtio::Kick)
        -> Result<(), QueueNotifyError>;
    fn unregister_all(&mut self);
}
```

KVM implements this with ioeventfd. HVF/WHP use a no-op or direct-kick
implementation. The trait is consumed by `dillo-virtio` transports, not by
device crates.

### `Interrupt`

Owned by `dillo-core`. Constructed by backend crates through `Vm::wired_irq`.
Consumed by transports and simple devices such as serial.

```rust
pub trait InterruptLine: Send + Sync + std::fmt::Debug {
    fn signal(&self);
    fn set_level(&self, level: bool) -> Result<(), InterruptError>;
}

#[derive(Clone, Debug)]
pub struct Interrupt(Arc<dyn InterruptLine>);
```

`signal` covers edge backends and pulse-style users. `set_level` is required
for self-leveling devices such as a 16550 on level-triggered backends. A backend
that cannot represent deassert returns `UnsupportedDeassert`; transports that
need deassert must fail closed on that backend.

### `MsixNotifier`

Owned by `vm-pci`. Kept as the PCI MSI-X callback trait. Backend crates
implement it. `dillo-pci` and `dillo-virtio` consume it through
`Arc<dyn MsixNotifier>` or an associated concrete notifier from `Vm`.

### `DeviceHost`

Owned by `dillo-core` or `dillo-device`. Selected by backend associated type.
It makes process vs thread execution explicit without source cfg in runtime.

```rust
pub trait DeviceHost: Send + Sync + 'static {
    const MODEL: DeviceModel;

    fn spawn_virtio(
        device: Arc<Mutex<Box<dyn virtio::VirtioDevice>>>,
        ctx: DeviceSpawn,
    ) -> Result<DeviceHandle, DeviceError>;
}
```

KVM selects a process host backed by vhost-user. HVF/WHP select an in-process
thread host. Device crates implement `VirtioDevice`; they do not choose the host
model.

## DTB ownership and slot filling

`dillo-runtime` performs a total DTB survey before creating a backend VM. Each
node/property has exactly one owner:

- `dillo-platform` parses and reports the base platform and overlay constraints.
- The selected `Vm` claims platform substrate: interrupt controllers, timer,
  CPU bringup, and power/reset semantics.
- `dillo-pci` claims a DTB-declared PCI host bridge and its slot capacity.
- `dillo-virtio` claims DTB-declared virtio-mmio transport slots.
- Device crates claim no DTB nodes. They occupy slots selected by runtime policy.

If a node/property is unclaimed, launch fails. If runtime wants to enable a
device but no declared slot exists, launch fails. If a property needed for
backend setup is absent from the DTB, launch fails or the arma device model is
extended; dillo must not substitute a guessed value.

## Source cfg policy

Allowed cfg locations:

- Cargo target dependencies selecting `dillo-backend`.
- Backend crates internally, for architecture details inside one backend.
- Tests that require a specific host API.

Forbidden cfg locations:

- `dillo` choosing KVM/HVF/WHP names in source.
- device crates changing behavior by `target_os`.
- `dillo-runtime` importing OS hypervisor APIs.
- `dillo-pci` or `dillo-virtio` importing OS hypervisor APIs.

Architecture variation is expressed through DTB facts and backend internals.
Runtime may branch on parsed PMI machine kind or DTB-compatible strings, but it
must not use architecture cfg to decide what guest hardware exists.

## Acceptance criteria for the split

This design is satisfied only when all of the following can be verified:

1. `dillo/src` contains no imports or references to KVM, HVF, WHP, or backend
   crate package names other than `dillo_backend`.
2. `dillo-runtime`, `dillo-pci`, `dillo-virtio`, and virtio device crates do not
   depend on `kvm-*`, `applevisor`/HVF bindings, WHP bindings, or
   `dillo-hypervisor`.
3. `dillo-kvm`, `dillo-hvf`, and `dillo-whp` are the only crates with direct
   OS hypervisor API dependencies.
4. All cross-crate traits in this document are public from their owning crates;
   previous `pub(crate)` attach traits have moved out of the monolith.
5. `PciRoot` is registered with the VM as one `IoDevice` and owns ECAM, BAR
   windows, MSI-X BARs, and any legacy config alias for that declared host
   bridge.
6. Device crates compile and test without any backend crate dependency.
7. Runtime can build the same device graph using only DTB-derived slot facts and
   the selected backend's `Vm` implementation.
8. Local verification passes:
   - `cargo fmt --all -- --check`
   - `CARGO_BUILD_RUSTFLAGS='-D warnings' cargo test --workspace` on Linux
   - `CARGO_BUILD_RUSTFLAGS='-D warnings' cargo test --workspace --exclude vhost-backend --exclude snuffler` on Windows and macOS
   - target checks for `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`, and `aarch64-apple-darwin`
9. CI passes all three supported platform lanes, including real boot tests.

## Current-state gaps

The current tree does not yet meet this design:

- `dillo-vm` is still a monolith containing runtime, backend adapters, PCI,
  MMIO bus, UART, syscon, IOAPIC, and backend device glue.
- `MmioDevice` and `PciDevice` are `pub(crate)`, so external device and backend
  crates cannot implement the final boundary.
- `BackendVm` is one trait shape now, but it is still too wide and contains
  optional methods that should become capability traits or backend-owned helper
  types.
- `dillo-vm` still imports `dillo_hypervisor::Vm` and `Vcpu` directly in the
  launcher.
- `virtio-pci::QueueNotifier` is transport-owned today; the final design needs
  it at the core/transport boundary so backend crates implement it without
  leaking KVM fd details.
- `dillo-device` contains an older process/thread abstraction that is not yet
  the active `VirtioDevice` activation contract.

These gaps are evidence that the target design is not merely a crate move. The
split requires the trait surfaces above before crates can be separated without
preserving backend knowledge in the wrong layer.
