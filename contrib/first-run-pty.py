#!/usr/bin/env python3
"""Drive a fresh `wizard` first run under a pty and time it.

Runs the binary with an empty HOME, answers the one first-run screen, and
writes the ANSI-stripped screen after each step to OUT. Prints the timings
the docs quote: start to first screen, Enter to the TUI prompt, and (with
--oauth) start to the sign-in URL.

    contrib/first-run-pty.py --bin target/release/wizard --out /tmp/screens
    contrib/first-run-pty.py --bin target/release/wizard --out /tmp/screens --oauth

The key path exports OPENAI_API_KEY so the provider row is preselected and no
key is pasted; the TUI's health probe of api.openai.com fails in the
background and does not block the prompt.
"""

import argparse
import os
import pty
import re
import select
import shutil
import subprocess
import sys
import tempfile
import time

COLS, ROWS = 100, 30
CSI = re.compile(r"\x1b\[([\x30-\x3f]*)([\x20-\x2f]*)([\x40-\x7e])")
OSC = re.compile(r"\x1b\][^\x07\x1b]*(\x07|\x1b\\)")
# DCS / APC strings (the terminal image probe is one): skipped whole.
STRING = re.compile(r"\x1b[P_].*?\x1b\\", re.S)


class Screen:
    """Enough of a VT100 to read ratatui's output back as rows of text."""

    def __init__(self):
        self.rows = [[" "] * COLS for _ in range(ROWS)]
        self.r = self.c = 0
        self.raw = ""

    def feed(self, data):
        self.raw += data
        i = 0
        while i < len(data):
            ch = data[i]
            if ch == "\x1b":
                m = CSI.match(data, i)
                if m:
                    self.csi(m.group(1), m.group(3))
                    i = m.end()
                    continue
                m = OSC.match(data, i) or STRING.match(data, i)
                if m:
                    i = m.end()
                    continue
                i += 2  # ESC + one char (e.g. ESC 7)
                continue
            if ch == "\r":
                self.c = 0
            elif ch == "\n":
                self.r = min(self.r + 1, ROWS - 1)
            elif ch == "\b":
                self.c = max(self.c - 1, 0)
            elif ch >= " ":
                if self.c < COLS and self.r < ROWS:
                    self.rows[self.r][self.c] = ch
                self.c += 1
            i += 1

    def csi(self, params, final):
        nums = [int(p) if p.isdigit() else 0 for p in params.lstrip("?").split(";")]
        if params.startswith("?") and final in "hl":
            # Entering or leaving the alternate screen starts from blank.
            if 1049 in nums or 47 in nums:
                self.rows = [[" "] * COLS for _ in range(ROWS)]
                self.r = self.c = 0
            return
        if final == "H" or final == "f":
            self.r = max(0, min(ROWS - 1, (nums[0] or 1) - 1))
            self.c = max(0, min(COLS - 1, (nums[1] if len(nums) > 1 else 1 or 1) - 1))
        elif final == "J":
            if nums[0] in (2, 3):
                self.rows = [[" "] * COLS for _ in range(ROWS)]
        elif final == "K":
            if self.r < ROWS:
                for x in range(self.c, COLS):
                    self.rows[self.r][x] = " "
        elif final == "A":
            self.r = max(0, self.r - (nums[0] or 1))
        elif final == "B":
            self.r = min(ROWS - 1, self.r + (nums[0] or 1))
        elif final == "C":
            self.c = min(COLS - 1, self.c + (nums[0] or 1))
        elif final == "D":
            self.c = max(0, self.c - (nums[0] or 1))
        elif final == "G":
            self.c = max(0, min(COLS - 1, (nums[0] or 1) - 1))
        elif final == "d":
            self.r = max(0, min(ROWS - 1, (nums[0] or 1) - 1))

    def text(self):
        return "\n".join("".join(row).rstrip() for row in self.rows).rstrip("\n") + "\n"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--oauth", action="store_true", help="pick 'Sign in with xAI' and stop at the URL")
    ap.add_argument("--timeout", type=float, default=20.0)
    ap.add_argument("--keep", action="store_true", help="leave the temp HOME behind and print its path")
    args = ap.parse_args()

    os.makedirs(args.out, exist_ok=True)
    home = tempfile.mkdtemp(prefix="wizard-first-run-")
    project = os.path.join(home, "project")
    os.makedirs(project)
    with open(os.path.join(project, "README.md"), "w") as f:
        f.write("# demo\n")
    with open(os.path.join(project, "Cargo.toml"), "w") as f:
        f.write('[package]\nname = "demo"\nversion = "0.1.0"\n')
    os.makedirs(os.path.join(project, "src"))
    with open(os.path.join(project, "src", "lib.rs"), "w") as f:
        f.write("pub fn one() -> u8 { 1 }\n")
    git = dict(cwd=project, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    subprocess.run(["git", "init", "-q"], **git)
    subprocess.run(["git", "-c", "user.name=demo", "-c", "user.email=d@x", "add", "-A"], **git)
    subprocess.run(
        ["git", "-c", "user.name=demo", "-c", "user.email=d@x", "commit", "-q", "-m", "init"],
        **git,
    )
    with open(os.path.join(project, "src", "lib.rs"), "a") as f:
        f.write("pub fn two() -> u8 { 2 }\n")

    env = {
        "HOME": home,
        "WIZARD_HOME": os.path.join(home, ".wizard"),
        "PATH": os.environ.get("PATH", ""),
        "TERM": "xterm-256color",
        "LANG": "C.UTF-8",
        "OPENAI_API_KEY": "dummy",
    }
    if "WIZARD_LOG" in os.environ:
        env["WIZARD_LOG"] = os.environ["WIZARD_LOG"]

    master, slave = pty.openpty()
    import fcntl
    import struct
    import termios

    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    t0 = time.monotonic()
    proc = subprocess.Popen(
        [args.bin],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        env=env,
        cwd=project,
        close_fds=True,
        start_new_session=True,
    )
    os.close(slave)
    screen = Screen()
    timings = {}
    step = [0]
    answered = [False]

    def answer_terminal_queries():
        # The TUI asks the terminal what it can draw (DA1, cell size) and
        # waits for the reply; answer as a plain xterm would, once.
        if not answered[0] and "\x1b[c" in screen.raw:
            answered[0] = True
            os.write(master, b"\x1b[6;16;8t\x1b[?62;4c\x1b[0n")

    def pump(until, what, budget=None):
        deadline = time.monotonic() + (budget or args.timeout)
        while time.monotonic() < deadline:
            if proc.poll() is not None and not select.select([master], [], [], 0)[0]:
                break
            ready, _, _ = select.select([master], [], [], 0.05)
            if ready:
                try:
                    data = os.read(master, 65536)
                except OSError:
                    data = b""
                if not data:
                    proc.wait(timeout=2)
                    if until(screen):
                        return time.monotonic()
                    break
                screen.feed(data.decode("utf-8", "replace"))
                answer_terminal_queries()
            if until(screen):
                return time.monotonic()
        sys.stderr.write(f"timed out waiting for {what}\n{screen.text()}\n")
        proc.kill()
        sys.exit(1)

    def dump(name):
        # Let the frame finish arriving: the match fires on its first row.
        end = time.monotonic() + 0.3
        while time.monotonic() < end:
            ready, _, _ = select.select([master], [], [], 0.05)
            if ready:
                try:
                    screen.feed(os.read(master, 65536).decode("utf-8", "replace"))
                except OSError:
                    break
                answer_terminal_queries()
        step[0] += 1
        path = os.path.join(args.out, f"{step[0]:02d}-{name}.txt")
        with open(path, "w") as f:
            f.write(screen.text())
        print(f"wrote {path}")

    def send(data):
        os.write(master, data.encode())

    t = pump(lambda s: "How do you want to run Wizard?" in s.text(), "the first screen")
    timings["start_to_first_screen"] = t - t0
    dump("first-screen")

    if args.oauth:
        t_enter = time.monotonic()
        send("\r")
        t = pump(lambda s: "open this URL" in s.raw, "the sign-in URL")
        timings["start_to_oauth_url"] = t - t0
        timings["enter_to_oauth_url"] = t - t_enter
        dump("oauth-url")
        proc.kill()
    else:
        send("\x1b[B\x1b[B")  # down twice: Paste an API key
        pump(lambda s: "▸ Paste an API key" in s.text(), "the key row")
        dump("first-screen-key-row")
        send("\r")
        pump(lambda s: "Which provider is the key for?" in s.text(), "the provider list")
        dump("provider-list")
        text = screen.text()
        assert "use $OPENAI_API_KEY" in text, text
        t_enter = time.monotonic()
        send("\r")
        t = pump(lambda s: "type a message" in s.text(), "the TUI prompt")
        timings["enter_to_tui_prompt"] = t - t_enter
        timings["start_to_tui_prompt"] = t - t0
        time.sleep(0.3)
        dump("tui")
        text = screen.text()
        for needle in ("set up: openai", "Explain how this project is put together", "Review my uncommitted changes"):
            assert needle in text, f"missing {needle!r}:\n{text}"
        for key, value in timings.items():
            print(f"{key}: {value:.3f}s")
        send("\x03")
        time.sleep(0.3)
        send("\x03")
        t_quit = time.monotonic()
        pump(lambda s: proc.poll() is not None, "exit", budget=15)
        print(f"ctrl_c_to_exit: {time.monotonic() - t_quit:.3f}s")

    config = os.path.join(home, ".wizard", "config.toml")
    assert os.path.exists(config), config
    with open(config) as f:
        print("config.toml:\n" + f.read())
    if args.oauth:
        for key, value in timings.items():
            print(f"{key}: {value:.3f}s")
    if args.keep:
        print(f"kept {home}")
    else:
        shutil.rmtree(home, ignore_errors=True)


if __name__ == "__main__":
    main()
