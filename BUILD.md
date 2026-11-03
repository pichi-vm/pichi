# Pichi: Image Build System

## Design Document

Status: **locked** — build-VM redesign (thin launcher). This document is
the source of truth for `pichi build`'s host↔guest appliance contract.
Conglobate owns plans, locks, OCI packaging, and build identity annotations; see
[conglobate `DESIGN.md`](https://github.com/pichi-vm/conglobate/blob/main/DESIGN.md).

> History: supersedes the earlier host-heavy BUILD.md (Conglobate-as-`/init`,
> virtiofs `$DIR`+manifests, guest `$DIR`→erofs serialization, host
> reassembly of OCI from scutes/manifests). Those stories are **retired**.
> Durable ideas kept: build-as-launch, measured guest work, Conglobate as
> the builder.

---

## 1. Thesis

**`pichi build` is a specialization of `pichi run`.**

Both share the same launch path: resolve a pichi image, prepare a dillo
device set, and boot. Build adds only the wires the guest needs to produce
an artifact — virtiofs (shared host folders) mounts and a named-console
CBOR (compact binary format) exchange — then waits
for a finished OCI layout (the standard folder format for container images) in the output mount when the guest exits 0.

Building is launching. The host does not check out Conglobate sources, does
not parse Conglobate plans, and does not run Conglobate as `/init`.

These restrictions apply to `pichi build`. The separate `pichi update [DIR]`
command reads the working-tree plan through shared `conglobate::plan` types
and maintains its lock on the host. It uses `--plan` and `--lock` relative to
DIR, defaults to refreshing images with `--pull=always`, and supports the
other existing pull policies. It needs neither Git nor a Conglobate process
or appliance. Commit the plan and generated lock together before building.

---

## 2. Locked contract

These seven points are locked. Implementation and docs conform to them.

### 2.1 Build specializes run

Same launch path as `pichi run`. Build adds:

- virtiofs wires (`repo`, `cache`, `output`);
- bidirectional named console port `dev.pichi.build` (v0 CBOR request/response);
- wait for guest exit 0, then take the OCI layout from the output mount.

### 2.2 Thin launcher

Host steps, in order:

1. Prepare an **empty** host output directory (RW).
2. Prepare a **read-only** cache of pichi images / carapaces the build needs.
3. Prepare the build-input payload (see §4) — do not push it yet.
4. Launch **Conglobate's pichi image** via dillo with both rx and tx pipe
   ends on named console `dev.pichi.build` (bidirectional; not rx-only).
5. After dillo starts, wait for the guest's build-input request on the
   port, then write the prepared response (hold-until-open / Linux discard).
6. On guest exit 0, take the OCI layout from the output directory and import
   / tag it locally as needed.

Explicitly **not** on the host:

- no host checkout of Conglobate;
- no host plan parse;
- no Conglobate-as-`/init`.

### 2.3 Virtiofs tags (by convention)

| Tag      | Host role                         | Guest path              | Mode |
| -------- | --------------------------------- | ----------------------- | ---- |
| `repo`   | git object store                  | `/run/pichi/repo`       | RO   |
| `cache`  | image / carapace cache            | `/run/pichi/cache`      | RO   |
| `output` | empty dir; receives OCI on success | `/run/pichi/output`    | RW   |

Tag names and guest mount points are convention. The CBOR does not carry
mount tags or commit identities for those mounts.

### 2.4 Named console `dev.pichi.build` (build command V1)

Port name: `dev.pichi.build` (dillo named-console convention). The port is
**bidirectional**: launch dillo with both `rx=pipe:…` and `tx=pipe:…`
(not rx-only). dillo named MULTIPORT ports supply the pipe inheritance
model (`pipe:N` = inherited host FD/handle).

Wire (self-delimiting CBOR, no length prefix; **one round trip**; no
streaming or multiplexing):

1. **Guest → host** — one request array: `["build", 1, null]`
2. **Host → guest** — one response array:
   - success: `[true, Input]`, with the build-input map below
   - failure: `[false, "error message"]`

The host prepares the build-input payload **before** launch, but writes
the response only **after** the guest request arrives (hold-until-open /
Linux discard of early console bytes).

Build-input fields (the `Input` map in the success response):

| Field      | Required | Notes |
| ---------- | -------- | ----- |
| `ref`      | yes      | git ref / commit identity the guest resolves in `repo` |
| `plan`     | yes      | path relative to `context`; default `pichi.yaml` |
| `lock`     | yes      | path relative to `context`; default `pichi.lock` |
| `context`  | yes      | `"."` explicitly selects the repository root |
| `location` | no       | optional locator / provenance hint |

Operator defaults are `pichi.yaml` and `pichi.lock`. `-p`/`--plan` and
`-l`/`--lock` override those paths; both must be relative (absolute
paths are rejected). The CBOR fields are the resolved
relative paths (for example `pichi.yaml` and `pichi.lock` when the
operator passes neither flag).

No commit field and no mount-tag fields in CBOR. Mount wiring is entirely
the virtiofs convention in §2.3. This supersedes the earlier host one-shot
CBOR push (bytes written before the guest opened the port).

### 2.5 Types and outputs

- Pichi depends on the **Conglobate library for types only** (portable
  plan / request shapes as needed by the CLI).
- **The annotated OCI layout is the Conglobate output.** It is not
  defined by, or reassembled through, `pichi-artifact` as the build
  product contract. Host import may still use ecosystem artifact types
  after the guest has published a complete OCI directory.

Build identity lives on the **image manifest**, rather than only on an
index, index descriptor, or configuration blob. Conglobate's annotation
contract is:

| Annotation | Value |
| ---------- | ----- |
| `dev.pichi.build.commit` | Required: full resolved Git commit, `sha1:<hex>` or `sha256:<hex>` |
| `dev.pichi.build.context` | Required: path in that commit; `.` selects the root |
| `dev.pichi.build.plan` | Required: plan path relative to the context |
| `dev.pichi.build.lock` | Required: lock path relative to the context |
| `dev.pichi.build.location` | Optional: repository provenance / locator hint |

The guest resolves the requested `ref` to the annotated commit. Location
is a locator, not an authenticated identity; omit it when absent and do
not include embedded credentials. The commit and selected paths bind the
plan and lock bytes; there are no separate tree or object-format keys or
plan/lock content-digest annotations.

Conglobate emits only the OCI layout (`oci-layout`, `index.json`, and
`blobs/`). The host rejects `build.yaml`, `inventory.cbor`, and loose disk
files beside it. Build identity is carried by the manifest annotations.

### 2.6 Conglobate ships a pichi image

The Conglobate **build appliance** (its build machine image) is a normal pichi image shipped from
the Conglobate repository (bootstrap). Fedora and carapace similarly each
ship a pichi image for their roles. `pichi build` launches that image the
same way `pichi run` launches any other bootable artifact.

### 2.7 Retired stories (must not return)

Do **not** document or reintroduce:

- Conglobate as `/init` of a PMI-only build image;
- virtiofs `$DIR` + separate manifests mount as the product API;
- guest serializes `$DIR` → erofs as the measured-context product contract;
- host packages OCI from scutes / manifests / virtiofs leftovers after the
  guest exits.

Guest work, measurement, plan execution, and annotated OCI emission live
in Conglobate (and its appliance wrapper). Pichi's job is the thin launch
and import.

---

## 3. Host flow (detail)

```
operator
  │  pichi build <dir> --ref <ref> -t <tag>
  ▼
pichi (thin launcher)
  │  empty output dir
  │  RO cache
  │  prepare build-input payload
  │  virtiofs: repo / cache / output
  │  named console dev.pichi.build (rx+tx pipes)
  ▼
dillo  ──boots──►  Conglobate pichi image
                      │  writes ["build", 1, null]
                      │     ◄── host success/failure CBOR
                      │  builds from /run/pichi/{repo,cache}
                      │  writes annotated OCI to /run/pichi/output
                      ▼  exit 0
pichi imports OCI from output dir → local tag
```


Failure: non-zero guest exit means no successful product. Do not treat
partial output as an artifact.

Resource sizing (`--memory` / `--cpus` and similar) follows the same
patterns as `pichi run` against the appliance image's launch contract.

The appliance is `-b`/`--build-image` when the operator passes it,
otherwise the digest-pinned default (digest means the image content fingerprint) (`DEFAULT_BUILD_IMAGE` in
`src/cmd/build.rs`). If that ref is not in the local cache, the host
pulls it with the same path as `pichi pull` (`--pull=missing`) and then
continues the build. The first build fetches the appliance.

Run and build share cached-reference resolution. They accept bare manifests,
stored image indexes, and index-digest references whose pull selected and
cached a platform manifest. An informational tag in `name:tag@digest` does
not change the digest selection.

### OCI import and verity verification

After guest exit 0, `pichi build` imports the complete OCI layout at the
output mount root using the same reader as `pichi load`. A build must have
exactly one image manifest in `index.json`. The host preserves its original
bytes and digest, including inherited layers, and applies only the requested
local tag. It never reconstructs a manifest or prepends cached source scutes.

Import first copies verified bytes to a private directory. It requires the
`oci-layout` version 1.0.0 marker, schema-2 OCI index and image manifest media
types, pichi artifact/config/layer types, and every referenced blob with its
exact SHA-256 digest and size. Nested indexes, duplicate layers/manifests,
unreferenced blobs, unknown files/directories, symlinks, special files, and
Unix hard links fail closed. All file access is confined to the opened output
directory. No output tag changes until the complete layout passes validation.

Control-data limits remain 4,096 files, 4 KiB for `oci-layout`, and 1 MiB
each for index, manifest, and config. Bulk input and expanded-scute bounds
come from available scratch filesystem capacity, independently of Carapace
geometry. The decoder retains its 128 MiB memory-window limit. These storage
checks are preflight bounds, not reservations: concurrent writes, copies,
and reconstructed verity data can still exhaust storage. Such I/O errors
abort the import and leave tags unchanged.

Every scute must declare a positive `dev.pichi.scute.verity.data-blocks`
count. The count describes the decoded data rounded up to 4,096-byte blocks;
it must agree with the raw descriptor size or the actual decoded length.
Compressed inputs feed the verity formatter while they are decoded, using
the declared count to place the hash tree. Missing or zero counts fail
before any output tag changes, including when the blob is already cached.
Prepared data files include the final block's zero padding so they expose
the full extent covered by its verity tree. The encoded OCI blob retains its
original bytes and digest. Raw inputs with a partial final block also receive
this padded data file; aligned raw inputs use their original cached blob.

Pull and load retain the registry blob with its original OCI digest and prepare
an uncompressed, block-aligned data file and its verity tree. For aligned raw
scutes, the original blob is already the prepared data file. Compressed scutes
and raw scutes with a partial final block have a separate prepared data file.
The registry index and manifest retain their original bytes and descriptors.

Run and build use the same file selection and pass prepared data and verity
files to Dillo's GPT backend. A build attaches only the appliance root. Source
images are available through the read-only cache. If preparation is missing,
Pichi recreates it from the retained blob through the same bounded path. Incorrect prepared extents fail
before Dillo starts. Dillo never receives compressed COW bytes.

Pichi constructs verity from the prepared data and full manifest salt using
SHA-256, format 1/hash type 1, 4,096-byte data/hash blocks, and a zero UUID.
Pull, run and build recompute data roots when reusing prepared files; they do
not rewrite existing hash trees. Load snapshots the supplied OCI blobs and
rebuilds every tree before importing it.

Scutes must appear in base-to-tip manifest order: the base has a zero parent
prefix, and each later prefix must match the preceding reconstructed root.
PMI and native bootconfig descriptors do not participate in scute order. The
reconstructed tip must equal the manifest's `dev.pichi.carapace.verity.hash`. Missing,
malformed, zero or incorrect commitments fail before pull/load publish a tag
or run/build launch Dillo. This single commitment covers the salt-linked
ancestry; there are no per-scute root annotations. The guest verifies data
blocks lazily against its trusted root. The v1 format identifiers are unchanged.

`pichi import raw` records the computed chain root in the manifest annotation
`dev.pichi.carapace.verity.hash`. Each import generates a fresh 32-byte salt
suffix using the operating system's cryptographic random source, after the
32-byte zero parent prefix. Importing identical data again preserves the COW
blob digest but produces a different salt, verity tree, root and manifest.
Loading or reusing an existing layer preserves its recorded salt.
`pichi import pmi --carapace` preserves this
root when adding boot components. Caller annotations cannot replace it.
PMI-only images carry no carapace root. Bootconfig is an optional accompanying
component with media type `application/vnd.pichi.bootconfig.v1+zstd`. It cannot
exist without PMI. Its compressed bytes are authenticated by the OCI descriptor.
Decompression yields native Linux text, preserving imported comments and spelling.
Empty configuration is omitted. Pichi does not interpret YAML overlays.

Run and build prepare that text for Dillo's `--bootconfig`, without padding
or a trailer. Dillo checks its reservation and applies native framing at launch.
PMI bytes, embedded
platform DTB and minimal command line remain immutable. Corium reads
`corium.root` from bootconfig, assembles the whole opaque Carapace chain and
mounts the discovered GPT root through its read-only partition view. Pichi's
outer scute/verity disk remains unchanged.

**Guest verity choice (#26):** guest verity is optional and never
authoritative. If supplied as `blobs/sha256/<cow-digest>.verity`, it must
match the rebuilt file byte for byte (header and tree); any mismatch fails
the import.

### Locked build inputs

The host reads the lock named by `-l`/`--lock` (default `pichi.lock`
under `--context`) for image reference keys and index digest pins. Inputs may
contain roots, boot components or both. Prepare any scutes in the cache without
attaching source images as VM disks; component-only inputs need no root. The
lock does not select or prepend layers during output import. Plan parse of
`pichi.yaml` stays on the guest (§2.2). Shape:

```yaml
version: 1
images:
  qualification.local/fedora:43: sha256:<digest>
```

The smoke test imports a small GPT disk, retains its actual platform index,
and uses `pichi update` to produce the lock. It prunes the cache and builds
from the committed lock. `PICHI_TEST_BUILD_IMAGE_LAYOUT` selects a rebuilt
OCI appliance layout for cross-project qualification; otherwise the smoke
uses the shipping default. Staged plans require an appliance built with the
matching Conglobate engine; `--build-image` selects it explicitly. The default image is
`ghcr.io/pichi-vm/conglobate@sha256:2e23b09b262e4d79b2741c43b6c5264aa4b1d19d16ac0f01c83bb0def630bb09`.
This amd64/arm64 index is built from Conglobate revision
`0b1e0ad7721d38315a4495076c010429c40bdebf` and uses the staged plan, GPT
root, embedded-DTB PMI and native bootconfig contracts described here.
The library revision in `Cargo.toml` requires `version: 1` in plans and the
flat digest lock shown above. The shipping default predates this format; select
an appliance built from the matching library revision with `--build-image`
before using these documents.
The appliance also includes scute compaction, journal-free ext4 creation and
command network isolation from the separate Conglobate integration branches.
The smoke checks clean poweroff and import of an inspectable OCI output tag.
Import rejects any retired output files. The guest manifest determines the
output layers; host-side input preparation never adds ancestry.

### Staged-image acceptance

The `staged_images` test runs the complete creation and composition path on
Linux x86-64 and ARM64. It requires usable KVM, at least 1536 free 2 MiB
hugepages for a 3 GiB build guest, Git, `strip`, and network access to Fedora's
package repositories. Boots use 2 GiB. Builds retain both the original and
packed scute during compaction. Missing prerequisites fail the test. No host block
devices or mounts are created by the harness.

Use a rebuilt appliance OCI layout, a static musl build of the pinned Arma,
and a newly compiled static release Corium for qualification. Arma runs inside
Fedora, so it must not depend on the host's dynamic loader. Corium's release
binary replaces the test artifact's firmware while preserving the selected root.
The platform CI workflow builds both guest tools for the native architecture:

```sh
PICHI_TEST_BUILD_IMAGE_LAYOUT=/path/to/appliance-oci \
PICHI_TEST_GUEST_ARMA=/path/to/musl/release/arma \
PICHI_TEST_REPLACEMENT_CORIUM=/path/to/release/corium \
PICHI_TEST_EVIDENCE=/path/to/evidence \
cargo +nightly-2026-07-06 test --locked --features vm-tests --test staged_images -- --nocapture
```

The suite installs a fresh Fedora root, generates its PMI inside a stage,
and boots through Corium to systemd. It independently reads both GPT copies
from the emitted scutes and compares their root geometry with the live
device-mapper table. It checks extension, host/sibling copies, ESP and tmpfs
lifetime, unchanged parents, firmware replacement, configuration overlay
and replacement, all five component combinations, and failed builds without
published output. Kernel bootconfig is compared with the selected artifact;
Linux displays bare keys as empty strings in `/proc/bootconfig`, while the
artifact checks retain the distinction. Logs, plans, locks and output
manifests are retained under `PICHI_TEST_EVIDENCE` when it is set.

---

## 4. Build-port CBOR (logical shape)

Encoding is self-delimiting CBOR (no length prefix). One guest request, then
one host response, each a single array. Command names and versions identify
request/response contracts; the protocol has no global version field.

### 4.1 Guest request

```json
["build", 1, null]
```

The three elements are command name, `u16` command version, and request body.
Build V1 has a unit body, encoded as null. The host decodes the shared
`conglobate::launch::Requests` enum; unsupported command/version pairs,
wrong bodies, and incorrect array lengths fail before success is sent.

### 4.2 Host success response

```json
[true, {
  "ref": "refs/heads/main",
  "plan": "pichi.yaml",
  "lock": "pichi.lock",
  "context": "."
}]
```

`ref`, `plan`, `lock`, and `context` are required strings. Path and Git parsers
validate their values when they are used. An optional `location` string carries
provenance; omitted or null means absent, and serialization
omits absent locations. Unknown and duplicate fields are rejected. Rust uses
`Input.r#ref` for the `ref` key and `Response<Input>::Ok` for success.

### 4.3 Host failure response

```json
[false, "error message"]
```

The error is a string. Rust uses `Response<Input>::Err`;
conversions to and from standard `Result` are available. The boolean selects
the payload type. The response array must have exactly two elements.

`plan` and `lock` are relative to `context`. Operator defaults remain
`pichi.yaml` and `pichi.lock`; the host always sends an explicit context.
`-p`/`--plan` and `-l`/`--lock` supply different relative paths. The guest
resolves `ref` against the `repo` virtiofs Git object store, then resolves the
selected paths in that committed tree. Context resolution must stay inside
the repository, and plan/lock resolution inside the selected context, even
through symlinks and parent components. Pichi uses capability-relative opens
for its host-side lock read so context and lock symlinks cannot escape.

The command types live in `conglobate::launch`; plans, locks and remote
references live in `conglobate::plan`. These array envelopes replace the old map
format. The default appliance digest must identify an image with this command
contract: updating the library/source pin alone does not update the guest.

---

## 5. Responsibilities

| Component | Owns |
| --------- | ---- |
| **pichi** (`pichi build`) | Thin launcher: dirs, build-port CBOR, dillo launch, import OCI on success |
| **dillo** | VMM + virtiofs + named console wiring |
| **Conglobate image** | Guest appliance: init/wrapper, invoke Conglobate, export annotated OCI, shut down |
| **Conglobate binary / lib** | Plans, locks, build execution, OCI layout, image-manifest build annotations (see its DESIGN.md) |
| **fedora / carapace images** | Each ships its own pichi image for bootstrap / base roles |

Pichi does not interpret Conglobate private intermediates to reconstruct
the image. The output mount's OCI layout is the product.

---

## 6. CLI surface (build)

`pichi build <dir> --ref <ref> -t <tag>` is the operator entry point.
The semantic contract is §2.

| Input | Default | Override |
| ----- | ------- | -------- |
| plan | `pichi.yaml` relative to `--context` | `-p` / `--plan` |
| lock | `pichi.lock` relative to `--context` | `-l` / `--lock` |
| appliance | digest-pinned `DEFAULT_BUILD_IMAGE` | `-b` / `--build-image` |
| context | `.` | `--context` |

`--context` defaults to `.` on the guest when the field is omitted.
`--location`, `--memory`, and `--cpus` stay optional. The happy path
uses `pichi.yaml` and `pichi.lock`.

`-b`/`--build-image` help text is "Build appliance image." When the
resolved appliance is missing from the cache, `pichi build` pulls it
and continues. The first build fetches that appliance.

Related commands unchanged in role:

| Command | Role |
| ------- | ---- |
| `pichi run` | Launch path that build specializes |
| `pichi import` / `pull` / `push` / `images` / … | Image management around the cache build consumes and the tag build produces |
| `pichi update` | Lock maintenance (caller-side); not mid-build resolution |

Plan and lock formats are Conglobate's, not redefined here.

---

## 7. Trust boundary (host vs guest)

- Host supplies git objects, RO cache bytes, and the build-port response.
- Guest verifies inputs it consumes, runs the build, and emits an annotated
  OCI layout into the output mount.
- Product integrity is guest-defined (image-manifest binding annotations
  and future attestation in the Conglobate appliance design). Re-reading
  mutable host paths as authoritative after an untrusted copy would break
  that binding — the successful layout the guest published is what the
  host imports.
- Confidential-computing attestation of the appliance remains an
  appliance concern; the thin launcher must not claim it by itself.

---

## 8. Pointers

| Doc | Role |
| --- | ---- |
| **This file (`BUILD.md`)** | Locked `pichi build` appliance contract (SoT) |
| [conglobate `DESIGN.md`](https://github.com/pichi-vm/conglobate/blob/main/DESIGN.md) | Builder, plans, locks, annotated OCI, sandbox |
| [README.md](README.md) | Operator overview of the pichi CLI |
| [AGENTS.md](AGENTS.md) | Agent / contributor architecture pointers |
| dillo named-console design | Port naming + `pipe:N` inheritance; `dev.pichi.build` is a consumer convention |

---

## 9. Non-goals (for this contract)

- Host-side plan intelligence or Conglobate checkout
- Host vGPT loops over source carapaces as the build product API
- Mid-build registry pulls inside `pichi build` (cache is prepared first)
- Reintroducing Conglobate-as-`/init` or `$DIR`→erofs host packaging
