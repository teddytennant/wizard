#!/usr/bin/env python3
"""Drive jcode and the look through the same keys and diff the screens cell by cell.

    python3 crates/jcodew/tools/compare.py                      # look vs the committed captures
    python3 crates/jcodew/tools/compare.py --jcode /path/jcode  # capture jcode again first

Everything runs in a private tmux server (`tmux -L jcodew-cmp`) with throwaway HOME
directories under --work. jcode talks to `fake_llm.py`; the look runs its scripted backend
(`JCODEW_BACKEND=mock`), which plays the same scenarios. Captures keep their escape sequences.

What is normalised before the diff, because it legitimately differs:
  - the product name, and jcode's server/client/login/provider rows in the header
    (the look prints a version row and the providers wizard reports instead)
  - jcode's Updates box and the info widgets wizard has no data for (session, transport, cache)
  - spinner frame, elapsed times, token counts, transport labels on the status line
  - the context meter before the first turn (wizard reports no size until then)
  - the scrollbar thumb, whose size and place follow the header's height (printed as a note)
  - the idle tip on the status line
Rows whose text comes from the backend (command lists, help) are compared by style only.
"""

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import time

sys.dont_write_bytecode = True
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import cells  # noqa: E402

SIZES = [(80, 24), (120, 40), (200, 50)]
REF_DIR = os.path.join(HERE, "..", "tests", "reference")
MODELS = ["grok-4.7", "grok-4.7-fast", "grok-4.6"]

T = ("type",)
K = ("key",)


def typ(s):
    return ("type", s)


def key(*names):
    return ("key", names)


IDLE = ("idle",)


def wait(s):
    return ("wait", s)


# name -> (steps, options)
SCENES = {
    "cold": ([], {}),
    "typing": ([typ("hello world")], {}),
    "multiline": ([typ("line one"), key("M-Enter"), typ("line two"), key("M-Enter"), typ("line three")], {}),
    "slash": ([typ("/")], {"loose": "below", "tail": True}),
    "slash-filter": ([typ("/mo")], {"loose": "below", "tail": True}),
    "slash-down": ([typ("/"), key("Down", "Down")], {"loose": "below", "tail": True}),
    "help": ([typ("/help"), key("Enter")], {"loose": "styles"}),
    "model": ([typ("/model")], {"tail": True}),
    "model-down": ([typ("/model"), key("Down")], {"tail": True}),
    "model-filter": ([typ("/model fast")], {"tail": True}),
    "text": ([typ("fake:text"), key("Enter"), IDLE], {}),
    "markdown": ([typ("fake:markdown"), key("Enter"), IDLE], {}),
    "bash": ([typ("fake:bash"), key("Enter"), IDLE], {}),
    "bashfail": ([typ("fake:bashfail"), key("Enter"), IDLE], {}),
    "edit": ([typ("fake:edit"), key("Enter"), IDLE], {}),
    "read": ([typ("fake:edit"), key("Enter"), IDLE, typ("fake:read"), key("Enter"), IDLE], {}),
    "todo": ([typ("fake:todo"), key("Enter"), IDLE], {}),
    "running": ([typ("fake:slow"), key("Enter"), wait(4)], {}),
    "interrupt": ([typ("fake:slow"), key("Enter"), wait(4), key("Escape"), IDLE, wait(4)], {}),
    "queued": ([typ("fake:slow"), key("Enter"), wait(3), typ("next one"), key("Enter"), wait(1)], {}),
    "busy-typing": ([typ("fake:slow"), key("Enter"), wait(3), typ("draft")], {}),
    "long": ([typ("fake:long"), key("Enter"), IDLE], {}),
    # jcode slides a long reply in a few rows a frame; let it land before scrolling
    "scrolled": ([typ("fake:long"), key("Enter"), IDLE, wait(2), key("PPage", "PPage")], {}),
    "prompt-jump": (
        [typ("fake:text"), key("Enter"), IDLE, typ("fake:long"), key("Enter"), IDLE, wait(2), key("C-k")],
        {"expect": "jump to the previous prompt"},
    ),
    "history": ([typ("fake:text"), key("Enter"), IDLE, key("Up")], {}),
    "two-turns": ([typ("fake:text"), key("Enter"), IDLE, typ("fake:bash"), key("Enter"), IDLE], {}),
    "quit-arm": ([key("C-c"), wait(0.4)], {"expect": "Press Ctrl+C again"}),
    "esc-clear": ([typ("some draft"), key("Escape"), wait(0.4)], {}),
    "clear": ([typ("fake:text"), key("Enter"), IDLE, typ("/clear"), key("Enter"), wait(1)], {}),
    "resume": ([typ("/resume"), key("Enter"), wait(2)], {"loose": "styles"}),
    "exit": ([typ("fake:text"), key("Enter"), IDLE, key("C-c"), wait(0.3), key("C-c"), wait(2)], {"expect": "to resume"}),
    "git": ([], {"project": "gitproj"}),
}

