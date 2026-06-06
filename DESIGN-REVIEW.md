# DESIGN.md review — proposed changes

> Research-backed review of `DESIGN.md`. The rough shape (four-trait stack,
> compile-time `Vm` seam, PCI-as-an-MMIO-device, virtio-as-one-device-two-adapters,
> proxy-behind-the-trait) holds up. The **trait sketches are naive** and need
> revision in seven concrete places. Every claim below is grounded in the current
> code with `file:line` citations.

## 0. Ground truth that reshapes the whole design

Three facts learned from reading all three backends change how the traits must
be drawn. State them up front in DESIGN.md §1 so the rest follows.

1. **The arch×backend matrix is not 3×2 today; it is three disjoint cells.**
   - KVM (`kvm.rs`) is **x86-only in this tree**: no GIC creation, no
     `KVM_ARM_VCPU_INIT`, no `set_aarch64_state`, no in-kernel PSCI; `create_vcpu`
     discards `cpu_profile` on non-x86 (`kvm.rs:199-202`). PLATFORMS.md claims
     "Linux aarch64 / KVM / implemented" — the code does not bear that out.
   - HVF (`hvf.rs`) is **aarch64-only** (no x86 VMX path; module header `hvf.rs:1`).
   - WHP (`whp.rs`) is **x86-only in practice** (`set_x86_64_state`, `WHvX64Register*`,
     PC IOAPIC/MSI/PIO); "Windows aarch64 builds" (PLATFORMS.md) means the type
     compiles, not that a guest model exists.

   Every aarch64-specific mechanism the design discusses (GIC, PSCI, `set_spi`,
   `send_msi`, GICv2m) currently lives **only on HVF**; every x86 mechanism
   (in-kernel/userspace IOAPIC, LAPIC, PIO config, syscon) lives on KVM **or** WHP.
   The `Vm` trait must therefore be honest that *each backend implements one arch's
   substrate*, not that any backend spans both. (DESIGN §1 implies otherwise.)

2. **`run(self)` is wrong.** No backend "runs the VM" by consuming it. Every
   backend spawns **N OS threads, each owning one `Vcpu`** (`lib.rs:1810-1819`
   KVM, `lib.rs:269-278` WHP, `lib.rs:1065-1094` HVF `thread::scope`). `Vcpu` is
   **not `Clone`** and is **moved** into its thread. The real run primitive is
   `Vcpu::run(&mut self, pio_read, mmio_read) -> VmExit` called in a loop
   (`kvm.rs:371`, `whp.rs:473`, `hvf.rs:300`). On HVF the VM is **reused across a
   warm-reboot loop** (`lib.rs:858-877`): `reset_gic()` between runs, `drop(vm)`
   only at final exit — a consuming `run(self)` cannot express this.

3. **The interrupt seam is `Interrupt` (a fire-once/assert handle), not
   `Fn(bool)`.** The existing abstraction (`virtio/src/interrupt.rs`) is
   `Interrupt(EventFd)` on Linux (irqfd, consumed directly by vhost
   `set_vring_call`, `vhost_frontend.rs:201`) and `Interrupt(Arc<dyn Fn()>)`
   elsewhere (`from_fn`, calls `hv_gic_send_msi`). It has **`signal()`, no bool**.
   Level/deassert/ack lives in the **transport**, not in a per-line closure
   (see §3 below).

---

## 1. `Vm` trait (DESIGN §3) — constructor, run model, memory

### Problems
- `fn new(opts: VmOptions /* vcpus, memory, base DTB */)` collapses three
  backends with **incompatible constructors and load-bearing ordering**:
  - KVM `Vm::new()` takes **no args**; the in-kernel irqchip is created inside
    `new()` and **must precede any vCPU** (`kvm.rs:108-123`); memory is added
    *after* via an `add_memslot` loop (`lib.rs:1497-1514`); the IRQ layer is built
    from `vm_fd_arc()` (`lib.rs:1568`).
  - HVF `Vm::new(&GicParams, min_addr_space_bits)` configures the **GIC before any
    vCPU** (`hvf.rs:82-103`); the real VM+GIC is a **process-global singleton**,
    and `hvf::Vm` owns only memory regions; it is **`!Send`** and stays on the
    main thread.
  - WHP `new_x86_64_with_local_apic_count(vcpus)` sets partition **properties
    before `WHvSetupPartition`** (`whp.rs:147-180`); `set_memory` **takes and owns
    a `GuestMemoryMmap`** (`whp.rs:191-213`).
