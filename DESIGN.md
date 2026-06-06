# dillo device architecture

> Status: **design / target architecture.** This describes where the dillo VMM
> code structure is headed, not (yet) what is fully implemented. It is the
> contract that should make all device-attach code architecture- and
> OS-independent. See "Mapping to today's code" for what already exists, and
> `DESIGN-REVIEW.md` for the research and `file:line` evidence behind the
> decisions recorded here.

## 1. Goal & principles

dillo runs the same guest device model (the arma device model — see
`arma/docs/device-model.md`) on three host hypervisors (KVM/Linux, HVF/macOS,
WHP/Windows) across two arches (x86-64, aarch64). The goal of this architecture
is that **device-attach code is written once, OS- and arch-neutral**, and only a
thin backend layer is host-specific.

Principles:

1. **Backend isolation.** No KVM/HVF/WHP-specific code leaks above the `Vm`
   trait. Each backend is one implementation of that trait, selected at compile
   time for the target OS. Everything arch-specific — GIC vs IOAPIC, `set_spi`
   vs irqfd vs fixed-interrupt, PSCI vs INIT-SIPI-SIPI, the x86 `0xcf8`/`0xcfc`
   config decoder — lives *below* this line.
2. **No runtime polymorphism in the hot path.** "Trait" here means a
   *compile-time seam*: exactly one backend is compiled per build, so the `Vm`
   trait is implemented once and dispatched statically (monomorphized / concrete
   type). The trait is the abstraction boundary for *source*, not a `dyn` object
   in the vCPU run loop. Devices may be `dyn` (cold attach path); the VM is not.
3. **One device model, two transports.** Every guest device reaches the guest
   over either **MMIO** or **PCI**. Both are expressed through the trait stack
   below; no device knows which host it runs on. The device model never mentions
   port I/O — PIO is an x86-backend implementation detail (§5).
4. **One process/threading model.** The supervisor owns a VM and N devices.
   Devices run as separate **processes** on Linux (isolation via vhost-user) and
   as **threads** everywhere else, behind the same device trait.

**Current backend×arch reality.** The design targets three hypervisors × two
arches, but today **each backend implements a single arch's substrate**: KVM →
x86-64, HVF → aarch64, WHP → x86-64. The aarch64-KVM and aarch64-WHP cells are
not yet built (every aarch64 mechanism — GIC, PSCI, `set_spi`, `send_msi`,
GICv2m — currently lives only in the HVF backend). The `Vm` trait is written so a
backend can add the missing arch without changing anything above it.
(Note: `PLATFORMS.md` lists "Linux aarch64 / KVM / implemented"; the KVM backend
in this tree has no aarch64 path — this needs reconciling.)

## 2. The trait stack

Four traits, layered top-to-bottom. Each layer depends only on the trait below
it, never on a concrete backend.

```
            VirtioDevice                (transport-agnostic device logic)
            /          \
   VirtioMmio        VirtioPci          (transport adapters)
       |                  |
   MmioDevice         PciDevice         (bus-attach traits)
       |                  |
       |              PciRoot           (PciRoot is itself an MmioDevice)
       |             /
        MmioDevice                      (everything that attaches to the VM)
            |
           Vm                           (the backend: KVM / HVF / WHP)
```

- A **`VirtioDevice`** is wrapped by exactly one adapter to become either a
  `PciDevice` (via `VirtioPci`) or an `MmioDevice` (via `VirtioMmio`).
- A **`PciDevice`** attaches to the **`PciRoot`**.
- The **`PciRoot`** is *itself* an **`MmioDevice`** (its ECAM window), so it
  attaches to the VM the same way any MMIO device does; its `read`/`write` decode
  ECAM accesses and fan out to the right `PciDevice`.
- An **`MmioDevice`** attaches directly to the **`Vm`**.

The result: `Vm` sees only `MmioDevice`s. PCI is "just" a particular MMIO device
(`PciRoot`) that fans out to `PciDevice`s. Virtio is "just" a `VirtioDevice`
behind one of two adapters. Attach code is uniform and host-neutral.

## 3. `Vm` — the backend trait

One implementation per host (`kvm.rs` / `hvf.rs` / `whp.rs`), `#[cfg]`-selected.
Nothing host-specific exists above it. Responsibilities:

