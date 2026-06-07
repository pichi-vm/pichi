# dillo crate split design

Status: design target. This is not an implementation plan. It defines the crate
boundaries and trait contracts needed for the `dillo` binary to compose PMI,
DTB-derived slots, devices, transports, and one host `Machine` implementation
without exposing KVM, HVF, or WHP APIs above backend crates.

## Empirical inputs

This design is derived from current code and primary specs.

| Fact | Evidence |
| --- | --- |
| PMI `.pmi.vm` launch order is read target, initialize hypervisor state, process actions, initialize boot vCPU, start guest. | `pichi-vm/pmi` `spec/vm.md:16` |
| PMI requires `vm:vcpu` and `cpu:profile`; both must match `PE.FileHeader.Machine`. | `pichi-vm/pmi` `spec/vm.md:26`, `spec/cpu.md:14` |
| PMI `merged` base DTB is platform definition; overlay may contribute only CPUs, memory, distance-map, and `numa-node-id`. | `pichi-vm/pmi` `spec/merged.md:31`, `spec/merged.md:53`, `spec/merged.md:98` |
| Arma defines platform as motherboard/slots; dillo plugs CPUs, memory, and runtime-discovered devices into those slots. | `arma/docs/device-model.md:21`, `arma/docs/device-model.md:51` |
| Arma forbids hidden guest hardware; device addresses and interrupts come from DTB. | `arma/docs/device-model.md:91` |
| KVM confidential-computing private memory uses `guest_memfd`; private pages cannot be mapped, read, or written by userspace. Shared/private state is selected per GFN with `KVM_MEMORY_ATTRIBUTE_PRIVATE`. | Linux KVM API `KVM_SET_USER_MEMORY_REGION2`, `KVM_SET_MEMORY_ATTRIBUTES`, `KVM_CREATE_GUEST_MEMFD` |
| KVM userspace must explicitly track page private/shared state; the KVM memory attributes API has no get operation. | Linux KVM API `KVM_SET_MEMORY_ATTRIBUTES` |
| KVM returns `KVM_EXIT_MMIO` only for MMIO that could not be satisfied by KVM; `KVM_IOEVENTFD` lets a registered MMIO write signal an eventfd instead of exiting. | Linux KVM API `KVM_EXIT_MMIO`, `KVM_IOEVENTFD` |
| KVM interrupt acceleration is also registration based: `KVM_IRQFD` lets an eventfd directly trigger a guest interrupt. | Linux KVM API `KVM_IRQFD` |
| TDX VM initialization is a VM-specific operation that must occur before vCPU creation; TDX also has specific VM/vCPU/memory init commands and CPUID handling. | Linux KVM TDX API `KVM_TDX_INIT_VM`, `KVM_TDX_INIT_VCPU`, `KVM_TDX_INIT_MEM_REGION`, `KVM_TDX_GET_CPUID` |
| SEV-ES/SNP initial CPU state is launch material with platform limits; unsupported initial-state fields must fail rather than be silently configured. | QEMU IGVM documentation, "Initial CPU state with VMSA" |
| Current `dillo-vm` has one `BackendVm` trait but it still exposes backend-shaped associated state and lives inside the monolith. | `dillo/deps/dillo-vm/src/backend.rs:58` |
| Current `MmioDevice` already supports multiple windows, which is required for `PciRoot` ECAM plus BAR windows. | `dillo/deps/dillo-vm/src/mmio_bus.rs:23` |
| Current `PciRoot` already owns ECAM plus BAR windows and implements `MmioDevice`. | `dillo/deps/dillo-vm/src/pci.rs:188`, `dillo/deps/dillo-vm/src/pci.rs:265` |
| Current `PciDevice`, `VirtioDevice`, `QueueNotifier`, and `MsixNotifier` are already separable traits, but some live in the wrong crates. | `dillo/deps/dillo-vm/src/pci.rs:23`, `dillo/deps/virtio/src/device.rs:27`, `dillo/deps/dillo-vm/deps/virtio-pci/src/transport.rs:63`, `dillo/deps/dillo-vm/deps/vm-pci/src/msix.rs:99` |
| Current `BackendVm` exposes PCI-specific MSI-X notifier construction; the target split must remove that transport leak from the machine boundary. | `dillo/deps/dillo-vm/src/backend.rs:64`, `dillo/deps/dillo-vm/src/backend.rs:74` |
| CI's supported platform matrix is Linux x86-64/KVM, Windows x86-64/WHP, and macOS arm64/HVF, with warnings denied and real boot tests. | `.github/workflows/ci.yml:13`, `.github/workflows/ci.yml:92`, `.github/workflows/ci.yml:100`, `.github/workflows/ci.yml:108` |

## Name stability

Names in the target graph are proposed logical crate names unless stated
otherwise. They are not a promise that today's organic workspace crates survive
with the same names or boundaries.

Current crates are evidence and migration sources:

- `dillo-vm` should disappear as a monolith.
- `dillo-hypervisor` should split into backend `Machine` implementation crates.
- `dillo-pmi` can become a `dillo` module unless reuse justifies a crate.
- `dillo-platform` can become a `dillo` DTB-survey module or a renamed crate.
- `dillo-device` is an older process/thread experiment, not the target boundary.
- current `virtio`, `virtio-pci`, `vm-pci`, and `vhost-backend` may be renamed,
  split, absorbed, or upstreamed depending on the final rust-vmm alignment.

## Target graph

