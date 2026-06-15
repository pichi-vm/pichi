# Pichi: Image Build System

## Design Document

Status: design converged on the build *method* and security model. This
document records those findings and defines the `pichi.yaml` schema and the
software to be built. No build code exists yet.

> History: this supersedes the build design imported verbatim from the dillo
> PoC. The durable concepts (build-as-launch, the carapace object model, the
> inner/outer GPT) are retained; the architecture is updated for the
> pichi/dillo split, PMI (not IGVM), a *measured, guest-driven* build
> model, and a confidential-computing threat model.

---

## 1. Thesis

Pichi's primary host job is *launching VMs from registry artifacts*. Image
construction is secondary, and pichi solves it the same way: **building is
launching, with one specific appliance.** A build runs *inside* a VM — the
"build image" is itself a pichi artifact — so the host needs only a hypervisor
(`/dev/kvm`, or HVF/WHP), no host-side root, and no host-side mount. (One
transitional exception: packing the build context currently shells out to
`mkfs.erofs` host-side — see §6.1; the deferred pure-Rust packer removes even
that.)

In the pichi/dillo split:

- **`pichi`** is the high-level, docker/podman-like front end. It owns the
  cache and orchestrates builds: it reads the recipe, packs the build context,
  launches the build VM by `exec()`ing **`dillo`**, waits, and packages the
  result.
- **`dillo`** is the VMM. It boots the build appliance like any other PMI +
  carapace, on whatever backend the host has (KVM/HVF/WHP).

The crucial property of the new model: **the build is guest-driven — the host
only responds.** It attaches the inputs, launches, and then answers the guest's
requests over one small vsock RPC (§7.5); it never injects commands or content.
The guest's chief request is the verity **root hash** of each attached carapace
— the independent anchor dm-verity needs, which the guest cannot safely read
from the (host-controlled) device itself. The host can serve those roots or
withhold them (a visible DoS), but it cannot forge: every block the guest reads
is verity-checked against the root, and that root is bound into the attestation
report (CC) or a signed manifest (pre-CC). So the build stays a pure function of
*its inputs* — reproducible, or at least falsifiable, under an untrusted host
(§5).

A build has exactly **three carapace inputs**, each attached as its own vGPT
device: the **build image carapace** (the bootable appliance — systemd as PID 1,
corium, and build tooling), the **source layer carapace** (`from:`, the layers
new scutes stack onto), and the **build environment carapace** (the packed
context, recipe included). The build image carapace's root rides its PMI (the
normal-boot anchor); corium **asks the host over the RPC for the other two
roots** (§7.5). Because every input is a measured carapace and every output root
is computed in guest-trusted RAM, the build is **attestable**: in CC all three
input roots are folded into the attestation report, binding the output scutes to
those exact inputs as source provenance (§10).

---

## 2. Object Model

A registry tag holds **one OCI artifact** containing two logical objects:

| Object | Required | Purpose |
|--------|----------|---------|
| Carapace | Yes | A stack of scutes (one or more layers). The bootable rootfs in composed form. Always read-only. |
| PMI | Optional | Boot payload (kernel, initrd, cmdline, measured platform layout, measured manifest binding the carapace top-hash). Required to launch the artifact. |

`pichi run <tag>` requires a PMI. Without one, the artifact is a base — usable
as a `from:` source but not bootable.

- A **scute** is a layer: one cow file (dm-snapshot persistent COW format) and
  one verity file (dm-verity hash tree over the cow).
- A **carapace** is N scutes composed via salt-chain binding. The top scute's
  verity root (`rootₙ₋₁`) is the trust anchor.
- The PMI's measured manifest binds the expected `rootₙ₋₁`. The guest verifies
  what it mounts against this measurement.

The media types and annotations are already implemented in `pichi-artifact`:
`application/vnd.pichi.scute.v1` (+`+zstd`), `application/vnd.pichi.pmi.v1`,
wrapper `application/vnd.pichi.artifact.v1+json`, chain annotations
`dev.pichi.carapace.verity.{algo,data-block-size,hash-block-size}`, per-scute
`dev.pichi.scute.verity.salt`.

