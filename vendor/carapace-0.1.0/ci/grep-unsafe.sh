#!/usr/bin/env bash
# SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
# SPDX-License-Identifier: Apache-2.0

# The compiler denies unsafe code except in the Windows allocated-range query.
# Reject additional unsafe blocks or lint exceptions, including in other files.
set -euo pipefail

cd "$(dirname "$0")/.."

python3 - <<'PYTHON'
from pathlib import Path
import re

allowed = {
    ('src/image/windows.rs', '#[expect(unsafe_code, reason = "private Windows allocated-range FFI boundary")]'),
    ('src/image/windows.rs', 'unsafe {'),
}
seen = set()
for path in Path('src').rglob('*.rs'):
    for number, line in enumerate(path.read_text().splitlines(), 1):
        code = line.split('//', 1)[0].strip()
        if re.fullmatch(r'#!?\[(?:deny|forbid)\(unsafe_code\)\]', code):
            continue
        if not re.search(r'\bunsafe(?:_code)?\b', code):
            continue
        entry = (str(path), code)
        if entry not in allowed or entry in seen:
            raise SystemExit(f'Unexpected unsafe code or exception: {path}:{number}: {line}')
        seen.add(entry)
if seen != allowed:
    raise SystemExit('Expected exactly one Windows allocated-range unsafe boundary')
print('OK: only the private Windows allocated-range query permits unsafe code')
PYTHON