```mermaid
flowchart TD
    dtb_survey["DTB survey layer"]
    pci_types["PCI helper types"]

    dillo-machine --> dillo-mmio

    dillo-pci --> dillo-mmio
    dillo-pci --> pci_types

    dillo-mmio-virtio --> dillo-mmio
    dillo-mmio-virtio --> dillo-virtio

    dillo-pci-virtio --> dillo-pci
    dillo-pci-virtio --> dillo-virtio

    dillo-mmio-uart --> dillo-mmio

    dillo-virtio-blk --> dillo-virtio
    dillo-virtio-console --> dillo-virtio
    dillo-virtio-net --> dillo-virtio
    dillo-virtio-vsock --> dillo-virtio

    dillo-x86 --> dillo-mmio
    dillo-arm --> dillo-mmio

    dillo-machine-kvm --> dillo-machine
    dillo-machine-kvm --> dillo-mmio
    dillo-machine-kvm -.-> dillo-x86
    dillo-machine-kvm -.-> kvm-ioctls
    dillo-machine-kvm -.-> kvm-bindings

    dillo-machine-hvf --> dillo-machine
    dillo-machine-hvf --> dillo-mmio
    dillo-machine-hvf -.-> dillo-arm
    dillo-machine-hvf -.-> applevisor

    dillo-machine-whp --> dillo-machine
    dillo-machine-whp --> dillo-mmio
    dillo-machine-whp -.-> dillo-x86
    dillo-machine-whp -.-> whp-api

    dillo --> pmi
    dillo --> dtb_survey
    dillo --> dillo-machine
    dillo --> dillo-mmio
    dillo --> dillo-pci
    dillo --> dillo-mmio-virtio
    dillo --> dillo-pci-virtio
    dillo --> dillo-mmio-uart
    dillo --> dillo-virtio-blk
    dillo --> dillo-virtio-console
    dillo --> dillo-virtio-net
    dillo --> dillo-virtio-vsock

    dillo -.-> dillo-machine-kvm
    dillo -.-> dillo-machine-hvf
    dillo -.-> dillo-machine-whp
```

Dashed edges are target-selected or architecture-selected dependencies. The
important dependency rule is stronger than the picture: backend crates never
depend on PCI, virtio transports, UART, or concrete devices; device crates never
depend on machine crates.

## Knowledge boundaries

`dillo` is the main user experience and the only composition point. It knows:

- PMI and dillo's PMI-loading module;
- the base DTB survey layer;
- every concrete device crate dillo can instantiate;
- every transport adapter crate dillo can use;
- the `dillo-machine::Machine` trait;
- exactly one selected backend package through a Cargo target dependency alias.

`dillo` must not know KVM, HVF, WHP, or raw architecture substrate details. It
may use target dependencies to bind a generic crate name, for example
`dillo_machine_backend`, to `dillo-machine-kvm`, `dillo-machine-hvf`, or
`dillo-machine-whp`.

Machine backend crates know:

- `dillo-machine`;
- `dillo-mmio`;
- their host OS hypervisor API;
- optional architecture substrate crates such as `dillo-x86` or `dillo-arm`.

Machine backend crates do not know PCI, virtio, UART, or concrete devices. A
backend exposes an inherent constructor for its concrete `Machine`, creates and
runs vCPUs, attaches opaque `MmioDevice`s, and provides interrupt/event plumbing
through `dillo-mmio` types.

Device crates know their own device protocol and the narrow traits they
implement. Virtio devices know `dillo-virtio`. MMIO devices know `dillo-mmio`.
PCI transport devices know `dillo-pci`. Devices do not know `dillo-machine`,
PMI, DTB, KVM, HVF, or WHP.

## Crate roles

`dillo-machine` owns the host-neutral VM contract. Its main trait is
`Machine`, not `Vm`, to match the role: one realized virtual machine with vCPU
lifecycle, private guest memory ownership, shared-page mediation, MMIO
attachment, and backend-neutral run outcomes.
It does not own backend construction policy; concrete backend crates expose
inherent constructors for launch.

`dillo-mmio` owns `MmioDevice`, MMIO windows, line/message interrupt
abstractions, and the backend-neutral I/O event registration shape. It does not
own PCI, virtio, or machine construction.

`dillo-pci` owns the concrete `PciRoot` and the `PciDevice` trait. `PciRoot`
implements `MmioDevice`, owns the ECAM window plus BAR windows for one declared
PCI host bridge, and can attach `PciDevice`s into DTB-declared slot capacity.
It may use a PCI helper layer for reusable PCI config/MSI-X structures.

`dillo-pci-virtio` owns `VirtioPciDevice`, the concrete adapter from
`dillo-virtio::VirtioDevice` to `dillo-pci::PciDevice`.

`dillo-mmio-virtio` owns the concrete adapter from
`dillo-virtio::VirtioDevice` to `dillo-mmio::MmioDevice`.

`dillo-virtio` owns the transport-neutral virtio device trait and queue,
feature, kick, and activation types shared by all virtio transports and devices.

`dillo-virtio-*` crates own concrete virtio device implementations. They expose
concrete inherent constructors. They do not parse DTB and do not decide slot
occupancy.

`dillo-mmio-uart` owns a concrete MMIO 16550-compatible UART device. It exposes
an inherent constructor that takes DTB-derived MMIO window and interrupt
requirements.

`dillo-x86` and `dillo-arm` are optional architecture substrate crates for
machine-owned architecture machinery such as IOAPIC, GIC, syscon, PSCI, and
architecture-specific interrupt decoding. Backend crates may depend on these as
appropriate. `dillo` should not directly manipulate their internals.

The DTB survey layer consumes base DTB and overlay rules and returns typed
platform facts with provenance. Today this logic lives in `dillo-platform`, but
the target design does not require that crate name or that it remain a separate
crate. It does not know backend crates or concrete device implementations.

`pmi` is the upstream PMI spec/data crate. Today's `dillo-pmi` crate is
dillo-specific PE parsing, resource caps, and defensive validation. That can
become a `dillo` module unless reuse justifies a crate.

## Runtime assembly flow

`dillo` owns the only end-to-end assembly flow:

1. Read PMI and validate the image contract.
2. Decide load/fill placement and host resource placement from PMI, the base DTB,
   and requested runtime resources.
3. Generate the PMI overlay containing only host-provided CPUs, memory,
   distance-map data, and `numa-node-id`.
4. Merge base DTB plus overlay into one mutable `devtree::OwnedTree`.
5. Construct the selected concrete `Machine` by draining machine-owned platform
   facts from the merged devtree.
6. Incrementally construct CPU, memory, MMIO, bus, transport, and device objects
   from the same mutable tree and attach each one to the machine.