### 2.1 GPT inside the carapace

The composed carapace block device contains a **GPT** following systemd's
Discoverable Partitions Specification (DDI). Two GPTs exist in the runtime
stack, serving different purposes:

| GPT | Where | Identifies | Consumer |
|-----|-------|------------|----------|
| Outer | Synthesized by the host's carapace device (`dillo-virtio-gpt`) | Individual scutes (DDI PARTUUIDs from the carapace spec) | Guest's carapace-assembly code |
| Inner | Inside the composed carapace block device | Filesystem partitions (Discoverable Partitions PARTUUIDs) | `systemd-gpt-auto-generator` at boot |

Appliance authors write no fstab and no explicit `root=`; `systemd-gpt-auto-
generator` discovers partitions by well-known PARTUUID. Writable scratch is
tmpfs overlays configured by the appliance; carapaces stay read-only.

The **outer-GPT PARTUUIDs are deterministic** — derived from each scute's
verity root — which is exactly what `pichi run` already stamps when it builds
the `--gpt` device for `dillo`, and what `dillo-config::derive_ids` hashes into
the disk device-id/disk-guid. Build and run share this path.

---

## 3. CLI surface

`pichi` mirrors podman/docker for image management. All verbs except `build`
are implemented today.

| Command | Status | Purpose |
|---------|--------|---------|
| `pichi import <raw> <tag>` | done | Convert a raw GPT image into a base carapace. Pure host-side userspace. |
| `pichi build [-t <tag>] [--build-image <ref>] <dir>` | **this doc** | Build an artifact from `<dir>/pichi.yaml`. Derives from a `from:` carapace; runs in a build VM (any tagged image; default the official build image). |
| `pichi run <tag>` | done | Launch a VM from a tag. Errors if not cached; requires a PMI. |
| `pichi pull` / `push` | done | Move artifacts to/from a registry. |
| `pichi images` / `inspect` / `rmi` / `tag` | done | Local cache management. |

---

## 4. `pichi import`

Equivalent in spirit to `podman import`: raw bytes in, base carapace out.

**Input:** a raw disk image with an inner GPT per the Discoverable Partitions
Specification. How the user produces it is out of scope (recommended: mkosi).
Pichi does not validate contents beyond a parseable GPT.

**Operation:** pure host-side userspace — no root, no kernel modules, no
mounts. Implemented in `pichi-import` (`cow.rs` emits the dm-snapshot
persistent COW append-only; `verity.rs` computes the dm-verity tree). Output is
a one-scute base carapace (no PMI), usable as a `from:` source.

The same machinery is reused to pack the build *context* (§6).

---

## 5. Trust & threat model

The target deployment is **confidential computing**: AMD SEV-SNP or Intel TDX
on a Linux/KVM host. In that model **the host is untrusted** — the carapace
mutual-distrust principle: verification belongs in the guest, and the host is a
potentially-malicious storage/transport medium on both ends.

What this forces (the rest of the document is the consequences):

- **Inputs must be verifiable by the guest**, not merely provided. Anchored by
  a dm-verity root that comes from the launch measurement, so every block read
  is checked.
- **Outputs carry their own integrity** — a verity root computed by the guest
  at production time, bound to attestation — so the untrusted host can transport
  the bytes but cannot forge or tamper them undetectably.
- **Integrity anchors are never derived from a read of a host-controlled
  medium** (that is a TOCTOU: the host can serve good bytes during hashing and
  keep bad bytes). Output roots are computed from guest-trusted memory; input
  roots arrive as host *claims* over the RPC (§7.5) — the guest never hashes
  host-served bytes to obtain them, dm-verity then enforces each claim on every
  read, and attestation records which root was used.
- **The host cannot influence the build.** It supplies inputs and resources and
  answers the guest-driven RPC (§7.5); it cannot inject commands or content.
  Substituting an input is possible but never silent — the substituted root is
  bound into provenance — and withholding a resource is a visible DoS, never
  silent corruption.

