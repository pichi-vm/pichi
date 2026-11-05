#!/usr/bin/env bash
# SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

# Native Zot releases keep the same real HTTP coverage on all four hosts.
# RUNNER_TEMP uses backslashes on Windows; JSON accepts forward-slash paths.
fixture_dir=${RUNNER_TEMP//\\//}
registry_port=${PICHI_TEST_REGISTRY_PORT:-5000}
if [[ ! "$registry_port" =~ ^[0-9]{1,5}$ ]] || (( 10#$registry_port < 1024 || 10#$registry_port > 65535 )); then
  echo "PICHI_TEST_REGISTRY_PORT must be an unprivileged TCP port" >&2
  exit 1
fi
registry_address="127.0.0.1:$registry_port"
zot="$fixture_dir/$ZOT_ASSET"
curl --fail --location --retry 3 \
  "https://github.com/project-zot/zot/releases/download/v2.1.23/$ZOT_ASSET" \
  --output "$zot"
if [ "${RUNNER_OS:-}" = macOS ]; then
  checksum=$(/usr/bin/shasum -a 256 "$zot")
else
  checksum=$(sha256sum "$zot")
fi
if [ "${checksum%% *}" != "$ZOT_SHA256" ]; then
  echo "Zot SHA-256 mismatch: $checksum" >&2
  exit 1
fi
chmod +x "$zot"
mkdir -p "$fixture_dir/zot-storage"
cat > "$fixture_dir/zot.json" <<JSON
{"storage":{"rootDirectory":"$fixture_dir/zot-storage"},"http":{"address":"127.0.0.1","port":"$registry_port"},"log":{"level":"warn"}}
JSON
# The ARM Go PIE release names the conventional Linux dynamic loader.
# NixOS provides that loader in the store instead of /lib.
registry=("$zot")
if [ -e /etc/NIXOS ]; then
  glibc=$(nix --extra-experimental-features 'nix-command flakes' build \
    github:NixOS/nixpkgs/08d3a97586f6b6343b3b5e799980773629432610#glibc.out \
    --no-link --print-out-paths)
  registry=("$glibc/lib/ld-linux-aarch64.so.1" "$zot")
fi
"${registry[@]}" serve "$fixture_dir/zot.json" > "$fixture_dir/zot.log" 2>&1 &
zot_pid=$!
trap 'kill "$zot_pid" 2>/dev/null || true; wait "$zot_pid" 2>/dev/null || true; cat "$fixture_dir/zot.log"' EXIT
for _ in {1..30}; do
  kill -0 "$zot_pid"
  # Connection refusal is expected until Zot opens its listening socket.
  if curl -fs "http://$registry_address/v2/" >/dev/null; then break; fi
  sleep 1
done
curl -fsS "http://$registry_address/v2/" >/dev/null
export PICHI_TEST_REGISTRY="$registry_address"
export PICHI_TEST_REGISTRY_INSECURE=1
cargo test --locked --workspace --no-fail-fast