7. Fail if any DTB node/property remains after assembly.
8. Run the attached vCPUs through the machine's `Vcpu` objects.

The merged devtree is therefore not drained up front into one large plan.
Consumption is incremental: each constructed object drains only the nodes and
properties it owns, then the next object sees the remaining tree. The final empty
tree check proves that all guest-visible hardware came from the DTB and that no
DTB fact was ignored.

The shape of the `dillo` orchestration is:

```rust
let pmi = PmiImage::read(input)?;
let base = pmi.base_dtb()?;
let placement = Placement::from_pmi_and_base_dtb(&pmi, &base, request)?;
let overlay = placement.overlay()?;
let mut tree = devtree::OwnedTree::merge(base, overlay)?;

let model = require(KvmTdxModel::from_devtree(&mut tree)?)?;
let mut machine = KvmTdxMachine::new(model)?;

for memory in KvmTdxMemory::all_from_devtree(&mut tree)? {
    machine.attach(memory)?;
}

let mut vcpus = Vec::new();
for cpu in KvmTdxCpu::all_from_devtree(&mut tree)? {
    vcpus.push(machine.attach(cpu)?);
}

let mut attached_mmio = Vec::new();
for device in dillo_mmio_devices_from_devtree(&mut tree)? {
    let attachment = machine.attach(Arc::clone(&device))?;
    attached_mmio.push((device, attachment));
}

tree.require_empty()?;
let device_hosts = spawn_device_hosts(attached_mmio)?;
run_supervisor(vcpus, device_hosts)?;
```

The concrete backend type above is selected by the target/backend alias. The
pattern is the same for plain KVM, KVM+SEV, KVM+TDX, HVF, and WHP, but the
machine model, CPU input type, and memory input type may differ.

`attached_mmio` is handed to the selected device-host wrapper. The wrapper knows
the concrete device protocol; the backend attachment knows the parallel
execution model. The wrapper creates a device-host launch request compatible
with the selected `Machine::DEVICE_MODEL`; the attachment consumes that request
to start or connect the host. In both cases, the machine has already registered
MMIO routing before vCPUs run.

`run_supervisor` owns VM lifecycle. vCPU worker threads run synchronous
`Vcpu::run()` calls and report `VcpuStop` outcomes to the supervisor over normal
Rust channels. A guest poweroff is handled in this order:

1. One vCPU worker reports `VcpuStop::GuestPoweroff`.
2. The supervisor asks the machine run control to make every still-running vCPU
   leave `Vcpu::run()`.
3. The supervisor joins all vCPU workers. At this point no guest CPU can issue
   new MMIO.
4. The supervisor requests device-host shutdown through each `MmioDeviceHandle`.
5. The supervisor joins all device hosts.
6. Machine-owned MMIO routing, interrupt routing, and memory state may drop.

Device hosts must not independently tear down guest-visible state while any vCPU
may still access it. Device-host shutdown is therefore after vCPU quiescence in
the normal poweroff path.

## Devtree consumption

Concrete devices have inherent constructors. Devtree consumption is not in
device crates.

`dillo` owns a local trait such as:

```rust
trait FromDevTree {
    type Error;

    fn from_devtree(tree: &mut devtree::OwnedTree) -> Result<Option<Self>, Self::Error>
    where
        Self: Sized;
}
```

`dillo` implements this trait for concrete machine inputs, device inputs, and
adapter types because only `dillo` knows all inputs at once: PMI, the mutable
devtree, selected transport, selected concrete `Machine`, and every concrete
device type it can instantiate. This is intentionally not an independent crate.

Construction uses the existing drain model incrementally. `dillo` constructs one
object at a time from one mutable `devtree::OwnedTree`. Each successful
constructor removes every node and property it owns from that tree. After all
constructors and attachments run, any remaining node or property is an error.
`FromDevTree` always receives the whole tree; a consumer may drain as many nodes
as it owns. The common MMIO-device case is one DTB node defining the device's
constructor parameters: `reg` windows, interrupts, DMA/notification facts, and
device-specific properties.

`Ok(None)` means the relevant node is absent and the implementation consumed
nothing. `Ok(Some(_))` means construction succeeded and all owned nodes and
properties were drained. `Err(_)` means a relevant node was present but
malformed, unsupported, or incomplete.

The rule is fail closed: if a DTB node/property is not consumed by exactly one
owner, launch fails. If dillo wants to plug a device but the base DTB did not
declare a compatible slot, launch fails. If required setup data could have been
in the DTB but is absent, dillo fails or the arma device model is extended; it
does not substitute guessed guest-visible hardware.

## Trait inventory

Every trait intended to cross a crate boundary is listed here. Traits not listed
are implementation details until proven otherwise.

## Confidential-computing memory model

The execution model is confidential-computing first. Guest RAM is private unless
the guest-visible platform declares a shared communication surface and the
backend marks or maps the corresponding pages as shared.

The only way to access guest memory is through successful device registration.
The target API exposes shared memory only through `SharedRegion` handles minted
by `Attach<Arc<dyn MmioDevice>>` for the registered `MmioDevice` and its
declared resources. There is no whole-guest-memory accessor, no raw
guest-to-host address translation API, and no API for devices to inspect private
pages. A standard VM must fit this model by treating its otherwise-readable RAM
as if only registered device attachments could access declared shared regions.

For KVM, this maps to `guest_memfd` plus memory attributes: private pages live
in guest memory that userspace cannot map, while userspace-visible shared pages
are selected by clearing the private attribute for the relevant GFNs. Because
KVM does not provide a memory-attribute get API, the backend must track
shared/private state explicitly and fail closed on requests that do not match
its tracked state.

### `Attach`

Owned by `dillo-mmio`. Implemented by concrete composition points such as
machine backends and `dillo-pci::PciRoot`.

```rust
pub trait Attach<T> {
    type Error: std::error::Error + Send + Sync + 'static;
    type Output;

    fn attach(&mut self, item: T) -> Result<Self::Output, Self::Error>;
}
```

