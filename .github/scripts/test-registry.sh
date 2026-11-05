#!/usr/bin/env bash
# SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

# Native Zot releases keep the same real HTTP coverage on all four hosts.
# RUNNER_TEMP uses backslashes on Windows; JSON accepts forward-slash paths.
fixture_dir=${RUNNER_TEMP//\\//}
zot="$fixture_dir/$ZOT_ASSET"
curl --fail --location --retry 3 \
  "https://github.com/project-zot/zot/releases/download/v2.1.23/$ZOT_ASSET" \
  --output "$zot"
if command -v sha256sum >/dev/null; then
  printf '%s  %s\n' "$ZOT_SHA256" "$zot" | sha256sum --check
else
  printf '%s  %s\n' "$ZOT_SHA256" "$zot" | shasum -a 256 --check
fi
chmod +x "$zot"
mkdir -p "$fixture_dir/zot-storage"
cat > "$fixture_dir/zot.json" <<JSON
{"storage":{"rootDirectory":"$fixture_dir/zot-storage"},"http":{"address":"127.0.0.1","port":"5000"},"log":{"level":"warn"}}
JSON
# The ARM Go PIE release names the conventional Linux dynamic loader.
# NixOS provides that loader in the store instead of /lib.
launcher=()
if [ -e /etc/NIXOS ]; then
  glibc=$(nix --extra-experimental-features 'nix-command flakes' build \
    github:NixOS/nixpkgs/08d3a97586f6b6343b3b5e799980773629432610#glibc \
    --no-link --print-out-paths)
  launcher=("$glibc/lib/ld-linux-aarch64.so.1")
fi
"${launcher[@]}" "$zot" serve "$fixture_dir/zot.json" > "$fixture_dir/zot.log" 2>&1 &
zot_pid=$!
trap 'kill "$zot_pid" 2>/dev/null || true; wait "$zot_pid" 2>/dev/null || true; cat "$fixture_dir/zot.log"' EXIT
for _ in {1..30}; do
  kill -0 "$zot_pid"
  if curl -fsS http://127.0.0.1:5000/v2/ >/dev/null; then break; fi
  sleep 1
done
curl -fsS http://127.0.0.1:5000/v2/ >/dev/null
export PICHI_TEST_REGISTRY=127.0.0.1:5000
export PICHI_TEST_REGISTRY_INSECURE=1
cargo test --locked --workspace --no-fail-fast