HEADER_REF_ONLY = re.compile(r"^ (server: |client: |/login to add provider|[○●◐] (anthropic|openrouter|openai|cursor|copilot|gemini|antigravity))")
HEADER_LOOK_ONLY = re.compile(r"^ (v\d+\.\d+\.\d+\S*$|● \S+$)")
OMITTED_WIDGETS = ("Overview", "Runtime", "KV cache", "Commits", "Changes")
BLANK = (" ", "default", "default", 0)
SKIP = ("\0", "default", "default", 0)
SKIP_ROW = [SKIP] * 400


class Tmux:
    def __init__(self, socket):
        self.socket = socket

    def run(self, *args, check=False):
        return subprocess.run(["tmux", "-L", self.socket, "-f", "/dev/null", *args], capture_output=True, text=True, check=check)

    def capture(self, session):
        return self.run("capture-pane", "-t", session, "-e", "-p").stdout

    def plain(self, session):
        return self.run("capture-pane", "-t", session, "-p").stdout

    def kill(self, session):
        self.run("kill-session", "-t", session)


def clean_env(home, extra):
    env = {
        "HOME": home,
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "TERM": "xterm-256color",
        "COLORTERM": "truecolor",
        "LANG": "C.UTF-8",
        "SHELL": "/bin/sh",
        "USER": os.environ.get("USER", "user"),
    }
    env.update(extra)
    return env


def env_prefix(env):
    return "env -i " + " ".join("%s=%s" % (k, shquote(v)) for k, v in env.items())


def shquote(s):
    return "'" + s.replace("'", "'\\''") + "'"


class Program:
    """One of the two programs: how to start it and where its captures go."""

    def __init__(self, name, work, binary):
        self.name = name
        self.work = work
        self.binary = binary
        self.home = os.path.join(work, "h-" + name)

    def command(self):
        raise NotImplementedError

    def prepare(self, tmux):
        pass

    def finish(self):
        pass


class Look(Program):
    def command(self):
        env = clean_env(self.home, {"JCODEW_BACKEND": "mock", "JCODEW_STATE_DIR": os.path.join(self.home, "state")})
        return "%s %s" % (env_prefix(env), shquote(self.binary))

    def prepare(self, tmux):
        shutil.rmtree(self.home, ignore_errors=True)
        os.makedirs(self.home)