`Attach<T>` is not machine-specific. It is the generic operation for registering
one constructed object with another constructed object while preserving the
owner's error type and returning the registered object's runtime handle. Machine
backends use it to attach memory, CPUs, and MMIO devices. `PciRoot` uses it to
attach PCI endpoints into DTB-declared slot capacity.

### `Machine`

Owned by `dillo-machine`. Implemented by `dillo-machine-kvm`,
`dillo-machine-hvf`, and `dillo-machine-whp`. Consumed by `dillo`.

```rust
pub enum DeviceModel {
    Thread,

    Process,
}

pub trait Machine: Sized + Send + Sync + 'static {
    type Error: std::error::Error + Send + Sync + 'static;
    type Vcpu: Vcpu<Error = Self::Error>;
    type Cpu: Send + 'static;
    type Memory: Send + 'static;

    const DEVICE_MODEL: DeviceModel;

    fn vcpu_run_control(&self) -> VcpuRunControl;
}

#[derive(Clone)]
pub struct VcpuRunControl {
    // opaque
}

impl VcpuRunControl {
    pub fn request_exit(&self) -> Result<(), VcpuRunControlError>;
}

pub struct VcpuRunControlError {
    // opaque
}
```

Concrete machine implementations must support the attachment set that `dillo`
uses. When `dillo` is generic over a selected machine type, the required bounds
express the shared error contract with fully qualified associated types:

```rust
where
    M: Machine,
    M: Attach<
        <M as Machine>::Memory,
        Error = <M as Machine>::Error,
        Output = (),
    >,
    M: Attach<
        <M as Machine>::Cpu,
        Error = <M as Machine>::Error,
        Output = <M as Machine>::Vcpu,
    >,
    M: Attach<
        Arc<dyn MmioDevice>,
        Error = <M as Machine>::Error,
        Output = Arc<dyn MmioAttachment>,
    >,
```

`Machine` has no trait constructor and no `launch()` API. Each backend crate
exposes small inherent constructors on concrete machine and machine-input types.
`dillo` owns local `FromDevTree` implementations that drain the merged devtree
into those typed inputs, then calls the inherent constructors. For example:

```rust
impl KvmTdxMachine {
    pub fn new(model: KvmTdxModel) -> Result<Self, KvmTdxError>;
}

impl KvmTdxCpu {
    pub fn new(model: KvmTdxCpuModel) -> Result<Self, KvmTdxError>;
}
```

The concrete machine is assembled by attaching typed objects:

```rust
impl Attach<KvmTdxMemory> for KvmTdxMachine {
    type Error = KvmTdxError;
    type Output = ();
}

impl Attach<KvmTdxCpu> for KvmTdxMachine {
    type Error = KvmTdxError;
    type Output = KvmTdxVcpu;
}

impl Attach<Arc<dyn MmioDevice>> for KvmTdxMachine {
    type Error = KvmTdxError;
    type Output = Arc<dyn MmioAttachment>;
}
```

`Machine::Cpu` and `Machine::Memory` are associated types because CPU and memory
attachment material differs by machine family. Plain KVM may need explicit CPU
initial register/state data. KVM+SEV and KVM+TDX may need confidential launch
material, accepted initial state, memory acceptance/private-page setup, or
opaque per-vCPU setup. `dillo` may request only PMI-defined CPU count, CPU
profile, memory, load/fill, and initial state. Its `FromDevTree`
implementations translate those facts into `Machine::Cpu` and `Machine::Memory`
values for the selected machine or fail closed.

`Machine` owns all guest memory, but exposes only attachment-scoped shared
regions that a confidential VM can expose. Standard VMs may implement this with
ordinary mapped memory internally, but the public API must not let callers read
or write arbitrary guest-private memory. Without a successful MMIO attachment
call, no device receives any guest-memory capability.

Attaching `Machine::Memory` grants memory ownership to the machine but does not
grant devices access to guest memory. Attaching `Arc<dyn MmioDevice>` validates
the device's DTB-derived windows, realizes interrupts, creates only the declared
shared-memory capabilities for that device, and returns a backend-implemented
`MmioAttachment`. Attaching `Machine::Cpu` returns a runnable `Vcpu`; the CPU
input type carries whatever non-CC or CC-specific construction material that
machine family requires.

`Machine` must stay device-model neutral. It must not expose PCI, MSI-X, UART,
IOAPIC, GIC, KVM irqfd, WHP vector, or HVF-specific concepts in its public
trait. Those details belong either in backend internals, architecture substrate
crates, or protocol adapters below `dillo`.

Interrupt needs are advertised by the `MmioDevice` being attached. `FromDevTree`
implementations drain interrupt properties from the devtree into typed
requirements stored on the constructed device. `Attach<Arc<dyn MmioDevice>>` is
obligated to resolve those requirements for the backend, fail if it cannot, and
return the resulting handles through its backend-specific `MmioAttachment`
implementation.

`DEVICE_MODEL` records process vs thread device-host policy. Backend crates own
the mechanics of launching or connecting the device host for their model, but
they must not depend on virtio, UART, PCI endpoint, or other concrete device
crates. The device-host wrapper supplies a backend-neutral host launch request
for the selected device model; the backend-implemented `MmioAttachment` consumes
that request to run a thread, connect to a process, or use a backend-specific
service.

### `Vcpu`

Owned by `dillo-machine`. Implemented by backend crates.

```rust
pub trait Vcpu: Send + 'static {
    type Error: std::error::Error + Send + Sync + 'static;

    fn run(&mut self) -> Result<VcpuStop, Self::Error>;
}

pub enum VcpuStop {
    GuestPoweroff,

    GuestReset,

    Stopped,

    Fatal,
}
```

`Vcpu::run` hides KVM `VcpuExit`, HVF syndrome decoding, and WHP emulator
callbacks from dillo and devices. The only guest I/O exit exposed above the
backend is MMIO. PIO, hypercalls, CPUID leaves, PSCI calls, and backend-specific
emulation exits are backend or architecture-substrate internals and must not
become dillo/device APIs.

