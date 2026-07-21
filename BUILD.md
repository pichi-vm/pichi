# Pichi: Image Build System

## Design Document

Status: design converged on the build _method_ and security model. This
document records those findings and defines the build inputs (`pichi.build/`),
the `pichi build` process, the artifact it produces, and the runtime that
consumes it. No build code exists yet.

> History: this supersedes the build design imported verbatim from the dillo
> PoC. The durable concepts (build-as-launch, the carapace object model, the
> inner/outer GPT) are retained; the architecture is updated for the
> pichi/dillo split, PMI (not IGVM), a _measured, guest-driven_ build
> model, and a confidential-computing threat model.

The document follows the artifact's life: **inputs** (the build directory) →
**process** (`pichi build`) → **output** (the artifact) → **runtime** (boot &
device model) → **guarantees** (trust, provenance, memory) → **reference** (CLI,
software, deferrals).

---

# Part I — Orientation

## 1. Thesis

**Building is launching.** Pichi builds images the same way it runs them: inside
a VM, booting a **build image** — a normal pichi artifact pulled from a registry.
The host sets up inputs and packages outputs; the build image drives the entire
interior build process. Every build input is measured — the build appliance by
its PMI, each source carapace by its dm-verity root hash. When running on
confidential-computing hardware (SEV-SNP, TDX), those measurements fold into an
attestation report that binds the measured inputs to the measured outputs, making
builds falsifiable under an untrusted host (§10, §11).

---

# Part II — Inputs: the build directory

## 2. `pichi.build/` — the authored files

A pichi project is a directory containing a **`pichi.build/`** subdirectory with
up to three authored YAML files, each corresponding to one output object
(Part IV):

| File                | Defines                  | Nature                          | Consumed by                       |
| ------------------- | ------------------------ | ------------------------------- | --------------------------------- |
| `carapace.yaml`     | the read-only carapace   | recipe (produces scute blobs)   | `pichi build`, once               |
| `pmi.yaml`          | the PMI boot payload     | recipe (produces a PMI blob)    | `pichi build`, once               |
| `config.yaml`       | the full launch + device configuration | configuration data | `pichi build` (filtered) + corium (whole) |

```
$DIR/                          # the project / build context (measured)
  pichi.build/                 # the only reserved name
    carapace.yaml              # recipe: from + derive
    pmi.yaml                   # recipe: base + build (conglobate)
    config.yaml               # full device + launch configuration
  ...                          # everything else is author's source,
                               # laid out however they like, referenced
                               # by carapace.yaml `copy:` directives
```

**Which files are present determines the artifact kind (Part IV):**

- `carapace.yaml` only → a **carapace artifact** (not bootable).
- `pmi.yaml` + `config.yaml` (± `carapace.yaml`) → an **application artifact**
  (bootable; carapace optional).

The whole `$DIR` is the build context and is **measured** (§4): the host mounts
it into the build VM via virtiofs; the guest serializes it to an erofs image,
wraps it in a carapace (cow + verity), and records the verity root — so every
authored file, including `config.yaml`, is covered by a guest-computed,
attestation-bound root hash.

### 2.1 `carapace.yaml` — the carapace recipe

Produces the read-only carapace (a stack of scutes, Part IV).

```yaml
from: registry.example.com/base/fedora:43
derive: # one scute per directive; optional
  - run: dnf install -y python3 torch
  - copy:
      from: ./app
      into: /opt/app
      owner: appuser # optional; set ownership/mode in the same scute
      group: appuser # rather than a separate `run: chown` step
      mode: "0755"
```

- **`from:`** — exactly one registry reference in v1 (derive from an existing
  carapace). No `raw:`/`tarball:`/`oci:` — run `pichi import` first, then
  reference the tag.