class Jcode(Program):
    def __init__(self, name, work, binary, port):
        super().__init__(name, work, binary)
        self.port = port
        self.fake = None

    def env(self):
        rt = os.path.join(self.home, "rt")
        return clean_env(
            self.home,
            {
                "JCODE_HOME": os.path.join(self.home, ".jcode"),
                "JCODE_RUNTIME_DIR": rt,
                "XDG_RUNTIME_DIR": rt,
                "JCODE_NO_TELEMETRY": "1",
                "DO_NOT_TRACK": "1",
                "JCODE_CHECK_UPDATES": "false",
                "JCODE_NO_AUTO_UPDATE": "1",
                # wizard queues a prompt typed during a turn, so compare with jcode doing the same
                "JCODE_QUEUE_MODE": "1",
            },
        )

    def command(self):
        return "%s %s --no-update --no-selfdev --provider-profile fake" % (env_prefix(self.env()), shquote(self.binary))

    def prepare(self, tmux):
        shutil.rmtree(self.home, ignore_errors=True)
        rt = os.path.join(self.home, "rt")
        os.makedirs(rt)
        os.chmod(rt, 0o700)
        log = open(os.path.join(self.work, "fake_llm.log"), "w")
        self.fake = subprocess.Popen(
            [sys.executable, "-I", os.path.join(HERE, "fake_llm.py"), str(self.port), os.path.join(self.work, "proj")], stdout=log, stderr=log
        )
        time.sleep(0.5)
        subprocess.run(
            [self.binary, "provider", "add", "fake", "--base-url", "http://127.0.0.1:%d/v1" % self.port, "--model", MODELS[0], "--no-api-key", "--set-default", "--json"],
            env=self.env(),
            cwd=self.work,
            capture_output=True,
            check=True,
        )
        jhome = os.path.join(self.home, ".jcode")
        with open(os.path.join(jhome, "config.toml"), "a") as f:
            for m in MODELS[1:]:
                f.write('\n[[providers.fake.models]]\nid = "%s"\n' % m)
            # wizard does not nudge a model that stops with open todos
            f.write("\n[features]\nauto_poke = false\n")
        # a first launch walks through onboarding and shows what changed; get both out of the way
        hints = json.dumps({"launch_count": 9, "desktop_shortcut_created": True, "startup_spawn_hint_dismissed": True})
        for n in ("setup_hints.json", "setup_hints.bak"):
            with open(os.path.join(jhome, n), "w") as f:
                f.write(hints)
        tmux.run("new-session", "-d", "-s", "prime", "-x", "120", "-y", "40", "-c", os.path.join(self.work, "proj"), self.command() + "; sleep 600")
        wait_ready(tmux, "prime", 60)
        time.sleep(2)
        tmux.run("send-keys", "-t", "prime", "-l", "/quit")
        tmux.run("send-keys", "-t", "prime", "Enter")
        time.sleep(0.5)
        tmux.run("send-keys", "-t", "prime", "Enter")
        time.sleep(2)
        tmux.kill("prime")

    def finish(self):
        try:
            servers = json.load(open(os.path.join(self.home, ".jcode", "servers.json")))
            for s in servers.values():
                try:
                    os.kill(int(s["pid"]), 15)
                except (OSError, KeyError, ValueError):
                    pass
        except (OSError, ValueError):
            pass
        if self.fake:
            self.fake.terminate()


COMPOSER = re.compile(r"^\d+(>|…|\$) ?", re.M)


def wait_ready(tmux, session, timeout):
    end = time.time() + timeout
    while time.time() < end:
        if COMPOSER.search(tmux.plain(session)):
            return True
        time.sleep(0.3)
    return False


def wait_idle(tmux, session, timeout=90):
    end = time.time() + timeout
    calm = 0
    while time.time() < end:
        text = tmux.plain(session)
        busy = re.search(r"^\d+… ?", text, re.M) or re.search(r"^[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏]", text, re.M)
        calm = 0 if busy else calm + 1
        if calm >= 3:
            return True
        time.sleep(0.4)
    return False


def reset_project(work):
    proj = os.path.join(work, "proj")
    shutil.rmtree(proj, ignore_errors=True)
    os.makedirs(proj)
    git = os.path.join(work, "gitproj")
    if not os.path.isdir(os.path.join(git, ".git")):
        shutil.rmtree(git, ignore_errors=True)
        os.makedirs(git)
        run = lambda *a: subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@example.com", *a], cwd=git, capture_output=True, check=True)
        run("init", "-q", "-b", "main", ".")
        open(os.path.join(git, "README.md"), "w").write("# proj\n")
        open(os.path.join(git, "main.rs"), "w").write("fn main() {}\n")
        run("add", "-A")
        run("commit", "-q", "-m", "Initial commit")
        open(os.path.join(git, "README.md"), "a").write("more\n")
        open(os.path.join(git, "new.txt"), "w").write("x\n")


