# carapace

**Carapace is OverlayFS for block devices.** Where OverlayFS composes filesystem layers in userspace, carapace composes cryptographically-bound block device layers underneath the filesystem. Each layer is a pair of content-addressable files; the composed stack presents as a unified, integrity-protected, read-only block device validated by a single trust anchor.

This repository contains a reference Rust implementation of the carapace assembler — the read-only consumer. The on-disk and chain composition format is specified normatively in [SPEC.md](SPEC.md); producer paths live elsewhere.

## Building & Running

```sh
cargo build --release   # produces target/release/carapace, ~400 KiB stripped

# attach a chain (requires CAP_SYS_ADMIN; typically run as root)
carapace attach --name <NAME> --root <HEX>

# tear it down
carapace detach --name <NAME>
```

The toolchain is pinned via `rust-toolchain.toml` (currently 1.85). All dependencies are vendored through `Cargo.lock`; reproducible builds are a goal.

`attach` discovers partitions by walking `/sys/class/block/*/uevent` for `PARTUUID=` entries — no `--storage` flag, no GPT parser. For loop-mounted images, `losetup --partscan IMAGE` first; the kernel populates sysfs synchronously after that ioctl returns.

`attach` prints `/dev/dm-<minor>` on success — the kernel-synchronous path created by devtmpfs at `DM_DEV_CREATE` time. `/dev/mapper/<NAME>` will appear once udev catches up; both refer to the same device.

## Test

```sh
cargo test --bins                                # unit tests (~1 s)
sudo cargo test --tests -- --test-threads=1     # integration (~45 s, root-required)
```

Integration tests build a real layered ext4 carapace via `tests/fixtures/build_carapace.sh` (a shell + python pipeline using `sgdisk` + `veritysetup` + `dmsetup`), then attach, mount, and verify per-scute file content.

## Runtime requirements

- Linux kernel ≥ 5.8 (`LOOP_CONFIGURE`, dm-verity, dm-snapshot in current form)
- `devtmpfs` (universal on modern distros)
- For test/dev: `cryptsetup-bin`, `gdisk`, `dmsetup`, `e2fsprogs`, `python3`

`udev` is NOT required at runtime — partition discovery is sysfs-driven.

## License

MIT. See [LICENSE-MIT](LICENSE-MIT).