**Platform reality.** CC exists only on Linux/KVM (SNP/TDX). On the macOS
(HVF) and Windows (WHP) dillo backends the build VM is a *plain* VM with a
trusted host. So the constructions below are the **CC/KVM path**; on non-CC
backends the same flow runs with a weaker anchor (registry/TLS + a signed
manifest instead of hardware attestation).

### 5.1 Device-mapper has no anti-rollback (validated)

Per the kernel docs (`Documentation/admin-guide/device-mapper/dm-integrity.rst`,
v6.6): dm-crypt is confidentiality-only; dm-integrity / dm-crypt+integrity
(AEAD) detect *modification* and *forgery* and (with `fix_hmac`) bind sector
position, but provide **no replay/rollback protection** — restoring an older
valid `(data, tag)` at the same sector verifies as authentic. dm-verity is the
only freshness anchor and it is read-only (the root, delivered out-of-band, is
what pins content). **There is no DM primitive for anti-rollback of mutable
state.** This is why the build keeps mutable state in CC-protected RAM (§7)
rather than on host-backed disk.

---

## 6. The build context (measured input)

The reason to bring the host directory into the VM is to `copy:` files into the
image. If those files are not measured *and verifiable by the guest*, the build
is neither reproducible nor trustworthy under an untrusted host. So the context
is not a live virtio-fs mount — it is a **measured, verity-protected, read-only
input**, the same primitive as a scute.

**Packing (host-side, fully unprivileged):**

1. Serialize `<dir>` into an **erofs image** via `mkfs.erofs` (§6.1).
2. Run it through `pichi import` → a **build environment carapace** (cow + verity).
3. The build environment's **verity root is its content address** and a
   first-class build input. (Standalone, fixed zero salt + 4096 block sizes;
   with the v1 `mkfs.erofs` packer the root is a content address of the packed
   bytes — see the determinism caveat in §6.1.)

The recipe (`pichi.yaml`) is packed **inside the context**, so the build
instructions are themselves measured (§8).

The build environment carapace is **ephemeral**: it is emitted with
`pichi import`'s carapace machinery (`cow.rs` + `verity.rs`) but written to
scratch and discarded after the build — it is **never tagged and never enters
the image cache**, even though its bytes are a valid carapace. (`pichi import`'s
cache-insertion step is factored out from its carapace-emission step so `build`
can call emission alone.)

Delivery + verification reuse the runtime path: the build environment carapace
is attached via `dillo-virtio-gpt` and the guest activates dm-verity over it,
exactly like a runtime carapace, found by a well-known context type-GUID.

### 6.1 erofs via `mkfs.erofs` (v1)

erofs is the format (mountable, read-only, lazy reads). v1 serializes `<dir>`
with the host's **`mkfs.erofs`** and runs the image through `pichi import`. This
is the one transitional host-side tooling dependency (§1) — it needs no root and
no mount, just an unprivileged read of the directory and a write of the image.

Packing rules (passed to `mkfs.erofs`): 4096 block size (matches verity), **no
compression** (simplest, and what the guest kernel must support), **no xattrs**
(the context is source material; final-image SELinux labels are set in-guest
during the build via package policy / `restorecon`, not carried from the host),
regular files / dirs / symlinks only.

**Determinism caveat.** `mkfs.erofs` output varies by tool version and flags, so
the build environment carapace's verity root is *not* yet a pure function of the
canonical input bytes — it also depends on the host's `mkfs.erofs`. The root is
still a faithful content address of *whatever bytes were packed* (so the guest
verifies every block it reads), but byte-for-byte reproducibility across hosts
is not guaranteed in v1. Restoring it — and removing the host-side dependency —
is deferred to the pure-Rust **`pichi-erofs`** emitter (§13).

Guest support is ours to guarantee: the build-image kernel needs
`CONFIG_EROFS_FS` (no compression variants) + `CONFIG_DM_VERITY`.

