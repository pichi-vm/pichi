#!/usr/bin/env bash
# SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

# Current functionality is always required. Compatibility is reported separately
# so intentional protocol changes remain visible without blocking development.
run_case() {
  local appliance=$1
  (
    unset PICHI_TEST_BUILD_IMAGE_LAYOUT PICHI_TEST_PREVIOUS_BUILD_IMAGE
    if [ "$appliance" = published ] && [[ "$previous" == ghcr.io/pichi-vm/pichi-build@* ]]; then
      export PICHI_TEST_PREVIOUS_BUILD_IMAGE="$previous"
    fi
    if [ "$appliance" = previous ]; then export PICHI_TEST_PREVIOUS_BUILD_IMAGE="$previous"; fi
    if [ "$appliance" = candidate ]; then export PICHI_TEST_BUILD_IMAGE_LAYOUT="$candidate"; fi
    cargo test --locked --test command_smoke -- --ignored --exact every_command_builds_and_boots --nocapture
  )
}

previous=${PICHI_TEST_PREVIOUS_BUILD_IMAGE:?previous appliance pin is required}
candidate=${PICHI_TEST_BUILD_IMAGE_LAYOUT:-}
summary=${GITHUB_STEP_SUMMARY:-/dev/stdout}
cargo test --locked --test command_smoke -- --list | tr -d '\r' | tee "$RUNNER_TEMP/command-tests.txt"
grep -Fxq 'every_command_builds_and_boots: test' "$RUNNER_TEMP/command-tests.txt"

required=published
if [ -n "$candidate" ]; then required=candidate; fi
echo "::group::🧪 Required current Pichi / $required appliance"
run_case "$required"
echo '::endgroup::'
printf '| Pichi | Appliance | Result |\n| --- | --- | --- |\n| Current | %s | PASS (required) |\n' "$required" >> "$summary"

if [ "${PICHI_TEST_COMPATIBILITY:-true}" != true ]; then
  echo '::notice::Compatibility checks explicitly disabled; required command/build/boot coverage passed.'
  echo 'Compatibility checks explicitly disabled.' >> "$summary"
  exit 0
fi

# Compare with the base revision even when the source default pin has changed.
if [ -n "$candidate" ] || [[ "$previous" != ghcr.io/pichi-vm/pichi-build@* ]]; then
  echo '::group::🧪 Compatibility: current Pichi / previous appliance'
  if run_case previous; then
    result=PASS
  else
    result=FAIL
    echo '::warning::Previous appliance compatibility failed. Required current functionality passed.'
  fi
  printf '| Current | previous | %s (compatibility) |\n' "$result" >> "$summary"
  echo '::endgroup::'
fi
