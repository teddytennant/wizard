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

COLS, ROWS = 100, 36
KEYS = {"enter": b"\r", "down": b"\x1b[B", "up": b"\x1b[A", "tab": b"\t", "esc": b"\x1b", "ctrl-c": b"\x03",
        "space": b" ", "backspace": b"\x7f"}
# Same terminal-query answers as measure.py, so nothing waits on a reply.
QUERIES = [
    (re.compile(rb"\x1b\[5n"), b"\x1b[0n"),
    (re.compile(rb"\x1b\[6n"), b"\x1b[1;1R"),
    (re.compile(rb"\x1b\[0?c"), b"\x1b[?62;22c"),
    (re.compile(rb"\x1b\[16t"), b"\x1b[6;20;10t"),
    (re.compile(rb"\x1b\[\?(\d+)\$p"), lambda m: b"\x1b[?" + m.group(1) + b";0$y"),
    (re.compile(rb"\x1b\]1[012];\?(?:\x07|\x1b\\)"), b"\x1b]11;rgb:0000/0000/0000\x1b\\"),
]


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
                for q, reply in QUERIES:
                    for m in q.finditer(chunk):
                        os.write(fd, reply(m) if callable(reply) else reply)
                # pyte has no alternate screen. When the app switches to
                # or from it, save what was on the main screen and start
                # from a blank one, which is what a terminal would show.
                for _ in re.finditer(rb"\x1b\[\?1049[hl]", chunk):
                    dump("altscreen")
                    screen.reset()
                stream.feed(chunk)
                last = time.monotonic()
            elif time.monotonic() - last > quiet or time.monotonic() - start > limit:
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