Validation: `fsck.erofs` structural check in CI and a real-kernel mount in a
boot test.

---

## 7. The build method (execution & integrity)

This is the core finding. It runs **entirely in CC-protected guest RAM** so
that there is no host-backed mutable medium to attack (which is why §5.1's
missing anti-rollback primitive never bites in v1).

### 7.1 Live execution in tmpfs

Each command's writable layer is a **kernel dm-snapshot whose COW exception
store is a file in tmpfs** (a loop device over a sparse `/tmp` file, or a `brd`
ramdisk). Origin = the composed previous layer (the **source layer carapace**
for the first command; chained snapshot-of-snapshot thereafter). corium mounts
the snapshot device and **chroots into it** to run the directive.

Two non-negotiables:

- **dm-snapshot is not append-only** (validated against `drivers/md/dm-snap.c`,
  v6.6): the first write to a chunk copies-out once to a freshly allocated COW
  chunk; **subsequent writes to that chunk overwrite the COW chunk in place**
  (`snapshot_map` → `remap_exception` for read *and* write). So a live
  dm-snapshot is write-many. Keeping it in tmpfs means those in-place rewrites
  happen in RAM and never touch host-backed storage.
- **Swapless.** If tmpfs spills to swap on a host-backed volume — even an
  encrypted one — write-many returns: swap slots are reused, so a rolled-back
  encrypted page is a valid past `(ct,tag)` and the host can feed the guest
  stale memory. The build VM runs with **no swap**; the live working set is pure
  RAM.

### 7.2 Finalizing a scute (write-once + deterministic)

The tmpfs COW is the live *store*, not the scute. Its layout is
non-deterministic (allocation follows write order; in-place rewrites). So we
**re-emit** the layer's final changes as a clean dm-snapshot persistent COW via
`cow.rs` — each unique non-zero chunk written exactly once, in canonical order.
That append-only emission is what makes the scute both **deterministic** and
**write-once**.

### 7.3 Output via virtiofs (untrusted sink), TOCTOU-safe

The new scutes are emitted in the **exact on-disk scute format** (cow + verity),
identical to runtime scutes, so the host repackages them without transformation.
Output integrity rides the **verity root**, not the transport, so virtiofs (a
host-readable shared dir) is fine as a pure *sink* — the inverse of why it is
unacceptable for input. The sequence is ordered to keep the hash over trusted
bytes:

1. The finalized scute COW lives in **tmpfs** (trusted).
2. **Compute dm-verity over the tmpfs COW**, writing the verity tree directly to
   virtiofs. The root is computed from the trusted RAM copy — never by reading
   back from virtiofs (that would be the TOCTOU).
3. **Then copy the COW** from tmpfs to virtiofs.
4. Write the signed/attested **manifest** (§10) to virtiofs — the attestation
   over the three input carapaces, binding them to the output roots.

Because the root is fixed in trusted RAM *before* the bytes enter host-readable
space, any host tampering on the virtiofs copy is caught downstream when a
consumer re-verifies bytes against the attested root. No export TOCTOU.

> virtiofs as an output sink means a **virtio-fs device must be built in dillo**
> (output-only use). The alternative — a sparse writable `--blk` the host reads
> after power-off — needs no new device but requires pre-sizing. We choose
> virtiofs for arbitrary-size ergonomics; this is on the software list (§11).

### 7.4 v2: spilling beyond RAM

The working set is bounded by guest RAM (§9). If a build needs more than
feasible RAM, the v2 escape is an **encrypted, write-once scratch** on
host-backed disk: dm-crypt authenticated mode + dm-integrity **no-journal**
(ephemeral guest key), with `fix_hmac` for positional binding, used **write-
once** so the missing anti-rollback primitive (§5.1) is supplied by "no second
valid version exists at any sector." It needs a raw append-only arena (no
filesystem journals) and a single ephemeral boot (no re-activation/recovery).
This is deferred — growing trusted RAM (§9) is preferred because it keeps
everything in hardware-trusted memory with none of this machinery.

