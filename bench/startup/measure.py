#!/usr/bin/env python3
"""Drive one agent under a pty and time it to its first drawn prompt.

Runs inside the agent's container. Output is one JSON document on --out.

  measure.py --name wizard --marker '› ' --cmd wizard --runs 10 --out /out/wizard.json

The clock starts right before fork+exec and stops when the ANSI-stripped
output first matches --marker. Run 1 is the cold start (nothing in the page
cache yet); runs 2 to N are warm starts and give the median and p90. After
--settle seconds at the prompt the process tree's RSS, PSS and CPU time are
read from /proc (summed over the child and every descendant, VmHWM for the
peak), then the tree is killed. --version-cmd is timed separately as the
simplest possible start. --dump writes the stripped output of the first run
for eyeballing the marker.
"""
import argparse
import fcntl
import json
import os
import re
import select
import signal
import statistics
import struct
import subprocess
import sys
import termios
import time

ANSI = re.compile(
    rb"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)"  # OSC
    rb"|\x1bP[^\x1b]*\x1b\\"  # DCS
    rb"|\x1b_[^\x1b]*\x1b\\"  # APC (kitty graphics)
    rb"|\x1b\[[0-?]*[ -/]*[@-~]"  # CSI
    rb"|\x1b[@-Z\\-_]"  # two-byte escapes
    rb"|[\x00-\x08\x0b-\x1f\x7f]"  # other control bytes
)


TRACE = os.environ.get("MEASURE_TRACE") == "1"


def strip(b):
    return ANSI.sub(b"", b)


