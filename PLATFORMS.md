# Platform support

dillo targets three host hypervisors — **KVM** (Linux), **WHP** (Windows), and
**HVF** (macOS) — across **x86-64** and **aarch64**. Every device dillo emulates
conforms to the same arma device model (see `arma/docs/device-model.md`); in
particular the serial port is an **MMIO `ns16550a`** on every host and arch (no
legacy port-I/O UART).

## CI coverage

CI builds and **boots a real guest** on the platforms below. Both are required
and must stay green.

| Platform        | Hypervisor | CI runner       | Build | Real boot |
| --------------- | ---------- | --------------- | ----- | --------- |
| Linux x86-64    | KVM        | `ubuntu-24.04`  | ✅    | ✅        |
| Windows x86-64  | WHP        | `windows-2025`  | ✅    | ✅        |

The build is enforced warning-clean (`-D warnings`), and the boot test fails
loudly if a guest does not report back — there is no silent "skip" that turns an
unbooted guest green.

## Not yet in CI

These are **not enabled in CI today**. The limiter is hosted-runner
virtualization, not missing dillo support: GitHub-hosted runners offer nested
virtualization only on Linux x86-64, so the platforms below cannot boot a guest
on hosted infrastructure. Real-boot coverage for them needs **bare-metal /
self-hosted runners** (where the hypervisor runs natively, no nesting required).

| Platform        | Hypervisor | dillo backend | Why not in CI                                   |
| --------------- | ---------- | ------------- | ----------------------------------------------- |
| Linux aarch64   | KVM        | implemented   | hosted arm64 runners expose no `/dev/kvm`; needs bare-metal arm |
| macOS aarch64   | HVF        | implemented   | hosted Apple Silicon has no nested HVF; needs bare-metal Apple Silicon |
| Windows aarch64 | WHP        | builds        | not yet brought up                              |
| macOS x86-64    | —          | none          | HVF/x86 is a separate VMX backend (not written); not planned — Apple is sunsetting Intel |

To bring one online: attach a self-hosted bare-metal runner for that
platform and add it to the CI matrix with the boot step enabled.