### 7.5 The carapace root-hash channel (guest-driven JSON-RPC over vsock)

Carapaces attach as **vGPT devices** (the outer GPT, §2.1), but dm-verity needs
the full **root hash**, and that cannot come from reading the device (TOCTOU,
§5.1) nor from the outer-GPT PARTUUID (128 bits — too short to be a 256-bit
root). So the root is always delivered out-of-band. On a **normal boot** there
is one carapace and its root is in the **PMI** (measured at launch). A **build**
attaches two extra carapaces — the source layer and the build environment —
whose roots are *not* in the build image's PMI, so corium obtains them over a
small control channel:

- **Transport:** vsock (`dillo-virtio-vsock`). Guest-driven: **corium is the
  client**, the host `pichi build` is the **server**.
- **Protocol:** **JSON-RPC 2.0**, newline-delimited (one object per line) — no
  schema compiler, no codegen, not gRPC.
- **Methods (minimal):** `carapace.list` → the attached carapaces with their
  role (`source-layer` | `build-env`), vGPT device id, and verity **root hash**;
  plus coarse `build.progress` / `build.done` notifications. That is the whole
  surface.

corium verity-activates each extra device against the root the host returned,
mounts, and proceeds. The root is a host *claim*: dm-verity enforces it on every
read (so the host cannot serve non-matching bytes), and in CC corium folds all
three input roots into the **attestation report** (§10) — so a host that
substitutes an input cannot hide which root it supplied. This is why a
host-provided root is sound here even though §5.1 forbids *deriving* an anchor by
hashing a host-served medium.

---

## 8. `pichi build` flow

The recipe lives in the context, so the host's role is setup + wait:

1. Host reads `pichi.yaml` (from `<dir>`) to resolve `from:` and the output
   tag, and resolves the **build image** ref (`--build-image` →
   `PICHI_BUILD_IMAGE` → config → default). The build image is just a tagged
   artifact — any cached tag works, which is what makes bootstrapping (build the
   build image with a prior one) and per-project customization free. Its
   measurement is recorded in provenance (§10) regardless of which image.
2. Host packs `<dir>` (including `pichi.yaml`) → erofs (`mkfs.erofs`) →
   `pichi import` → **build environment carapace** (§6).
3. Host ensures the **source layer carapace** (`from:`) and the **build image
   carapace** are cached (pulls if needed).
4. Host launches the build VM via the `pichi run` path. Each carapace is its own
   vGPT device; the attach set is `[build image carapace (bootable), source
   layer carapace, build environment carapace, virtiofs output]`, plus the
   guest-driven root-hash RPC on vsock (§7.5), with a generous memory ceiling
   (§9).
5. Inside the guest, **systemd (PID 1)** starts **corium**. corium asks the host
   over the RPC for the source-layer and build-environment **root hashes**
   (§7.5), verity-activates those two vGPT devices, mounts the build environment
   carapace, reads `pichi.yaml`, and executes the whole build autonomously (§7):
   per command, a tmpfs dm-snapshot over the parent → chroot + run directive →
   `cow.rs` re-emit → verity → write to virtiofs.
6. corium writes the signed/attested manifest (the attestation over the three
   input carapaces, §10), then powers off.
7. Host reads the scutes + manifest from virtiofs, packages them (+ PMI, if the
   recipe produced one) into one OCI artifact, tags it per `-t`, and verifies
   the bytes against the attested roots.

One boot per `pichi build`; the build VM persists across all steps of one
build.

### 8.1 Layer caching

Cache key per scute:

```
hash(parent_rootₙ₋₁ || directive_kind || directive_content)
```

`copy:` content includes the content hash of every file under `src` (already
covered by the context verity root); `run:` is the literal command string. A
hit lets the agent reuse a cached scute and jump the dm-snapshot stack ahead; a
miss invalidates that layer and everything above it. Cache lives in the local
image cache; registry push/pull warms remote/local consumers.