- **Construction.** vCPU count, the RAM placement plan, the DTB-derived
  *substrate*, and the address-space watermark are all constructor inputs, in one
  `VmOptions` value so the backend can enforce its own ordering invariants
  internally (KVM must create the in-kernel irqchip before any vCPU; HVF must
  configure the GIC before any vCPU; WHP must set partition properties before
  setup). Callers never see that ordering.
- **DTB substrate.** The VM claims the nodes that are *its own* — the platform
  substrate the backend implements: the interrupt controller (and its
  interrupt-encoding), timer, and power/reset. It leaves device nodes (serial,
  virtio-mmio slots, the PCIe bridge) for the device layer to claim. Ownership is
  realized by the **`survey` / `ResourcePlan`** machinery (§6 of the review):
  every node is claimed by exactly one owner (total-coverage invariant) and every
  region is tagged `SubstrateMmio` vs `Mmio`/`EcamWindow`/`BarWindow`, with one
  shared plan re-aggregating all regions for RAM placement.
- **Guest memory.** The VM owns the backend memslot mappings *and* vends a
  `GuestMemoryMmap` DMA view back out (`guest_memory()`) — device backends clone
  it to walk descriptor rings. (There are genuinely two representations: what the
  guest sees, and what devices DMA through; they must stay coherent.)
- **Attach MMIO devices.** A method to attach an `MmioDevice` at its
  DTB-declared window. This is the *only* attach primitive the VM exposes;
  PCI and virtio reach the guest through it.
- **Interrupts.** Vend a wired `Interrupt` for a device's DTB interrupt source,
  and an `MsixNotifier` for MSI (§7) — the one place the backend's injection
  mechanism is abstracted.
- **vCPUs & run.** Vend per-thread vCPU seeds (vCPUs may be thread-bound and
  non-`Send` — HVF — so each vCPU thread mints its own from a `Send` seed). The
  **supervisor** (§8), not the `Vm`, owns thread spawning and the per-vCPU run
  loop; a run returns a `RunOutcome` so an in-place warm reboot (HVF) reuses the
  same VM rather than consuming it.

Sketch (illustrative, not final):

