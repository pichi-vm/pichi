# SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
# SPDX-License-Identifier: Apache-2.0

"""Native Unix terminal frontend acceptance; launched by cmd_workload.rs."""
import fcntl
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import sys
import termios
import time

# Keep the controlling session alive while checking restoration. macOS revokes
# the slave when its session leader exits, even if another process holds it open.
# This fixture plays the terminal's shell; Madrillo is still launched by Pichi.
SESSION = """
import signal, subprocess, sys, termios
signal.signal(signal.SIGTERM, signal.SIG_IGN)
signal.signal(signal.SIGHUP, signal.SIG_IGN)
original = termios.tcgetattr(0)
child = subprocess.Popen(sys.argv[2:])
# Like an interactive shell, relay the session owner's terminal hangup to
# its owned foreground child. The kernel need not signal every process.
signal.signal(signal.SIGHUP, lambda number, frame: child.send_signal(number))
try:
    code = child.wait(timeout=30)
finally:
    if child.poll() is None:
        child.kill()
        child.wait(timeout=5)
if sys.argv[1] == 'restore':
    assert termios.tcgetattr(0) == original, 'terminal modes not restored'
sys.exit(code)
"""


class Terminal:
    def __init__(self, arguments, check_restoration=True, size=(300, 77)):
        self.master, self.slave = pty.openpty()
        self.output = bytearray()
        self.resize(*size)
        self.process = subprocess.Popen(
            [sys.executable, "-c", SESSION,
             "restore" if check_restoration else "hangup", sys.argv[1], *arguments],
            stdin=self.slave, stdout=self.slave, stderr=self.slave,
            start_new_session=True,
            preexec_fn=lambda: fcntl.ioctl(0, termios.TIOCSCTTY, 0),
        )

    def __enter__(self):
        return self

    def __exit__(self, *error):
        if self.process.poll() is None:
            # The unreaped session leader reserves this test-owned group ID.
            os.killpg(self.process.pid, signal.SIGKILL)
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

    def resize(self, cols, rows):
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack('HHHH', rows, cols, 0, 0))

    def receive(self, expected):
        deadline = time.monotonic() + 5
        while expected not in self.output:
            assert time.monotonic() < deadline, (expected, self.output)
            assert self.process.poll() is None, (self.process.returncode, self.output)
            if select.select([self.master], [], [], 0.05)[0]:
                self.output.extend(os.read(self.master, 8192))
                assert len(self.output) <= 65536, "unbounded terminal fixture output"
        end = self.output.index(expected) + len(expected)
        output = bytes(self.output[:end])
        del self.output[:end]
        return output

    def restored(self, code):
        # The live session owner checks native settings before returning this
        # status; revocation after session exit cannot hide a missing restore.
        assert self.process.wait(timeout=5) == code


records = Path(os.environ["XDG_DATA_HOME"]) / "pichi/storage.workloads"
try:
    with Terminal(["run", "-i", "fixture:1"]) as terminal:
        terminal.receive(b"console-ready")
        terminal.receive(b"size=300x77")
        terminal.resize(120, 41)
        terminal.receive(b"size=120x41")
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

    with Terminal(["attach", "-i", identity], size=(81, 25)) as terminal:
        terminal.receive(b"size=81x25")
        terminal.send(b"after-detach\n")
        terminal.receive(b"after-detach")
        os.killpg(terminal.process.pid, signal.SIGTERM)
        terminal.restored(143)
    assert not (record / "monitor/result.json").exists(), "caller signal stopped the workload"

    with Terminal(["attach", "-i", identity], check_restoration=False) as terminal:
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
        stopped = subprocess.run([sys.argv[1], "stop", identity],
                                 capture_output=True, timeout=10)
        assert stopped.returncode == 0, stopped.stderr
        terminal.restored(0)
    # A workload launched detached acquires its first size on later attachment.
    started = subprocess.run([sys.argv[1], "run", "-d", "fixture:1"],
                             capture_output=True, timeout=5)
    assert started.returncode == 0, started.stderr
    identity = started.stdout.decode().strip()
    with Terminal(["attach", "-i", identity], size=(132, 43)) as terminal:
        terminal.receive(b"size=132x43")
        stopped = subprocess.run([sys.argv[1], "stop", identity],
                                 capture_output=True, timeout=10)
        assert stopped.returncode == 0, stopped.stderr
        terminal.restored(0)
    print("native terminal: initial size, resize, detached start, control bytes, detach, signal restoration, hangup survival, reattach and normal-exit restoration passed")
finally:
    # Only records in this test's private storage root are eligible for cleanup.
    for record in records.glob("*"):
        subprocess.run([sys.argv[1], "stop", "--time", "0", record.name],
                       capture_output=True, timeout=10)