- `vcpus` is needed at construction on HVF (redist sizing) and WHP (processor
  count) but **not** on KVM.
- **Two memory representations.** The guest needs backend memslots; *devices*
  need a `vm_memory::GuestMemoryMmap` to walk descriptors (`virtio_pci_dev.set_mem`,
  `lib.rs:1690`). KVM builds these separately (`memory::build_guest_memory`,
  `memory.rs:91`); WHP clones one `GuestMemoryMmap` into both the VM and the device
  (`lib.rs:196,251`). DESIGN's "memory is a constructor input" must vend the
  `GuestMemoryMmap` **back out** to devices.
- `vm_fd_arc()` (`kvm.rs:94`) is pervasive — `IrqManager`, `IrqfdNotifier`, and
  `VirtioPciDevice::set_vm_fd` all need the raw `Arc<VmFd>` to register
  irqfds/ioeventfds themselves. A trait that hides the backend handle cannot
  serve these; HVF/WHP have no equivalent. **This is the central abstraction
  tension.**

### Proposed revision
Split the monolithic `Vm` into **construction**, **memory accessor**, **vCPU
vending**, and an explicit **run outcome**, and make ordering internal to the
constructor:

```rust
/// Everything the backend needs to stand up the substrate, in one value so the
/// backend can enforce its own ordering (irqchip/GIC before vCPUs) internally.
struct VmOptions {
    vcpus: u32,
    memory: MemoryPlan,          // placement.rs output: memslots + memory_nodes
    substrate: Substrate,        // DTB-derived substrate the Vm claims (see §6)
    min_addr_space_bits: u32,    // F7 watermark; HVF validates host IPA >= this
}

trait Vm: Sized {
    type Vcpu: VcpuRun;          // NOT Clone, NOT necessarily Send (HVF: !Send)

    fn new(opts: VmOptions) -> Result<Self, Error>;

    /// The device-facing DMA view of guest RAM. Devices clone this for
    /// descriptor access; lifetime is tied to the VM's mappings.
    fn guest_memory(&self) -> GuestMemoryMmap;

    fn attach_mmio(&mut self, dev: Box<dyn MmioDevice>) -> Result<(), Error>;

    /// Wired interrupt line vended from a DTB GSI/SPI. Returns the existing
    /// `Interrupt` handle, not an Fn(bool) (see §3).
    fn wired_irq(&self, spec: WiredIrq) -> Result<Interrupt, Error>;

    /// MSI routing for a PCI device. Returns the backend's MsixNotifier, which
    /// the device stores and the *guest* drives lazily (see §3).
    fn msi_notifier(&self, vectors: u16) -> Result<Arc<dyn MsixNotifier>, Error>;

    /// Create the vCPUs. On HVF these are created lazily *on each vCPU thread*
    /// (vCPUs are !Send/thread-bound), so this returns thread-launchable seeds,
    /// not live Vcpus. See VcpuFactory note below.
    fn vcpus(&self) -> Result<Vec<Self::Vcpu>, Error>;
}
```

- **Drop `fn run(self)`.** Replace with a supervisor-owned run loop over
  `Vcpu::run`, returning a `RunOutcome { Exit(i32), Reboot }` so HVF's
  warm-reboot is expressible and the VM survives between runs. Document that the
  supervisor (not the `Vm`) owns thread spawning, per DESIGN §8.
- **Thread-bound vCPUs (HVF).** Note explicitly that `Self::Vcpu` may be `!Send`
  and must be *created on the thread that runs it* (`hvf.rs:131,173-179`). The
  trait should expose a `VcpuFactory` (a `Send + Sync` seed each thread calls to
  mint its own vCPU) rather than handing pre-built vCPUs across threads. KVM/WHP
  factories just wrap pre-created vCPUs; HVF's factory calls
  `create_vcpu_current_thread()` against the singleton.
