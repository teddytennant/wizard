#!/usr/bin/env python3
"""Drive a fresh `wizard` first run under a pty and time it.

Runs the binary with an empty HOME in a small demo git repo, answers the one
first-run screen, writes the ANSI-stripped screen after each step to OUT, and
prints timings: start to first screen, Enter to the TUI prompt, and with
--scenario oauth, start to the sign-in URL.

    contrib/first-run-pty.py --bin target/release/wizard --out /tmp/screens
    contrib/first-run-pty.py --bin target/release/wizard --out /tmp/screens --scenario stale

Scenarios:
  happy    paste a key from $WIZARD_PTY_KEY (`ANTHROPIC_API_KEY=sk-…`) and reach
           the TUI; without one, runs offline (`unshare -rn`) so the check is
           inconclusive and the TUI opens with the key saved unchecked
  badkey   paste a bad Anthropic key: the list comes back with the reason
  stale    OPENAI_API_KEY=dummy is exported: preselected, kept, rejected
  oauth    pick xAI, stop at the URL with Ctrl-C, relaunch: the card names /login
  notty    no terminal, and `-p` in one: no onboarding, one line, exit 1
  narrow   the happy path at 80x24

The child gets the pty as its controlling terminal, so Ctrl-C is a real SIGINT.
"""

import argparse
import codecs
import fcntl
import os
import pty
import re
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time

CSI = re.compile(r"\x1b\[([\x30-\x3f]*)([\x20-\x2f]*)([\x40-\x7e])")
OSC = re.compile(r"\x1b\][^\x07\x1b]*(\x07|\x1b\\)")
# DCS / APC strings (the terminal image probe is one): skipped whole.
STRING = re.compile(r"\x1b[P_].*?\x1b\\", re.S)


class Screen:
    """Enough of a VT100 to read ratatui's output back as rows of text."""

    def __init__(self, cols, rows):
        self.cols, self.rows_n = cols, rows
        self.rows = [[" "] * cols for _ in range(rows)]
        self.r = self.c = 0
        self.raw = ""

    def blank(self):
        self.rows = [[" "] * self.cols for _ in range(self.rows_n)]
        self.r = self.c = 0

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
                i += 2
                continue
            if ch == "\r":
                self.c = 0
            elif ch == "\n":
                self.r = min(self.r + 1, self.rows_n - 1)
            elif ch == "\b":
                self.c = max(self.c - 1, 0)
            elif ch >= " ":
                if self.c < self.cols and self.r < self.rows_n:
                    self.rows[self.r][self.c] = ch
                self.c += 1
            i += 1

    def csi(self, params, final):
        nums = [int(p) if p.isdigit() else 0 for p in params.lstrip("?").split(";")]
        if params.startswith("?") and final in "hl":
            if 1049 in nums or 47 in nums:
                self.blank()
            return
        if final in "Hf":
            self.r = max(0, min(self.rows_n - 1, (nums[0] or 1) - 1))
            self.c = max(0, min(self.cols - 1, ((nums[1] if len(nums) > 1 else 1) or 1) - 1))
        elif final == "J":
            if nums[0] in (2, 3):
                self.blank()
        elif final == "K":
            if self.r < self.rows_n:
                for x in range(self.c, self.cols):
                    self.rows[self.r][x] = " "
        elif final == "A":
            self.r = max(0, self.r - (nums[0] or 1))
        elif final == "B":
            self.r = min(self.rows_n - 1, self.r + (nums[0] or 1))
        elif final == "C":
            self.c = min(self.cols - 1, self.c + (nums[0] or 1))
        elif final == "D":
            self.c = max(0, self.c - (nums[0] or 1))
        elif final == "G":
            self.c = max(0, min(self.cols - 1, (nums[0] or 1) - 1))
        elif final == "d":
            self.r = max(0, min(self.rows_n - 1, (nums[0] or 1) - 1))

    def text(self):
        return "\n".join("".join(row).rstrip() for row in self.rows).rstrip("\n") + "\n"


