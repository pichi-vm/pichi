#!/bin/sh
# SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
# SPDX-License-Identifier: Apache-2.0

# Build conglobate + assemble the official pichi build image initramfs and
# kernel. Runs INSIDE an arch-matched Alpine container (see build-image.sh):
# the host stages the conglobate source tree in /work/src and collects the
# kernel, config and uncompressed cpio from /work/out afterwards, then seals
# them into the PMI with arma on the host (arma needs only the files, not a
# musl runtime).
#
# conglobate is compiled here, in the Alpine context, so it links Alpine's musl
# dynamically and shares it (and libgcc_s) with mkfs.erofs instead of bundling
# a second static libc. Requires an Alpine new enough for conglobate's edition
# (2024 -> rust >= 1.85; alpine:3.23 ships 1.91).
#
# The build image is a PMI-only pichi VM whose /init is conglobate. conglobate
# reads each source carapace from a virtio-blk GPT disk (virtio_blk) through
# dm-verity + dm-snapshot over a dm-zero origin, mounts ext4 (the working
# snapshot) and virtiofs
# (/context, /output), and writes erofs purely in userspace via mkfs.erofs — so
# the kernel needs virtio_blk, virtio_net (conglobate does in-process DHCP for
# `network:` stages), fuse, virtiofs, ext4, loop and the dm (snapshot + verity)
# stack, but NOT the erofs module. apk linux-virt ships them all as modules; the
# VIRTIO PCI/MMIO transport is built in.
set -eu

: "${ALPINE_PKGS:=cargo git linux-virt erofs-utils kmod cpio util-linux device-mapper cryptsetup}"
apk add --no-cache $ALPINE_PKGS >/dev/null
KV=$(ls /lib/modules)
echo "assemble: kernel $KV ($(uname -m)), $(rustc --version)"

IRFS=/tmp/irfs
rm -rf "$IRFS"
mkdir -p "$IRFS/modules" "$IRFS/usr/bin"

# /init = conglobate, built in the Alpine context (dynamic musl).
(cd /work/src && cargo build --release --bin conglobate)
install -m 0755 /work/src/target/release/conglobate "$IRFS/init"

# Kernel modules in dependency order. `modprobe --show-depends` lists each
# module's prerequisites before it; concatenating the top-level modules and
# de-duplicating (first occurrence wins) yields a valid global load order.
# crc/crypto helpers are arch-specific, so resolve them here in the
# target-arch container rather than hardcoding a list.
modlist=/tmp/modlist
: >"$modlist"
for top in virtio_blk virtio_net virtiofs ext4 loop dm-snapshot dm-verity dm-zero; do
	modprobe -S "$KV" --show-depends "$top" 2>/dev/null \
		| sed -n 's|^insmod \([^ ]*\).*|\1|p' >>"$modlist"
done

i=0
awk '!seen[$0]++' "$modlist" | while read -r ko; do
	base=$(basename "$ko")
	base=${base%.gz}
	base=${base%.ko}
	nn=$(printf '%02d' "$i")
	zcat "$ko" >"$IRFS/modules/${nn}-${base}.ko"
	echo "  module ${nn}-${base}.ko"
	i=$((i + 1))
done

# Userspace tools /init shells out to during the build, each bundled to
# /usr/bin (on the default exec search path) plus every shared library it and
# /init need (musl loader, libc, libgcc_s, libuuid/liblz4/libz, libcrypto, …):
#   mkfs.erofs  — output erofs writer
#   losetup     — COW loop device (op 3; util-linux, not busybox: needs --show)
#   blockdev    — origin size in sectors (op 3)
#   dmsetup     — the writable dm-snapshot (op 3)
#   veritysetup — the dm-verity seal (op 4)
tool_bins=""
for t in mkfs.erofs losetup blockdev dmsetup veritysetup; do
	p=$(command -v "$t") || { echo "assemble: required tool '$t' not found" >&2; exit 1; }
	install -D -m 0755 "$p" "$IRFS/usr/bin/$(basename "$p")"
	tool_bins="$tool_bins $p"
	echo "  bin $p -> /usr/bin/$(basename "$p")"
done
needed_libs() {
	ldd "$1" 2>/dev/null | awk '
		/=>/ && $3 ~ /^\// { print $3 }   # libfoo.so => /path
		!/=>/ && $1 ~ /^\// { print $1 }  # /lib/ld-musl-*.so (loader)
	'
}
{
	needed_libs "$IRFS/init"
	for b in $tool_bins; do needed_libs "$b"; done
} | sort -u | while read -r lib; do
	install -D -m 0755 "$lib" "$IRFS$lib"
	echo "  lib $lib"
done

# initramfs (newc cpio, UNcompressed — arma rejects compressed initrds; the
# PMI is no-compression by design) + kernel + its config (arma reads the
# config to size virtio-mmio transport slots).
mkdir -p /work/out
(cd "$IRFS" && find . | cpio -o -H newc 2>/dev/null) >/work/out/initramfs.cpio
cp "/boot/vmlinuz-virt" /work/out/vmlinuz
cp "/boot/config-$KV" /work/out/config
echo "assemble: initramfs $(wc -c </work/out/initramfs.cpio) bytes"
echo "assemble: kernel + config + cpio staged in /work/out"
