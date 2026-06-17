<!-- SPDX-License-Identifier: Apache-2.0 -->
# dillo-virtio-net

In-process virtio-net device for dillo: a cross-platform frontend plus three
host-side backends, selected with `--net backend=…`.

| backend | platforms | privilege | what it does |
|---------|-----------|-----------|--------------|
| `user` (default) | Linux, macOS, Windows | none | user-mode NAT (smoltcp) with outbound masquerade, guest→host, and inbound port forwarding |
| `bridge` | Linux (tap+bridge), macOS (vmnet) | `CAP_NET_ADMIN` / root+entitlement | put the guest on the host's real L2 segment |
| `macvtap` | Linux | `CAP_NET_ADMIN` | attach to an existing macvtap endpoint |

The device itself (the `VirtioDevice`/queue marshalling) is host-agnostic; each
backend just moves raw Ethernet frames in and out (see [`NetBackend`]).

## `user` — default, no privilege, everywhere

A smoltcp-based transport-terminating proxy. The guest sits on a private
`10.0.2.0/24` (gateway/host alias `10.0.2.2`, guest `10.0.2.15`, DNS `10.0.2.3`,
MTU 1500) and the proxy re-originates its flows as ordinary host sockets driven
by a `mio` event loop. No `/dev/net/tun`, no permissions, identical on every OS.

```
--net                                              # user mode, no forwards
--net backend=user,forwards=[2222:22,udp:5353:53]  # inbound forwards (shorthand)
```

Forward shorthand is `[proto:]port[:guest]` (proto defaults to `tcp`, guest
defaults to the host port, bind defaults to loopback). To bind a non-loopback
address (e.g. expose on `0.0.0.0`), use the JSON `--layout` struct form:

```json
{ "net": { "backend": "user",
           "forwards": [ { "ip": "0.0.0.0", "port": 2222, "guest": 22 } ] } }
```

The guest needs no in-guest tooling: configure it from the kernel cmdline, e.g.
`ip=10.0.2.15::10.0.2.2:255.255.255.0::eth0:off` (needs `CONFIG_IP_PNP=y`).

DNS forwarding (`10.0.2.3`) and a DHCP responder are not implemented in v1;
configure the guest statically and use numeric addresses (or set a resolver).

## `bridge` — join the host L2 segment

Linux creates a tap, enslaves it to the **existing** bridge named by `iface`,
and brings it up. The operator owns the bridge out of band.

macOS uses `vmnet` in bridged mode on the named physical interface (needs root
or the `com.apple.vm.networking` entitlement).

```
--net backend=bridge,iface=br0     # Linux: tap enslaved to bridge br0
--net backend=bridge,iface=en0     # macOS: vmnet bridged onto en0
```

Bridge mode is **unsupported on Windows** (a clean error).

### Setup recipe (Linux) + running the integration test

The bridge datapath needs root and a real bridge, so it is **never exercised in
CI**. Its construction is unit-tested (`braddif_ifreq_layout`,
`open_is_clean_without_privilege`), and a real enslave is covered by an opt-in,
`#[ignore]`d integration test you run manually:

```sh
# 1. Create a bridge (and optionally enslave a NIC to reach the LAN).
sudo ip link add br0 type bridge
sudo ip link set br0 up
# sudo ip link set eth0 master br0     # to bridge onto the physical network

# 2. Run the integration test as root with the bridge named.
sudo DILLO_NET_BRIDGE_TEST=br0 \
  cargo test -p dillo-virtio-net -- --ignored bridge_enslaves_tap

# 3. Tear down when done.
sudo ip link del br0
```

The test creates a tap via `BridgeBackend::open("br0")`, then asserts the tap's
`/sys/class/net/<tap>/master` link points at `br0` and that the tap is `IFF_UP`.

### Setup recipe (macOS) + running the integration test

`vmnet` bridged mode needs root or the `com.apple.vm.networking` entitlement.
The XPC param-dict construction is unit-tested (`bridged_desc_builds`); a real
start is covered by an opt-in, `#[ignore]`d test run manually:

```sh
# `en0` is typically the primary Ethernet/Wi-Fi interface (see `ifconfig`).
sudo DILLO_NET_VMNET_TEST=en0 \
  cargo test -p dillo-virtio-net -- --ignored vmnet_starts
```

The test starts vmnet bridged mode on the interface and writes one frame.

## `macvtap` — Linux only

Attach to an existing macvtap link (the operator creates it out of band):

```sh
sudo ip link add link eth0 name macvtap0 type macvtap mode bridge
sudo ip link set macvtap0 address 52:54:00:ab:cd:ef up
```

```
--net backend=macvtap,iface=macvtap0
```

## Testing

- **Layer 1 (all platforms, no VM):** the in-process two-smoltcp-stack datapath
  harness in `src/user/tests.rs` exercises outbound TCP, gateway→host redirect,
  inbound forward, UDP, and edge cases (RST, half-close, multi-segment); plus
  config and bridge/macvtap construction unit tests.
- **Layer 2 (real guest, CI):** dillo's `boots_with_net` (attach + MAC) and
  `boots_with_net_user` (TCP/UDP/forward round-trips + `NetBench`).
- **Layer 3 (opt-in, never CI):** the bridge integration test above.
- **Layer 5 (fuzz):** `dillo/fuzz` target `net_demux` fuzzes the untrusted
  guest-frame demux.

[`NetBackend`]: src/backend.rs