- **Keep `vm_fd_arc()` as a backend-private escape hatch, not on the trait.**
  The honest resolution: irqfd/ioeventfd registration is KVM-only. Push it behind
  the `wired_irq`/`msi_notifier`/activate hooks so KVM constructs eventfd-backed
  `Interrupt`s internally and never exposes `VmFd` above the trait. (See §3, §5.)

---

## 2. `MmioDevice` trait (DESIGN §4) — receiver, Send+Sync, the UART problem

### Problems
- `read(&self, …)` / `write(&self, …)` assume `&self`, but **every device is
  stateful**. Today the receiver split is inconsistent: `VirtioMmio::read/write`
  are `&self` over `Mutex<Inner>` (`virtio_mmio.rs:122,155`) — clean; but
  `Ns16550::read/write` are **`&mut self`** behind a static `Mutex`
  (`uart.rs:172,203`), and `VirtioPciDevice::config_write/bar_write` are
  **`&mut self`** with **no internal lock** (they rely on `PciBus`'s per-slot
  `Mutex`, `pci.rs:59`).
- The bus closures are `Arc<dyn Fn(u64,&mut[u8])->bool + Send + Sync>`
  (`mmio_bus.rs:16`). A trait object replacing them must be **`Send + Sync`**.
- **The UART does not fit at all.** It is a process-global
  `OnceLock<Mutex<Ns16550>>` + free functions, with **three different per-OS
  `init` signatures** (`uart.rs:233/245/257`): macOS `(reg_shift)`, Linux
  `(reg_shift, EventFd)`, Windows `(reg_shift, InterruptController, Arc<IoApic>,
  gsi)`. Interrupt delivery is baked into a per-OS `Trigger` at init. There is no
  per-instance object, no `window()`.

### Proposed revision
```rust
trait MmioDevice: Send + Sync {
    fn window(&self) -> MmioWindow;                 // base + size (DTB-derived)
    fn read(&self, offset: u64, data: &mut [u8]) -> bool;
    fn write(&self, offset: u64, data: &[u8]) -> bool;
}
```
- **Commit to `&self` + interior mutability** as the rule (matching the frozen,
  `Arc`-shared, lock-free dispatcher at `mmio_bus.rs:8-11,82-96`). State this is a
  **required refactor** for `Ns16550` and `VirtioPciDevice`, which must grow a
  `Mutex`/atomic interior rather than relying on a caller-supplied lock.
- **Require `Send + Sync`** in the trait bound (DESIGN currently says only `Send`).
- **Rebuild the UART as a real `MmioDevice`.** One struct owning its
  `vm_superio::Serial` + an injected `Interrupt` (from `Vm::wired_irq`), replacing
  the static `OnceLock` and the three per-OS `init` signatures with one
  constructor taking an `Interrupt`. This is the single largest device-layer
  change and should be called out in the §10 mapping table (currently the table
  omits the UART entirely).
- Cite `VirtioMmio` (`virtio_mmio.rs`) as the **reference implementation** of the
  target shape — it already is `read(&self)/write(&self)->bool` with window-relative
  offsets and `Mutex<Inner>`. Correct DESIGN §10's claim that it is "macOS-only":
  the module is compiled unconditionally (`lib.rs:29`); it is only *used* on the
  macOS run path.

---

## 3. Interrupts (DESIGN §6/§7) — `Interrupt`, not `IrqLine(Fn(bool))`

This is the section that most needs rewriting. DESIGN proposes wired interrupts
as `IrqLine (Fn(bool) assert/deassert)`. The research contradicts this on all
three backends.

### Findings
- **The natural handle already exists and has no bool.** `Interrupt`
  (`interrupt.rs`): Linux `Interrupt(EventFd)` with `signal()=write(1)` and
  `as_eventfd()` (vhost needs the raw fd, `vhost_frontend.rs:201`); non-Linux
  `Interrupt(Arc<dyn Fn()>)` with `from_fn`. Wrapping a KVM line in `Fn(bool)`
  **loses** the `&EventFd` vhost requires.
