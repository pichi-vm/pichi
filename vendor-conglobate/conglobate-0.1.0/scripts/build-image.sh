#!/usr/bin/env bash
# SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
# SPDX-License-Identifier: Apache-2.0

# Build one architecture's official pichi build image (a PMI-only pichi VM
# whose /init is conglobate) and write it to $OUT/boot.pmi.
#
# Runs natively on a runner of the target arch — cross-compilation and qemu are
# deliberately avoided, so the CI matrix uses one native runner per arch
# (ubuntu-24.04 + ubuntu-24.04-arm). Steps:
#   1. assemble the initramfs + collect the kernel inside an arch-matched
#      Alpine container (scripts/assemble.sh), which also compiles conglobate
#      in the Alpine context so /init links Alpine's musl dynamically;
#   2. build arma on the host and seal the kernel + initramfs into the PMI
#      (arma only consumes the files, so it needs no musl runtime).
#
# Inputs: ARMA_DIR (path to an arma checkout). Optional: OUT (default
# ./image-out), PMI_CMDLINE, ALPINE_IMAGE.
set -euo pipefail

ARMA_DIR="${ARMA_DIR:?set ARMA_DIR to an arma checkout}"
OUT="${OUT:-$(pwd)/image-out}"
# dillo routes the guest's virtio-console (hvc0) to stdout, so the build VM
# logs (conglobate's progress) reach `pichi build`'s captured output there.
PMI_CMDLINE="${PMI_CMDLINE:-console=hvc0}"
ALPINE_IMAGE="${ALPINE_IMAGE:-alpine:latest}"
root=$(cd "$(dirname "$0")/.." && pwd)

case "$(uname -m)" in
	x86_64) platform=linux/amd64 ;;
	aarch64) platform=linux/arm64 ;;
	*) echo "build-image: unsupported arch $(uname -m)" >&2; exit 1 ;;
esac
echo ">>> building pichi build image for $platform"

# 1. initramfs + kernel, inside a native (no-qemu) Alpine container. The
#    container compiles conglobate too, so stage the source tree (working
#    tree minus build/VCS dirs) for it to build.
work=$(mktemp -d)
mkdir -p "$work/out" "$work/src"
tar -C "$root" --exclude=./target --exclude=./.git -cf - . | tar -xf - -C "$work/src"
podman run --rm --platform "$platform" -v "$work:/work:z" "$ALPINE_IMAGE" sh /work/src/scripts/assemble.sh

# 2. arma seals the PMI (host build; honours its own pinned nightly toolchain
#    via $ARMA_DIR/rust-toolchain.toml).
(cd "$ARMA_DIR" && cargo build --release --bin arma)
arma="$ARMA_DIR/target/release/arma"

mkdir -p "$OUT"
"$arma" build \
	--kernel "$work/out/vmlinuz" \
	--initrd "$work/out/initramfs.cpio" \
	--config "$work/out/config" \
	--cmdline "$PMI_CMDLINE" \
	"$OUT/boot.pmi"
echo ">>> $OUT/boot.pmi ($(wc -c <"$OUT/boot.pmi") bytes)"