- **`derive:`** — ordered directives, each producing one retained scute:
  - `run: <command>` — execute a shell command inside the build VM (working dir
    `/`); tools come from the parent scute.
  - `copy: { from, into, owner?, group?, mode? }` — copy from the build context
    (the verity'd context carapace) into the guest filesystem, setting ownership
    and mode in the **same scute** (à la `install(1)` / Docker `COPY
    --chown/--chmod`). `from` is a single path or a list of paths; when a list,
    `into` must be a directory and all sources are installed there. `mode` is a
    build error when `from` is a list (unconditionally) or when `from` is a single
    path that resolves to a non-empty directory (checked at build time in the guest
    before the build proceeds, and also at `pichi update` time on the host as an
    early-fail convenience).
    Folding permissions into
    the copy avoids a separate `run: chown -R` directive, which would otherwise be
    its own scute. `owner`/`group` accept a name (resolved against the **parent
    scute's** `/etc/passwd` + `/etc/group`, so the user must already exist from a
    prior `run:`) or a numeric id; `mode` is quoted octal.

### 2.2 `pmi.yaml` — the PMI recipe

Produces the PMI boot payload. `pmi.yaml` shares `from:` and `derive:` with
`carapace.yaml` but adds a required `into:` key and differs in two decisive ways:

- **Nothing is retained but the PMI.** Every intermediate state is discarded;
  only the file named by `into:` survives. (Contrast `carapace.yaml`, whose
  `derive:` steps _are_ the retained scutes.)
- **Steps are not materialized as scutes.** The `derive:` list is an ordered
  command sequence run in a working filesystem; no intermediate scute is produced.

```yaml
from: registry.example.com/base/kernel-builder:latest
derive:
  - run: dnf install -y kernel corium dracut
  - copy:
      from: [pichi.build/config.yaml, pichi.build/refs.lock]
      into: /usr/lib/corium/
  - run: dracut --add corium /tmp/initramfs.img
  - run: arma build --kernel /boot/vmlinuz-* --initramfs /tmp/initramfs.img -o /tmp/boot.pmi
into: /tmp/boot.pmi
```

- **`from:`** — omit to build the PMI against the carapace produced by
  `carapace.yaml` (the common case). Required when there is no `carapace.yaml`
  (carapace-less/initramfs-only artifact), or when the PMI should build against a
  different carapace than the one `carapace.yaml` produces. Any PMI build needs
  corium present in the rootfs so its dracut module and binary are available to
  the initramfs arma generates (§16).
- **`derive:`** — an ordered `run:`/`copy:` sequence. All intermediate state is
  discarded; only the file at `into:` survives into the artifact.
- **`into:`** — path where the `derive:` sequence writes the finished `.pmi`.
  Required; no default. The author controls this path and passes it to whatever
  tool produces the PMI.

**The entire build context is available during the PMI build.** The whole `$DIR`
— including `pichi.build/` and all author source — is available via the build
environment (§4), so the author can `copy:` any file from it into the rootfs. A typical PMI build copies `config.yaml` and
`refs.lock` into the rootfs, builds an initramfs (e.g. via dracut, which picks up
corium's module and bakes them in), then invokes a PMI packager to seal the kernel
+ initramfs + cmdline into the finished `.pmi` (§5.4). The kernel cmdline lives
inside the PMI (author's domain); pichi never injects it.

### 2.3 `config.yaml` — the launch + device configuration

The **complete** configuration of the instance: its compute requirements, its
devices, and — for each device — the guest-side meaning (mountpoints, fstype,
mount options, …). It is authored once and serves two consumers:

- **`pichi build` filters it** → `requirements.yaml` (Part IV, §7), keeping only
  the host-facing fields and dropping the internal/guest-side ones. This filtered
  file is what is distributed in the artifact.
- **corium reads it whole** at boot (delivered via the PMI initramfs, §5.4) and
  acts on every field — mounting devices, then **enforcing the contract**: if a
  `required` resource the host was asked for is missing or undersized, corium
  fails and powers the instance off (§9).

So `requirements.yaml` is **derived from** `config.yaml`; the host never sees the
internal fields, and the guest is the party that enforces the requirements.

```yaml
# config.yaml — authored; pichi build filters → requirements.yaml; corium reads whole
cpus:
  required: 1 # error below this (defaulted if omitted)
  recommended: 4 # optional; warn below this; no upper bound

memory:
  required: 2GiB
  recommended: 4GiB # optional

carapaces: # extra read-only carapaces (virtio-gpt)
  models:
    carapace: registry.example.com/models/llama:7b # full registry ref; pinned in refs.lock
    description: LLM weights # optional, host-facing
    mount: /opt/models # INTERNAL — stripped from requirements.yaml
    slot: pci # optional; omit to let dillo choose

volumes: # persistent mutable virtio-blk; key = name = device serial
  data:
    required: 10GiB # host-facing
    recommended: 100GiB # optional, host-facing
    description: agent working state # optional, host-facing
    mount: /var/lib/agent # INTERNAL
    fstype: ext4 # INTERNAL
    format: if-empty # INTERNAL — mkfs when the device is blank
    options: [noexec, nodev, nosuid] # INTERNAL — passed to mount(8)
    slot: mmio # optional; omit to let dillo choose

interfaces: # virtio-net; identity underspecified in v1 — see §8.3
  public:
    description: internet-facing ingress # optional, host-facing
    ingress: # host-facing — ports the host must/should expose
      required: [443, [8000, 8999]] # integer, [low, high] range, or bare *
      recommended: [80]
    slot: pci # optional; omit to let dillo choose
  internal:
    description: east-west service mesh
    ingress:
      required: *
```

**Host-facing vs. internal — the filter boundary.** `pichi build` keeps a fixed
**keep-list** per section and drops everything else into the guest's hands:

| Section      | Kept in `requirements.yaml` (host-facing)         | Stripped (internal; corium-only)            |
| ------------ | ------------------------------------------------- | ------------------------------------------- |
| `cpus`       | `required`, `recommended`                          | —                                           |
| `memory`     | `required`, `recommended`                          | —                                           |
| `carapaces`  | `carapace` (digest-qualified from lock), `description`, `slot` | `mount`, …                    |
| `volumes`    | `required`, `recommended`, `description`, `slot`   | `mount`, `fstype`, `format`, `options`, … |
| `interfaces` | `description`, `ingress`, `slot`                   | …                                           |

pichi only needs to know the keep-list; it does **not** model the internal fields
(it never acts on a mountpoint or a mount option — corium does). This keeps pichi
out of the open-ended mount-option space: it carries internal fields through to
the guest unparsed and strips them from the distributed file.

**Carapace references in `config.yaml` may be a bare tag or a digest-qualified
ref (`name:tag@sha256:…`).** The authoritative pins live in `pichi.build/refs.lock`
(§2.4), written by `pichi update` and checked in alongside the yaml files. If an
inline digest is present, `pichi update` validates it against the lock rather than
overwriting it.

The device-identity and storage semantics behind these fields (why `volumes` is
keyed by serial, the default-`/var`
volume, overlays as guest composition) are the **runtime** model — see §8.

## 2.4 `refs.lock`

Machine-written by `pichi update`; checked in; never hand-edited. Maps every
carapace reference (exactly as written in `*.yaml`, registry host always required)
to its two hashes:

```yaml
# pichi.build/refs.lock — generated by `pichi update`; do not edit
registry.example.com/models/llama:7b:
  manifest: sha256:abc…   # SHA-256 over OCI manifest bytes
  carapace: sha256:def…   # dm-verity Merkle root over scute blocks
```

The two hashes are **parallel, independent cryptographic commitments to the same
content**: `manifest` is a flat SHA-256 over the OCI manifest bytes; `carapace`
is a dm-verity Merkle root over 4096-byte blocks of the same scute content.
Neither is derived from the other.

`pichi build` reads both files together — if any reference in `*.yaml` has no
entry in `refs.lock`, the build is rejected ("run `pichi update`"). At build time,
`pichi build` rewrites each bare tag in `requirements.yaml` to a digest-qualified
ref (`name:tag@manifest`) from the lock. The `carapace` hash is packed into the
BE (and thus measured into the PMI via `config.yaml`) for guest-side verification;
it does not appear in `requirements.yaml`.

---

# Part III — Process: `pichi build`

## 3. Overview

`pichi build [-t <tag>] [--build-image <ref>] <dir>` turns `<dir>` — including
its `pichi.build/` subdirectory — into an artifact. The build runs in a VM and is guest-driven (§1); the
host's role is setup + wait + package.

1. Host reads `pichi.build/` (from `<dir>`) to resolve `carapace.yaml`'s `from:`,
   the output tag, and the **build image** ref (`--build-image` →
   `PICHI_BUILD_IMAGE` → config → default). The build image is just a tagged
   artifact — any cached tag works, which is what makes bootstrapping (build the
   build image with a prior one) and per-project customization free. Its
   measurement is recorded in provenance (§11) regardless of which image.
2. Host reads `refs.lock` (§2.4) and cross-references every `carapace:` tag in
   `*.yaml` — rejects with "run `pichi update`" if any reference is missing from
   the lock. Stages OCI manifests for every referenced carapace (keyed by artifact
   digest from the lock) into a scratch manifests directory. Manifests are never
   written back to `$DIR`.
3. Host ensures all carapace **blobs** (keyed by digest) are in the local cache,
   pulling by digest if needed (deterministic — digest is already pinned). The
   **build image** is a PMI-only appliance (no carapace, §1).
4. Host launches the build VM: boots the **build-image PMI**; attaches two
   virtiofs mounts (read-only `$DIR` and read-only manifests directory), each
   source carapace as its own vGPT device, and the virtiofs output sink. The
   build VM is sized **exactly as `pichi run` sizes an instance** — from the
   build image's own `requirements.yaml` (§7, §12), with `--memory`/`--cpus`
   overriding. No vsock RPC.
5. Inside the build VM, the build image drives the build. In the default build
   image, **conglobate runs as `/init`** (no systemd, §16) and establishes the
   trust structure (§5.5):
   a. Serializes the virtiofs `$DIR` mount into an erofs image in RAM, wraps it
      as a carapace (cow + verity), and records the resulting verity root as the
      **build environment (BE) root hash** — this goes into the attestation report
      (CC) as the root of the trust structure. The erofs tool version is part of
      the build image's PMI measurement.
   b. Reads all `*.yaml` files from the in-RAM BE; extracts every carapace
      reference and resolves each to its `manifest` hash via `refs.lock` (also in
      the BE).
   d. Scans all attached vGPT devices; computes the dm-verity root hash of each
      from the raw device content and activates dm-verity against the computed hash.
   e. Validates every manifest file (read from the virtiofs manifests mount): hash
      the manifest bytes → must equal the `manifest` value in `refs.lock`. Reads
      the full content of each carapace partition and validates its hash against
      the manifest entry.
   f. Validates the dm-verity root hash computed in step d for each carapace
      against the `carapace` value in `refs.lock`. Steps e and f are **two
      independent cryptographic paths over the same content** — both must pass;
      either failing aborts the build.
   g. Executes the build autonomously (§5): per `carapace.yaml` directive, a
      dm-snapshot → chroot + run → `cow.rs` re-emit → verity → virtiofs. If
      `pmi.yaml` is present, runs its directives and invokes the PMI packager
      (§5.4).
6. conglobate writes the signed/attested provenance manifest (§11), then powers
   off.
7. Host reads the scutes + PMI + manifest from virtiofs, **filters `config.yaml`
   → `requirements.yaml`** (§2.3), packages everything into one OCI artifact,
   tags it per `-t`, and verifies the bytes against the attested roots.

One boot per `pichi build`; the build VM persists across all steps of one build.


## 4. The build context (measured input)

The reason to bring the host directory into the VM is to `copy:` files into the
image. For those files to be trustworthy under an untrusted host, the context
must be measured and verifiable by the guest. The host mounts `$DIR` read-only
into the build VM via virtiofs; the guest then serializes it into a measured,
verity-protected carapace entirely in RAM.

**In-guest packing:**

1. Guest serializes the virtiofs `$DIR` mount into an **erofs image** in RAM.
   Packing rules: 4096 block size (matches verity), no compression, no xattrs,
   regular files / dirs / symlinks only. Canonical file ordering ensures
   reproducibility.
2. Guest emits it as a **build environment (BE) carapace** (cow + verity) using
   the same `cow.rs` + `verity.rs` machinery as `pichi import` — no tagging, no
   cache insertion.
3. The BE's **verity root is its content address** and the root of the trust
   structure. It is placed in the attestation report (CC). Because the erofs
   tool lives in the build image, its version is part of the build image's PMI
   measurement — no separate host-side dependency.

The whole `pichi.build/` directory is serialized **inside the context**, so the
build recipes, `config.yaml`, and `refs.lock` are all measured.

The BE is **ephemeral** and **reproducible**: given the same `$DIR` contents and
the same build image (same erofs tool version), the output bytes and verity root
are identical. A test verifies this invariant. The BE is never tagged and never
enters the image cache.

## 5. The build method (execution & integrity)

This is the core finding. It runs **entirely in CC-protected guest RAM** so
that there is no host-backed mutable medium to attack (which is why §10.1's
missing anti-rollback primitive never bites in v1).

### 5.1 Live execution in tmpfs

Each command's writable layer is a **kernel dm-snapshot whose COW exception
store is a file in tmpfs** (a loop device over a sparse `/tmp` file, or a `brd`
ramdisk). Origin = the composed previous layer (the **source carapace** — the
recipe's `from:` — for the first command; chained snapshot-of-snapshot
thereafter). conglobate mounts the snapshot device and **chroots into it** to run
the directive.

Two non-negotiables:

- **dm-snapshot is not append-only** (validated against `drivers/md/dm-snap.c`,
  v6.6): the first write to a chunk copies-out once to a freshly allocated COW
  chunk; **subsequent writes to that chunk overwrite the COW chunk in place**
  (`snapshot_map` → `remap_exception` for read _and_ write). So a live
  dm-snapshot is write-many. Keeping it in tmpfs means those in-place rewrites
  happen in RAM and never touch host-backed storage.
- **Swapless.** If tmpfs spills to swap on a host-backed volume — even an
  encrypted one — write-many returns: swap slots are reused, so a rolled-back
  encrypted page is a valid past `(ct,tag)` and the host can feed the guest
  stale memory. The build VM runs with **no swap**; the live working set is pure
  RAM.

### 5.2 Finalizing a scute (write-once + deterministic)

The tmpfs COW is the live _store_, not the scute. Its layout is
non-deterministic (allocation follows write order; in-place rewrites). So we
**re-emit** the layer's final changes as a clean dm-snapshot persistent COW via
`cow.rs` — each unique non-zero chunk written exactly once, in canonical order.
That append-only emission is what makes the scute both **deterministic** and
**write-once**.

### 5.3 Output via virtiofs (untrusted sink), TOCTOU-safe

The new scutes are emitted in the **exact on-disk scute format** (cow + verity),
identical to runtime scutes, so the host repackages them without transformation.
Output integrity rides the **verity root**, not the transport, so virtiofs (a
host-readable shared dir) is fine as a pure _sink_ — the inverse of why it is
unacceptable for input. The sequence is ordered to keep the hash over trusted
bytes:

1. The finalized scute COW lives in **tmpfs** (trusted).
2. **Compute dm-verity over the tmpfs COW**, writing the verity tree directly to
   virtiofs. The root is computed from the trusted RAM copy — never by reading
   back from virtiofs (that would be the TOCTOU).
3. **Then copy the COW** from tmpfs to virtiofs.
4. Write the signed/attested **provenance manifest** (§11) to virtiofs.

Because the root is fixed in trusted RAM _before_ the bytes enter host-readable
space, any host tampering on the virtiofs copy is caught downstream when a
consumer re-verifies bytes against the attested root. No export TOCTOU.

> virtiofs as an output sink means a **virtio-fs device must be built in dillo**
> (output-only use). The alternative — a sparse writable `--blk` the host reads
> after power-off — needs no new device but requires pre-sizing. We choose
> virtiofs for arbitrary-size ergonomics; this is on the software list (§16).

### 5.4 Getting `config.yaml` into the PMI

When `pmi.yaml` is present, the build image runs its directives. The chain that
carries `config.yaml` into the booted guest reuses the distro's own initramfs
machinery rather than reinventing it:

| Actor                          | When               | Job                                                                                                          |
| ------------------------------ | ------------------ | ----------------------------------------------------------------------------------------------------------- |
| **author** (`pmi.yaml`)        | authored           | `copy:` `config.yaml` (and `refs.lock`) from the build context into the build root at `/usr/lib/corium/config.yaml` — explicit, not implicit. |
| **corium package** (dracut module) | initramfs generation | Pull the corium binary **and** `/usr/lib/corium/config.yaml` into the initramfs cpio.                  |
| **arma**                       | PMI build          | Generate the initramfs and seal kernel + initramfs + cmdline → measured `.pmi`.                              |
| **corium** (initramfs phase)   | boot, pre-pivot    | Copy `config.yaml` → `/run`, mount the root carapace, `switch_root` (§8.1).                                  |
| **corium** (post-pivot)        | boot, post-network | Enforce the contract; mount secondary devices (§8.1).                                                        |

Why the initramfs (not injecting bytes into guest memory): arbitrary bytes placed
in a memory region are not a structure Linux preserves — the kernel reclaims the
page. To survive, data must arrive via something the kernel treats as
first-class. The initramfs is the one channel that is **both measured** (folded
into the PMI by arma, so attested at launch) **and materializes as real files** at
a known path — and it works whether or not a root carapace exists (the carapace
is optional, so `config.yaml` cannot live there). So `config.yaml` rides the
initramfs, and arma's seal makes it ride the PMI measurement.

The known path `/usr/lib/corium/config.yaml` is the **contract between the
author's `pmi.yaml` and the corium dracut module**: the author copies the file
there explicitly (as shown in §2.2), the dracut module pulls it into the
initramfs, and runtime corium reads it (§8.1.1).

### 5.5 Trust establishment (no vsock)

The host is a pure CDN: it mounts `$DIR` and manifests via virtiofs, attaches
source carapaces as vGPT devices, and waits for the VM to power off. There is
no vsock RPC. The build image establishes the trust structure entirely in RAM:

1. **Build the BE.** Serialize the virtiofs `$DIR` mount → erofs image → carapace
   (cow + verity) in RAM (§4). Record the **BE verity root** — this is the root
   of the entire trust structure and goes into the **attestation report** (CC).
   The host can supply any virtiofs content, but cannot forge the root the guest
   computes and reports.

2. **Resolve references.** Read all `*.yaml` files from the in-RAM BE; extract
   every carapace reference. Resolve each to its pinned `manifest` and `carapace`
   hashes via `refs.lock` (also in the BE).

3. **Scan and mount source carapaces.** Enumerate all attached vGPT devices.
   For each, compute its dm-verity root hash from the raw device content and
   activate dm-verity against the computed hash — reads are now
   integrity-protected.

4. **Validate manifests.** Read each manifest from the virtiofs manifests mount;
   hash the bytes → must equal the `manifest` value in `refs.lock`.

5. **Validate content.** Read the full content of each carapace partition;
   validate its hash against the manifest entry.

6. **Validate dm-verity roots.** The dm-verity root hash computed in step 3 for
   each carapace must equal the `carapace` value in `refs.lock`. Steps 4–6 are
   **two independent cryptographic paths over the same content** — neither derived
   from the other; both must pass.

The trust chain is entirely guest-computed: BE root (self-measured from virtiofs
input) → `refs.lock` (in the BE) → manifest hashes → content hashes +
dm-verity roots. The host supplies bytes; the guest decides what to trust.

---

# Part IV — Output: the artifact

## 6. Object model

A registry tag resolves to an **OCI artifact**. Its contents are
**compositional**, not two fixed kinds: an artifact carries any combination of
scutes (a carapace), a PMI, and a base DTB, plus a config. Capability follows
from what is present:

- **Bootable ⟺ a PMI is present.** The PMI is the only thing `pichi run`
  strictly requires; the launch contract it needs to size the VM rides in the
  manifest **config** (§7), not a separate object.
- **Has a root carapace ⟺ scutes are present.** Independent of the PMI.
- **Base DTB** accompanies a PMI built in the `dt` detached channel mode; it is
  supplied to the VMM out-of-band at run time.

| Composition                | Bootable | Role                                                                                             |
| -------------------------- | -------- | ----------------------------------------------------------------------------------------------- |
| scutes only                | No       | A layered read-only block device — a `from:` source, or a carapace attached to an application.  |
| PMI (+ base DTB)           | Yes      | A self-contained appliance; its initramfs carries everything, no separate root carapace.        |
| PMI (+ base DTB) + scutes  | Yes      | The common path: a bootable instance whose root carapace is attached automatically.             |

This yields the concrete shapes (Part II): `carapace.yaml` only (base);
`carapace.yaml` + `pmi.yaml` + `config.yaml` (bootable with a root carapace);
`pmi.yaml` + `config.yaml` only (self-contained bootable). `pichi run <tag>`
requires a PMI; a scutes-only artifact is not launchable — it exists to be
derived from or attached.

### 6.1 Objects

An artifact's envelope packages these logical objects:

| Object           | Where it rides                                | Purpose                                                                                                                                                                                                      |
| ---------------- | --------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Carapace         | 0..N scute layers                             | A stack of scutes composed into a single read-only block device. When it is an artifact's root carapace, it backs the root; it may also be attached elsewhere.                                               |
| PMI              | ≤1 PMI layer (makes it bootable)              | Boot payload (kernel, initramfs incl. corium, cmdline, measured platform layout, measured boot manifest binding the carapace top-hash). The cmdline lives in the base DTB (`/chosen/bootargs`).              |
| Base DTB         | ≤1 DTB layer (detached-mode PMI)              | The measured base devicetree for a detached-channel PMI; supplied to the VMM out-of-band at run time (`dillo --dtb`). Carries the cmdline, incl. `root.carapace=<top-hash>`.                                 |
| config (requirements) | the manifest **config** blob              | The host-facing launch contract — the filtered projection of `config.yaml` (§7), carried in `application/vnd.pichi.config.v1+json`. Consumed by `pichi run` to size and wire the VM.                         |

- A **scute** is a layer: one cow file (dm-snapshot persistent COW format) and
  one verity file (dm-verity hash tree over the cow).
- A **carapace** is N scutes composed via salt-chain binding. The top scute's
  verity root (`rootₙ₋₁`) is the trust anchor.

## 7. `requirements.yaml` (the launch contract)

The launch contract is the **host-facing projection of `config.yaml`** (§2.3): the
build keeps the host-facing fields and strips the internal ones. It rides in the
artifact's **manifest config blob** (`application/vnd.pichi.config.v1+json`,
§7.1) — not a separate layer — so a consumer that pulls a tag knows what a launch
demands before booting; it is the input to `pichi run`'s device-assembly +
allocation path. (This document calls the contract "requirements" throughout; on
the wire it is the `requirements` object inside the config blob.)

**`requirements.yaml` is the complete launch contract.** If the guest depends on
the host to provide _anything_ — cpus, memory, a volume, a network interface, an
exposed port — it **MUST** be declared in `config.yaml` and therefore appear
here. The host decides _how_ to satisfy each obligation (which NIC backing, which
port-forwarding mechanism, which storage), but the guest **may not depend on any
host-provided resource not in this file**. The file is exhaustive by contract,
not best-effort. **corium enforces it from inside the guest** (§9): the untrusted
host cannot be the enforcer.

Every quantity is stated as two tiers — `required` and `recommended` — never a
fixed size: pichi declares _requirements_; the **host allocates** the actual
value. The tiers differ in **severity**, and neither is a ceiling:

- **`required`** — the host MUST provide it; if it cannot, launch **errors** (at
  `pichi run` pre-flight where checkable, and at boot by corium) and the instance
  does not run;
- **`recommended`** — the host SHOULD provide it; if it cannot (but `required` is
  met), the instance starts with a **warning**;
- beyond `recommended` → silent. There is no upper bound.

The shape is exactly the host-facing subset of `config.yaml` (§2.3) — same dicts,
same keys, with the internal fields removed:

```yaml
# requirements.yaml — filtered from config.yaml by `pichi build`
cpus:
  required: 1
  recommended: 4

memory:
  required: 2GiB
  recommended: 4GiB

carapaces:
  models:
    carapace: registry.example.com/models/llama:7b@sha256:abc… # tag rewritten to digest-qualified ref by pichi build
    description: LLM weights

volumes:
  data:
    required: 10GiB
    recommended: 100GiB
    description: agent working state

interfaces:
  public:
    description: internet-facing ingress
    ingress:
      required: [443, [8000, 8999]]
      recommended: [80]
  internal:
    description: east-west service mesh
    ingress:
      required: *
```

- **`cpus` / `memory`** — `required`/`recommended` bands. The operator's
  `--memory` / `--cpus` set the actual values, checked against these floors (§12).
- **`carapaces`** — extra read-only carapaces to pull and attach as virtio-gpt
  devices. The `carapace:` field is a digest-qualified ref (`name:tag@sha256:…`)
  injected by `pichi build` from `refs.lock` — the host uses it to locate and
  validate the correct content. The dm-verity root hash is internal: it is packed
  into the BE and measured into the PMI, not distributed in `requirements.yaml`.
  (The artifact's _own_ root carapace is attached automatically and is not listed
  here.)
- **`volumes`** — persistent mutable block devices to provision; the **key is the
  `name`, which _is_ the virtio-blk serial** (§8.2). Each is a size band.
- **`interfaces`** — virtio-net interfaces to create; each may carry an `ingress`
  band (`required`/`recommended`). Each entry in a port list is an integer, a
  `[low, high]` inclusive range, or the bare scalar `*` (all ports). An
  `ingress.required` port or range the host cannot expose is a launch error; the
  host chooses the forwarding mechanism. This is a host-exposure obligation,
  **not** a guest firewall — the guest still binds and filters. The key is an
  operator-facing label the host maps to a backing (TAP/macvtap/…); it is **not**
  the guest-visible interface name. Guest-side interface identity is underspecified
  in v1 — see §8.3.

### 7.1 The manifest (OCI artifact envelope)

The packaging layer a tag points at, and what `pichi pull`/`push` move. It is the
OCI Image Manifest 1.1 envelope, fully typed in `pichi-artifact` (`Manifest`),
with invariants enforced by `Manifest::validate`.

The layer set is **compositional** — any combination of scutes, a PMI, and a
base DTB — rather than two fixed kinds. Presence determines capability:

- **`config`** is a real blob, `application/vnd.pichi.config.v1+json`, carrying
  the launch contract (the `requirements` projection of `config.yaml`, §7). This
  replaces the former OCI 1.1 empty-config descriptor; the launch contract now
  rides in the config, not a layer.
- **`layers`** — each is one of:
  - a **scute** (`application/vnd.pichi.scute.v1`, +`+zstd`) — 0..N; together
    they form the carapace. Order is not load-bearing.
  - the **PMI** (`application/vnd.pichi.pmi.v1`) — **at most one**. Its presence
    is what makes the artifact bootable.
  - the **base DTB** (`application/vnd.pichi.dtb.v1`) — **at most one**. The
    measured base devicetree for a PMI built in the `dt` extension's *detached*
    channel mode; `pichi run` hands it to the VMM out-of-band (`dillo --dtb`).
    It SHOULD appear only alongside a (detached-mode) PMI.
- **`artifactType`** = `application/vnd.pichi.artifact.v1+json`.
- Top-level annotations carry the carapace verity-chain parameters
  `dev.pichi.carapace.verity.{algo,data-block-size,hash-block-size}`; each scute
  layer carries `dev.pichi.scute.verity.salt` (hex; the salt-chain binding).

**Capability by composition:** bootable ⟺ a PMI is present; has a root carapace
⟺ scutes are present; the two are independent (a PMI-only artifact is a
self-contained appliance; a scutes-only artifact is a `from:` base). See §6.

The manifest is content-addressed (`Manifest::digest`), so the tag→digest binding
is what registry/TLS (pre-CC) or attestation (CC) anchors.

> **Disambiguation — this document uses "manifest" for several distinct things:**
>
> 1. **manifest** (§7.1) — the OCI 1.1 packaging envelope referencing the scute /
>    PMI / base-DTB layers, with the launch contract in its config blob. The
>    `pichi-artifact::Manifest` type. (The launch contract is the `requirements`
>    object in the config, §7 — _not_ a manifest.)
> 2. **boot manifest** — measured _inside_ the PMI; binds the expected carapace
>    root `rootₙ₋₁` so the guest verifies what it mounts (§7.1, §5.5).
> 3. **provenance manifest** — emitted by a _build_, binding input carapaces to
>    output scute roots, signed/attested (§11).

The PMI's **boot manifest** binds the expected `rootₙ₋₁`; the guest verifies what
it mounts against that measurement.

---

# Part V — Runtime: boot & device model

## 8. Boot and the device model

`pichi run <tag>` launches an application artifact: dillo wires the VM per
`requirements.yaml`, and the guest boots the PMI. corium — present because it
ships in the distro **base layer**, and pulled into the initramfs by its dracut
module (§5.4) — drives a **two-stage boot**, then resolves and enforces
`config.yaml`.

### 8.1 Two-stage boot

The principle: the **initramfs does the bare minimum to establish `/` and
pivot**; all hardware/contract validation happens **post-pivot**, where network,
logging, and full device enumeration are available and failures can be
diagnosed. Overlaying a subdirectory (`/usr`, `/etc`, …) is a live, post-pivot
operation; only the root itself must be established early — so nothing drags
device unlocking, networking, or attestation into the initramfs.

**Initramfs phase (corium, minimal):**

1. Read `config.yaml` from the initramfs (search path below).
2. Copy `config.yaml` → `/run/corium/config.yaml`. systemd's
   initrd-to-rootfs transition **moves** `/run` (a tmpfs) into the new root, so
   the file survives the pivot while the rest of the initramfs is freed. (This
   `/run`-preservation behavior is load-bearing and is to be confirmed by a boot
   test — §17.)
3. Mount the **root carapace** as `/` — its root hash from the PMI's measured
   boot manifest (§7.1) — using the existing carapace-assembly path. _Only_ what
   is needed to pivot; no secondary volume is touched here.
4. `switch_root`.

**Post-pivot phase (corium, full — a systemd unit ordered
`After=network-online.target`):**

1. Re-read `config.yaml` from `/run/corium/config.yaml`.
2. **Enforce the contract** (§9): cpus, memory, every `required` volume present
   and sized, interfaces present, ingress exposed — fail and power off if any
   `required` is unmet; warn on `recommended`.
3. Mount attached carapaces (`carapaces:` in `config.yaml`): enumerate vGPT
   devices, identify each by matching its artifact digest against the pinned
   `carapace:` value (content-addressed; no host identity claim trusted), activate
   dm-verity using the pinned `root_hash` (measured into the PMI from
   version-controlled source — trusted without an RPC), mount at the `mount:`
   path.
4. Mount secondary volumes per the internal fields (`mount`, `fstype`,
   `options`, `format`), set up per-directory overlays (§8.4), etc.

### 8.1.1 corium's `config.yaml` search path

corium resolves `config.yaml` from an ordered search path; **first hit wins**:

1. `/run/corium/config.yaml` — the per-instance copy (placed by the initramfs
   phase; the future home of any per-instance override channel, §17).
2. `/usr/lib/corium/config.yaml` — the vendor copy conglobate placed into the
   PMI initramfs (§5.4).

This ordering lets a later-delivered per-instance config supersede the baked-in
one without changing v1 (which only ever populates the vendor copy, propagated
into `/run`).

### 8.2 Device identity

corium must tell one device of a kind from another. Content-addressed devices
identify themselves; mutable and transport devices need an author-assigned
handle:

| Device                      | Identity                 | Source            |
| --------------------------- | ------------------------ | ----------------- |
| virtio-blk (volume)         | **serial** (= the name)  | author-chosen     |
| virtio-gpt (carapace)       | **carapace verity root** | derived           |
| virtio-net (interface)      | **underspecified (v1)**  | see §8.3          |
| vsock / console / output-fs | **singleton**            | none (≤1 of each) |

- **Volumes** — the `config.yaml`/`requirements.yaml` key **is** the device
  serial: it names the device, enforces uniqueness, and is how the guest finds it
  (`/dev/disk/by-id/…-<name>`).
- **Carapaces** — identified by verity **root hash** (derived; the author only
  names a ref).
- **Interfaces** — see §8.3.

### 8.3 Interface identity (underspecified, v1)

virtio-net interface identity is **not enforced in v1**. In practice most
artifacts will have a single interface, making identity moot. MAC-encoding and
PCI-`Path=` matching were both considered and rejected (transport-dependent; no
glob match on MAC; virtio-mmio has no stable PCI path).

> **Implementor's note:** document order is a reasonable implementation
> convention today given single-interface artifacts. A `label:` field on
> virtio-net (matching the guest interface name) is the intended direction and
> will make order irrelevant when implemented (§17).

Insertion order is **not** a spec requirement.

### 8.4 Storage model

`volumes:` is the **one storage primitive**: a persistent mutable block device
(virtio-blk), serial-identified, host-allocated within its size band. Encryption
is **not** in v1 (it implies key management we have not built — §17).

- **`volumes` absent** → default: a single volume named `var`, mounted at `/var`.
  This mirrors read-only-root + writable-`/var`, but as a real persistent volume —
  the carapace stays read-only and mutable state lands in `/var` unless the author
  says otherwise.
- **`volumes: {}`** → no mutable storage at all.
- **explicit dict** → author-defined volumes; each key is a `name`/serial with a
  size band and internal mount fields (§2.3).

Richer models are **guest-side composition** over this primitive, invisible to the
host: a per-directory writable overlay uses a volume as its overlay _upper_ (so
`/usr`, `/etc`, … become writable while the carapace stays read-only); dm-crypt
under the volume's filesystem adds encryption-at-rest. The host manifest only ever
says "provision a persistent volume named X within this size band"; what it
_means_ (mountpoint, fstype, overlay role) is corium's domain, carried in
`config.yaml`'s internal fields. Whole-`/` overlay is deliberately avoided — it
forces overlay assembly into the initramfs before pivot; per-directory overlays
are live, post-pivot mounts (§8.1).

---

# Part VI — Guarantees (cross-cutting)

## 9. corium as contract enforcer

`requirements.yaml` tells the host _what to bring_; **corium verifies it was
brought**, from inside the guest, post-pivot (§8.1). It reads `config.yaml`,
observes what the host actually provided (cpu count, memory, each named volume and
its size, each interface, exposed ports), and:

- a missing or undersized **`required`** resource → corium logs the specific
  shortfall and **powers the instance off**;
- a missing **`recommended`** resource → corium **warns** and continues.

This placement is forced by the threat model: the host is untrusted, so it cannot
be trusted to honor the contract it was handed. The enforcer must be the guest.
`pichi run` may additionally do best-effort pre-flight checks host-side (fail fast
before boot), but those are convenience, not the security boundary — corium's
post-pivot check is authoritative.

## 10. Trust & threat model

The target deployment is **confidential computing**: AMD SEV-SNP or Intel TDX
on a Linux/KVM host. In that model **the host is untrusted** — the carapace
mutual-distrust principle: verification belongs in the guest, and the host is a
potentially-malicious storage/transport medium on both ends.

What this forces (much of this document is the consequences):

- **Inputs must be verifiable by the guest**, not merely provided. Anchored by
  a dm-verity root that comes from the launch measurement, so every block read
  is checked.
- **Outputs carry their own integrity** — a verity root computed by the guest
  at production time, bound to attestation — so the untrusted host can transport
  the bytes but cannot forge or tamper them undetectably.
- **Integrity anchors are never derived from a read of a host-controlled
  medium** (that is a TOCTOU: the host can serve good bytes during hashing and
  keep bad bytes). Output roots are computed from guest-trusted memory. For input
  carapaces, the trust chain is: BE root hash (self-computed from virtiofs input,
  goes into attestation) → `refs.lock` (in the BE) → **parallel verification**
  of host-supplied blobs via manifest hash + dm-verity root (§5.5) — the guest
  never accepts a host-supplied root hash directly, it computes and verifies
  independently.
- **The host cannot influence the build.** It supplies inputs and resources; it
  cannot inject commands or content.
  Substituting an input is possible but never silent — the substituted root is
  bound into provenance — and withholding a resource is a visible DoS, never
  silent corruption.
- **The launch contract is guest-enforced** (§9): the host is told what to
  provide but cannot be trusted to comply, so corium checks.

**Platform reality.** CC exists only on Linux/KVM (SNP/TDX). On the macOS
(HVF) and Windows (WHP) dillo backends the build VM is a _plain_ VM with a
trusted host. So the constructions here are the **CC/KVM path**; on non-CC
backends the same flow runs with a weaker anchor (registry/TLS + a signed
manifest instead of hardware attestation). The CC/attestation platform
requirement is measured into the **PMI** (not host-tweakable, so not in
`requirements.yaml`).

### 10.1 Device-mapper has no anti-rollback (validated)

Per the kernel docs (`Documentation/admin-guide/device-mapper/dm-integrity.rst`,
v6.6): dm-crypt is confidentiality-only; dm-integrity / dm-crypt+integrity
(AEAD) detect _modification_ and _forgery_ and (with `fix_hmac`) bind sector
position, but provide **no replay/rollback protection** — restoring an older
valid `(data, tag)` at the same sector verifies as authentic. dm-verity is the
only freshness anchor and it is read-only (the root, delivered out-of-band, is
what pins content). **There is no DM primitive for anti-rollback of mutable
state.** This is why the build keeps mutable state in CC-protected RAM (§5)
rather than on host-backed disk, and why persistent encrypted volumes are
deferred (§17).

## 11. Provenance manifest (build output)

conglobate emits a **provenance manifest** binding the build inputs to the output
scutes — the artifact that makes the build **attestable** (distinct from the OCI
artifact manifest of §7.1 and the PMI's boot manifest):

```
{ build image PMI measurement,
  build environment carapace root (carries config.yaml + manifests),
  each referenced carapace: artifact digest + root_hash (both verified, §5.5) }
        →  { output scute roots, optional PMI measurement }
```

- **Pre-CC:** conglobate signs the manifest with a build-VM key; trust falls back
  to registry/TLS for the build image.
- **CC:** the build image's anchor is its **PMI measurement**; the BE root is
  self-computed in RAM from the virtiofs input (§4, §5.5) and folded into the
  attestation report; all other input anchors (artifact digests and root hashes)
  are measured via the BE — conglobate folds them all into the attestation report
  alongside the output scute roots it computes in trusted RAM. Hardware-rooted
  source provenance, not signature-rooted.

Either way the build is **reproducible or falsifiable**: same measured inputs →
same outputs (modulo non-deterministic `run:` steps, e.g. network fetches —
see §17), and the manifest commits the claim. The production-time-hashing +
signed-manifest discipline is built **from v1** even pre-CC, because without it
the output is forgeable; only the _anchor_ changes when CC lands.

## 12. Memory elasticity (researched)

The build's working set lives in tmpfs (§5), so guest RAM must grow to fit it.
In CC this is not free: CVM private memory is **not overcommittable or
swappable** (it is hardware-encrypted and must be _accepted_ by the guest —
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

- **v1 = lazy acceptance + a ceiling sized to essentially all free host
  memory.** `pichi build` sets the `maxmem` ceiling to (near) the host's entire
  currently-available memory (free + reclaimable cache — `MemAvailable` on Linux,
  the equivalent elsewhere), less a small reserve for the host itself, rounded
  down; overridable with `--memory`. Sizing high is safe **because of lazy
  acceptance**: an over-large ceiling never over-commits — cost is address
  space/metadata until a page is actually touched — so there is no reason to be
  stingy, and the build gets the largest possible working set without the operator
  guessing. `dillo` launches with that ceiling; the guest leaves memory
  unaccepted and accepts on first touch; with `guest_memfd` the host commits
  private pages on acceptance → demand-commit up to the ceiling. No virtio-mem, no
  new device, no hotplug RFC.
- **The build image runs entirely in RAM** (initramfs-rootfs, §1, §16): its
  userland (conglobate + arma + tooling) plus the tmpfs build working set (§5) all
  live in guest memory. The appliance itself is small — heavy package work runs
  chrooted into the source carapace snapshot, whose tooling comes from that
  carapace — so the initramfs baseline is modest and the ceiling is dominated by
  the working set.
- **Exceeding the ceiling at runtime is not supported in v1** (virtio-mem
  deferred; pc-dimm hotplug RFC/constrained). Over-ceiling → honest OOM → raise
  `maxmem` and re-run. Revisit dynamic grow when virtio-mem-in-CVM lands.
- Non-CC backends (HVF/WHP) get this for free via ordinary host overcommit.

---

# Part VII — Reference

## 13. GPT inside the carapace

The composed carapace block device contains a **GPT** following systemd's
Discoverable Partitions Specification (DDI). Two GPTs exist in the runtime
stack, serving different purposes:

| GPT   | Where                                                          | Identifies                                                | Consumer                             |
| ----- | -------------------------------------------------------------- | --------------------------------------------------------- | ------------------------------------ |
| Outer | Synthesized by the host's carapace device (`dillo-virtio-gpt`) | Individual scutes (DDI PARTUUIDs from the carapace spec)  | Guest's carapace-assembly code       |
| Inner | Inside the composed carapace block device                      | Filesystem partitions (Discoverable Partitions PARTUUIDs) | `systemd-gpt-auto-generator` at boot |

Appliance authors write no fstab and no explicit `root=`; `systemd-gpt-auto-
generator` discovers partitions by well-known PARTUUID. carapaces stay read-only;
writable state lands on volumes (§8.4).

The **outer-GPT PARTUUIDs are deterministic** — derived from each scute's
verity root — which is exactly what `pichi run` already stamps when it builds
the `--gpt` device for `dillo`, and what `dillo-config::derive_ids` hashes into
the disk device-id/disk-guid. Build and run share this path.

## 14. CLI surface

`pichi` mirrors podman/docker for image management. All verbs except `build`
are implemented today.

| Command                                              | Status       | Purpose                                                                                                                                              |
| ---------------------------------------------------- | ------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| `pichi import <raw> <tag>`                           | done         | Convert a raw GPT image into a base carapace. Pure host-side userspace.                                                                              |
| `pichi update [<dir>]`                               | **this doc** | Resolve all carapace references in `pichi.build/*.yaml` → write `pichi.build/refs.lock` with artifact digests and dm-verity root hashes (§2.4). Downloads and validates all manifests and blobs into the local cache. Makes no changes until all content validates. Run before first build and whenever updating a reference. |
| `pichi build [-t <tag>] [--build-image <ref>] <dir>` | **this doc** | Build an artifact from `<dir>/pichi.build/`. Requires all references to be pinned (run `pichi update` first). Boots `--build-image` (falls back to `PICHI_BUILD_IMAGE` env → config default) as the build VM; this is a normal pichi artifact pulled from a registry — the default is a convenience, not special. Anyone can publish and use their own. |
| `pichi run <tag>`                                    | done         | Launch a VM from a tag. Errors if not cached; requires an application artifact (PMI + `requirements.yaml`).                                           |
| `pichi pull` / `push`                                | done         | Move artifacts to/from a registry.                                                                                                                   |
| `pichi images` / `inspect` / `rmi` / `tag`           | done         | Local cache management.                                                                                                                              |

## 15. `pichi import`

Equivalent in spirit to `podman import`: raw bytes in, base carapace out.

**Input:** a raw disk image with an inner GPT per the Discoverable Partitions
Specification. How the user produces it is out of scope (recommended: mkosi).
Pichi does not validate contents beyond a parseable GPT.

**Operation:** pure host-side userspace — no root, no kernel modules, no
mounts. Implemented in `pichi-import` (`cow.rs` emits the dm-snapshot
persistent COW append-only; `verity.rs` computes the dm-verity tree). Output is
a one-scute base carapace (no PMI), usable as a `from:` source.

The same machinery is reused to pack the build _context_ (§4).

## 16. Software to build

### 16.1 Independent components (start now, in parallel)

Each is self-contained with a clear interface and isolated tests; none depends
on another _new_ component, so they can proceed concurrently and they unblock
the integrators below.

| Component          | Where         | Role                                                                                                                                                                                   | Isolated test              |
| ------------------ | ------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------- |
| `pichi-recipe`     | `pichi/deps/` | `pichi.build/` types (`carapace.yaml`/`pmi.yaml`/`config.yaml`) + parse + validate + the `config.yaml`→`requirements.yaml` filter (§2.3). Shared by host (`pichi build`) and guest (corium). | parse/validate/filter fixtures |
| `pichi-provenance` | `pichi/deps/` | Provenance-manifest schema (binds inputs→outputs, §11) + sign/verify with a **pluggable anchor** (signature now, attestation later). Shared by corium (produce) and `pichi build` (verify). | roundtrip + verify         |
| `dillo-virtio-fs`  | `dillo/deps/` | virtio-fs device — read-only input mounts (`$DIR`, manifests) and the untrusted output sink (§5.3). Follows the existing `VirtioDevice` pattern.                                       | device harness + boot test |

### 16.2 Integrators (depend on the above + existing code)

| Component               | Where                                                               | Role                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| ----------------------- | ------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **corium**              | `corium/` (root product; static musl guest binary, like `snuffler`) | The **pichi guest-services binary** — the **runtime** guest agent, shipped in the distro **base layer**. Runs under **systemd (PID 1)**. Capabilities: **two-stage boot** (§8.1) — initramfs-phase root-carapace assembly + `config.yaml`→`/run`, post-pivot contract enforcement (§9) + device/overlay setup; attestation/reporting. Runtime only — it does **not** run builds. (`snuffler` stays the _test-only_ probe.)                                                                                                                                                                                                                                                                          |
| **conglobate**          | `conglobate/` (build-time tool; lives in the build image only)       | The **build driver** — corium's build-time counterpart, runs as **`/init`** (PID 1) in the PMI-only build VM, no systemd (never invoked by a runtime guest). Does init duties (mount `/proc`,`/sys`,`/dev` via devtmpfs, reap children, `poweroff` at the end) and executes the build (§5): serialize virtiofs `$DIR` → erofs → BE carapace in RAM (§4), trust establishment (§5.5), tmpfs dm-snapshot per `carapace.yaml` directive, chroot + run, `cow.rs`+`verity.rs` re-emit, virtiofs output, provenance manifest (§11). For `pmi.yaml`, runs its directives (the author is responsible for copying `config.yaml` into place explicitly, §5.4) and invokes **arma** to seal the `.pmi`. Build-image-only — **not** distributed with the guest. |
| `pichi build`           | `pichi/src/cmd/build.rs`                                            | Host orchestrator (§3): resolve recipes + `from:` + build-image, auto-size memory, stage manifests, launch dillo (two read-only virtiofs mounts + source carapace vGPT devices + virtiofs output), wait, filter `config.yaml`→`requirements.yaml`, package + verify (`pichi-provenance`). Builds on the `pichi run` exec path.                                                                                                                                                                                                                                                                                                                                                                  |
| dillo build wiring      | `dillo`                                                             | Attach source carapaces (each its own vGPT) + two read-only virtiofs mounts (`$DIR`, manifests) + the virtiofs output sink + the `maxmem` ceiling / lazy-acceptance config.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| build image + bootstrap | new                                                                 | A **PMI-only** pichi artifact (no carapace, §1): a kernel (`EROFS`+`DM_VERITY`+`DM_SNAPSHOT`+unaccepted-memory) + an initramfs whose **`/init` is conglobate**, carrying **arma** (PMI packager) + erofs tooling + build tooling (`mount`, dm/verity/loop tools). Everything runs in RAM (§12). The erofs tool version is part of the PMI measurement. Selectable per build (`--build-image`); a one-off host-side `arma` seal of a kernel + conglobate cpio seeds the first.                                                                                                                                                                                                                      |

Reused as-is: `pichi-import` (`cow.rs`, `verity.rs`), the `carapace` crate
(guest assembly), `dillo-virtio-gpt` (vgpt), the `pichi run` device-assembly +
exec path, and the snuffler-style musl build for corium.

## 17. Non-goals, deferrals, open questions

**Deliberately not done.** No host-side mount of scutes (the build VM exists to
keep this off the host). No FUSE for layer mounting (blocks `security.*`
xattrs). No bundled world in pichi (the build image carries tooling). No raw-
image production in pichi (mkosi etc. live outside; `pichi import` consumes the
result). No cmdline injection. No `entrypoint`/fstab (inner GPT + systemd
discovers everything). **No `env:`/runtime image-config** — containers carry a
runtime config object (`ENV`/`CMD`/`ENTRYPOINT`) read when launching their single
entrypoint process; a pichi guest is a full systemd boot with no such object.
Runtime environment for a service belongs in that service's systemd unit
(`Environment=`) inside the carapace; build-time env/workdir is just an inline
`run: FOO=bar make` or `run: cd /x && …`.

**Deferred (build).** **Layer caching** — reuse of previously built scutes keyed
by `hash(parent_rootₙ₋₁ || directive_kind || directive_content)`; v1 rebuilds all
scutes on every `pichi build`. `+zstd` scutes in the build path;
multi-stage (`copy --from`); cross-host build cache; the §5-RAM encrypted-disk
spill (dm-crypt+dm-integrity no-journal, write-once — deferred in favor of
growing trusted RAM, §12); reproducibility tooling for non-deterministic `run:`
steps (frozen package indices, faked time — author's choice today).

**Deferred (device model & runtime).**

- **Volume encryption** — `volumes:` are plaintext-at-rest in v1; dm-crypt
  backing implies key management (sealing/attestation key release) we have not
  built. It is a guest-side composition when it lands (§8.4), not a host-schema
  change.
- **Per-instance device delivery** — corium resolves `config.yaml` from the
  baked-in vendor copy (§8.1.1); overriding it _per launch_ needs a host→guest
  instance-metadata channel that does not exist yet. The search path already
  reserves `/run/corium/config.yaml` as its landing spot.
- **Robust virtio-net identity** — identity is underspecified in v1 (§8.3).
  The intended direction is a `label:` field on `interfaces` entries that matches
  the guest-visible interface name, making document order irrelevant.

**Deferred (launch contract).**

- **Accelerators / GPUs** — the AI-agent target needs declarable GPU
  requirements (count band + class/kind, SR-IOV-vs-passthrough). Same band shape
  as `cpus`/`memory`; not yet specified.
- **Egress / outbound reachability** — `interfaces` declares `ingress` but not the
  guest's dependency on outbound connectivity. A destination allow-list is _not_
  the model (an untrusted host cannot be trusted to enforce egress policy — that
  is guest- or operator-network-enforced); the open question is only whether to
  declare egress as presence (boolean) or coarse scope (`internet`/`internal`),
  banded by severity. Until then, outbound is assumed available.
- **Entropy (virtio-rng)** — a crypto-using guest depends on a host entropy
  source; declaring it as a contract requirement is deferred (assumed present).
(The **CC/attestation platform requirement** is _not_ in `requirements.yaml` — it
is measured into the **PMI** (§6, §10), since it must be attestable, not
host-tweakable. Artifact metadata (name/version/labels) belongs in OCI
annotations, not the contract.)

**Resolved.**

- **Output transport: virtiofs** (untrusted sink, §5.3); a `dillo-virtio-fs`
  device is on the software list.
- **Build-VM memory: auto-sized** from host available memory, overridable with
  `--memory` (§12).
- **Build image is any tagged artifact** (`--build-image`, precedence in §3) —
  which makes bootstrapping and per-project customization free.
- **Runtime guest agent = corium**, the pichi guest-services binary (root
  product, static musl), shipped in the distro **base layer** so it is present in
  every carapace and pulled into the initramfs by its dracut module (§5.4).
  **systemd runs as PID 1**; corium runs under it. Runtime only — it does not run
  builds. `snuffler` remains the test-only probe.
- **Build driver = conglobate**, corium's build-time counterpart (§5, §16) — runs
  the build inside the build VM, never invoked by a runtime guest, lives in the
  build image only. **PMI packager = arma** (existing): conglobate ensures
  `config.yaml` is in the build root (§5.4) and invokes arma to seal the `.pmi`.
  All PMI build directives are ephemeral — only the `.pmi` is retained (§2.2).
- **`config.yaml` delivery = PMI initramfs** (measured), read by corium via the
  search path (§8.1.1). `requirements.yaml` is the host-facing filtered
  projection (§2.3, §7).
- **No vsock RPC** (§5.5): the host is a pure CDN. The BE root hash is
  self-computed by the guest from the virtiofs `$DIR` input and placed directly
  in the attestation report — no host claim needed. Carapace trust flows from
  `refs.lock` (in the BE) via two independent checks (manifest hash + dm-verity
  root) per carapace. The **build environment carapace is ephemeral** — produced
  in-guest RAM from the virtiofs input, never tagged, never cached.

**Still open.**

1. **First build image's base.** The PMI-only build image (§16) needs only a
   minimal initramfs userland — conglobate (`/init`) + arma + dm/verity/snapshot/
   loop/mount tools — _not_ a full distro: package managers etc. come from the
   chrooted source carapace during `run:` steps. So the distro-trademark
   remix/redistribution concern is **much weaker** than for a full rootfs image
   (if any third-party userland ships at all). If a distro base _is_ used, the
   same constraint applies — **Fedora** (Remix allowance) or **Debian**; **Ubuntu
   excluded** (Canonical trademark policy) — but a from-scratch minimal initramfs
   may sidestep it entirely. Open.
2. **Attestation binding details** under SNP/TDX (which measurement register /
   report field carries the manifest digest; the guest-key-in-measurement
   alternative). §11.
3. **Re-verify mid-2026 status** of the SNP pc-dimm hotplug RFC and the
   `PrivateSharedManager` 3-state work before finalizing §12.
4. **`/run` preservation across `switch_root`** (§8.1) — confirm by boot test
   that systemd moves the initramfs `/run` tmpfs into the new root.