def capture_scene(tmux, prog, scene, size, out_dir, retried=False):
    steps, opts = SCENES[scene]
    w, h = size
    reset_project(prog.work)
    session = "s"
    tmux.kill(session)
    cwd = os.path.join(prog.work, opts.get("project", "proj"))
    tmux.run("new-session", "-d", "-s", session, "-x", str(w), "-y", str(h), "-c", cwd, prog.command() + "; echo; echo EXIT=$?; sleep 600", check=True)
    if not wait_ready(tmux, session, 60):
        print("  %s %s %dx%d: never became ready" % (prog.name, scene, w, h))
    time.sleep(4.0 if prog.name == "ref" else 1.0)
    typed = None
    for step in steps:
        if step[0] == "type":
            typed = step[1]
            for _ in range(3):
                tmux.run("send-keys", "-t", session, "-l", typed)
                seen = False
                for _ in range(10):
                    time.sleep(0.3)
                    if re.search(r"^\d+(>|…|\$) .*" + re.escape(typed[-6:]), tmux.plain(session), re.M):
                        seen = True
                        break
                if seen:
                    break
        elif step[0] == "key":
            for k in step[1]:
                tmux.run("send-keys", "-t", session, k)
                time.sleep(0.4)
                # jcode drops an Enter that comes too soon after a burst of typing
                if k == "Enter" and typed and re.search(r"^\d+(>|…) " + re.escape(typed) + r"\s*$", tmux.plain(session), re.M):
                    time.sleep(0.6)
                    tmux.run("send-keys", "-t", session, k)
                    time.sleep(0.4)
            typed = None
        elif step[0] == "wait":
            time.sleep(step[1])
        elif step[0] == "idle":
            time.sleep(0.6)
            wait_idle(tmux, session)
    # a prompt that scrolls into view pulses for 450 ms; let it settle
    time.sleep(1.5)
    text = tmux.capture(session)
    if opts.get("expect") and opts["expect"] not in tmux.plain(session) and not retried:
        tmux.kill(session)
        return capture_scene(tmux, prog, scene, size, out_dir, retried=True)
    os.makedirs(out_dir, exist_ok=True)
    with open(os.path.join(out_dir, "%s-%dx%d.ansi" % (scene, w, h)), "w") as f:
        f.write(text)
    tmux.kill(session)


# ---- normalisation -------------------------------------------------------------------------


def row_text(row):
    return "".join(c[0] for c in row)


def set_text(row, start, text, style=None):
    """Replace the cells from `start` to the end of the row's text with `text`."""
    end = len(row_text(row).rstrip())
    st = style or (row[start][1:] if start < len(row) else BLANK[1:])
    cells_new = [(ch,) + tuple(st) for ch in text]
    return row[:start] + cells_new + [BLANK] * max(0, len(row) - start - len(cells_new))


def sub_row(row, pattern, repl):
    """Regex-replace inside a row's text, keeping the style of the first replaced cell."""
    text = row_text(row)
    m = re.search(pattern, text)
    if not m:
        return row
    new = m.expand(repl) if isinstance(repl, str) else repl(m)
    st = row[m.start()][1:] if m.start() < len(row) else BLANK[1:]
    mid = [(ch,) + tuple(st) for ch in new]
    out = row[: m.start()] + mid + row[m.end() :]
    width = len(row)
    return (out + [BLANK] * width)[:width]


def right_align(row):
    text = row_text(row).rstrip()
    lead = len(text) - len(text.lstrip())
    body = row[lead : len(text)]
    width = len(row)
    return [BLANK] * (width - len(body)) + body


def blank_widgets(rows):
    """Clear the boxes of widgets the look has no data for."""
    for y, row in enumerate(rows):
        for x, c in enumerate(row):
            if c[0] != "╭" or c[1] != "#464650":
                continue
            top = row_text(row)[x:]
            close = top.find("╮")
            if close < 0:
                continue
            if not any(top[1:close].strip().startswith(t) for t in OMITTED_WIDGETS):
                continue
            # down the left border to the bottom corner; an overlay may have cut the box short
            y2 = y
            while y2 + 1 < len(rows) and rows[y2][x][0] != "╰" and rows[y2 + 1][x][0] in "│╰":
                y2 += 1
            # what the box hides is unknown, so those cells match anything
            for yy in range(y, y2 + 1):
                for xx in range(x, min(x + close + 1, len(rows[yy]))):
                    rows[yy][xx] = SKIP
    return rows


def is_blank(row):
    return not row_text(row).replace("\0", " ").strip() and all(c[2] == "default" for c in row)


SPIN = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"


