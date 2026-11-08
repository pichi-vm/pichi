#!/usr/bin/env bash
# SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

# Run current command/build/boot acceptance against the selected appliance.
cargo test --locked --test command_smoke -- --list | tr -d '\r' | tee "$RUNNER_TEMP/command-tests.txt"
grep -Fxq 'every_command_builds_and_boots: test' "$RUNNER_TEMP/command-tests.txt"
cargo test --locked --test command_smoke -- --ignored --exact every_command_builds_and_boots --nocapture