`Vcpu::run` has no external MMIO callback argument.
`Attach<Arc<dyn MmioDevice>>` registers the device's MMIO windows in
machine-owned routing state, and vCPUs created by that machine carry whatever
handle they need to route unresolved MMIO exits internally. On Linux/KVM,
the returned `MmioAttachment` may also bind ioeventfd for MMIO writes so those
notifications bypass the vCPU thread. Interrupt delivery likewise uses resolved
`Interrupt` or `MessageInterruptDomain` handles exposed by the returned
attachment; it is not sent through the vCPU run loop.

`VcpuStop` is a report to the supervisor, not a complete shutdown sequence.
Architecture-specific shutdown triggers such as PSCI system-off, syscon
poweroff, ACPI power button, or backend fatal exits are decoded inside backend
or architecture-substrate code and surfaced as `VcpuStop`. The supervisor owns
the fan-out. It uses `VcpuRunControl::request_exit` to make outstanding
synchronous `Vcpu::run()` calls return, then joins every vCPU worker before
shutting down device hosts. This keeps shutdown policy out of devices and out of
backend-specific vCPU exit types.

`VcpuRunControl` is not cancellation of arbitrary Rust work, reset control, or
device shutdown. It is the backend's VM-wide vCPU run-exit mechanism. Its only
contract is: after `request_exit()` succeeds, every currently running
`Vcpu::run()` call for that machine will return promptly. If a backend cannot
make a blocked vCPU leave `run()`, it cannot implement reliable guest poweroff
for this model.

For Linux/KVM, `VcpuRunControl` is implemented by making each vCPU thread's
`KVM_RUN` ioctl return `-EINTR`. KVM documents two relevant facts:

- `KVM_RUN` returns `EINTR` when an unblocked signal is pending for the vCPU
  thread.
- `struct kvm_run::immediate_exit` is the common signal-kick path; when set
  nonzero before `KVM_RUN`, the next run exits immediately with `EINTR`, and a
  signal handler can set it to keep a kicked vCPU from re-entering the guest.

Therefore the KVM backend must keep per-vCPU run records in the shared machine
run-control state. When a vCPU worker thread enters `Vcpu::run()`, the KVM
`Vcpu` records the current Linux thread identity and the mmap'd `kvm_run`
pointer in that state. `VcpuRunControl::request_exit` sets a VM-wide stop flag,
marks each recorded vCPU's `immediate_exit`, and sends a thread-directed signal
such as `pthread_kill` or `tgkill` to each recorded vCPU thread. The signal is
sent to vCPU worker threads, not to an arbitrary process PID. When `KVM_RUN`
returns `EINTR`, `Vcpu::run()` checks the stop flag and returns
`VcpuStop::Stopped` instead of re-entering KVM.

For macOS/HVF, the run-control state stores each vCPU's `hv_vcpus_exit` handle.
The current local HVF wrapper already exposes this shape as
`force_vcpus_exit(handles: &[VcpuHandle])`, and each vCPU exposes `handle()` as
a sendable handle usable from another thread only to force it out of `run()`.
`VcpuRunControl::request_exit` sets the VM-wide stop flag and calls the
HVF exit helper with all recorded handles. When `hv_vcpu_run` returns,
`Vcpu::run()` checks the stop flag and returns `VcpuStop::Stopped`.

For Windows/WHP, the backend must store the partition handle and each virtual
processor index in the run-control state. `VcpuRunControl::request_exit` sets
the VM-wide stop flag and calls `WHvCancelRunVirtualProcessor(partition,
vp_index, 0)` for each still-running virtual processor. `WHvRunVirtualProcessor`
then returns with `WHvRunVpExitReasonCanceled`; the local WHP code already
imports that exit reason, but the target implementation must add the cancel
binding and translate that exit to `VcpuStop::Stopped` when the stop flag is
set.

### `MmioDevice`

Owned by `dillo-mmio`. Implemented by MMIO devices and by `dillo-pci::PciRoot`.

```rust
pub struct MmioWindow {
    pub base: u64,
    pub size: u64,
}

pub enum MmioInterruptRequirement {
    Line { source: InterruptSource },

    MessageDomain {
        source: MessageInterruptSource,
        vectors: u16,
    },
}

pub struct SharedMemoryRequirement {
    pub range: AddressRange,
    pub access: SharedAccess,
}

pub enum SharedAccess {
    ReadOnly,
    WriteOnly,
    ReadWrite,
}

pub trait MmioDevice: Send + Sync + std::fmt::Debug {
    fn windows(&self) -> &[MmioWindow];
    fn interrupts(&self) -> &[MmioInterruptRequirement];
    fn shared_memory(&self) -> &[SharedMemoryRequirement];
    fn read(&self, window: &MmioWindow, offset: u64, data: &mut [u8]) -> Result<(), MmioError>;
    fn write(&self, window: &MmioWindow, offset: u64, data: &[u8]) -> Result<(), MmioError>;
}
```

The resource methods are DTB-derived device metadata, not hardware-enablement
decisions. `Attach<Arc<dyn MmioDevice>>` registers the declared windows into
machine-owned MMIO routing state, realizes the declared interrupt and
shared-memory requirements, returns a backend-implemented `MmioAttachment`, and
fails closed if any requirement cannot be satisfied. PCI is therefore invisible
to machine backends; a PCI host bridge is just one MMIO device with windows,
shared-memory requirements, and interrupt requirements from their point of view.

The resource slices are borrowed fixed constructor state, not values to allocate
or recompute during attachment. They must be stable for the lifetime of the
device. This is the device's claim over DTB-derived resources, not a negotiation
hook. Requirements do not carry public names. The machine realizes requirements
in slice order, and the attachment exposes resolved handles in the same order. A
device that needs semantic labels for its own windows or interrupts keeps those
labels in its own concrete type. The machine validates that all windows are
nonzero, non-overlapping, outside guest RAM unless the DTB explicitly defines
the aperture, and compatible with the selected backend. MMIO read/write methods
are called only after a successful attachment. If a routed access is malformed
or unsupported, the device returns `Err`; the machine treats that as a VM
execution error rather than silently ignoring the access.