def normalise(rows, side, look_has_context):
    width = len(rows[0]) if rows else 0
    # after exit: what came before the resume hint is jcode's client/server chatter
    for y, row in enumerate(rows):
        if row_text(row).startswith("Session "):
            rows = rows[y:]
            break
    for row in rows:
        # the marker at the end of a prompt row moves left by one when there is a scrollbar
        if width > 2 and row[width - 2][0] == "│" and row[width - 2][1] == "#8ab4f8" and re.match(r"^ ?\d+› ", row_text(row)):
            row[width - 1] = row[width - 2]
            row[width - 2] = BLANK
    if side == "ref":
        rows = blank_widgets(rows)
    out = []
    in_updates = False
    for row in rows:
        text = row_text(row)
        if side == "ref":
            if re.match(r"^ ╭─+ Updates ─+╮", text):
                in_updates = True
            if in_updates:
                if text.startswith(" ╰"):
                    in_updates = False
                continue
            if HEADER_REF_ONLY.match(text):
                continue
            if re.match(r"^ jcode( ·|\s*$)", text):
                row = set_text(row, 1, "wizard")
                row = [row[i] if i < 7 else BLANK for i in range(width)]
                for i in range(1, 7):
                    row[i] = (row[i][0], "#bed2eb", "default", cells.BOLD)
            # rows the look cannot have: copy badges, backgrounding, transport
            row = sub_row(row, r" \[Alt\] \[⇧\] \[\w\]( expand)?", "")
            row = sub_row(row, r" · Alt\+B bg", "")
            row = sub_row(row, r" · https(/sse)?", "")
            row = sub_row(row, r" · \d+(\.\d+)? tps", "")
            row = sub_row(row, r"(sending context|waiting for response|connecting|refreshing auth|streaming)…", "thinking…")
            row = sub_row(row, r"^⚠ ", "▸ " if any(c[1] == "#ffd580" for c in row[:3]) else "  ")
            if " · api key" in text:
                if " default" in text:
                    row = sub_row(row, r" default", "")
                else:
                    row = sub_row(row, r" {8}(?=  \S+ · api key)", "")
            row = sub_row(row, r" · api key", "")
            row = sub_row(row, r"Esc cancel · Ctrl\+O.*$", "Esc cancel")
            if text.startswith("⚠ http") or text.startswith("  http"):
                # jcode's route notice covers a row the look leaves showing
                out.append(SKIP_ROW)
                continue
            if not look_has_context:
                before = row_text(row)
                row2 = sub_row(row, r"\S+/\S+ [▰▱]{4,10} \d+% ", "")
                if row_text(row2) != before:
                    row = right_align(row2)
        else:
            if HEADER_LOOK_ONLY.match(text.rstrip()):
                continue
            row = sub_row(row, r"(sending)…", "thinking…")
        text = row_text(row)
        if text.startswith("💡"):
            row = [BLANK] * width
        if text[:1] in SPIN and text[:1] != "":
            row = [(SPIN[0],) + row[0][1:]] + row[1:]
        row = sub_row(row, r"(\d+h )?(\d+m )?\d+(\.\d+)?m?s( ·| *$)", lambda m: "Ns" + m.group(4).rstrip())
        row = sub_row(row, r"\d+(\.\d+)?k? tok", "N tok")
        row = sub_row(row, r"↑\S+ ↓\S+", "↑N ↓N")
        row = sub_row(row, r"^(Session )\S+( - to resume:)", r"\1NAME\2")
        row = sub_row(row, r"^(  (jcode|wizard-ui-jcode) --resume ).*$", r"  PROG --resume ID")
        row = sub_row(row, r"^Starting server\.\.\.$", "")
        if len(row) == width and width:
            # the mark at the right edge of the composer
            if re.match(r"^\d+(>|…|\$)", text) and ord(row[-1][0][:1] or " ") >= 0xE000:
                row = row[:-1] + [BLANK]
        out.append(row)
    return out


SUGGESTION = re.compile(r"^/\S+  ")


def move_suggestions(rows):
    """Put the command list right under the composer, where it is when there is room."""
    sugg = [r for r in rows if SUGGESTION.match(row_text(r))]
    if not sugg:
        return rows
    rest = [r for r in rows if not SUGGESTION.match(row_text(r))]
    for y, r in enumerate(rest):
        if COMPOSER.match(row_text(r)):
            return rest[: y + 1] + sugg + rest[y + 1 :]
    return rows


def strip_blank(rows):
    while rows and is_blank(rows[0]):
        rows = rows[1:]
    while rows and is_blank(rows[-1]):
        rows = rows[:-1]
    return rows


