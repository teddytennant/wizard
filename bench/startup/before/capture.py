#!/usr/bin/env python3
"""Run a TUI under a pty and dump each screen as plain text.

  capture.py OUTDIR CMD... -- STEP...

Steps: `dump:NAME` writes OUTDIR/NN-NAME.txt once output has been quiet for
half a second; `key:enter|down|up|tab|esc|ctrl-c`, `text:STRING` send input;
`wait:SECONDS` sleeps. The screen is a pyte emulator, so what lands in the
file is what a terminal would show, not the byte stream.
"""
import fcntl
import os
import pty
import re
import select
import struct
import sys
import termios
import time

import pyte

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
from measure import Responder  # noqa: E402  (same replies, same order, as the benchmark)

COLS, ROWS = 100, 36
APC = re.compile(rb"\x1b_[^\x1b]*\x1b\\")
# An escape that a read cut short: a bare ESC, an APC with no terminator yet,
# or a CSI still in its parameters. Held back until the next read completes it.
CUT = re.compile(rb"\x1b(?:_[^\x1b]*\x1b?|\[[0-?]*)?$")
KEYS = {"enter": b"\r", "down": b"\x1b[B", "up": b"\x1b[A", "tab": b"\t", "esc": b"\x1b", "ctrl-c": b"\x03",
        "space": b" ", "backspace": b"\x7f"}


def main():
    outdir = sys.argv[1]
    sep = sys.argv.index("--")
    cmd, steps = sys.argv[2:sep], sys.argv[sep + 1 :]
    os.makedirs(outdir, exist_ok=True)
    screen = pyte.Screen(COLS, ROWS)
    stream = pyte.ByteStream(screen)
    pid, fd = pty.fork()
    if pid == 0:
        os.environ["TERM"] = "xterm-256color"
        os.execvp(cmd[0], cmd)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    n = 0
    raw = bytearray()
    held = b""
    responder = Responder(fd)
    log = open(os.path.join(outdir, "session.log"), "a")

    def dump(name):
        nonlocal n
        n += 1
        lines = [line.rstrip() for line in screen.display]
        while lines and not lines[-1]:
            lines.pop()
        if not lines:
            n -= 1
            return
        path = os.path.join(outdir, f"{n:02d}-{name}.txt")
        with open(path, "w") as f:
            f.write("\n".join(lines) + "\n")
        print(f"{path}: {sum(1 for l in lines if l)} non-empty lines", file=log, flush=True)

    def pump(quiet=0.5, limit=15.0):
        nonlocal raw, held
        last = time.monotonic()
        start = last
        while True:
            r, _, _ = select.select([fd], [], [], 0.05)
            if r:
                try:
                    chunk = os.read(fd, 65536)
                except OSError:
                    return False
                if not chunk:
                    return False
                raw += chunk
                responder.feed(raw)
                # pyte has no alternate screen. When the app switches to
                # or from it, feed what came before the switch, save the
                # screen, and start from a blank one, which is what a
                # terminal would show.
                # pyte does not know the kitty graphics APC and would print
                # it as text; a terminal shows nothing for it. The APC or the
                # altscreen switch can straddle two reads, so the unfinished
                # tail waits for the next one.
                chunk = held + chunk
                cut = CUT.search(chunk)
                held = chunk[cut.start():] if cut else b""
                chunk = APC.sub(b"", chunk[: cut.start()] if cut else chunk)
                at = 0
                for m in re.finditer(rb"\x1b\[\?1049[hl]", chunk):
                    stream.feed(chunk[at : m.start()])
                    dump("altscreen")
                    screen.reset()
                    at = m.end()
                stream.feed(chunk[at:])
                last = time.monotonic()
            elif time.monotonic() - last > quiet or time.monotonic() - start > limit:
                # Quiet with a tail still held: it was a real bare ESC, not a cut.
                stream.feed(held)
                held = b""
                return True

    for step in steps:
        kind, _, arg = step.partition(":")
        if kind == "dump":
            pump()
            dump(arg)
        elif kind == "key":
            os.write(fd, KEYS[arg])
        elif kind == "text":
            os.write(fd, arg.encode())
        elif kind == "wait":
            pump(quiet=float(arg), limit=float(arg) + 0.5)
    pump()
    try:
        os.kill(pid, 9)
    except OSError:
        pass


if __name__ == "__main__":
    main()