```rust
struct VmOptions {
    vcpus: u32,
    memory: MemoryPlan,          // RAM placement: memslots + /memory nodes
    substrate: Substrate,        // DTB-claimed intc/timer/power + IRQ namespace
    min_addr_space_bits: u32,    // address-space watermark (F7)
}

enum RunOutcome { Exit(i32), Reboot }

trait Vm: Sized {
    /// Backend vCPU. May be `!Send` and is never `Clone` (HVF binds it to its
    /// creating thread). A `VcpuSeed` is the `Send` factory each vCPU thread uses
    /// to mint its own `Vcpu`; on KVM/WHP it wraps a pre-created vCPU.
    type Vcpu;
    type VcpuSeed: Send;

    fn new(opts: VmOptions) -> Result<Self, Error> where Self: Sized;

    fn guest_memory(&self) -> GuestMemoryMmap;          // device-facing DMA view
    fn attach_mmio(&mut self, dev: Box<dyn MmioDevice>) -> Result<(), Error>;

    /// Resolve a device node's interrupt against the controller the VM claimed
    /// (decoding `interrupts` per the parent's `#interrupt-cells`) and return a
    /// wired `Interrupt` handle (§7). The device never sees a GSI/SPI/pin.
    fn wired_irq(&self, src: &IrqSource) -> Result<Interrupt, Error>;

    /// MSI routing for a PCI device; the guest drives it lazily (§7).
    fn msi_notifier(&self, vectors: u16) -> Result<Arc<dyn MsixNotifier>, Error>;

    fn vcpu_seeds(&self) -> Result<Vec<Self::VcpuSeed>, Error>;
}
```

The KVM-only `vm_fd` (needed for in-kernel irqfd/ioeventfd registration) stays a
**backend-private** detail: the backend uses it inside `wired_irq` /
`msi_notifier` / the activate notify-hook so it never appears above the trait.

## 4. `MmioDevice` — the universal attach trait

Everything the guest reaches over MMIO is an `MmioDevice`. It owns its
guest-physical window (base/size, from the DTB node it claimed) and answers
reads/writes. The MMIO dispatcher is built once at startup, frozen, and shared as
`Arc` with no outer lock — so the trait is **`Send + Sync`** and uses `&self`
with **interior mutability** (every real device is stateful).

```rust
trait MmioDevice: Send + Sync {
    fn window(&self) -> MmioWindow;                 // base + size (DTB-derived)
    fn read(&self, offset: u64, data: &mut [u8]) -> bool;
    fn write(&self, offset: u64, data: &[u8]) -> bool;
}
```

The VM's MMIO dispatcher routes a guest access to the owning device by window
(returning `false` ⇒ unclaimed ⇒ zero-fill on read). Nothing here is host- or
arch-specific.

The existing macOS `VirtioMmio` transport is the reference shape — it is already
`read(&self)/write(&self) -> bool` over window-relative offsets behind a
`Mutex<Inner>`. Three existing pieces must be reshaped to this trait, and they
fall on **two sides of the internal/external line**:

- **The serial UART** (`ns16550a`) is a **fully external device, not Vm
  substrate.** The device layer claims its `serial@` node (tagged
  `RegionKind::Mmio`, like virtio-mmio — not `SubstrateMmio`) and attaches it with
  `attach_mmio`. It is the simplest device in the model: it needs **nothing from
  the Vm but a single wired `Interrupt`** (from `wired_irq`) — no `guest_memory()`,
  no kicks, no DMA, no role in routing or lifecycle. Today it is process-global
  state (`OnceLock<Mutex<…>>`) with free-function read/write and three per-OS
  `init` signatures that bake interrupt delivery into a per-OS trigger; it becomes
  one struct owning its `Serial` register model + its own output sink (and an
  optional input thread), taking an injected `Interrupt`, `signal()`-ing on
  THR-empty / data-ready. (Contrast the virtio-console: also external, but it
  *additionally* needs the guest-memory view and per-queue kicks.)
- **The userspace IOAPIC and the x86 syscon poweroff/reset** are, by contrast,
  **Vm-owned substrate** *realized as attached `MmioDevice`s* (§6). The VM claims
  the node and owns the semantics — interrupt routing for the IOAPIC,
  `process::exit`/reboot for syscon — because `wired_irq` and `RunOutcome` depend
  on them; it just attaches the register model the same way as any device. Serial
  depends on neither, which is why it stays external.

## 5. `PciDevice` and `PciRoot`

- **`PciRoot`** models the device-model's PCIe host bridge. It exposes a
  **transport-neutral config accessor** (`config_read(bdf, reg)` /
  `config_write(bdf, reg, …)`) plus the set of attached `PciDevice`s and their
  BAR windows. **`PciRoot` implements `MmioDevice` for its ECAM window**, decoding
  ECAM offsets into `(bdf, reg)` and calling its own config accessor — so it
  attaches to the VM like anything else. ECAM is the *only* config path the
  device model knows, and the *sole* config path on aarch64.
- **`PciDevice`** is the per-slot trait: config-space read/write, BAR regions,
  and BAR read/write. (`config_read`/`config_write`/`bar_regions`/`bar_read`/
  `bar_write` already exist.) MSI-X is **not** a `PciDevice` method — its table
  and PBA live in BAR space and are handled through `bar_read`/`bar_write` plus a
  `config_write` interception, driving the `MsixNotifier` (§7).

**Port I/O is not in the device model.** On x86 the kernel also reaches base
config (the first 256 bytes) through the architectural `0xcf8`/`0xcfc` ports.
These are *not* a device and *not* a separate config space — they are a legacy
**second decoder onto the exact same config space ECAM reaches** (ports = base
config, ECAM = extended; both must return identical bytes). They are handled
**below** the `Vm` trait: the x86 backend/supervisor recognizes the two ports on
a raw PIO exit and calls the same `PciRoot` config accessor ECAM uses. Nothing in
`MmioDevice`/`PciDevice`/`PciRoot`/`VirtioDevice` mentions PIO; the model stays
fully arch-neutral. (PIO is used for nothing else — x86 serial is MMIO.)

This makes "PCI support" a single MMIO device plus a slot trait — no special
casing in the VM, and no x86-ism in the device model.

## 6. Virtio — one device, two adapters

The top layer. A **`VirtioDevice`** is transport-agnostic: it knows queues,
features, config space, and activation — never how it is wired to the guest.

```rust
trait VirtioDevice: Send {
    fn device_type(&self) -> u32;
    fn num_queues(&self) -> usize;
    fn queue_max_sizes(&self) -> &[u16];
    fn features(&self) -> u64;
    fn activate(&mut self, mem: GuestMemoryMmap, queues: Vec<Queue>, kicks: Vec<Kick>)
        -> Result<(), ActivateError>;
    fn read_config(&self, offset: u64, data: &mut [u8]);
    fn write_config(&mut self, offset: u64, data: &[u8]);
}
```

Two adapters wrap a `VirtioDevice` and give it a transport:

- **`VirtioMmio`** wraps a `VirtioDevice` and **implements `MmioDevice`** — the
  virtio-mmio v2 register file at a `virtio_mmio@…` window, with a single wired
  interrupt line. It attaches directly to the `Vm`.
- **`VirtioPci`** wraps a `VirtioDevice` and **implements `PciDevice`** — the
  virtio-pci transport (config + BARs + MSI-X). It attaches to the `PciRoot`.

Because the adapters terminate at `MmioDevice`/`PciDevice`, the same
`VirtioDevice` (e.g. the console) runs over either transport on any host with no
device-specific changes. Which transport a guest gets is decided at image-build
time by arma (`--pci-slots` / `--mmio-slots`); dillo honors whatever the PMI
declares.

Three details the adapters must get right:

- **The device is held as `Arc<Mutex<Box<dyn VirtioDevice>>>`** (load-bearing:
  lets the console be swapped in place on soft-reconnect), not a bare `Box`.
- **Interrupt resolution direction.** A device must not pull its own interrupt by
  vector. The adapter resolves each queue's MSI-X vector (or the single wired
  line) into a concrete `Interrupt` and passes the resolved interrupts into
  `activate` — so the device never holds a backend lookup closure.
- **Notify wiring is a backend hook.** Queue-notify delivery (KVM ioeventfd vs
  in-process kick) lives behind the adapter, not `cfg`-gated into the device or
  transport. The KVM `vm_fd`/ioeventfd machinery must not appear in the
  transport struct.

## 7. Interrupts

This is the seam that most needs care, because injection is the most
host-specific operation. There are **two genuinely different mechanisms**.

### Wired interrupts → `Interrupt`

A `VirtioMmio`/serial device asserts one line. The abstraction is the existing
**`Interrupt`** handle — *signal/assert only, no `bool`*. The VM vends it from a
device's DTB interrupt source via `wired_irq`, which is where the node's
`interrupts` cells are **decoded against the controller the VM claimed**
(`#interrupt-cells` = 2 for the x86 IOAPIC `<pin, sense>`, = 3 for the GIC
`<type, number, flags>`). The device passes its source; the VM owns the encoding,
the controller identity, and the SPI/pin namespace. (Today this decoding is a
buggy position heuristic that discards `interrupt-parent` — moving it into the
substrate owner fixes that.)