Shared-memory requirements are capabilities, not static shared pages. The DTB
may describe the device's DMA aperture or shared-memory eligibility, but virtio
queue descriptors and buffers are runtime guest protocol state. The target model
flattens shared-memory requirements: one `SharedMemoryRequirement` describes one
range and access mode. A device that has multiple apertures returns multiple
entries. A device gets one shared-memory capability per entry through the
attachment returned by machine `Attach`; each requested runtime range must be
inside that capability and must currently be tracked as shared by the backend.
MMIO windows, PCI ECAM, and BAR apertures are not automatically shared-memory
capabilities.

### `PciDevice`

Owned by `dillo-pci`. Implemented by `dillo-pci-virtio::VirtioPciDevice` and
future PCI endpoint devices.

```rust
pub trait PciDevice: Send + Sync + std::fmt::Debug {
    fn config_read(&self, reg_idx: usize) -> Result<u32, PciError>;
    fn config_write(&self, reg_idx: usize, offset: u64, data: &[u8])
        -> Result<(), PciError>;
    fn bar_regions(&self) -> &[BarRegion];
    fn bar_read(&self, bar_idx: u8, offset: u64, data: &mut [u8])
        -> Result<(), PciError>;
    fn bar_write(&self, bar_idx: u8, offset: u64, data: &[u8]) -> Result<(), PciError>;
}
```

`PciDevice` uses shared references because PCI endpoints sit behind `PciRoot`,
and `PciRoot` is itself an `MmioDevice` whose MMIO callbacks take `&self`.
Mutable endpoint state therefore lives behind endpoint-owned synchronization or
interior mutability. BAR declarations are borrowed fixed constructor state, just
like `MmioDevice` windows.

### `PciRoot`

`PciRoot` is a concrete type owned by `dillo-pci`, not a trait. It implements
`MmioDevice` and attaches `PciDevice`s.

```rust
pub struct PciRootOptions {
    pub ecam: MmioWindow,
    pub slots: u8,
    pub bar_window: AddressRange,
    pub message_interrupts: MessageInterruptSource,
}

impl Attach<Arc<dyn PciDevice>> for PciRoot {
    type Error = PciAttachError;
    type Output = PciFunction;
}
```

One `PciRoot` instance owns all guest MMIO windows for the declared PCI host:
ECAM, BAR windows, and MSI-X table/PBA BARs that belong to attached devices.
`Attach<Arc<dyn PciDevice>>` assigns a DTB-declared PCI slot/function and folds
the endpoint's BARs into the root's PCI MMIO routing. The backend receives only
the single `PciRoot` as an `MmioDevice`. `PciRoot` or `dillo-pci-virtio` adapts
PCI MSI/MSI-X table updates to the generic message-interrupt service; machine
backends never see PCI transport types.

PCI endpoints do not attach directly to `Machine` and do not receive
`MmioAttachment`. `PciRoot` is the machine-facing MMIO device. After
`Machine::attach(PciRoot)` succeeds, the PCI-root host/wrapper uses the returned
`MmioAttachment` to provide the root's message-interrupt domain, shared-memory
capabilities, and notify registrations to the PCI transport adapters it owns.
It also uses that same attachment to spawn the PCI-root device host.

`PciRoot` handles absent bus/device/function routing itself, including standard
all-ones config reads for non-existent functions. Once an access has been routed
to an attached endpoint or BAR, endpoint errors are explicit `PciError`s and
propagate through `PciRoot`'s `MmioDevice` implementation as `MmioError`s.

### `VirtioDevice`

Owned by `dillo-virtio`. Implemented by concrete `dillo-virtio-*` device
crates.

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

`VirtioActivate` contains shared-memory handles, queues, kicks, and resolved
interrupt handles. It contains no whole-guest-memory handle, machine handle, or
OS backend handle.

### `SharedMemory`

Owned by `dillo-mmio`. Implemented by backend crates and exposed only through
`MmioAttachment` as the maximum memory authority an attached device can use.

```rust
pub trait SharedMemory: Send + Sync {
    fn region(&self, range: SharedRange) -> Result<SharedRegion, SharedMemoryError>;
}

pub struct SharedRange {
    pub gpa: u64,
    pub size: u64,
    pub access: SharedAccess,
}

pub struct SharedRegion {
    // opaque
}

impl SharedRegion {
    pub fn read(&self, offset: u64, data: &mut [u8]) -> Result<(), SharedMemoryError>;
    pub fn write(&self, offset: u64, data: &[u8]) -> Result<(), SharedMemoryError>;
}
```

`SharedMemory` is an attachment-scoped capability, not a whole-guest-memory
view. `region()` succeeds only for ranges that are inside the capability's
DTB-derived aperture and that the backend currently tracks as shared. In a
non-confidential VM, the backend may implement `SharedRegion` with ordinary
mapped RAM, but only for ranges reachable through a successful device
attachment. Devices must never receive a handle that can inspect arbitrary
guest-private memory.

### `MmioAttachment`

Trait owned by `dillo-mmio`. Implemented by `dillo-machine-*` backends. Returned
by `Attach<Arc<dyn MmioDevice>>`. Consumed by device-host wrappers and
transports that need backend-neutral services for an already-attached MMIO
device.

```rust
pub enum MmioInterrupt {
    Line(Interrupt),

    MessageDomain(Arc<dyn MessageInterruptDomain>),
}

pub enum MmioDeviceHost {
    Thread(MmioThreadHost),

    Process(MmioProcessHost),
}

pub struct MmioDeviceHandle {
    // opaque
}

impl MmioDeviceHandle {
    pub fn shutdown(&self) -> Result<(), MmioShutdownError>;

    pub fn join(self) -> Result<(), MmioJoinError>;
}

pub struct MmioThreadHost {
    // opaque
}

pub struct MmioProcessHost {
    // opaque
}

pub struct MmioSpawnError {
    // opaque
}

pub struct MmioShutdownError {
    // opaque
}

pub struct MmioJoinError {
    // opaque
}

impl MmioDeviceHost {
    pub fn thread(
        run: impl FnOnce(MmioRunToken) -> Result<(), MmioJoinError> + Send + 'static,
    ) -> Self;

    pub fn process(spec: MmioProcessHost) -> Self;

    pub fn model(&self) -> DeviceModel;
}

pub struct MmioRunToken {
    // opaque
}

impl MmioRunToken {
    pub fn is_shutdown_requested(&self) -> bool;
}

pub trait MmioAttachment: Send + Sync {
    fn interrupts(&self) -> &[MmioInterrupt];

    fn shared_memory(&self) -> &[Arc<dyn SharedMemory>];

    fn register_notify(
        &self,
        event: MmioNotifyEvent,
    ) -> Result<MmioNotifyRegistration, MmioNotifyError>;

    fn spawn(
        self: Arc<Self>,
        host: MmioDeviceHost,
    ) -> Result<MmioDeviceHandle, MmioSpawnError>;
}
```

