#!/usr/bin/env bash
# CI gate: `unsafe` keyword permitted only in src/dm/uapi.rs (the
# iocuddle ioctl-number declarations). Comment-only lines are exempt
# (we discuss `unsafe` in module docstrings).
set -euo pipefail

cd "$(dirname "$0")/.."

GREP=rg
if ! command -v rg >/dev/null 2>&1; then
    GREP="grep -RnE"
fi

if [ "$GREP" = "rg" ]; then
    matches=$(rg --line-number --type rust '\bunsafe\b' src/ \
        | grep -vE ':\s*//' \
        | grep -vE '\s*//.*\bunsafe\b' \
        | grep -vE '^src/dm/uapi\.rs:' \
        || true)
else
    matches=$(grep -RnE '\bunsafe\b' src/ \
        | grep -vE ':\s*//' \
        | grep -vE '\s*//.*\bunsafe\b' \
        | grep -vE '^src/dm/uapi\.rs:' \
        || true)
fi

if [ -n "$matches" ]; then
    echo "FAIL: unsafe keyword outside src/dm/uapi.rs:" >&2
    echo "$matches" >&2
    exit 1
fi

echo "OK: unsafe boundary intact (only src/dm/uapi.rs)"