- **Level/deassert is a transport concern, not a line concern (HVF).**
  `set_spi(intid, level)` is level-triggered, but **assert happens on the device
  worker thread** (`virtio_mmio.rs:223`) and **deassert happens on the vCPU
  thread** when the guest writes `INTERRUPT_ACK` and the shared `int_status`
  `AtomicU32` reaches 0 (`virtio_mmio.rs:193-198`). No single closure owns the
  level; the **virtio-mmio register file owns it**. A bare `Fn(bool)` cannot say
  *who* calls `false`.
- **Edge backends have no bool and no deassert (KVM/WHP).** KVM's UART path only
  ever pulses (`register_irqfd_at_gsi` → write the eventfd; no resample irqfd is
  set up, `irq.rs:149`). WHP's wired path is `IoApic::inject_gsi(&ic, gsi)` —
  edge, assert-only, **no bool parameter** (`uart.rs:113-117`, `ioapic.rs:55-71`).
- **WHP's wired line is coupled to another attached device.** `inject_gsi` reads
  the **guest-programmed redirection table inside the userspace `IoApic`**, which
  is itself an `MmioDevice` on the bus (`lib.rs:377-383`). So "the `Vm` vends a
  wired line independent of devices" is false on WHP — the line's vector/dest
  come from a device the guest programs.
- **MSI is already correct in DESIGN.** `vm_pci::MsixNotifier`
  (`Send+Sync`, `vector_updated` + `msix_enabled`, `msix.rs:99`) is implemented by
  all three backends (`IrqfdNotifier`/`HvfMsixNotifier`/`WhpMsixNotifier`) and
  injected into `VirtioPciDevice::new`. **Crucially, MSI vectors are minted
  lazily by the guest**: `IrqfdNotifier::vector_updated` allocates a GSI + irqfd
  *during a guest MSI-X table write* (`pci_irq.rs:88-143`). So MSI lines cannot be
  vended one-shot at attach.

### Proposed revision
Rewrite §6/§7 around **two genuinely different mechanisms**, both already present:

1. **Wired interrupts use `Interrupt`, vended by `Vm::wired_irq(WiredIrq) ->
   Interrupt` at attach time.** Semantics: `signal()` = "assert/pulse." Edge
   backends (KVM irqfd write; WHP `inject_gsi`) pulse and have nothing to
   deassert. The **level + ack/deassert coupling is owned by the transport**
   (the virtio-mmio `INTERRUPT_STATUS`/`INTERRUPT_ACK` registers + shared
   `int_status`), which on a level backend (HVF) drives `set_spi(false)` when
   status hits 0. Document that the wired handle is therefore *fire-style*, and
   any level management is the device/transport's, not the line's.
   - Add an explicit note that on WHP the wired `Interrupt` closure must capture
     the `Arc<IoApic>` + `InterruptController`; i.e. `wired_irq` may depend on the
     IOAPIC device having been attached. Resolve the IOAPIC's dual role in §6
     (see this doc §6).
2. **MSI uses the existing `Arc<dyn MsixNotifier>`, vended by `Vm::msi_notifier`
   and driven lazily by the guest** through `VirtioPci` BAR2 writes. Keep this
   exactly; it is the part DESIGN gets right. State the lazy/re-entrant
   allocation explicitly so nobody tries to fold MSI into `wired_irq`.
3. **Delete the `IrqLine (Fn(bool))` abstraction.** Replace its three bullets with
   the `Interrupt`/`MsixNotifier` split above and the per-backend table:

   | backend | wired assert | wired deassert | MSI |
   | --- | --- | --- | --- |
   | KVM | irqfd `write(1)` at DTB GSI (edge) | none (no resample irqfd) | irqfd + MSI route, GSI allocated lazily on table write |
   | HVF | `set_spi(intid,true)` on worker thread | `set_spi(intid,false)` on vCPU thread when `int_status==0` | `hv_gic_send_msi(addr,intid)` via GICv2m frame |
   | WHP | `IoApic::inject_gsi` → `request_fixed_interrupt` (edge) | none | `request_fixed_interrupt` decoded from MSI-X addr/data (edge) |

