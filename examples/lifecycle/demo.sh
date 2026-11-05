#!/usr/bin/env bash
# SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

auto=false
case "${1:-}" in
    --auto) auto=true ;;
    "") ;;
    *) printf 'Usage: %s [--auto]\n' "$0" >&2; exit 2 ;;
esac

step() {
    printf '\n$'
    printf ' %s' "$@"
    printf '\n'
    if ! "$auto"; then
        read -r -p 'Press Enter to run ' _
    fi
    "$@"
}

step pichi build . --ref HEAD -t lifecycle:demo --memory 4096 --cpus 2
step pichi inspect lifecycle:demo --format layers
step pichi inspect lifecycle:demo --format annotations
step pichi run lifecycle:demo
printf '\nLifecycle complete. The VM has exited successfully.\n'