`interrupts()` returns one resolved interrupt object for each
`MmioInterruptRequirement`, preserving the order exposed by
`MmioDevice::interrupts`. `shared_memory()` likewise preserves the order exposed
by `MmioDevice::shared_memory`.

KVM implements notify registration with ioeventfd so supported MMIO writes wake a
device host without returning through the vCPU thread. HVF/WHP can return
`Unsupported`, and the machine falls back to its internal MMIO routing state for
ordinary MMIO exits. Notify registration is an optional acceleration path.
Interrupt requirements and shared-memory requirements come from
`MmioDevice::interrupts` and `MmioDevice::shared_memory` and must already have
been drained from the DTB by the device constructor.

MMIO attachment is all-or-fail. The machine must not leave windows, interrupts,
notify registrations, or shared-memory capabilities partially installed if any
resource realization step fails.

`MmioAttachment` is the only object in this layer whose implementation is
backend-specific. A KVM implementation may contain eventfds, irqfds, and process
device-host wiring. HVF/WHP implementations may contain in-process thread
channels or direct interrupt handles. Those details are opaque behind the
`dillo-mmio` trait; device crates and `dillo` can use only the trait methods.

`spawn` is on `MmioAttachment` because only the backend attachment knows whether
the selected machine model runs an in-process thread, connects to an
out-of-process host, or uses a backend-specific service. The `self: Arc<Self>`
receiver consumes the attachment handle returned by `Machine::attach`; callers
must extract or clone any resolved interrupt/shared-memory/notify services the
host needs before calling `spawn`.

`MmioDeviceHost` is not the MMIO device and not the backend attachment. It is the
device-host wrapper's launch request for an already-attached device. A thread
host contains a long-lived run loop over the concrete device and attachment
services. A process host cannot be a closure; it describes or connects to an
external long-lived device host using backend-neutral process/channel material.
The wrapper chooses the host variant from `Machine::DEVICE_MODEL`. The selected
backend validates that the variant matches what it supports and fails closed
otherwise.

`spawn` starts the host and returns immediately with an `MmioDeviceHandle`.
Device hosts are expected to run until VM teardown, device removal, backend
failure, or explicit shutdown. Normal operation is not modeled as a synchronous
return from `spawn`. `MmioDeviceHandle::shutdown` requests termination through
backend-owned mechanics; `join` waits for the host to finish and reports host
failure. Dropping the handle must not silently detach registered MMIO routing or
guest-visible device state.

### `InterruptController`

Owned by `dillo-mmio`. Implemented by backend crates and used by
`Attach<Arc<dyn MmioDevice>>` to realize `MmioInterruptRequirement`s.

```rust
pub trait InterruptController: Send + Sync {
    fn line(&self, source: InterruptSource) -> Result<Interrupt, InterruptError>;

    fn message_domain(
        &self,
        source: MessageInterruptSource,
        vectors: u16,
    ) -> Result<Arc<dyn MessageInterruptDomain>, InterruptError>;
}
```

`InterruptSource` and `MessageInterruptSource` are typed facts derived from the
drained devtree and carried by `MmioDevice::interrupts`. They are not raw x86
GSIs, raw ARM SPIs, KVM irqfds, WHP vectors, or HVF handles.

### `Interrupt`

Owned by `dillo-mmio`. Constructed by machine backends through
`InterruptController::line`. Consumed by transports and simple MMIO devices
such as UART.

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
that cannot represent deassert returns `UnsupportedDeassert`; devices or
transports that need deassert must fail closed on that backend.

### `MessageInterruptDomain`

Owned by `dillo-mmio`. Implemented by backend crates. Consumed by `dillo-pci`
through a PCI-owned adapter.

```rust
pub struct MessageInterrupt {
    pub address: u64,
    pub data: u32,
    pub masked: bool,
}

pub trait MessageInterruptDomain: Send + Sync {
    fn update(&self, vector: u16, msg: MessageInterrupt) -> Result<(), InterruptError>;
    fn enabled(&self, enabled: bool) -> Result<(), InterruptError>;
    fn interrupt(&self, vector: u16) -> Option<Interrupt>;
}
```

PCI MSI/MSI-X is one producer of message interrupts, not a machine trait
concept. `dillo-pci` owns the adapter from PCI table/config writes to
`MessageInterruptDomain`. Today the implementation shape is exposed through
`vm-pci::MsixNotifier`; the target design confines that name to PCI helper code
or upstreamed PCI code.

## Process vs thread model

The design constraints are:

- process/thread hosting must not make backend crates depend on virtio or
  concrete devices;
- concrete devices must not branch on `target_os`;
- `dillo` may select a host wrapper based on `Machine::DEVICE_MODEL`;
- Linux/KVM should remain able to use a process/vhost-user model;
- HVF/WHP should remain able to use an in-process thread model;
- event/interrupt acceleration such as KVM ioeventfd/irqfd belongs to
  attachment services and device-host wrappers, not to a public vCPU dispatch
  callback.

The device-aware wrapper lives in a future device-host crate or a `dillo` module.
The process/thread launch primitive lives in `MmioAttachment`, implemented by
the selected `dillo-machine-*` crate.

## Source cfg policy

Allowed cfg locations:

