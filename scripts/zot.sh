#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Local OCI registry (zot) for pichi push/pull integration tests.
#
#   scripts/zot.sh up      # start zot on localhost:5000
#   scripts/zot.sh down    # stop and remove it
#   scripts/zot.sh logs    # follow zot logs
#   scripts/zot.sh test    # ensure zot is up, then run the gated round-trip tests
#
# The push/pull/round-trip tests in pichi/tests/ skip silently unless
# PICHI_TEST_REGISTRY is set. Once zot is up you can run them by hand with:
#
#   PICHI_TEST_REGISTRY=localhost:5000 PICHI_TEST_REGISTRY_INSECURE=1 \
#     cargo test -p pichi --test cmd_pull --test cmd_push --test cmd_pull_push_roundtrip
#
# Env overrides: PICHI_ZOT_PORT, PICHI_ZOT_IMAGE, PICHI_CONTAINER_ENGINE.
set -euo pipefail

NAME=pichi-zot
PORT="${PICHI_ZOT_PORT:-5000}"
IMAGE="${PICHI_ZOT_IMAGE:-ghcr.io/project-zot/zot-linux-amd64:latest}"
CFG_DIR="${TMPDIR:-/tmp}/pichi-zot"

engine() {
  if [ -n "${PICHI_CONTAINER_ENGINE:-}" ]; then echo "$PICHI_CONTAINER_ENGINE"; return; fi
  if command -v podman >/dev/null 2>&1; then echo podman; return; fi
  if command -v docker >/dev/null 2>&1; then echo docker; return; fi
  echo "error: need podman or docker on PATH" >&2; exit 1
}
ENG="$(engine)"

ready() { curl -fsS "http://localhost:${PORT}/v2/" >/dev/null 2>&1; }

up() {
  if ready; then echo "zot already up on :${PORT}"; return 0; fi
  "$ENG" rm -f "$NAME" >/dev/null 2>&1 || true
  mkdir -p "$CFG_DIR"
  cat > "${CFG_DIR}/config.json" <<EOF
{"storage":{"rootDirectory":"/var/lib/registry"},"http":{"address":"0.0.0.0","port":"${PORT}"},"log":{"level":"warn"}}
EOF
  "$ENG" run -d --name "$NAME" -p "${PORT}:${PORT}" \
    -v "${CFG_DIR}/config.json:/etc/zot/config.json:Z" \
    "$IMAGE" serve /etc/zot/config.json >/dev/null
  printf 'waiting for zot on :%s' "$PORT"
  for _ in $(seq 1 30); do
    if ready; then echo ' ready'; return 0; fi
    printf '.'; sleep 1
  done
  echo ' FAILED' >&2; "$ENG" logs "$NAME" >&2 || true; exit 1
}

down() { "$ENG" rm -f "$NAME" >/dev/null 2>&1 || true; echo "zot removed"; }
logs() { "$ENG" logs -f "$NAME"; }

run_tests() {
  up
  PICHI_TEST_REGISTRY="localhost:${PORT}" PICHI_TEST_REGISTRY_INSECURE=1 \
    cargo test -p pichi --test cmd_pull --test cmd_push --test cmd_pull_push_roundtrip "$@"
}

case "${1:-}" in
  up)   up ;;
  down) down ;;
  logs) logs ;;
  test) shift; run_tests "$@" ;;
  *)    echo "usage: $0 {up|down|logs|test}" >&2; exit 2 ;;
esac