`Interrupt::signal()` means "assert/pulse." **Level, ack, and deassert are owned
by the transport, not the line**: the virtio-mmio `INTERRUPT_STATUS`/
`INTERRUPT_ACK` registers and a shared status word drive the deassert on a level
backend; edge backends have nothing to deassert. This is why the handle is
assert-only rather than `Fn(bool)`.

| backend | wired assert | wired deassert |
| --- | --- | --- |
| KVM (x86) | irqfd `write(1)` at the DTB GSI (edge; in-kernel IOAPIC route) | none |
| WHP (x86) | userspace IOAPIC redirection lookup → `request_fixed_interrupt` (edge) | none |
| HVF (arm) | `set_spi(intid, true)` on the device worker thread (level) | `set_spi(intid, false)` on the vCPU thread when status hits 0 |

On WHP the wired `Interrupt` closes over the userspace IOAPIC (an attached
`MmioDevice`) whose redirection table the guest programs — so `wired_irq` depends
on that substrate device having been attached.

**Self-leveling devices (the serial exception).** "Level/ack lives in the
transport" holds for virtio-mmio, whose `INTERRUPT_STATUS`/`INTERRUPT_ACK`
registers drive the deassert — so its line can be assert-only. The serial 16550 is
the exception: it has **no external ack register**; it owns its own interrupt
state (IIR / THR-empty / data-ready, cleared by guest reads), so the *device
itself* is the level owner. On edge backends (KVM, WHP) assert-only `signal()`
still suffices. On a level backend the line is declared `LEVEL_HIGH` in the DTB,
so a correctly-interrupting 16550 needs to **deassert** when its register logic
clears — which assert-only `signal()` cannot express. The wired-line abstraction
must therefore admit an *optional deassert* for devices that own their own level
(otherwise such a device stays polled, as HVF serial does today). This is the one
spot where an external device needs more than `signal()`.

