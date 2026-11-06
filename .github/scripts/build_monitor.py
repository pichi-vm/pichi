# SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
# SPDX-License-Identifier: Apache-2.0

"""Build the locked monitor for CI, and remove its temporary checkout afterward."""

import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile
import tomllib

temporary = Path(os.environ["RUNNER_TEMP"]).resolve()


def remove_checkout(root):
    # Git pack files can be read-only on Windows.
    def retry_writable(operation, path, _error):
        Path(path).chmod(stat.S_IWRITE)
        operation(path)

    if root.exists():
        shutil.rmtree(root, onerror=retry_writable)


if sys.argv[1:] == ["--cleanup"]:
    source = os.environ.get("PICHI_MONITOR_SOURCE")
    if source:
        root = Path(source)
        assert root.parent.resolve() == temporary and root.name.startswith("pichi-monitor-")
        remove_checkout(root)
elif not sys.argv[1:]:
    with open("Cargo.lock", "rb") as source:
        packages = tomllib.load(source)["package"]
    monitor = next(package for package in packages if package["name"] == "madrillo")
    revision = monitor["source"].rsplit("#", 1)[1]
    root = Path(tempfile.mkdtemp(prefix="pichi-monitor-", dir=temporary))
    try:
        # Publish before building so the always-run cleanup step also handles
        # cancellation during clone or compilation.
        with open(os.environ["GITHUB_ENV"], "a", encoding="utf-8") as output:
            output.write(f"PICHI_MONITOR_SOURCE={root}\n")
        subprocess.run(["git", "clone", "--no-checkout", "https://github.com/pichi-vm/madrillo", str(root)], check=True)
        subprocess.run(["git", "checkout", "--detach", revision], cwd=root, check=True)
        environment = os.environ.copy()
        environment["CARGO_TARGET_DIR"] = str(root / "target")
        subprocess.run(["cargo", "+" + os.environ["NIGHTLY"], "build", "--locked", "--features", "qualification", "--bins"], cwd=root, env=environment, check=True)
        binary = root / "target/debug" / ("madrillo.exe" if os.name == "nt" else "madrillo")
        with open(os.environ["GITHUB_ENV"], "a", encoding="utf-8") as output:
            output.write(f"PICHI_MADRILLO={binary}\nMADRILLO_BIN={binary}\n")
    except BaseException:
        remove_checkout(root)
        raise
else:
    raise SystemExit("expected no arguments or --cleanup")