---

## 4. `PciDevice` / `PciRoot` (DESIGN §5) — no `msix` method, and the PIO gap

### Problems
- **`PciDevice` has no `msix` method.** DESIGN lists "MSI-X" as a `PciDevice`
  method; the real trait (`pci.rs:22`) is `config_read`/`config_write`/`name`/
  `bar_regions`/`bar_read`/`bar_write` only. MSI-X is **entirely internal to
  `VirtioPciDevice`** (`MsixTable` + `Arc<dyn MsixNotifier>`), reached via BAR2
  `bar_read/bar_write` and a `config_write` interception (`transport.rs:412-418`).
- **`PciBus` is the de-facto `PciRoot`** (host bridge at slot 0, `config_read/
  write`, `bar_read/write`, `enumerate_bars`, `pci.rs:58-181`) but is **not** an
  `MmioDevice` — it is bridged onto the bus by ECAM/BAR closures
  (`lib.rs:1699-1749`).
- **x86 base config is PIO, not MMIO.** `0xcf8`/`0xcfc` (`pio_pci.rs`) reach the
  *same* `PciBus` (`Arc<PciBus>` shared with ECAM), but the trait stack
  terminates at `MmioDevice{window,read,write}` — there is **no PIO surface**.
  Today `LegacyPciState` lives outside `MmioBus`, dispatched by port-range checks
  in the vCPU loop (`lib.rs:461-505`). DESIGN §5 says "PciRoot ... and on x86 the
  `0xcf8`/`0xcfc` ports" and "PciRoot implements MmioDevice" — these are
  contradictory as written.

### Proposed revision
- **Drop `msix` from the `PciDevice` trait** in §5; keep `MsixNotifier` as the
  MSI abstraction (already done in §6). State that MSI-X table/PBA live in BAR
  space and are handled through `bar_read/bar_write` + a `config_write`
  interception — no dedicated trait method.
- **Keep PIO out of the device model entirely.** `0xcf8`/`0xcfc` are not a device
  and not a distinct config path — they are x86's *legacy decoder* onto the **same
  PCI config space** ECAM reaches (ports serve base config, ECAM serves extended;
  both hit the same `PciBus::config_read/write`, and `pio_pci.rs:10-12` /
  device-model.md:414-418 require identical bytes). The earlier draft's
  `PioDevice` trait was wrong: it pushed an x86-ism into the arch-neutral stack
  for a facility used by *nothing else* (x86 serial is MMIO, so the config ports
  are the *only* PIO, `lib.rs:462-463`). Instead:
  - **`PciRoot` exposes a transport-neutral config accessor** —
    `config_read(bdf, reg) -> u32` / `config_write(bdf, reg, offset, data)` —
    which `PciBus` already provides (`pci.rs:112,125`). The full `reg` range is a
    superset: the ECAM decoder passes the full index, the legacy decoder passes
    only the low 6 bits (256 B).
  - **`PciRoot` implements `MmioDevice` for the ECAM window** (the universal path;
    the *sole* config path on aarch64).
  - **The `0xcf8`/`0xcfc` bridge is backend-internal, below the trait boundary.**
    On x86, the supervisor recognizes the two architectural ports on a
    `PioRead`/`PioWrite` exit and calls the *same* `PciRoot` config accessor —
    a second decoder, not a second device. It lives with the LAPIC/IOAPIC
    substrate per DESIGN principle #1. `MmioDevice`/`PciDevice`/`PciRoot`/
    `VirtioDevice` never mention PIO; the device model stays 100% arch-neutral.
  - `PioRead`/`PioWrite` remain in the backend `VmExit` enum (raw x86 CPU exits,
    like MMIO) but are an x86-backend/supervisor concern, not a device-model one.
    HVF never emits them.
