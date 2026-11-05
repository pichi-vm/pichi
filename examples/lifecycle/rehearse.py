#!/usr/bin/env python3
# SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
# SPDX-License-Identifier: Apache-2.0

"""Rehearse the real lifecycle and reject clutter, missing provenance or hangs."""

import argparse
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import tempfile
import time


def main():
    if not __debug__:
        raise SystemExit("Run rehearsal checks without Python's -O option.")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("project", type=Path)
    parser.add_argument("--pichi", default="pichi")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--iterations", type=int, default=2)
    args = parser.parse_args()
    if args.iterations < 1:
        parser.error("--iterations must be positive")
    args.output.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, NO_COLOR="1")
    env.pop("RUST_LOG", None)  # Exercise the product defaults.
    measurements = []

    def run(name, command, timeout=30, overrides=None, failure=False):
        started = time.monotonic()
        process = subprocess.Popen(
            command, cwd=args.project, env=env | (overrides or {}),
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            start_new_session=os.name == "posix",
        )
        timed_out = False
        try:
            output, _ = process.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            if os.name == "posix":
                os.killpg(process.pid, signal.SIGKILL)
            else:
                process.kill()
            output, _ = process.communicate()
        (args.output / f"{name}.log").write_bytes(output)
        text = output.decode("utf-8")
        measurements.append({
            "step": name, "seconds": round(time.monotonic() - started, 2),
            "lines": len(text.splitlines()), "exit_code": process.returncode,
            "timed_out": timed_out,
        })
        (args.output / "results.json").write_text(json.dumps(measurements, indent=2) + "\n")
        assert not timed_out, f"{name} exceeded {timeout}s; see {args.output / (name + '.log')}"
        assert (process.returncode != 0) if failure else (process.returncode == 0), text
        if not failure:
            assert "\x1b" not in text, f"terminal control sequence in {name}"
            assert not any(word in text for word in [" INFO ", " WARN ", "fedora login:"]), text
        return text

    commit = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=args.project, text=True,
    ).strip()
    object_format = subprocess.check_output(
        ["git", "rev-parse", "--show-object-format"], cwd=args.project, text=True,
    ).strip()
    # Read the Fedora pin from this example's generated lock, not a mutable tag.
    lock = (args.project / "pichi.lock").read_text()
    pin = re.search(r'"ghcr.io/pichi-vm/fedora:43": (sha256:[0-9a-f]{64})', lock)
    assert pin, "expected the generated Fedora 43 pin in pichi.lock"
    base_ref = "ghcr.io/pichi-vm/fedora:43@" + pin[1]
    base = json.loads(run("base", [args.pichi, "inspect", base_ref]))
    build = [args.pichi, "build", ".", "--ref", "HEAD", "-t", "lifecycle:demo", "--memory", "4096", "--cpus", "2"]

    for iteration in range(1, args.iterations + 1):
        prefix = f"pass-{iteration}"
        built = run(f"{prefix}-build", build, timeout=240)
        assert "Built lifecycle:demo" in built and len(built.splitlines()) <= 8, built
        image = json.loads(run(f"{prefix}-json", [args.pichi, "inspect", "lifecycle:demo"]))
        manifest = image["manifest"]
        assert manifest["annotations"]["dev.pichi.build.commit"] == f"{object_format}:{commit}"
        assert manifest["layers"][0]["digest"] == base["manifest"]["layers"][0]["digest"], "Fedora base layer was not reused"
        layers = run(f"{prefix}-layers", [args.pichi, "inspect", "lifecycle:demo", "--format", "layers"])
        assert "Filesystem 1" in layers and "Filesystem 2" in layers and "Boot payload" in layers
        assert len(layers.splitlines()) <= 12, layers
        annotations = run(f"{prefix}-annotations", [args.pichi, "inspect", "lifecycle:demo", "--format", "annotations"])
        assert commit in annotations and "Build provenance" in annotations
        assert len(annotations.splitlines()) <= 30, annotations
        guest = run(f"{prefix}-run", [args.pichi, "run", "lifecycle:demo"])
        assert guest.splitlines() == [
            "Starting lifecycle:demo", "Hello from Pichi.", "Workload complete. Shutting down.",
        ], guest
        print(f"Pass {iteration}: build, layer reuse, provenance, and clean shutdown verified", flush=True)

    # A clean success path must not conceal an actual launcher failure.
    if os.name == "posix":
        with tempfile.TemporaryDirectory(prefix="pichi-failure-") as directory:
            launcher = Path(directory) / "dillo"
            launcher.write_text("#!/bin/sh\necho 'lifecycle rehearsal: launcher unavailable' >&2\nexit 12\n")
            launcher.chmod(0o700)
            for verbose in [False, True]:
                text = run(f"failure-verbose-{verbose}", build + (["--verbose"] if verbose else []),
                           overrides={"PICHI_DILLO": str(launcher)}, failure=True)
                assert "lifecycle rehearsal: launcher unavailable" in text
                assert "Built lifecycle:demo" not in text
                if not verbose:
                    assert "full log:" in text
    (args.output / "results.json").write_text(json.dumps(measurements, indent=2) + "\n")
    print(f"Evidence saved to {args.output}")


if __name__ == "__main__":
    main()
