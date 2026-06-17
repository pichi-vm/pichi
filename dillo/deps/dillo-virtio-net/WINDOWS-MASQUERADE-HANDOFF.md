<!-- SPDX-License-Identifier: Apache-2.0 -->
# Handoff: Windows masquerade-to-external-host bug

**Branch:** `feat/virtio-net` (tip `4dbf1c0` at time of writing, +2 commits adding
this doc and the repro test).
**Owner picking this up:** debugging on a Windows machine.
**Delete this file before merge.**

## TL;DR

The user-mode virtio-net backend (`src/user/`) masquerades guest connections
onto host sockets. On **Windows only**, masquerade to a **non-loopback external
host** (e.g. `1.1.1.1:443`) fails — the proxy ends up resetting the guest
connection. Everything else works on Windows: guest→host via the gateway,
inbound port-forwarding, UDP, and the in-process masquerade-**to-loopback** unit
test all pass. Linux (x86_64 + arm64), macOS, and local dev all reach the real
internet fine.

It is **not** an egress firewall: the boot test's host-side pre-check connects
to `1.1.1.1:443` directly from the runner and succeeds, then the guest's
masquerade to the same address through the proxy fails (`external_ok: false`).
So the host has egress; the proxy's path to it is what breaks on Windows.

## CI state

`feat/virtio-net` is **green on ubuntu-24.04, linux-arm64, macos-arm64** and
**red on windows-2025**, failing only at `boots_with_net_user`'s real-internet
leg. All other tests pass on all four lanes. The failing assertion:

```
boots_with_net_user … assertion failed: guest could not reach the real internet
via masquerade (host can reach ["1.1.1.1:443", "1.0.0.1:443"]):
NetBench { tx: …verified, rx: …verified:Some(true), udp_ok: Some(true),
           forward_ok: Some(true), external_ok: Some(false), error: None }
```

## Fast reproduction on Windows (no VM, ~8s)

A host-only repro drives the same masquerade path through the in-process
two-smoltcp-stack harness — **iterate here, not via the 10-minute CI boot**:

```powershell
cargo test -p dillo-virtio-net -- --ignored masquerade_holds_to_real_internet
```

(Needs outbound internet on 443.) It dials `1.1.1.1:443` from the guest-stack;
the proxy masquerades to the real host. **Expected PASS** (connection holds);
on the affected Windows runtime it should **FAIL** ("masquerade … was reset").

- If it **reproduces** (fails on Windows): the bug is in the host-socket
  masquerade path (mio `TcpStream` to an external IP) — debug it here, fast.
- If it **does NOT reproduce** (passes on Windows) but the boot test still
  fails: the bug is VM/WHP-specific (guest networking under WHP), not the
  host-socket path — fall back to the VM repro below and suspect the data path
  under real guest timing.

Test source: `src/user/tests.rs::masquerade_holds_to_real_internet`.

## Full reproduction (VM, needs WHP + internet)

```powershell
cargo test -p dillo --features vm-tests boots_with_net_user -- --nocapture
```

dillo's own logs are captured in the test output (look for `[dillo] … WARN …`),
so logging added in the proxy (below) shows up here too.

## What's been tried (and ruled out)

1. **Egress firewall** — ruled out (host pre-check reaches `1.1.1.1:443`).
2. **Connection-refused detection** — added a 5s connect-timeout
   (`tcp.rs CONNECT_TIMEOUT`). Fixed the separate `connection_refused` test on
   Windows; did not fix masquerade-external.
3. **RST vs FIN on host error** — a host read *error* now RSTs the guest
   (`abort`) instead of FIN (`close`) (`tcp.rs host_to_guest` → `ReadOutcome`).
   Fixed a CloseWait-forever hang; did not fix masquerade-external.
4. **Event-driven connect completion** — `WRITABLE` readiness now drives connect
   completion via `take_error` (`stack.rs tcp_flow_tokens` + the writable-event
   loop; `tcp.rs note_writable`), the canonical cross-platform mio pattern.
   Did **not** fix masquerade-external. This is the surprising one and the best
   place to start.

## Suspect code paths

- `src/user/stack.rs`
  - `connect_host()` — `mio::net::TcpStream::connect(target)` then
    `registry().register(.., READABLE|WRITABLE)`. (Order matches the mio docs.)
  - `promote_listeners()` — creates the flow once the guest handshake completes;
    computes the host target from the smoltcp socket's `local_endpoint()`.
  - `host_target()` — gateway→loopback, else the literal dst.
  - the `run()` event loop's WRITABLE handling → `flow.note_writable()`.
- `src/user/tcp.rs`
  - `ensure_connected()` (peer_addr/take_error poll + 5s timeout backstop),
  - `note_writable()` (take_error on the writable event),
  - `host_to_guest()` / `guest_to_host()` (the `reset` paths),
  - `CONNECT_TIMEOUT`.

## Hypotheses, ranked

1. **mio Windows readiness for an external connect.** Loopback connects
   complete synchronously (peer_addr Ok immediately → works); an external
   connect is async and must be observed via the WRITABLE event. Despite the
   event handling added, the flow still resets — so either the WRITABLE event
   isn't being delivered/routed for this flow, or `take_error()` reports an
   error for a connect that actually succeeded, or `peer_addr()`/the 5s timeout
   fires first. **Instrument which path sets `reset`.**
2. **First-I/O-after-connect error.** mio may report writable before the connect
   truly completes; `take_error` returns `Ok(None)` → we mark connected, then
   the first `host_to_guest` read returns an error → `reset`. Log the read
   outcome and error kind.
3. **A standalone mio sanity check.** A ~15-line program that
   `TcpStream::connect("1.1.1.1:443")`, registers WRITABLE, polls, and prints
   `take_error()`/`peer_addr()` on the writable event will isolate mio behavior
   from our code. If that misbehaves, it's mio/OS; if it's fine, the bug is in
   how we drive it.

## How to instrument (shows up in both repros)

The proxy uses the `log` crate. For the **harness repro**, the simplest is to
temporarily replace the `log::warn!`/add `eprintln!` in the suspect spots (the
harness test prints stderr with `--nocapture`). Add, temporarily:

- in `connect_host`: the `target` and whether `TcpStream::connect` returned
  Ok/Err;
- in `note_writable`: the `take_error()` result and which branch ran;
- in `ensure_connected`: when the 5s timeout fires, and the `peer_addr()` kind;
- in `host_to_guest`/`guest_to_host`: when `reset` is set and the io error kind.

Run `cargo test -p dillo-virtio-net -- --ignored --nocapture
masquerade_holds_to_real_internet` and read which branch resets the flow.

## Validating a fix

1. Harness repro passes on Windows:
   `cargo test -p dillo-virtio-net -- --ignored masquerade_holds_to_real_internet`
2. Boot test passes on Windows:
   `cargo test -p dillo --features vm-tests boots_with_net_user`
3. `cargo fmt --all --check`; build with `-D warnings`.
4. Push; confirm CI green on all four lanes.

## Context / invariants

- The real-internet boot leg is gated on a host egress pre-check
  (`tests/boot.rs`), so it only hard-asserts where the host has egress — but the
  Windows runner *does* have egress, so it asserts there (correctly catching
  this bug).
- Workspace lint is `unsafe_code = "deny"`; the user backend is unsafe-free.
- Don't commit `pichi/deps/pichi-erofs/` or the fuzz generated dirs.
- macOS code type-checks from Linux via
  `cargo +nightly check -Z bindeps --target aarch64-apple-darwin -p dillo-virtio-net`.