- Confirm the rest of §5 is accurate: `VirtioPciAdapter` already forwards
  `PciDevice` 1:1 (`pci.rs:227`); `bar_regions` returns GPA windows
  (`BarRegion{bar_idx,base_gpa,size}`) the supervisor maps as MMIO.

---

## 5. `VirtioDevice` + adapters (DESIGN §6, §10) — small corrections, one inversion

### Findings
- DESIGN §10's "VirtioDevice already matches — none" is **essentially right**.
  The real trait (`device.rs:27`) adds **`num_queues(&self) -> usize`** (DESIGN's
  sketch omits it) and `activate(&mut self, mem: GuestMemoryMmap, queues:
  Vec<Queue>, queue_evts: Vec<Kick>)` — i.e. exactly `(mem, queues, kicks)`. Add
  `num_queues` to the sketch.
- **Interrupts are *pulled by the device*, not pushed by the transport.** The
  console is constructed with a `CallFdLookup = Arc<dyn Fn(u16) ->
  Option<Interrupt>>` (`console lib.rs:96,113`) and resolves
  `Queue.msix_vector -> Interrupt` *itself* at activate (`console lib.rs:162`).
  DESIGN's mental model (the transport hands the device its MSI-X interrupts) is
  inverted. **Decide and document one of:** (a) pass a resolved `Vec<Interrupt>`
  into `activate` (cleaner, removes the lookup closure), or (b) keep the
  `CallFdLookup` injection. Recommend (a) for the redesign.
- **`set_vm_fd` leaks KVM into the transport.** `VirtioPciDevice` has
  `#[cfg(linux)]` fields `vm_fd: Option<Arc<VmFd>>` + `registered_ioeventfds`
  vs `#[cfg(not linux)] queue_kicks` (`transport.rs:147-160`), and ioeventfd
  registration in `activate_device` (`transport.rs:673-697`). The `VirtioPci`
  adapter in the redesign must push notify-wiring behind a backend hook (an
  ioeventfd registrar the Vm vends on KVM, a direct `queue_kicks[i].write(1)`
  notify path elsewhere) instead of `cfg`-gating `VmFd` into the struct.
- **`Arc<Mutex<Box<dyn VirtioDevice>>>` is load-bearing**, not incidental: the
  transport holds the device that way (`transport.rs:112`) so the console can be
  swapped in place on soft-reconnect (`device_arc()`, `reset_for_reconnect`). The
  adapter chain in §6 must preserve this indirection, not flatten to
  `Box<dyn VirtioDevice>`.

### Proposed revision
- Update the `VirtioDevice` sketch to include `num_queues`.
- Add a §6 paragraph stating the interrupt-resolution direction decision and the
  notify-wiring backend hook (so KVM ioeventfd vs in-process kick is behind the
  adapter, not in the device).
- In the §10 table, change `VirtioPci adapter: none/minor` to call out the
  `set_vm_fd`/ioeventfd de-leak as real work.

---

## 6. DTB node ownership split (DESIGN §3, open Q2) — use `survey`, decide three cases

### Findings
- The "claim every node, total coverage" machinery **already exists** as
  `Machine::survey` (`machine.rs:219-279`): self-routing `from_tree` constructors
  that `require`/`ack` properties and `ensure_drained`, ending in an empty-tree
  check (`Uncovered`) + disjointness (`check_disjoint`). **`RegionKind` already
  tags `SubstrateMmio` vs `Mmio` vs `EcamWindow` vs `BarWindow`** (`machine.rs:73-85`)
  — i.e. the substrate/device distinction DESIGN wants is already encoded. But
  `survey` is called once and **not consumed**; every backend instead uses the
  older `extract()->Platform`, which has **no coverage proof** and does not even
  parse `/timer`.
- **Natural owners** (from the per-constructor table, `machine.rs`):
  - **Vm substrate:** GIC dist/redist/v2m frame, `timer`, `psci`, x86 LAPIC, x86
    IOAPIC. (All `SubstrateMmio` or no-region.)
  - **Device layer:** `serial` (ns16550a), `virtio_mmio@*` slots, `pcie@*` bridge.
