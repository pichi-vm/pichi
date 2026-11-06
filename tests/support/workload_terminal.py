# SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
# SPDX-License-Identifier: Apache-2.0

"""Native Unix terminal frontend acceptance; launched by cmd_workload.rs."""
import fcntl
import os
from pathlib import Path
import pty
import select
import signal
import subprocess
import termios
import time


class Terminal:
    def __init__(self, arguments):
        self.master, self.slave = pty.openpty()
        self.original = termios.tcgetattr(self.slave)
        self.process = subprocess.Popen(
            [os.environ["PICHI_BIN"], *arguments],
            stdin=self.slave, stdout=self.slave, stderr=self.slave,
            start_new_session=True,
            preexec_fn=lambda: fcntl.ioctl(0, termios.TIOCSCTTY, 0),
        )

    def __enter__(self):
        return self

    def __exit__(self, *error):
        if self.process.poll() is None:
            self.process.kill()
        self.process.wait(timeout=5)
        self.disconnect()
        os.close(self.slave)

    def disconnect(self):
        if self.master is not None:
            os.close(self.master)
            self.master = None

    def send(self, data):
        # Input is valid only after the attachment owns it and enters raw mode.
        # Otherwise cooked host echo could satisfy a false positive, or Ctrl-C
        # could kill the attaching CLI before its handlers are installed.
        deadline = time.monotonic() + 5
        while termios.tcgetattr(self.slave)[3] & (termios.ICANON | termios.ECHO | termios.ISIG):
            assert self.process.poll() is None
            assert time.monotonic() < deadline, "attachment did not enter raw mode"
            time.sleep(0.005)
        assert os.write(self.master, data) == len(data)

    def receive(self, expected):
        output = bytearray()
        deadline = time.monotonic() + 5
        while expected not in output:
            assert time.monotonic() < deadline, (expected, output)
            assert self.process.poll() is None, (self.process.returncode, output)
            if select.select([self.master], [], [], 0.05)[0]:
                output.extend(os.read(self.master, 8192))
                assert len(output) <= 65536, "unbounded terminal fixture output"
        return bytes(output)

    def restored(self, code):
        assert self.process.wait(timeout=5) == code
        assert termios.tcgetattr(self.slave) == self.original, "terminal modes not restored"


records = Path(os.environ["XDG_DATA_HOME"]) / "pichi/storage.workloads"
try:
    with Terminal(["run", "-i", "fixture:1"]) as terminal:
        terminal.receive(b"console-ready")
        record, = records.iterdir()
        identity = record.name
        flags = termios.tcgetattr(terminal.slave)[3]
        assert not flags & (termios.ICANON | termios.ECHO | termios.ISIG), "not raw mode"
        payload = b"\x03\x04\x00\xff"
        terminal.send(payload)
        terminal.receive(payload)
        terminal.send(b"\x10x\x10")
        terminal.receive(b"\x10x")
        terminal.send(b"\x11")
        terminal.restored(0)
    assert not (record / "monitor/result.json").exists(), "detach stopped the workload"

    with Terminal(["attach", "-i", identity]) as terminal:
        terminal.send(b"after-detach\n")
        terminal.receive(b"after-detach")
        terminal.process.send_signal(signal.SIGTERM)
        terminal.restored(143)
    assert not (record / "monitor/result.json").exists(), "caller signal stopped the workload"

    with Terminal(["attach", "-i", identity]) as terminal:
        # A binary control byte cannot be satisfied by the host's cooked echo.
        terminal.send(b"terminal-loss\x03\n")
        terminal.receive(b"terminal-loss\x03")
        assert terminal.process.poll() is None
        terminal.disconnect()
        terminal.process.wait(timeout=5)
    assert not (record / "monitor/result.json").exists(), "terminal loss stopped the workload"

    with Terminal(["attach", "-i", identity]) as terminal:
        terminal.send(b"reattached\x03\n")
        terminal.receive(b"reattached\x03")
        stopped = subprocess.run([os.environ["PICHI_BIN"], "stop", identity],
                                 capture_output=True, timeout=10)
        assert stopped.returncode == 0, stopped.stderr
        terminal.restored(0)
    print("native terminal: control bytes, detach, signal restoration, hangup survival, reattach and normal-exit restoration passed")
finally:
    # Only records in this test's private storage root are eligible for cleanup.
    for record in records.glob("*"):
        subprocess.run([os.environ["PICHI_BIN"], "stop", "--time", "0", record.name],
                       capture_output=True, timeout=10)