def style_runs(row):
    runs = []
    for c in row:
        if c[0] == " " and c[2] == "default":
            continue
        st = c[1:]
        if not runs or runs[-1] != st:
            runs.append(st)
    return runs


def cell_eq(a, b):
    if a[0] == "\0" or b[0] == "\0":
        return True
    if a[0] == " " and b[0] == " ":
        strip = ~(cells.BOLD | cells.DIM | cells.ITALIC)
        return a[2] == b[2] and (a[3] & strip) == (b[3] & strip)
    return a == b


def split_scrollbar(rows):
    """Take the scrollbar column out: returns the rows and the thumb's row span."""
    thumb = [y for y, r in enumerate(rows) if r and r[-1][0] in "╷│╵•" and r[-1][1] == "#8894ac"]
    if not thumb:
        return rows, None
    out = [r[:-1] + [BLANK] if y in thumb else r for y, r in enumerate(rows)]
    return out, (thumb[0], thumb[-1])


def compare(ref_text, look_text, size, opts):
    w, h = size
    ref = cells.parse(ref_text, w)
    look = cells.parse(look_text, w)
    ref, ref_thumb = split_scrollbar(ref)
    look, look_thumb = split_scrollbar(look)
    look_ctx = any(re.search(r"[▰▱]{4,10}", row_text(r)) for r in look)
    ref = strip_blank(move_suggestions(normalise(ref, "ref", look_ctx)))
    look = strip_blank(move_suggestions(normalise(look, "look", look_ctx)))
    diffs = []
    if opts.get("loose") == "styles":
        # the text is the backend's; what has to match is the palette the screen is drawn in
        sa = {c[1:] for r in ref for c in r if c[0].strip()}
        sb = {c[1:] for r in look for c in r if c[0].strip()}
        for st in sorted(sa ^ sb, key=str):
            diffs.append("style only in %s: %r" % ("ref" if st in sa else "look", st))
        return diffs, ""
    scrolling = ref_thumb is not None or look_thumb is not None
    loose = opts.get("loose")

    def rows_diff(a_rows, b_rows, from_top):
        """Compare two blocks row by row, aligned at the top or at the bottom."""
        n = min(len(a_rows), len(b_rows))
        below_extra = []
        longer = a_rows if len(a_rows) > len(b_rows) else b_rows
        extra = longer[n:] if from_top else longer[: len(longer) - n]
        if not scrolling:
            for r in extra:
                if loose == "below" and (SUGGESTION.match(row_text(r)) or re.search(r"Grok \S+\s*$", row_text(r))):
                    # the list covers the session line when it opens below the composer,
                    # and whether it fits below depends on the header's height
                    below_extra.append(r)
                    continue
                if not is_blank(r):
                    which = "ref" if longer is a_rows else "look"
                    diffs.append("row only in %s: %r" % (which, row_text(r).rstrip()))
                    diffs.extend([""] * (len(row_text(r).strip()) - 1))
        composer_y = None
        pairs = []
        for y in range(n):
            a = a_rows[y] if from_top else a_rows[len(a_rows) - n + y]
            b = b_rows[y] if from_top else b_rows[len(b_rows) - n + y]
            pairs.append((a, b))
            if COMPOSER.match(row_text(b)):
                composer_y = y
        below_a, below_b = set(), set()
        for r in below_extra:
            if re.search(r"Grok \S+\s*$", row_text(r)):
                continue
            (below_a if len(a_rows) > len(b_rows) else below_b).update(c[1:] for c in r if c[0].strip())
        for y, (a, b) in enumerate(pairs):
            if loose == "below" and composer_y is not None and y > composer_y:
                # the commands are wizard's; the rows have to be drawn in the same styles
                below_a |= {c[1:] for c in a if c[0].strip()}
                below_b |= {c[1:] for c in b if c[0].strip()}
                continue
            for x in range(w):
                if x < len(a) and x < len(b) and not cell_eq(a[x], b[x]):
                    diffs.append("(%d,%d) ref %r look %r   | %s" % (y, x, a[x], b[x], row_text(b).rstrip()[:60]))
        for st in sorted(below_a ^ below_b, key=str):
            diffs.append("list style only in %s: %r" % ("ref" if st in below_a else "look", st))

    def anchor(rows):
        for y, r in enumerate(rows):
            if re.match(r"^ ?\d+› ", row_text(r)):
                return y
        return None

    if opts.get("tail"):
        # an overlay covers whatever transcript rows are under it, and those rows differ
        # because the headers do; compare from the overlay down
        def tail(rows):
            for y, r in enumerate(rows):
                t = row_text(r)
                if r is SKIP_ROW or re.match(r"^(▸ |  )\S|^↑↓ choose|^No matching models", t) or COMPOSER.match(t):
                    return rows[y:]
            return rows
        rows_diff([r for r in tail(ref) if r is not SKIP_ROW], tail(look), True)
        return diffs, ""

    def band(rows):
        n = 0
        while n < len(rows) and not is_blank(rows[n]) and re.match(r"^ {3}[●○✓]|^ {5}[✓●○] |^  … ", row_text(rows[n])):
            n += 1
        return n

    nb_r, nb_l = band(ref), band(look)
    if nb_r or nb_l:
        rows_diff(ref[:nb_r], look[:nb_l], True)
        ref, look = strip_blank(ref[nb_r:]), strip_blank(look[nb_l:])
    ra, la = anchor(ref), anchor(look)
    if ra is not None and la is not None:
        # everything from the first prompt down lines up; what is above it is the header
        rows_diff(ref[:ra], look[:la], False)
        rows_diff(ref[ra:], look[la:], True)
    else:
        rows_diff(ref, look, False)
    note = ""
    if scrolling:
        note = "scrollbar ref %s look %s" % (ref_thumb, look_thumb)
    return diffs, note


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--jcode", help="jcode binary: capture the reference again before comparing")
    ap.add_argument("--look", default=os.path.join(HERE, "..", "..", "..", "target", "debug", "jcodew"))
    # the captures record this path, so it is fixed rather than taken from TMPDIR
    ap.add_argument("--work", default="/tmp/jcodew-cmp")
    ap.add_argument("--socket", default="jcodew-cmp")
    ap.add_argument("--port", type=int, default=18793)
    ap.add_argument("--scene", action="append", help="only these scenes")
    ap.add_argument("--size", action="append", help="only these sizes, as 120x40")
    ap.add_argument("--show", type=int, default=0, help="print this many differences per capture")
    ap.add_argument("--no-capture", action="store_true", help="compare the look captures already in --work")
    args = ap.parse_args()

    scenes = args.scene or list(SCENES)
    sizes = [tuple(map(int, s.split("x"))) for s in args.size] if args.size else SIZES
    work = os.path.realpath(args.work)
    os.makedirs(work, exist_ok=True)
    tmux = Tmux(args.socket)
    look_dir = os.path.join(work, "look")
    try:
        if args.jcode:
            ref = Jcode("ref", work, os.path.realpath(args.jcode), args.port)
            reset_project(work)
            try:
                ref.prepare(tmux)
                for scene in scenes:
                    for size in sizes:
                        capture_scene(tmux, ref, scene, size, REF_DIR)
            finally:
                ref.finish()
        if not args.no_capture:
            look = Look("look", work, os.path.realpath(args.look))
            look.prepare(tmux)
            for scene in scenes:
                for size in sizes:
                    capture_scene(tmux, look, scene, size, look_dir)
    finally:
        tmux.run("kill-server")

    total = 0
    print("%-14s %s" % ("scene", "  ".join("%9s" % ("%dx%d" % s) for s in sizes)))
    for scene in scenes:
        counts = []
        notes = []
        for size in sizes:
            name = "%s-%dx%d.ansi" % (scene, size[0], size[1])
            try:
                r = open(os.path.join(REF_DIR, name), encoding="utf-8", errors="replace").read()
                l = open(os.path.join(look_dir, name), encoding="utf-8", errors="replace").read()
            except OSError:
                counts.append("missing")
                continue
            diffs, note = compare(r, l, size, SCENES[scene][1])
            counts.append(str(len(diffs)) + ("*" if note else ""))
            total += len(diffs)
            if args.show and diffs:
                notes.append("  %s:" % name)
                notes.extend("    " + d for d in [d for d in diffs if d][: args.show])
        print("%-14s %s" % (scene, "  ".join("%9s" % c for c in counts)))
        for n in notes:
            print(n)
    print("total mismatched cells: %d" % total)
    return 0 if total == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