- **Three genuinely ambiguous cases the design must rule on:**
  1. **x86 IOAPIC is dual-role** — interrupt-routing substrate **and** an
     `MmioDevice` the device layer programs and the wired-IRQ path reads
     (`uart.rs:114`, `lib.rs:375`). Proposal: the **Vm claims the node** (owns the
     SPI/pin namespace and the `request_fixed_interrupt` primitive) but
     **attaches the IOAPIC register model as an `MmioDevice` itself**, and
     `wired_irq` closes over it. Document that some substrate is *realized as an
     attached MMIO device by the Vm*.
  2. **x86 syscon poweroff/reboot** — power/reset is substrate semantically, but
     it is emulated as a plain MMIO register (`RegionKind::Mmio`, run-loop match
     at `lib.rs:1962-1973`). aarch64 has no such device (power = PSCI HVC in the
     run loop). Proposal: treat syscon as **Vm-owned substrate realized as an
     attached `MmioDevice`** (parallel to IOAPIC), so the device layer stays
     arch-neutral and `process::exit`/reboot stays a Vm concern.
  3. **Cross-cutting reference props are discarded.** `interrupt-parent`,
     `msi-parent`, `phandle` are `ack`-ed without recording the linkage
     (`machine.rs:320-322,345-346,475-476,539`). A device that independently
     claims `serial`/`virtio`/`pcie` **cannot recover which controller it parents
     to** — only the bare IRQ *number* survives (serial cell-0, virtio cell-1).
     Proposal: when the Vm claims the intc/v2m nodes it must **record the phandle →
     (controller, SPI namespace) map** and hand each device a resolved
     `WiredIrq`/`MsixNotifier` keyed by that number, so the device never needs the
     phandle. The SPI namespace bound (`GicConfig.spi_base/spi_count` from the
     v2m frame) is substrate-owned; individual SPI assignments live on device
     nodes — `wired_irq` is where the two are reconciled.

### Proposed revision
- Rewrite §3's DTB bullet to say the split **is realized by promoting
  `survey`/`ResourcePlan` into the run path** (replacing `extract`), with `Vm`
  consuming the `SubstrateMmio`/substrate constructors and the device layer
  consuming the `Mmio`/`EcamWindow`/`BarWindow` constructors — **one shared
  `ResourcePlan` keeps the total-coverage + disjointness proof and re-aggregates
  every region for RAM placement** (`placement::device_holes` needs all regions,
  `placement.rs:149-158`).
- Add the `Substrate` value passed in `VmOptions` (§1): the typed substrate
  fields (`GicConfig`/LAPIC+IOAPIC region/`Psci`/syscon) + the phandle→IRQ-namespace
  map.
- Add the three rulings above to open-question #2 as *resolved*.

---

## 7. Execution / PSCI / run-loop (DESIGN §8, open Q3/Q4) — make the run loop a first-class owner

### Findings
- **vCPU exit dispatch (Q3)** already avoids per-exit dynamic dispatch: reads are
  serviced by `Fn` closures passed into `Vcpu::run` (`kvm.rs:371`, `whp.rs:473`),
  writes come back as `VmExit` and are dispatched against the frozen `Arc<MmioBus>`
  (linear window scan, `mmio_bus.rs:98`). WHP additionally services reads **inside**
  `run()` via the WinHvEmulation callbacks (fresh emulator per exit,
  `whp.rs:1053/1086`) — so the dispatcher contract must allow a device `read` to
  run *inside* the backend `run()` call, not only post-return.
- **PSCI / CPU bringup is neither MMIO nor interrupt (Q4 is broader than stated).**
  On HVF it is **userspace** in the run loop: `psci.rs` decodes `Hvc` args; the
  loop needs per-vCPU `set_gpr(x0)`, `set_aarch64_state` for secondaries, and a
  per-vCPU `CpuSlot` condvar mailbox (`lib.rs:973-1017,1152-1205`), plus the
  warm-reboot loop. On KVM (x86) bringup is in-kernel INIT-SIPI-SIPI; aarch64-KVM
  PSCI would be in-kernel but is **absent in this tree**.
