# SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
# SPDX-License-Identifier: Apache-2.0

"""Build the locked monitor for CI, and remove its temporary checkout afterward."""

import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tomllib

root = Path(os.environ["RUNNER_TEMP"]).resolve() / "pichi-monitor"


def remove_checkout(root):
    # Git pack files can be read-only on Windows.
    def retry_writable(operation, path, _error):
        Path(path).chmod(stat.S_IWRITE)
        operation(path)

    if root.exists():
        shutil.rmtree(root, onerror=retry_writable)


if sys.argv[1:] == ["--cleanup"]:
    remove_checkout(root)
elif not sys.argv[1:]:
    with open("Cargo.lock", "rb") as source:
        packages = tomllib.load(source)["package"]
    monitor = next(package for package in packages if package["name"] == "madrillo")
    revision = monitor["source"].rsplit("#", 1)[1]
    try:
        subprocess.run(["git", "clone", "--no-checkout", "https://github.com/pichi-vm/madrillo", str(root)], check=True)
        subprocess.run(["git", "checkout", "--detach", revision], cwd=root, check=True)
        with (root / "rust-toolchain.toml").open("rb") as source:
            toolchain = tomllib.load(source)["toolchain"]["channel"]
        subprocess.run(["rustup", "toolchain", "install", toolchain, "--profile", "minimal", "--no-self-update"], check=True)
        environment = os.environ.copy()
        target = Path("target/madrillo").resolve()
        environment["CARGO_TARGET_DIR"] = str(target)
        subprocess.run(["cargo", "+" + toolchain, "build", "--locked", "--features", "qualification", "--bins"], cwd=root, env=environment, check=True)
        binary = target / "debug" / ("madrillo.exe" if os.name == "nt" else "madrillo")
        with open(os.environ["GITHUB_PATH"], "a", encoding="utf-8") as output:
            output.write(f"{binary.parent}\n")
    except BaseException:
        remove_checkout(root)
        raise
else:
    raise SystemExit("expected no arguments or --cleanup")