### MSI (PCI) interrupts → `MsixNotifier`

`VirtioPci` raises MSI-X vectors through the existing **`MsixNotifier`**
(`Send + Sync`; `vector_updated` + `msix_enabled`), vended by `msi_notifier`.
Unlike wired lines, **MSI vectors are minted lazily by the guest**: programming
an MSI-X table entry (a BAR2 write) is what allocates routing (KVM: a GSI +
irqfd; HVF: a GICv2m send; WHP: a decoded fixed-interrupt). MSI therefore cannot
be folded into the one-shot `wired_irq` path.

| backend | MSI delivery |
| --- | --- |
| KVM (x86) | irqfd + MSI route; GSI allocated lazily on table write |
| WHP (x86) | `request_fixed_interrupt` decoded from the MSI-X address/data (edge) |
| HVF (arm) | `hv_gic_send_msi(addr, intid)` via the GICv2m frame |

Devices consume both mechanisms transport-appropriately and never name a backend.

## 8. Execution model: supervisor ⇒ { vm, device₀ … deviceₙ }

The supervisor owns one VM and N device backends, **and the run loop**: it spawns
one thread per vCPU (each thread mints its `Vcpu` from a `VcpuSeed`), runs the
per-vCPU `run → VmExit` loop, and dispatches exits against the frozen MMIO bus
(plus, on x86, the architectural config ports → `PciRoot`). Shutdown and in-place
warm reboot surface uniformly as `RunOutcome { Exit, Reboot }` rather than ad-hoc
`process::exit`. The device trait boundary is the same regardless of how a device
actually runs:

- **Linux — process model.** Each device runs in a **separate process** for
  isolation (seccomp, fault containment). In-VM it is represented by a **proxy
  that implements the device trait** and forwards the data plane over vhost-user
  (shared guest memory + per-queue kick/call eventfds). From the VM's side the
  proxy *is* an ordinary `MmioDevice`/`PciDevice`; the process boundary is
  invisible above the trait. (The vhost-user frontend already implements
  `VirtioDevice` today.)
- **macOS / Windows — threading model.** Each device runs **in-process on its
  own thread**, implementing the same trait directly.

`activate` (not attach) is the device's data-plane hook: it receives the guest
memory view, the queues, the kicks, and the resolved interrupts — shared by the
in-process device and the vhost proxy alike.

**PSCI / CPU bringup is a backend choice, not a device.** It is neither MMIO nor
an interrupt: on KVM (x86) bringup is the architectural INIT-SIPI-SIPI; on HVF it
is a **userspace** handler in the run loop, decoding `VmExit::Hvc` and using
per-vCPU register access plus a per-vCPU mailbox to power secondaries on and to
drive `SYSTEM_OFF`/`SYSTEM_RESET` (the latter into the warm-reboot `RunOutcome`).
The trait exposes per-vCPU register access on `Vcpu` so the supervisor can host
this where the backend doesn't do it in-kernel.

## 9. Why this is coherent

- **Dependency inversion is clean and acyclic:** `Vm ← MmioDevice ← {PciRoot ←
  PciDevice}` and `VirtioDevice → {VirtioMmio: MmioDevice, VirtioPci:
  PciDevice}`. No layer references a backend.
- **PCI-as-an-MMIO-device** removes PCI special-casing from the VM: the VM only
  attaches `MmioDevice`s, and the x86 config ports stay below the trait as a
  second decoder onto the same config accessor.
- **Transport adapters** mean device logic is written once and is
  transport- and host-neutral — the property that makes attach code
  architecture/OS-independent.
- **Two honest interrupt seams** (`Interrupt` for wired, `MsixNotifier` for MSI)
  match how the three backends actually inject, instead of forcing one closure
  shape onto edge and level hosts.
- **The compile-time `Vm` seam** delivers OS-independence without the
  runtime-polymorphism cost dillo deliberately avoids.
- **The proxy pattern** lets the Linux process model and the
  thread-everywhere model share one device trait.

## 10. Mapping to today's code

