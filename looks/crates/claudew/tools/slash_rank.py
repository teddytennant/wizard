#!/usr/bin/env python3
"""Take what Claude Code's slash menu shows for a list of queries, to hold the look's ranking to it.

    slash_rank.py SOCKET SESSION OUT.json [query ...]

SESSION (in the tmux server SOCKET) is a `claude` at the prompt, at least 300 columns wide so no
description is cut, started the way `compare.sh` starts the reference (fake Anthropic server, a HOME of
its own). For each query the script types it after a slash, then presses Down until the selection comes
back to the first row and writes every row it saw, in order. A row is the command's name with its alias in
brackets where the menu shows one (`/clear (reset)`); a name the menu cut short keeps its leading `…`.
OUT.json gets {"commands": [[name, description], ...] in the menu's own order, "ranks": {query: [row, ...]}}.

What it cannot see is added by hand into `tests/fixtures/slash-rank.json`: a bundled skill's menu line is
its short `menuDescription` while the search reads its long `description`, which the binary holds as text;
and the order the program keeps its commands in, which only breaks ties, is worked out from the ranks by
`registry_order_constraints` in `tests/slash_rank.rs`.
"""
import json, re, subprocess, sys, time

SOCKET, SESSION, OUT = sys.argv[1:4]
QUERIES = sys.argv[4:] or (
    "mo hel cl co pl re he mod mem perm ex sta doc usa i a e r t x z mcp con comp cle res rew rwd plug pln hlp "
    "exi qui sett set cost stat tas ba bug share fork branch radio debug visual dir add thm th the ag agent skill "
    "ski sandb out sty sl ke key ide mob log login logout lo md claude ctx cfg ef eff fas fa go goal ini init "
    "rel release ver tui pr rev review ses session up undo chk prompt wor work hook ho allowed proactive").split()
ROW = re.compile(r'^  ([❯ ]) ?\s?(…?/?\S+(?: \([^)]*\))?)\s{2,}(.*?)\s*$')


def tmux(*args):
    return subprocess.run(['tmux', '-L', SOCKET, *args], capture_output=True, text=True).stdout


def screen():
    return tmux('capture-pane', '-p', '-t', SESSION).split('\n')


def rows():
    return [m.groups() for m in map(ROW.match, screen()) if m]


def type_query(q):
    tmux('send-keys', '-t', SESSION, 'C-u')
    time.sleep(0.2)
    tmux('send-keys', '-t', SESSION, '-l', '/' + q)
    for _ in range(40):
        time.sleep(0.1)
        if any(l.startswith('❯ /' + q) and l.rstrip() == '❯ /' + q for l in screen()):
            break
    time.sleep(0.4)


def scroll(limit):
    seen = []
    first = None
    for step in range(limit):
        got = rows()
        for sel, name, desc in got:
            if name not in [s[0] for s in seen]:
                seen.append((name, desc))
        picked = next((n for s, n, _ in got if s == '❯'), None)
        if first is None:
            first = picked
        elif picked == first:
            break
        tmux('send-keys', '-t', SESSION, 'Down')
        time.sleep(0.15)
    return seen


type_query('')
commands = scroll(150)
ranks = {}
for q in QUERIES:
    type_query(q)
    ranks[q] = [n for n, _ in scroll(150)]
    print(q, len(ranks[q]), flush=True)
json.dump({'commands': commands, 'ranks': ranks}, open(OUT, 'w'), indent=1, ensure_ascii=False)
