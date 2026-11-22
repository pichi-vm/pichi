#!/usr/bin/env bash
# SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
# SPDX-License-Identifier: Apache-2.0

# One-time native qualification; this branch does not publish registry tags.
set -euo pipefail
repo=$PWD
old="$repo/old/target/debug/pichi"
current="$repo/target/debug/pichi"
temporary="$RUNNER_TEMP/schema-bootstrap"
mkdir -p "$temporary" artifacts

for generation in old current; do
  mkdir -p "$temporary/$generation/config/pichi"
  cat > "$temporary/$generation/config/pichi/config.toml" <<EOF
[storage]
graphroot = "$temporary/$generation/store"
runroot = "$temporary/$generation/run"
EOF
done
old_host() {
  PATH="$repo/target/dillo-old/bin:$PATH" \
    XDG_CONFIG_HOME="$temporary/old/config" "$old" "$@"
}
current_host() {
  PATH="$repo/target/dillo-current/bin:$PATH" \
    XDG_CONFIG_HOME="$temporary/current/config" "$current" "$@"
}

# These are the verified single-layer Fedora 43 container inputs, including
# their numeric ownership. No host OCI manifest rewriting is involved.
case "$(uname -m)" in
  x86_64) arch=amd64; layer=5959203eef7837e8cb23c6daba4e3450e2b2b623f1512dfee10f3182f86b8169 ;;
  aarch64) arch=arm64; layer=4a35c0c32db4c64a6f399f79a9f89c90fcbcab299c241fc3f5544544977ca933 ;;
  *) exit 1 ;;
esac
curl --fail --location --retry 3 \
  "https://registry.fedoraproject.org/v2/fedora/blobs/sha256:$layer" \
  -o "$temporary/root.tar.gz"
echo "$layer  $temporary/root.tar.gz" | sha256sum --check
mkdir "$temporary/root"
sudo tar --numeric-owner -xzf "$temporary/root.tar.gz" -C "$temporary/root"
sudo mkfs.ext4 -F -E lazy_itable_init=0 -d "$temporary/root" "$temporary/seed.raw" 1G
current_host import raw "$temporary/seed.raw" --arch "$arch" -t localhost/fedora:seed
current_host tag localhost/fedora:seed ghcr.io/pichi-vm/fedora:43
current_host save localhost/fedora:seed -o "$temporary/seed-layout"
# The bridge's engine needs current input annotations, although the old host
# and runtime must still launch its old-format PMI.
XDG_CONFIG_HOME="$temporary/old/config" "$current" load \
  "$temporary/seed-layout" --tag localhost/fedora:seed

builder=ghcr.io/pichi-vm/pichi-build@sha256:577b0d1fcbff0849985ce3158589497fcb0ab3b0201bbfbf53f9f3bbd5498acb
fedora=$(sed -n 's/^  "ghcr.io\/pichi-vm\/fedora:43": //p' appliance/image/pichi.lock)
old_host pull "$builder"
old_host pull "ghcr.io/pichi-vm/fedora@$fedora"
old_host build . --ref HEAD --plan appliance/image/pichi.yaml --lock appliance/image/pichi.lock \
  --build-image "$builder" --memory 4096m --cpus 2 --storage 16384m \
  -t localhost/schema-bridge:current 2>&1 | tee artifacts/bridge.log

# Successful guest output crosses the schema boundary once. The old host must
# refuse it, preserve the layout, and report a completed guest with exit zero.
if old_host build . --ref HEAD --plan .github/qualification/bootstrap.yaml \
  --build-image localhost/schema-bridge:current --memory 4096m --cpus 2 --storage 16384m \
  -t localhost/schema-bootstrap:current > artifacts/bootstrap.log 2>&1; then
  echo 'Expected the old host to refuse the new annotation schema.' >&2
  exit 1
fi
cat artifacts/bootstrap.log
grep -F 'missing field `dev.pichi.scute.verity.data-blocks`' artifacts/bootstrap.log
workload=$(sed -n 's/^build workload //p' artifacts/bootstrap.log)
record="$temporary/old/store.workloads/$workload"
python3 - "$record/monitor/result.json" <<'PY'
import json, sys
result = json.load(open(sys.argv[1]))
assert result['leader'] == {'type': 'exit', 'code': 0}, result
assert result['cleanup']['type'] == 'complete', result
assert not result['monitor_errors'], result
PY
cp "$record/monitor/result.json" artifacts/bootstrap-completion.json
current_host load "$record/output" --tag localhost/schema-bootstrap:current

current_host build fedora --ref 9350482e991706bd75c025ad29c8a1ac4179c662 \
  --build-image localhost/schema-bootstrap:current --memory 4096m --cpus 2 --storage 16384m \
  -t ghcr.io/pichi-vm/fedora:43 2>&1 | tee artifacts/fedora.log
current_host update . --plan appliance/image/pichi.yaml --lock appliance/image/pichi.lock --pull=never
git add appliance/image/pichi.lock
git -c user.name=Qualification -c user.email=qualification@example.invalid commit \
  -m 'test: bind the freshly built Fedora input' -m 'Record the native qualification input before building the production appliance.'
current_host build . --ref HEAD --plan appliance/image/pichi.yaml --lock appliance/image/pichi.lock \
  --build-image localhost/schema-bootstrap:current --memory 4096m --cpus 2 --storage 16384m \
  -t qualification.local/appliance:current 2>&1 | tee artifacts/appliance.log

current_host update . --plan .github/qualification/inherited.yaml \
  --lock .github/qualification/inherited.lock --pull=never
git add .github/qualification/inherited.lock
git -c user.name=Qualification -c user.email=qualification@example.invalid commit \
  -m 'test: bind the freshly built appliance input' -m 'Exercise inherited layers using the exact native appliance output.'
for plan in smoke inherited; do
  current_host build . --ref HEAD --plan ".github/qualification/$plan.yaml" \
    --lock .github/qualification/inherited.lock --build-image qualification.local/appliance:current \
    --memory 4096m --cpus 2 --storage 16384m -t "localhost/qualification:$plan" \
    2>&1 | tee "artifacts/$plan.log"
done
current_host save ghcr.io/pichi-vm/fedora:43 -o artifacts/fedora-oci
current_host save qualification.local/appliance:current -o artifacts/appliance-oci
git rev-parse HEAD > artifacts/qualified-ref
