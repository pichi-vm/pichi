# Pichi: Image Build System

## Design Document

Status: **locked** — build-VM redesign (thin launcher). This document is
the source of truth for `pichi build`'s host↔guest appliance contract.
Conglobate owns recipes, locks, OCI packaging, and inventory; see
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
an artifact — virtiofs mounts and a named-console CBOR exchange — then waits
for a finished OCI layout in the output mount when the guest exits 0.

Building is launching. The host does not check out Conglobate sources, does
not parse Conglobate recipes, and does not run Conglobate as `/init`.

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
- no host recipe parse;
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
| `recipe`   | yes      | path relative to `subdir` |
| `lock`     | yes      | path relative to `subdir` |
| `subdir`   | no       | default `"."` |
| `location` | no       | optional locator / provenance hint |

No commit field and no mount-tag fields in CBOR. Mount wiring is entirely
the virtiofs convention in §2.3. This supersedes the earlier host one-shot
CBOR push (bytes written before the guest opened the port).

### 2.5 Types and outputs

- Pichi depends on the **Conglobate library for types only** (portable
  recipe / request shapes as needed by the CLI).
- **OCI layout and inventory are Conglobate outputs.** They are not
  defined by, or reassembled through, `pichi-artifact` as the build
  product contract. Host import may still use ecosystem artifact types
  after the guest has published a complete OCI directory.

### 2.6 Conglobate ships a pichi image

The Conglobate **build appliance** is a normal pichi image shipped from
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

Guest work, measurement, recipe execution, and OCI+inventory emission live
in Conglobate (and its appliance wrapper). Pichi's job is the thin launch
and import.

---

## 3. Host flow (detail)

```
operator
  │  pichi build … (ref, recipe, lock, …)
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
                      │  writes OCI (+ inventory) to /run/pichi/output
                      ▼  exit 0
pichi imports OCI from output dir → local tag
```


Failure: non-zero guest exit means no successful product. Do not treat
partial output as an artifact.

Resource sizing (`--memory` / `--cpus` and similar) follows the same
patterns as `pichi run` against the appliance image's launch contract.

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
recipe: <path-relative-to-subdir>
lock: <path-relative-to-subdir>
# optional
subdir: "."          # default when omitted
location: <string>   # optional provenance / locator hint
```

### 4.3 Host failure response

```yaml
v: 1
ok: false
error: <string>
```

`recipe` and `lock` are always relative to `subdir`. The guest resolves
`ref` against the `repo` virtiofs git object store. This document locks
the field set and the v0 request/response delivery; Conglobate owns guest
consumption of the success payload.

---

## 5. Responsibilities

| Component | Owns |
| --------- | ---- |
| **pichi** (`pichi build`) | Thin launcher: dirs, build-port CBOR, dillo launch, import OCI on success |
| **dillo** | VMM + virtiofs + named console wiring |
| **Conglobate image** | Guest appliance: init/wrapper, invoke Conglobate, export OCI+inventory, shut down |
| **Conglobate binary / lib** | Recipes, locks, build execution, OCI layout, `inventory.cbor` (see its DESIGN.md) |
| **fedora / carapace images** | Each ships its own pichi image for bootstrap / base roles |

Pichi does not interpret Conglobate private intermediates to reconstruct
the image. The output mount's OCI layout is the product.

---

## 6. CLI surface (build)

`pichi build` remains the operator entry point. Flags and defaults evolve
with the thin-launcher implementation; the semantic contract is §2.

Related commands unchanged in role:

| Command | Role |
| ------- | ---- |
| `pichi run` | Launch path that build specializes |
| `pichi import` / `pull` / `push` / `images` / … | Image management around the cache build consumes and the tag build produces |
| `pichi update` | Lock maintenance (caller-side); not mid-build resolution |

Recipe and lock formats are Conglobate's, not redefined here.

---

## 7. Trust boundary (host vs guest)

- Host supplies git objects, RO cache bytes, and the build-port response.
- Guest verifies inputs it consumes, runs the build, and emits OCI +
  inventory into the output mount.
- Product integrity is guest-defined (inventory / attestation in the
  Conglobate appliance design). Re-reading mutable host paths as
  authoritative after an untrusted copy would break that binding — the
  successful layout the guest published is what the host imports.
- Confidential-computing attestation of the appliance remains an
  appliance concern; the thin launcher must not claim it by itself.

---

## 8. Pointers

| Doc | Role |
| --- | ---- |
| **This file (`BUILD.md`)** | Locked `pichi build` appliance contract (SoT) |
| [conglobate `DESIGN.md`](https://github.com/pichi-vm/conglobate/blob/main/DESIGN.md) | Builder, recipes, locks, OCI+inventory, sandbox |
| [README.md](README.md) | Operator overview of the pichi CLI |
| [AGENTS.md](AGENTS.md) | Agent / contributor architecture pointers |
| dillo named-console design | Port naming + `pipe:N` inheritance; `dev.pichi.build` is a consumer convention |

---

## 9. Non-goals (for this contract)

- Host-side recipe intelligence or Conglobate checkout
- Host vGPT loops over source carapaces as the build product API
- Mid-build registry pulls inside `pichi build` (cache is prepared first)
- Reintroducing Conglobate-as-`/init` or `$DIR`→erofs host packaging