def make_project(home):
    project = os.path.join(home, "project")
    os.makedirs(os.path.join(project, "src"))
    with open(os.path.join(project, "README.md"), "w") as f:
        f.write("# demo\n")
    with open(os.path.join(project, "Cargo.toml"), "w") as f:
        f.write('[package]\nname = "demo"\nversion = "0.1.0"\n')
    with open(os.path.join(project, "src", "lib.rs"), "w") as f:
        f.write("pub fn one() -> u8 { 1 }\n")
    git = dict(cwd=project, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    ident = ["-c", "user.name=demo", "-c", "user.email=d@x"]
    subprocess.run(["git", "init", "-q"], **git)
    subprocess.run(["git", *ident, "add", "-A"], **git)
    subprocess.run(["git", *ident, "commit", "-q", "-m", "init"], **git)
    with open(os.path.join(project, "src", "lib.rs"), "a") as f:
        f.write("pub fn two() -> u8 { 2 }\n")
    return project


def _child_setup():
    # A shell that started this in the background left SIGINT ignored, and
    # the child would inherit that; Ctrl-C has to mean what it means. Reset
    # here, in the child only: the parent keeps KeyboardInterrupt so its
    # cleanup still runs.
    signal.signal(signal.SIGINT, signal.SIG_DFL)
    # The pty becomes the controlling terminal, so Ctrl-C is SIGINT.
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


class Session:
    """One `wizard` process on a pty, with a screen model and step dumps."""

    def __init__(self, args, home, project, env, cols, rows, extra_args=(), offline=False):
        self.args = args
        self.timings = {}
        self.screen = Screen(cols, rows)
        # One read can end mid-glyph; the decoder holds the tail for the next.
        self.decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
        self.answered = False
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        cmd = [args.bin, *extra_args]
        if offline:
            cmd = ["unshare", "-rn", *cmd]
        self.t0 = time.monotonic()
        self.proc = subprocess.Popen(
            cmd,
            stdin=slave,
            stdout=slave,
            stderr=slave,
            env=env,
            cwd=project,
            close_fds=True,
            start_new_session=True,
            preexec_fn=_child_setup,
        )
        os.close(slave)
        self.master = master

    def read_some(self, wait):
        ready, _, _ = select.select([self.master], [], [], wait)
        if not ready:
            return True
        try:
            data = os.read(self.master, 65536)
        except OSError:
            data = b""
        if not data:
            self.screen.feed(self.decoder.decode(b"", final=True))
            return False
        self.screen.feed(self.decoder.decode(data))
        # The TUI asks the terminal what it can draw (DA1, cell size) and
        # waits for the reply; answer as a plain xterm would, once.
        if not self.answered and "\x1b[c" in self.screen.raw:
            self.answered = True
            os.write(self.master, b"\x1b[6;16;8t\x1b[?62;4c\x1b[0n")
        return True

    def wait_for(self, until, what, budget=None):
        deadline = time.monotonic() + (budget or self.args.timeout)
        while time.monotonic() < deadline:
            alive = self.read_some(0.05)
            if until(self.screen):
                return time.monotonic()
            if not alive:
                try:
                    self.proc.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    pass
                if until(self.screen):
                    return time.monotonic()
                break
        sys.stderr.write(f"timed out waiting for {what}\n{self.screen.text()}\n")
        self.proc.kill()
        sys.exit(1)

    def settle(self, seconds=0.5):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            self.read_some(0.05)

    def send(self, data):
        os.write(self.master, data.encode())

    def exited(self):
        return self.proc.poll() is not None

    def close(self):
        if self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait()
        os.close(self.master)

    def quit(self):
        self.send("\x03")
        time.sleep(0.3)
        self.send("\x03")
        t = time.monotonic()
        self.wait_for(lambda s: self.exited(), "exit", budget=15)
        self.timings["ctrl_c_to_exit"] = time.monotonic() - t


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument(
        "--scenario",
        default="happy",
        choices=["happy", "badkey", "stale", "oauth", "notty", "narrow"],
    )
    ap.add_argument("--timeout", type=float, default=20.0)
    ap.add_argument("--keep", action="store_true", help="leave the temp HOME behind and print its path")
    args = ap.parse_args()
    args.bin = os.path.abspath(args.bin)

    os.makedirs(args.out, exist_ok=True)
    home = tempfile.mkdtemp(prefix="wizard-first-run-")
    sessions = []
    try:
        run_scenario(args, home, sessions)
    finally:
        # Runs on Ctrl-C, on a timed-out wait and on a failed assert alike, so
        # the typed key never outlives the run and no child is left on the pty.
        for session in sessions:
            session.close()
        if args.keep:
            print(f"kept {home}")
        else:
            shutil.rmtree(home, ignore_errors=True)


def run_scenario(args, home, sessions):
    project = make_project(home)
    env = {
        "HOME": home,
        "WIZARD_HOME": os.path.join(home, ".wizard"),
        "PATH": os.environ.get("PATH", ""),
        "TERM": "xterm-256color",
        "LANG": "C.UTF-8",
    }
    if "WIZARD_LOG" in os.environ:
        env["WIZARD_LOG"] = os.environ["WIZARD_LOG"]
    cols, rows = (80, 24) if args.scenario == "narrow" else (100, 30)
    step = [0]

    def start(**kw):
        session = Session(args, home, project, env, cols, rows, **kw)
        sessions.append(session)
        return session

    def dump(session, name):
        session.settle()
        step[0] += 1
        path = os.path.join(args.out, f"{step[0]:02d}-{name}.txt")
        with open(path, "w") as f:
            f.write(session.screen.text())
        print(f"wrote {path}")
        return session.screen.text()

    def first_screen(session):
        t = session.wait_for(lambda s: "How do you want to run Wizard?" in s.text(), "the first screen")
        session.timings["start_to_first_screen"] = t - session.t0
        return dump(session, "first-screen")

    def to_key_list(session):
        session.send("\x1b[B\x1b[B")  # Paste an API key (row 3)
        session.wait_for(lambda s: "▸ Paste an API key" in s.text(), "the key row")
        session.send("\r")
        session.wait_for(lambda s: "Which provider?" in s.text(), "the provider list")
        return dump(session, "provider-list")

    def type_key(session, key):
        for ch in key:
            session.send(ch)
        session.settle(0.2)

    def expect_tui(session, name="tui"):
        t_enter = time.monotonic()
        session.send("\r")
        t = session.wait_for(lambda s: "type a message" in s.text(), "the TUI prompt")
        session.timings["enter_to_tui_prompt"] = t - t_enter
        session.timings["start_to_tui_prompt"] = t - session.t0
        text = dump(session, name)
        for needle in ("saved ~/.wizard/config.toml", "Explain how this project is put together", "Review my uncommitted changes", "Add a test for src/lib.rs"):
            assert needle in text, f"missing {needle!r}:\n{text}"
        return text

    config = os.path.join(home, ".wizard", "config.toml")

    if args.scenario in ("happy", "narrow"):
        key = os.environ.get("WIZARD_PTY_KEY", "")
        offline = not key
        if offline:
            print("no WIZARD_PTY_KEY: running offline, the check is inconclusive and the key is saved unchecked")
            key = "ANTHROPIC_API_KEY=sk-ant-offline"
        var, value = key.split("=", 1)
        session = start(offline=offline)
        text = first_screen(session)
        for row in text.splitlines():
            assert len(row) <= cols, f"row wider than {cols}: {row!r}"
        assert "Run a model locally" in text and "download, llama.cpp" in text or "via Ollama" in text or "already downloaded" in text, text
        to_key_list(session)
        # Anthropic is the second row.
        session.send("\x1b[B")
        session.settle(0.2)
        session.send("\r")
        session.wait_for(lambda s: "API key" in s.text() and "Stored in" in s.text(), "the paste screen")
        type_key(session, value[:6])
        text = dump(session, "paste-masked")
        assert "sk-a••" in text, f"the key is masked: {text}"
        type_key(session, value[6:])
        expect_tui(session)
        assert os.path.exists(config), config
        session.quit()

    elif args.scenario == "badkey":
        session = start()
        first_screen(session)
        to_key_list(session)
        session.send("\x1b[B")
        session.settle(0.2)
        session.send("\r")
        session.wait_for(lambda s: "Anthropic (Claude) API key" in s.text(), "the paste screen")
        dump(session, "paste")
        type_key(session, "sk-ant-not-a-real-key")
        session.send("\r")
        session.wait_for(lambda s: "rejected that key (401)" in s.text(), "the rejection")
        text = dump(session, "rejected")
        assert "api.anthropic.com rejected that key (401). Paste another, or Esc." in text, text
        assert "{" not in text, f"raw JSON on screen: {text}"
        assert not os.path.exists(config), "a rejected key must not be saved"
        session.send("\x1b")
        session.wait_for(lambda s: "How do you want to run Wizard?" in s.text(), "the first screen again")
        dump(session, "back-to-first-screen")
        session.send("\x1b")
        session.wait_for(lambda s: session.exited(), "exit", budget=5)
        assert not os.path.exists(config), "Esc saves nothing"

    elif args.scenario == "stale":
        env["OPENAI_API_KEY"] = "dummy"
        session = start()
        first_screen(session)
        text = to_key_list(session)
        assert "▸ OpenAI   gpt-5.6-sol · $OPENAI_API_KEY is set" in text, text
        session.send("\r")
        session.wait_for(lambda s: "$OPENAI_API_KEY is set (" in s.text(), "the keep-or-paste screen")
        text = dump(session, "keep-or-paste")
        assert "Enter keeps it, or paste another key." in text, text
        session.send("\r")
        session.wait_for(lambda s: "rejected that key (401)" in s.text(), "the rejection")
        text = dump(session, "rejected")
        assert "api.openai.com rejected that key (401)" in text, text
        assert not os.path.exists(config)
        session.send("\x1b")
        session.wait_for(lambda s: "How do you want to run Wizard?" in s.text(), "the first screen again")
        session.send("\x1b")
        session.wait_for(lambda s: session.exited(), "exit", budget=5)

    elif args.scenario == "oauth":
        session = start()
        first_screen(session)
        t_enter = time.monotonic()
        session.send("\r")
        t = session.wait_for(lambda s: "open this URL" in s.raw, "the sign-in URL")
        session.timings["start_to_oauth_url"] = t - session.t0
        session.timings["enter_to_oauth_url"] = t - t_enter
        dump(session, "oauth-url")
        session.send("\x03")
        session.wait_for(lambda s: session.exited(), "exit on Ctrl-C", budget=5)
        session.timings["ctrl_c_to_exit"] = time.monotonic() - t
        assert os.path.exists(config), "the config is saved before the sign-in"
        for k, v in session.timings.items():
            print(f"{k}: {v:.3f}s")
        # Relaunch: no wizard, straight to the TUI, the card says what to do.
        session = start()
        session.wait_for(lambda s: "type a message" in s.text(), "the TUI after an abandoned sign-in")
        session.wait_for(lambda s: "not signed in to xAI" in s.text(), "the card's remedy line", budget=10)
        text = dump(session, "relaunch-tui")
        assert "⚠ not signed in to xAI: /login xai" in text, text
        assert "provider unreachable" not in text, text
        session.quit()

    elif args.scenario == "notty":
        # Piped: no terminal at all.
        piped = subprocess.run([args.bin], env=env, cwd=project, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=20)
        line = "no config yet: run `wizard` once to pick a provider"
        assert piped.returncode != 0 and line in piped.stderr, (piped.returncode, piped.stderr)
        with open(os.path.join(args.out, "01-piped.txt"), "w") as f:
            f.write(piped.stderr)
        print(f"wrote {os.path.join(args.out, '01-piped.txt')}")
        # A terminal, but a prompt on the command line: still no wizard.
        session = start(extra_args=["-p", "hi"])
        session.wait_for(lambda s: session.exited(), "exit", budget=10)
        text = dump(session, "prompt-in-a-terminal")
        assert line in text and "How do you want" not in text, text
        assert session.proc.returncode != 0
        assert not os.path.exists(config)

    if sessions:
        for k, v in sessions[-1].timings.items():
            print(f"{k}: {v:.3f}s")


if __name__ == "__main__":
    main()