| Layer / piece            | Today                                                                 | Remaining work |
| ------------------------ | --------------------------------------------------------------------- | -------------- |
| `VirtioDevice`           | trait, transport-agnostic (`virtio/src/device.rs`)                    | add `num_queues` to the sketch; pass resolved interrupts into `activate` |
| `VirtioPci` adapter      | `VirtioPciAdapter: PciDevice` (`dillo-vm/src/pci.rs`) with backend-owned queue notification | none known |
| `PciDevice`              | trait (`dillo-vm/src/pci.rs`)                                          | none known |
| `PciRoot`                | owns ECAM plus BAR windows and implements `MmioDevice`                 | none known |
| `0xcf8`/`0xcfc`          | `pio_pci.rs` + `LegacyPciState`, dispatched in the x86 vCPU loop       | keep below the `Vm` trait as a second decoder onto `PciRoot`; not in the model |
| `VirtioMmio` adapter     | `virtio_mmio::VirtioMmio: MmioDevice`; used on the macOS path          | make cross-platform when dillo plugs virtio-mmio devices on x86 |
| `MmioDevice`             | `Send + Sync` trait with owned windows and `&self` read/write          | none known |
| serial UART              | `uart::Ns16550: MmioDevice`, attached from DTB-derived UART nodes      | none known |
| IOAPIC / x86 syscon      | Vm-owned substrate realized as attached `MmioDevice`s                  | none known |
| `Vm`                     | single `BackendVm` compile-time trait shape (`dillo-vm/src/backend.rs`) with associated backend types for options, vCPU, interrupt state, IRQ handles, and MSI notifier; per-target impls are cfg-selected | split optional backend capabilities before moving this into `dillo-core` |
| Interrupts               | backend-owned irqfd / WHP fixed interrupt / HVF SPI plus MSI notifiers | finish replacing raw GSI/SPI plumbing with resolved interrupt handles |
| DTB ownership            | run paths use `Machine::survey`/`ResourcePlan`; stale `extract -> Platform` adapters removed | retire legacy `Platform` extractor when remaining tests/users no longer need it |
| Run loop / PSCI          | supervisor-owned loops return `RunOutcome`; HVF warm-reboot is preserved | implement x86 warm reboot |
| Process/thread model     | vhost-user proxy on Linux; in-process threads on macOS/Windows         | keep; express both behind the device trait |

## 11. Open questions to tease out

1. **`vm_fd` escape hatch.** The cleanest boundary keeps the KVM `VmFd` backend-
   private, used only inside `wired_irq`/`msi_notifier`/the activate notify-hook.
   Confirm no remaining caller above the trait needs it (the irqfd/ioeventfd
   registration that today reaches for `vm_fd_arc()`).
2. **vCPU exit dispatch contract.** Reads are serviced by closures *inside*
   `Vcpu::run` (and on WHP literally during instruction emulation), writes
   post-return against the frozen MMIO bus. Pin down the dispatcher contract so a
   device `read` running inside the backend `run()` call is well-defined on all
   three backends.
3. **Device lifecycle across the process boundary** — activation, memory
   sharing, and shutdown ordering for Linux child-process devices behind the
   trait.
4. **macOS coverage** — macOS is not in CI and cannot be compiled in this
   environment; this layer needs a real-hardware (or bare-metal runner) build
   path before its backend impl can be trusted.
5. **aarch64 second cells** — bringing up aarch64-KVM (and aarch64-WHP) to make
   the backend×arch matrix real, and reconciling the `PLATFORMS.md` claim with
   the code (§1).
6. **Wired-line deassert for self-leveling devices** (§7) — whether to give the
   wired `Interrupt` an optional deassert path so an external 16550 can interrupt
   correctly on a level backend (HVF), or to keep serial polled there. Affects the
   `Interrupt` type shape, so decide before the UART rebuild.

### Resolved (recorded in the body above, see `DESIGN-REVIEW.md` for evidence)

- Interrupt abstraction — wired `Interrupt` + `MsixNotifier`, not `IrqLine(Fn(bool))` (§7).
- DTB node ownership split — via `survey`/`ResourcePlan`; IOAPIC and x86 syscon are
  Vm-owned substrate realized as attached `MmioDevice`s; `interrupt-parent` linkage
  is recorded and resolved by the Vm, not discarded (§3, §6, §7).
- PCI port I/O — kept out of the device model as a backend-internal decoder (§5).
- `run(self)` — replaced by a supervisor-owned loop returning `RunOutcome` (§8).