- Cargo target dependencies selecting the backend package behind a generic
  dependency name;
- backend crates internally, for host OS and architecture details;
- architecture substrate crates internally;
- tests that require a specific host API.

Forbidden cfg locations:

- `dillo` choosing KVM/HVF/WHP APIs in source;
- concrete device crates changing behavior by `target_os`;
- backend crates depending on `dillo-pci`, `dillo-pci-virtio`,
  `dillo-mmio-virtio`, `dillo-mmio-uart`, `dillo-virtio`, or concrete device
  crates;
- transport/device crates importing OS hypervisor APIs.

Architecture variation is expressed through DTB facts and backend or substrate
internals. `dillo` may branch on parsed PMI machine kind or surveyed DTB facts,
but it must not use architecture cfg to decide what guest hardware exists.

## Rust-vmm and upstreaming

Current workspace evidence:

- `vm-pci` contains Firecracker-derived code and may be a candidate for cleanup
  or upstream discussion.
- `virtio`, `virtio-pci`, `vm-pci`, and `vhost-backend` were migrated from an
  earlier local dillo tree.
- the workspace uses rust-vmm crates such as `vm-memory`, `vmm-sys-util`,
  `kvm-ioctls`, `kvm-bindings`, `vhost`, `vhost-user-backend`, and
  `virtio-queue`.

These names are current-state evidence only. The target design should not
preserve a local crate solely because it exists today.

The split should keep rust-vmm-like reusable pieces small and dependency-light
so upstreaming is possible later. Reusable protocol crates should not depend on
dillo application policy, PMI, DTB, or machine backend crates.

## Acceptance criteria

This design is satisfied only when all of the following can be verified:

1. `dillo/src` contains no imports or references to KVM, HVF, WHP, or backend
   crate package names other than the generic target-selected backend alias.
2. Backend crates depend only on `dillo-machine`, `dillo-mmio`, optional
   architecture substrate crates, and host OS hypervisor APIs.
3. Device crates do not depend on `dillo-machine`, backend crates, PMI, or DTB.
4. `Attach<T>` is owned by `dillo-mmio`, not `dillo-machine`, and is used by
   both machine backends and `dillo-pci::PciRoot`.
5. `Machine` exposes no PCI, MSI-X, UART, IOAPIC, GIC, KVM irqfd, WHP vector,
   HVF, raw host handles, or hypervisor API objects.
6. `Machine` has no trait constructor; backend crates expose inherent
   constructors on concrete machine types.
7. `Machine` has no universal `VcpuConfig`; `Machine::Cpu` and
   `Machine::Memory` are associated per machine family and are produced by
   dillo-owned devtree consumption for the selected concrete backend.
   Unsupported PMI/DTB CPU or memory requests fail closed.
8. `Machine` exposes no whole-guest-memory accessor; guest memory is accessible
   only through attachment-scoped `SharedRegion` handles minted by successful
   device registration.
9. `dillo-pci` owns `PciRoot` and `PciDevice`; `PciRoot` implements
   `MmioDevice`.
10. `dillo-pci-virtio` owns `VirtioPciDevice`, the adapter from `VirtioDevice`
   to `PciDevice`.
11. `dillo-mmio-virtio` owns the adapter from `VirtioDevice` to `MmioDevice`.
12. `Attach<Arc<dyn MmioDevice>>` realizes every interrupt and shared-memory
   capability requirement advertised by `MmioDevice::interrupts` and
   `MmioDevice::shared_memory` or fails
   closed without partial attachment.
13. `MmioDevice` has no attach/init callback; machine attachment returns an
   `Arc<dyn MmioAttachment>` implemented by the selected backend.
14. Routed MMIO and PCI endpoint accesses return typed errors; boolean
   "unhandled after routing" is not part of the target device API.
15. Shared memory is exposed only as attachment-scoped capabilities whose
   runtime regions must be inside a DTB-derived aperture and currently tracked
   shared by the backend.
16. `dillo` owns devtree consumption glue, including local `FromDevTree` impls
   for all concrete devices and transports over `&mut devtree::OwnedTree`.
   `Ok(None)` consumes nothing and means the relevant node is absent.
17. Every DTB node/property is drained by exactly one owner, or launch fails.
18. Local verification passes:
   - `cargo fmt --all -- --check`
   - `CARGO_BUILD_RUSTFLAGS='-D warnings' cargo test --workspace` on Linux
   - `CARGO_BUILD_RUSTFLAGS='-D warnings' cargo test --workspace` on Windows
     and macOS, with only genuinely platform-incompatible current crates
     excluded until the split removes or relocates them
   - target checks for `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`, and `aarch64-apple-darwin`
15. CI passes all three supported platform lanes, including real boot tests.

## Current-state gaps

The current tree does not yet meet this design:

- `dillo-vm` is still a monolith containing launch orchestration, backend
  adapters, MMIO bus, PCI root, UART, syscon, IOAPIC, PSCI, and backend device
  glue.
- `dillo-hypervisor` is one cfg-selected crate rather than separate
  `dillo-machine-kvm`, `dillo-machine-hvf`, and `dillo-machine-whp` crates.
- `MmioDevice` and `PciDevice` are `pub(crate)` inside `dillo-vm`.
- `PciDevice` is not yet in a standalone `dillo-pci` crate.
- `VirtioPciDevice` still lives in `virtio-pci`; final naming and upstreaming
  need to decide whether that crate becomes `dillo-pci-virtio` or remains a
  rust-vmm-style transport crate with a different boundary.
- `virtio-pci::QueueNotifier` is transport-owned and virtio-shaped today; the
  final design needs backend-neutral I/O event registration in `dillo-mmio`.
- `dillo-vm` and current virtio activation paths still pass whole guest-memory
  handles; the target design must replace those with attachment-scoped
  `SharedRegion` handles.
- `dillo-device` contains an older process/thread abstraction that is not yet
  the active `VirtioDevice` activation contract.

These gaps are evidence that the target design is not merely a crate move. The
split requires the trait surfaces above before crates can be separated without
preserving backend knowledge in the wrong layer.