# What a terminal answers when an agent asks about it: VT220 DA1, no sixel,
# kitty graphics yes (Ghostty, WezTerm, kitty), xterm's version and colors.
# Without these every agent that queries (DSR, DA1, cell size, colors) sits in
# its own reply timeout, which no real terminal makes it do.
QUERIES = [
    (re.compile(rb"\x1b_Gi=(\d+)[^\x1b]*\x1b\\"), lambda m: b"\x1b_Gi=" + m.group(1) + b";OK\x1b\\"),  # kitty graphics
    (re.compile(rb"\x1b\[5n"), b"\x1b[0n"),  # DSR: ok
    (re.compile(rb"\x1b\[6n"), b"\x1b[1;1R"),  # cursor position
    (re.compile(rb"\x1b\[\?6n"), b"\x1b[?1;1R"),  # DECXCPR
    (re.compile(rb"\x1b\[0?c"), b"\x1b[?62;22c"),  # DA1: VT220, ANSI color
    (re.compile(rb"\x1b\[>0?c"), b"\x1b[>41;390;0c"),  # DA2: xterm 390
    (re.compile(rb"\x1b\[=0?c"), b"\x1bP!|00000000\x1b\\"),  # DA3
    (re.compile(rb"\x1b\[>0?q"), b"\x1bP>|XTerm(390)\x1b\\"),  # XTVERSION
    (re.compile(rb"\x1b\[14t"), b"\x1b[4;800;1200t"),  # window size in px
    (re.compile(rb"\x1b\[16t"), b"\x1b[6;20;10t"),  # cell size in px
    (re.compile(rb"\x1b\[18t"), b"\x1b[8;40;120t"),  # size in cells
    (re.compile(rb"\x1b\[\?(\d+)\$p"), lambda m: b"\x1b[?" + m.group(1) + b";0$y"),  # DECRQM: not recognised
    (re.compile(rb"\x1b\]10;\?(?:\x07|\x1b\\)"), b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\"),  # fg
    (re.compile(rb"\x1b\]11;\?(?:\x07|\x1b\\)"), b"\x1b]11;rgb:0000/0000/0000\x1b\\"),  # bg
    (re.compile(rb"\x1b\]12;\?(?:\x07|\x1b\\)"), b"\x1b]12;rgb:ffff/ffff/ffff\x1b\\"),  # cursor
    (re.compile(rb"\x1b\]4;(\d+);\?(?:\x07|\x1b\\)"), lambda m: b"\x1b]4;" + m.group(1) + b";rgb:8080/8080/8080\x1b\\"),
    (re.compile(rb"\x1bP\+q[0-9a-fA-F;]*\x1b\\"), b"\x1bP0+r\x1b\\"),  # XTGETTCAP: unknown
]
QUERY_RE = re.compile(b"|".join(b"(" + q.pattern + b")" for q, _ in QUERIES))


class Responder:
    def __init__(self, fd):
        self.fd = fd
        self.pos = 0
        self.answered = []

    def feed(self, raw):
        # One pass over the stream in byte order, so replies go back in the
        # order the queries were sent: ratatui-image stops parsing at the DSR
        # reply, so it must come last.
        last = None
        for m in QUERY_RE.finditer(raw, self.pos):
            seq = m.group(0)
            for q, reply in QUERIES:
                mm = q.fullmatch(seq)
                if mm:
                    out = reply(mm) if callable(reply) else reply
                    os.write(self.fd, out)
                    self.answered.append(seq.decode(errors="replace"))
                    break
            last = m.end()
        if last is not None:
            self.pos = last
        else:
            self.pos = max(self.pos, len(raw) - 4096)


def descendants(root):
    kids = {}
    for d in os.listdir("/proc"):
        if not d.isdigit():
            continue
        try:
            with open(f"/proc/{d}/stat", "rb") as f:
                stat = f.read()
        except OSError:
            continue
        ppid = int(stat[stat.rindex(b")") + 2 :].split()[1])
        kids.setdefault(ppid, []).append(int(d))
    out, todo = [], [root]
    while todo:
        p = todo.pop()
        out.append(p)
        todo.extend(kids.get(p, []))
    return out


def mem(pids):
    rss = hwm = pss = 0
    cpu_ticks = 0
    names = []
    for p in pids:
        try:
            with open(f"/proc/{p}/stat", "rb") as f:
                stat = f.read()
            fields = stat[stat.rindex(b")") + 2 :].split()
            cpu_ticks += int(fields[11]) + int(fields[12])  # utime + stime
            with open(f"/proc/{p}/status") as f:
                for line in f:
                    if line.startswith("VmRSS:"):
                        rss += int(line.split()[1]) * 1024
                    elif line.startswith("VmHWM:"):
                        hwm += int(line.split()[1]) * 1024
                    elif line.startswith("Name:"):
                        names.append(line.split(None, 1)[1].strip())
            with open(f"/proc/{p}/smaps_rollup") as f:
                for line in f:
                    if line.startswith("Pss:"):
                        pss += int(line.split()[1]) * 1024
        except OSError:
            pass
    hz = os.sysconf("SC_CLK_TCK")
    return {"rss": rss, "hwm": hwm, "pss": pss, "cpu_ms": cpu_ticks * 1000 // hz, "procs": names}


def kill_tree(pid):
    for p in reversed(descendants(pid)):
        try:
            os.kill(p, signal.SIGKILL)
        except OSError:
            pass


def one_run(cmd, marker, settle, timeout, dump):
    # Size the pty before the child exists so nothing can read 0x0.
    fd, child_fd = os.openpty()
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
    t0 = time.monotonic()
    pid = os.fork()
    if pid == 0:
        os.close(fd)
        os.login_tty(child_fd)
        os.environ["TERM"] = "xterm-256color"
        os.environ["COLUMNS"] = "120"
        os.environ["LINES"] = "40"
        try:
            os.execvp(cmd[0], cmd)
        except OSError as e:
            os.write(2, f"exec failed: {e}\n".encode())
            os._exit(127)
    os.close(child_fd)
    raw = bytearray()
    responder = Responder(fd)
    t_prompt = None
    deadline = t0 + timeout
    hit_at = None
    while True:
        now = time.monotonic()
        if t_prompt is None and now > deadline:
            break
        if t_prompt is not None and now > hit_at + settle:
            break
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            raw += chunk
            if TRACE:
                print(f"  +{time.monotonic() - t0:7.3f}s {len(chunk):6d}B {strip(chunk)[:70]!r}", file=sys.stderr)
            responder.feed(raw)
            if t_prompt is None and marker.search(strip(bytes(raw))):
                t_prompt = time.monotonic() - t0
                hit_at = time.monotonic()
        # child exited before drawing a prompt
        if t_prompt is None:
            wp, _ = os.waitpid(pid, os.WNOHANG)
            if wp == pid:
                break
    m = mem(descendants(pid)) if t_prompt is not None else None
    if dump:
        with open(dump, "wb") as f:
            f.write(strip(bytes(raw)))
    kill_tree(pid)
    try:
        os.close(fd)
    except OSError:
        pass
    try:
        os.waitpid(pid, 0)
    except ChildProcessError:
        pass
    if m is not None:
        m["queries"] = responder.answered
    return t_prompt, m, bytes(raw)


def time_version(name, cmd, runs, timeout):
    out, lines = [], []
    for i in range(runs):
        t0 = time.monotonic()
        try:
            p = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=timeout)
        except subprocess.TimeoutExpired as e:
            print(f"[{name}] version run {i + 1}: no exit within {timeout}s", file=sys.stderr)
            print((e.output or b"")[-600:].decode(errors="replace"), file=sys.stderr)
            continue
        out.append(time.monotonic() - t0)
        lines = p.stdout.decode(errors="replace").strip().splitlines()[:3]
    return out, lines


def pctl(xs, q):
    xs = sorted(xs)
    if not xs:
        return None
    k = (len(xs) - 1) * q
    lo, hi = int(k), min(int(k) + 1, len(xs) - 1)
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--name", required=True)
    ap.add_argument("--cmd", required=True, help="shell words of the agent command")
    ap.add_argument("--marker", required=True, help="regex matched against ANSI-stripped output")
    ap.add_argument("--version-cmd", default=None)
    ap.add_argument("--runs", type=int, default=10)
    ap.add_argument("--settle", type=float, default=3.0)
    ap.add_argument("--timeout", type=float, default=90.0)
    ap.add_argument("--dump", default=None)
    ap.add_argument("--out", required=True)
    a = ap.parse_args()
    import shlex

    cmd = shlex.split(a.cmd)
    marker = re.compile(a.marker.encode())
    times, mems, fails = [], [], 0
    cold = None
    for i in range(a.runs):
        t, m, raw = one_run(cmd, marker, a.settle, a.timeout, a.dump if i == 0 else None)
        if t is None:
            fails += 1
            print(f"[{a.name}] run {i + 1}: no prompt within {a.timeout}s", file=sys.stderr)
            tail = strip(raw)[-600:].decode(errors="replace")
            print(tail, file=sys.stderr)
            continue
        if i == 0:
            cold = t
        else:
            times.append(t)
        mems.append(m)
        print(f"[{a.name}] run {i + 1}: {t * 1000:.0f} ms, rss {m['rss'] / 1e6:.1f} MB, cpu {m['cpu_ms']} ms", file=sys.stderr)
        time.sleep(0.5)
    res = {
        "name": a.name,
        "cmd": a.cmd,
        "marker": a.marker,
        "runs": a.runs,
        "settle_s": a.settle,
        "failed_runs": fails,
        "cold_ms": round(cold * 1000, 1) if cold is not None else None,
        "warm_ms": [round(t * 1000, 1) for t in times],
        "warm_ms_median": round(statistics.median(times) * 1000, 1) if times else None,
        "warm_ms_p90": round(pctl(times, 0.9) * 1000, 1) if times else None,
        "warm_ms_min": round(min(times) * 1000, 1) if times else None,
        "warm_ms_max": round(max(times) * 1000, 1) if times else None,
        "rss_bytes_median": statistics.median(m["rss"] for m in mems) if mems else None,
        "hwm_bytes_median": statistics.median(m["hwm"] for m in mems) if mems else None,
        "pss_bytes_median": statistics.median(m["pss"] for m in mems) if mems else None,
        "cpu_ms_median": statistics.median(m["cpu_ms"] for m in mems) if mems else None,
        "procs": mems[0]["procs"] if mems else None,
        "terminal_queries_answered": mems[0]["queries"] if mems else None,
    }
    if a.version_cmd:
        vt, vout = time_version(a.name, shlex.split(a.version_cmd), a.runs, a.timeout)
        res["version_cmd"] = a.version_cmd
        res["version_output"] = vout
        res["version_failed_runs"] = a.runs - len(vt)
        res["version_ms"] = [round(t * 1000, 1) for t in vt]
        res["version_ms_median"] = round(statistics.median(vt) * 1000, 1) if vt else None
        res["version_ms_p90"] = round(pctl(vt, 0.9) * 1000, 1) if vt else None
    with open(a.out, "w") as f:
        json.dump(res, f, indent=2)
    print(json.dumps({k: v for k, v in res.items() if not isinstance(v, list)}, indent=1), file=sys.stderr)


if __name__ == "__main__":
    main()