- **Shutdown is inconsistent.** HVF returns a structured `RunOutcome`
  (`Exit`/`Reboot`); WHP/KVM call `std::process::exit(0)` directly from the
  syscon/PSCI path (`lib.rs:363-368,1559-1561`). The proxy/process model (§8) is
  already satisfied on Linux — `VhostUserFrontend` *is* a `VirtioDevice`
  (`vhost_frontend.rs:108`) — but `activate` needs the `IrqfdNotifier` to resolve
  per-queue call fds (`vhost_frontend.rs:168`), i.e. a device needs IRQ-layer
  access at activate, which a pure `MmioDevice` cannot express.

### Proposed revision
- Add a **`run loop / supervisor`** subsection making explicit that: the
  supervisor owns thread spawning and the per-`Vcpu::run` loop; the `Vm` vends a
  `VcpuFactory` and an exit-dispatch context (the `Arc<MmioBus>` + `PioDevice`s);
  PSCI/bringup is a **backend choice** (in-kernel on KVM, a supervisor-driven
  userspace handler + `CpuSlot` mailbox on HVF) surfaced through `VmExit::Hvc` and
  per-vCPU register access on `Self::Vcpu`.
- Make `RunOutcome { Exit(i32), Reboot }` the uniform return and require backends
  to surface shutdown/reboot through it rather than `process::exit`, so the
  warm-reboot loop is the same shape everywhere.
- Note that `activate` (not just attach) is where a device gets memory + kicks +
  resolved interrupts; the §8 proxy and the in-process device share this path.

---

## 8. Summary of concrete edits to DESIGN.md

| DESIGN section | Change |
| --- | --- |
| §1 | State the matrix is three disjoint arch×backend cells today (KVM=x86, HVF=arm, WHP=x86); flag the PLATFORMS.md "Linux aarch64 KVM implemented" discrepancy. |
| §3 `Vm` | Replace `new(opts)`+`run(self)` with `VmOptions{vcpus,memory,substrate,min_addr_space_bits}`, `guest_memory()`, `wired_irq()`, `msi_notifier()`, a `VcpuFactory` for thread-bound vCPUs, and a supervisor-owned run loop returning `RunOutcome{Exit,Reboot}`. Keep `vm_fd_arc` as backend-private. |
| §3 DTB | Realize the substrate/device split by promoting `survey`/`ResourcePlan` (already tags `SubstrateMmio`); resolve the IOAPIC/syscon/phandle cases (this doc §6). |
| §4 `MmioDevice` | Require `Send + Sync`; commit to `&self`+interior mutability; flag the UART rebuild (its three per-OS `init`s → one constructor taking an `Interrupt`); cite `VirtioMmio` as the template and correct its "macOS-only" label. |
| §5 PCI | Drop `msix` from `PciDevice`; keep PIO out of the device model — `PciRoot` exposes a transport-neutral config accessor + an ECAM `MmioDevice` face, and the x86 `0xcf8`/`0xcfc` ports are a backend-internal second decoder onto that same accessor (below the trait boundary). Keep `MsixNotifier`. |
| §6 virtio | Add `num_queues`; decide interrupt-resolution direction (recommend resolved `Vec<Interrupt>` into `activate`); push notify-wiring behind a backend hook; preserve `Arc<Mutex<Box<dyn VirtioDevice>>>`. |
| §6/§7 interrupts | **Delete `IrqLine(Fn(bool))`.** Use `Interrupt` (signal-only) for wired, vended by `wired_irq`; `MsixNotifier` for MSI (lazy, guest-driven). Add the per-backend assert/deassert table. State the transport owns level/ack. |
| §8 | Add the run-loop/supervisor ownership of threads, exit dispatch, and userspace-vs-kernel PSCI; uniform `RunOutcome`; note `activate` is the device's memory/kick/interrupt hook (shared by the vhost proxy). |
| §11 | Mark open-Q1 (interrupts) and Q2 (DTB split) resolved per above; broaden Q4 to include userspace PSCI + `CpuSlot` bringup; keep Q3 (note WHP's in-`run()` read callbacks) and Q5 (macOS CI). |
