#!/usr/bin/env python3
"""Run vim-mode key sequences through the real `claude` and record what it did.

    vimdiff.py CASES OUT [--cmd CMD] [--cwd DIR] [--only SUBSTRING]

CASES is a tab separated file of `text<TAB>keys` (see tests/fixtures/vim-cases.tsv). For each
case the prompt is cleared, the text typed in INSERT, the cursor put on the first character
with `gg0`, the keys sent, and the prompt read back from the screen: its text, and the cursor
(Claude hides the terminal cursor and paints one inverse cell). OUT gets
`text<TAB>keys<TAB>result<TAB>cursor` and is what src/vim_tests.rs replays against the Rust
engine. Every tmux call uses the socket named by $LOOK_SOCKET (default `lookvim`).
"""
import argparse, os, subprocess, sys, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import cells

SOCKET = os.environ.get('LOOK_SOCKET', 'lookvim')
SESSION = 'vimcase'
# seconds between keys; a slower pace is for re-recording a case that came out wrong
DELAY = float(os.environ.get('VIM_KEY_DELAY', '0.08'))


def tmux(*args, check=True):
    env = dict(os.environ)
    env.pop('TMUX', None)
    r = subprocess.run(['tmux', '-L', SOCKET, *args], capture_output=True, text=True, env=env)
    if check and r.returncode != 0:
        raise SystemExit('tmux %s: %s' % (' '.join(args), r.stderr.strip()))
    return r.stdout


def send_keys(keys):
    """Send a key string: `<Esc>`, `<BS>`, `<lt>`, other characters as typed."""
    i = 0
    while i < len(keys):
        ch = keys[i]
        if ch == '<' and '>' in keys[i:]:
            j = keys.index('>', i)
            name = keys[i + 1:j]
            i = j + 1
            if name == 'lt':
                tmux('send-keys', '-t', SESSION, '-l', '<')
            else:
                tmux('send-keys', '-t', SESSION, {'Esc': 'Escape', 'BS': 'BSpace'}.get(name, name))
        else:
            # a lone `;` is tmux's command separator
            tmux('send-keys', '-t', SESSION, '-l', '\\;' if ch == ';' else ch)
            i += 1
        time.sleep(DELAY)


def screen():
    return tmux('capture-pane', '-t', SESSION, '-e', '-p')


def prompt(w):
    """(text, cursor index) of the prompt box on screen."""
    rows = cells.parse(screen(), w, None)
    rules = [y for y, r in enumerate(rows) if sum(1 for c, _ in r if c == '─') > w - 4]
    if len(rules) < 2:
        return None, -1
    top, bottom = rules[-2], rules[-1]
    lines, cursor = [], -1
    offset = 0
    for y in range(top + 1, bottom):
        row = rows[y]
        text = ''.join(c for c, _ in row[2:]).rstrip()
        for x, (c, k) in enumerate(row):
            if x >= 2 and k[6] and cursor < 0:
                cursor = offset + (x - 2)
        lines.append(text)
        offset += len(text) + 1
    return '\n'.join(lines), cursor


def unesc(s):
    return s.replace('\\n', '\n').replace('\\t', '\t')


def esc(s):
    return s.replace('\n', '\\n').replace('\t', '\\t')


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('cases')
    ap.add_argument('out')
    ap.add_argument('--cmd', required=True, help='shell command that starts claude in vim mode')
    ap.add_argument('--cwd', required=True)
    ap.add_argument('--only', default='')
    ap.add_argument('--size', default='120x40')
    a = ap.parse_args()
    w, h = (int(x) for x in a.size.split('x'))
    tmux('kill-session', '-t', SESSION, check=False)
    tmux('-f', '/dev/null', 'new-session', '-d', '-s', SESSION, '-x', str(w), '-y', str(h), '-c', a.cwd,
         'env', '-i', 'HOME=' + os.environ['HOME'], 'PATH=' + os.environ['PATH'], 'LANG=en_US.UTF-8',
         'TERM=tmux-256color', 'TERM_PROGRAM=tmux', 'SHELL=/bin/sh', *os.environ.get('LOOK_ENV', '').split(), 'sh')
    for opt, val in (('status', 'off'), ('escape-time', '0')):
        tmux('set', '-g', opt, val)
    tmux('send-keys', '-t', SESSION, '-l', "PS1='$ '; clear; " + a.cmd)
    tmux('send-keys', '-t', SESSION, 'Enter')
    end = time.time() + 40
    while time.time() < end:
        if '❯' in screen():
            break
        time.sleep(0.3)
    else:
        raise SystemExit('claude did not start:\n' + screen())
    time.sleep(3)
    # cases already recorded in OUT are kept, so an interrupted run goes on where it stopped
    done = set()
    if os.path.exists(a.out):
        for line in open(a.out, encoding='utf-8'):
            f = line.rstrip('\n').split('\t')
            if len(f) == 4:
                done.add((f[0], f[1]))
    out = open(a.out, 'a', encoding='utf-8')
    for line in open(a.cases, encoding='utf-8'):
        line = line.rstrip('\n')
        if not line or line.startswith('#'):
            continue
        text, keys = line.split('\t')
        if (a.only and a.only not in line) or (text, keys) in done:
            continue
        # clear what the last case left
        # drop whatever mode and text the last case left, without Ctrl+C's two second hint
        send_keys('<Esc><Esc>ggVGd')
        time.sleep(0.2)
        send_keys('i')
        for ch in unesc(text):
            if ch == '\n':
                tmux('send-keys', '-t', SESSION, 'C-j')
            else:
                tmux('send-keys', '-t', SESSION, '-l', ch)
            time.sleep(0.02)
        send_keys('<Esc>')
        time.sleep(0.15)
        send_keys('gg0')
        send_keys(keys)
        time.sleep(0.5)
        result, cursor = prompt(w)
        if result is None:
            raise SystemExit('lost the prompt on %r\n%s' % (line, screen()))
        row = '\t'.join([text, keys, esc(result), str(cursor)])
        out.write(row + '\n')
        out.flush()
        print(row, flush=True)
    tmux('kill-session', '-t', SESSION, check=False)


if __name__ == '__main__':
    main()