---

## 9. Memory elasticity (researched)

The build's working set lives in tmpfs (§7), so guest RAM must grow to fit it.
In CC this is not free: CVM private memory is **not overcommittable or
swappable** (it is hardware-encrypted and must be *accepted* by the guest —
PVALIDATE on SNP, TDX accept — to enter the protected boundary).

**Findings (verify current status before committing — sources are late-2025):**

- **virtio-mem is not usable in a CVM today, on either SNP or TDX.** It manages
  guest memory through QEMU's `RamDiscardManager` (a binary populated/discarded
  axis), but `guest_memfd` CVMs already use that axis for shared/private. Doing
  both needs a 3-state model (shared-populated / private-populated / discarded);
  a generic framework for it (`PrivateSharedManager`) was prototyped and
  deferred. virtio-mem-in-CVM is explicitly listed future work.
  ([QEMU shared-device-assignment series](https://www.mail-archive.com/qemu-devel@nongnu.org/msg1120810.html),
  [PrivateSharedManager v4](https://www.mail-archive.com/qemu-devel@nongnu.org/msg1106876.html))
- **Dynamic growth via pc-dimm/ACPI hotplug + unaccepted memory** is the
  in-progress alternative on SNP — but it is an **RFC, not merged** (AMD,
  Nov 2025), and uses `accept_memory=eager|lazy` (lazy default).
  ([LWN](https://lwn.net/Articles/1048251/))
- **TDX host-side hotplug is more constrained**: the kernel guarantees all
  page-allocator pages are TDX memory, refuses to online non-TDX memory, assumes
  convertible memory is always present, and does not handle ACPI memory removal
  (no hot-unplug). ([kernel TDX docs](https://docs.kernel.org/arch/x86/tdx.html))
- **Lazy acceptance is merged on both** and is what we use.

**Decision:**

- **v1 = lazy acceptance + an auto-sized `maxmem` ceiling.** `pichi build`
  sizes the ceiling from the host's currently-available memory (free +
  reclaimable cache — `MemAvailable` on Linux, the equivalent elsewhere),
  rounded down; overridable with `--memory`. `dillo` launches with that ceiling;
  the guest leaves memory unaccepted and accepts on first touch; with
  `guest_memfd` the host commits private pages on acceptance → demand-commit up
  to the ceiling. No virtio-mem, no new device, no hotplug RFC. Lazy acceptance
  means an over-large ceiling never over-commits (cost is address
  space/metadata until touched).
- **Exceeding the ceiling at runtime is not supported in v1** (virtio-mem
  deferred; pc-dimm hotplug RFC/constrained). Over-ceiling → honest OOM → raise
  `maxmem` and re-run. Revisit dynamic grow when virtio-mem-in-CVM lands.
- Non-CC backends (HVF/WHP) get this for free via ordinary host overcommit.

---

## 10. Provenance / output manifest

corium emits a manifest binding the three input carapaces to the output scutes —
the artifact that makes the build **attestable**:

```
{ build image carapace measurement, source layer carapace root,
  build environment carapace root (recipe included) }
        →  { output scute roots, optional PMI measurement }
```

- **Pre-CC:** the agent signs the manifest with a build-VM key; trust falls
  back to registry/TLS for the build image.
- **CC:** the build image carapace root is in its PMI (the launch measurement);
  corium receives the source-layer and build-environment roots over the RPC
  (§7.5) and folds **all three** input roots into the attestation report,
  alongside the output scute roots it computes in trusted RAM. Hardware-rooted
  source provenance, not signature-rooted.

Either way the build is **reproducible or falsifiable**: same measured inputs →
same outputs (modulo non-deterministic `run:` steps, e.g. network fetches —
see §13), and the manifest commits the claim. The production-time-hashing +
signed-manifest discipline is built **from v1** even pre-CC, because without it
the output is forgeable; only the *anchor* changes when CC lands.

---

## 11. Software to build

### 11.1 Independent components (start now, in parallel)

Each is self-contained with a clear interface and isolated tests; none depends
on another *new* component, so they can proceed concurrently and they unblock
the integrators below.

| Component | Where | Role | Isolated test |
|-----------|-------|------|---------------|
| `pichi-recipe` | `pichi/deps/` | `pichi.yaml` types + parse + validate (§12). Shared by host (`pichi build`) and guest (corium). | parse/validate fixtures |
| `pichi-provenance` | `pichi/deps/` | Build-manifest schema (binds inputs→outputs, §10) + sign/verify with a **pluggable anchor** (signature now, attestation later). Shared by corium (produce) and `pichi build` (verify). | roundtrip + verify |
| `pichi-buildrpc` | `pichi/deps/` | JSON-RPC 2.0 message types + thin client/server for the guest-driven root-hash channel (§7.5) over `dillo-virtio-vsock`. Shared by corium (client) and `pichi build` (server). | roundtrip |
| `dillo-virtio-fs` | `dillo/deps/` | virtio-fs device — the untrusted output sink (§7.3). Follows the existing `VirtioDevice` pattern. | device harness + boot test |

> Context packing uses the host's `mkfs.erofs` in v1 — no new component. The
> pure-Rust `pichi-erofs` emitter that restores determinism and drops the
> host-side dependency is deferred (§13).

### 11.2 Integrators (depend on the above + existing code)

| Component | Where | Role |
|-----------|-------|------|
| **corium** | `corium/` (root product; static musl guest binary, like `snuffler`) | The **pichi guest-services binary** — everything the guest needs, build being one capability. Runs under **systemd (PID 1)** as the guest-services agent. Modular capabilities: **carapace assembly** at boot (systemd-invoked; uses the `carapace` crate), **build execution** (fetch input root hashes over the vsock RPC §7.5, read recipe, tmpfs dm-snapshot per step, chroot + run, `cow.rs`+`verity.rs` re-emit, write to the virtiofs output, emit the provenance/attestation manifest — §7–8), attestation/reporting. Extensible. A skeleton (dispatch entrypoint) can start now; capabilities plug in. (`snuffler` stays the *test-only* probe; corium is production.) |
| `pichi build` | `pichi/src/cmd/build.rs` | Host orchestrator (§8): resolve recipe + `from:` + build-image, auto-size memory, pack context (`mkfs.erofs`→`pichi import`), launch dillo, wait, package + verify (`pichi-provenance`). Builds on the `pichi run` exec path. |
| dillo build wiring | `dillo` | Attach the source layer + build environment carapaces (each its own vGPT) + the virtiofs output + the vsock channel (§7.5) + the `maxmem` ceiling / lazy-acceptance config. |
| build image + bootstrap | new | A pichi artifact carrying **systemd (PID 1)** + corium + build tooling (`mount`, dm/verity tools, package managers, `arma` for PMI) + a kernel with `EROFS`+`DM_VERITY`+`DM_SNAPSHOT`+unaccepted-memory. Selectable per build (`--build-image`); a one-off raw `pichi import` seeds the first. |

Reused as-is: `pichi-import` (`cow.rs`, `verity.rs`), the `carapace` crate
(guest assembly), `dillo-virtio-gpt` (vgpt), `dillo-virtio-vsock` (the RPC
transport, §7.5), the `pichi run` device-assembly + exec path, and the
snuffler-style musl build for corium.

---

## 12. `pichi.yaml`

The recipe. Lives in the build context, so it is part of `context_root` and
therefore measured.

```yaml
from:
  scute: registry.example.com/base/fedora:43   # exactly one variant in v1

layer:                          # zero or more retained scutes
  - run: dnf install -y python3 torch
  - copy:
      src: ./app
      dst: /opt/app
  - run: chown -R appuser:appuser /opt/app

pmi:                            # optional; produces a bootable artifact
  layer:                        # zero or more ephemeral scutes (discarded after PMI extraction)
    - run: dracut --add-drivers "virtio_blk virtio_console" /tmp/initramfs.img
  build: arma build --kernel /boot/vmlinuz-* --initrd /tmp/initramfs.img --cmdline "$CMDLINE" -o /out/boot.pmi
```

- **`from:`** — exactly one `scute: <ref>` in v1 (derive from an existing
  carapace). No `raw:`/`tarball:`/`oci:` — run `pichi import` first, then
  reference the tag.
- **`layer:`** — ordered directives, each producing one retained scute:
  - `run: <command>` — execute a shell command inside the build VM (working dir
    `/`); tools come from the parent scute.
  - `copy: { src, dst }` — copy from the build context (the verity'd context
    carapace) into the guest filesystem.
  - `env:` / `workdir:` are deferred; inline them in `run` for now.
- **`pmi:`** (optional) — if present, the artifact is bootable.
  - `pmi.layer:` — ephemeral scutes for PMI-production tooling, discarded after.
  - `pmi.build:` — the command producing the PMI at `/out/boot.pmi`. The PMI
    builder is the user's choice; the build image ships `arma`. The kernel
    cmdline lives inside the PMI (author's domain) and pichi never injects it.

---

## 13. Non-goals, deferrals, open questions

**Deliberately not done.** No host-side mount of scutes (the build VM exists to
keep this off the host). No FUSE for layer mounting (blocks `security.*`
xattrs). No bundled world in pichi (the build image carries tooling). No raw-
image production in pichi (mkosi etc. live outside; `pichi import` consumes the
result). No cmdline injection. No `entrypoint`/fstab (inner GPT + systemd
discovers everything).

**Deferred.** Multi-carapace per artifact; mutable storage/volumes; explicit
launch config; `+zstd` scutes in the build path; `env:`/`workdir:`; multi-stage
(`copy --from`); cross-host build cache; the §7.4 encrypted-disk spill;
reproducibility tooling for non-deterministic `run:` steps (frozen package
indices, faked time — author's choice today); **deterministic, host-tool-free
context packing** (`pichi-erofs`, the pure-Rust erofs emitter — v1 packs via
host `mkfs.erofs` per §6.1, trading byte-for-byte cross-host reproducibility and
one host-side dependency for simplicity).

**Resolved.**

- **Output transport: virtiofs** (untrusted sink, §7.3); a `dillo-virtio-fs`
  device is on the software list.
- **Build-VM memory: auto-sized** from host available memory, overridable with
  `--memory` (§9).
- **Build image is any tagged artifact** (`--build-image`, precedence in §8) —
  which makes bootstrapping and per-project customization free.
- **Guest agent = corium**, the pichi guest-services binary (root product,
  static musl). **systemd runs as PID 1**; corium runs under it as the umbrella
  for *everything the guest needs* — carapace assembly, build execution,
  attestation — not just building (init/service management stays systemd's).
  "corium" had no surviving definition anywhere (the runtime concept it once
  clashed with was renamed/dropped in the dillo→pichi rework), so the name is
  reclaimed here. `snuffler` remains the test-only probe.
- **Input root-hash delivery = guest-driven JSON-RPC 2.0 over vsock** (§7.5):
  carapaces attach as vGPT, their roots are host *claims* (verity-enforced,
  attestation-bound). The **build environment carapace is ephemeral** — emitted
  by `pichi import` to scratch, never tagged, never cached.

**Still open.**

1. **First build image's distro base** must permit remix/redistribution of a
   customized image: **Fedora** (Remix trademark allowance) or **Debian**;
   **Ubuntu is excluded** (Canonical trademark policy restricts redistribution
   of modified images). Fedora-vs-Debian still open.

**Deferred.**

- **Attestation binding details** under SNP/TDX (which measurement register /
  report field carries the manifest digest; the guest-key-in-measurement
  alternative). §10.
- **Re-verify mid-2026 status** of the SNP pc-dimm hotplug RFC and the
  `PrivateSharedManager` 3-state work before finalizing §9.
```
