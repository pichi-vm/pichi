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

### 2.4 Named console `dev.pichi.build` (v0 build-port)

Port name: `dev.pichi.build` (dillo named-console convention). The port is
**bidirectional**: launch dillo with both `rx=pipe:…` and `tx=pipe:…`
(not rx-only). dillo named MULTIPORT ports supply the pipe inheritance
model (`pipe:N` = inherited host FD/handle).

Wire (self-delimiting CBOR, no length prefix; **one round trip**; no
streaming or multiplexing in v0):

1. **Guest → host** — one request map: `{"v":1,"op":"build-input"}`
2. **Host → guest** — one response map:
   - success: `{"v":1,"ok":true, …}` plus build-input fields below
   - failure: `{"v":1,"ok":false,"error":"…"}`

The host prepares the build-input payload **before** launch, but writes
the response only **after** the guest request arrives (hold-until-open /
Linux discard of early console bytes).

Build-input fields (success response body, in addition to `v` / `ok`):

| Field      | Required | Notes |
| ---------- | -------- | ----- |
| `ref`      | yes      | git ref / commit identity the guest resolves in `repo` |
| `plan`     | yes      | path relative to `context`; default `pichi.yaml` |
| `lock`     | yes      | path relative to `context`; default `pichi.lock` |
| `context`  | no       | default `"."` |
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
                      │  writes {"v":1,"op":"build-input"}
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

Limits: 4,096 files; 4 KiB for `oci-layout`; 1 MiB each for index, manifest,
and config; 16 GiB per blob; 64 GiB total input and total expanded scute
bytes. Compressed scutes retain the streaming decoder's 128 MiB window limit.
Decoded output is bounded by available scratch capacity, with a declared
block extent checked before expansion. Concurrent allocations and hash-tree
writes can still exhaust that filesystem; write failures abort publication.

For **every scute**, including cache hits, the host rebuilds dm-verity from
the cow bytes and the full manifest salt. Reconstruction uses SHA-256,
format 1/hash type 1, 4,096-byte data/hash blocks, and a zero UUID. The host
requires a complete salt chain from the zero-prefix base to exactly one tip;
manifest layer order does not determine that chain. A tip-hash annotation
is never used as a trust anchor. Only rebuilt sidecars enter the cache.

**Guest verity choice (#26):** guest verity is optional and never
authoritative. If supplied as `blobs/sha256/<cow-digest>.verity`, it must
match the rebuilt file byte for byte (header and tree); any mismatch fails
the import.

### Transitional `pichi.lock` host glue (Conglobate Follow-up A)

The host reads the lock named by `-l`/`--lock` (default `pichi.lock`
under `--context`) solely for **source-carapace reference keys + index
digest pins** used to wire source-carapace vGPTs as guest build inputs.
They do not select or prepend layers during output import. Plan parse of
`pichi.yaml` stays on the guest (§2.2). Shape:

```yaml
version: 1
images:
  qualification.local/fedora:43:
    index: sha256:<digest>
```

Smoke context (Conglobate `f88a7215`): seed the exact index blob for
`qualification.local/fedora:43` →
`sha256:4e335e141a3f950ed68c68f23c112930c51af5ec9658d1263353eb229a6429d1`
(343 index bytes; placeholder inner manifest `sha256:` + 32×`0x11`) and
supply a tiny imported carapace behind that descriptor. The default image is
`ghcr.io/pichi-vm/conglobate@sha256:b9e26efd9d81ecb5b1a950e3f4891e9a04fbc7d9d222444621bccc95fad3babd`.
The smoke checks clean poweroff and import of an inspectable OCI output tag.
Import rejects any retired output files. Source vGPT wiring remains an
input compatibility measure; the guest manifest determines output layers.


---

## 4. Build-port CBOR (logical shape)

Encoding is self-delimiting CBOR (no length prefix). Field names are
strings. One guest request, then one host response.

### 4.1 Guest request

```yaml
v: 1
op: "build-input"
```

### 4.2 Host success response

```yaml
v: 1
ok: true
# required build-input fields
ref: <git-ref-or-full-commit-id>
plan: pichi.yaml     # default; -p/--plan overrides, still relative to context
lock: pichi.lock     # default; -l/--lock overrides, still relative to context
# optional
context: "."        # default when omitted
location: <string>   # optional provenance / locator hint
```

### 4.3 Host failure response

```yaml
v: 1
ok: false
error: <string>
```

`plan` and `lock` are relative to `context`. Defaults are `pichi.yaml`
and `pichi.lock`. `-p`/`--plan` and `-l`/`--lock` supply different
relative paths (absolute paths are rejected); the host sends those resolved strings. The guest
resolves `ref` against the `repo` virtiofs git object store. This
document locks the field set and the v0 request/response delivery;
Conglobate owns guest consumption of the success payload.

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
