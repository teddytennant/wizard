"""Parse `tmux capture-pane -e -p` output into a grid of cells."""
import re, sys, unicodedata

SGR = re.compile(r'\x1b\[([0-9;:]*)m')
# OSC 8 opens (or with an empty target closes) a hyperlink; its target is part of the cell
OSC8 = re.compile(r'\x1b\]8;[^;\x07\x1b]*;([^\x07\x1b]*)(\x07|\x1b\\)')
OTHER = re.compile(r'\x1b\][^\x07\x1b]*(\x07|\x1b\\)|\x1b\[[0-9;?]*[A-Za-ln-z]')
# a link target under this directory is compared as a path relative to it (compare.py sets it)
BASE = None


def link_of(target):
    if not target:
        return None
    if target.startswith('file://'):
        path = target[len('file://'):]
        if BASE and path.startswith(BASE.rstrip('/') + '/'):
            path = path[len(BASE.rstrip('/')) + 1:]
        return 'file:' + path
    return target

def width(ch):
    if unicodedata.combining(ch): return 0
    return 2 if unicodedata.east_asian_width(ch) in ('W', 'F') else 1

def new_style():
    return {'fg': None, 'bg': None, 'bold': False, 'dim': False, 'italic': False,
            'underline': False, 'inverse': False, 'strike': False}

def apply(st, params):
    p = [int(x) if x else 0 for x in re.split('[;:]', params)] if params else [0]
    i = 0
    while i < len(p):
        c = p[i]
        if c == 0: st.update(new_style())
        elif c == 1: st['bold'] = True
        elif c == 2: st['dim'] = True
        elif c == 3: st['italic'] = True
        elif c == 4: st['underline'] = True
        elif c == 7: st['inverse'] = True
        elif c == 9: st['strike'] = True
        elif c == 22: st['bold'] = False; st['dim'] = False
        elif c == 23: st['italic'] = False
        elif c == 24: st['underline'] = False
        elif c == 27: st['inverse'] = False
        elif c == 29: st['strike'] = False
        elif 30 <= c <= 37: st['fg'] = str(c - 30)
        elif 90 <= c <= 97: st['fg'] = str(c - 90 + 8)
        elif c == 39: st['fg'] = None
        elif 40 <= c <= 47: st['bg'] = str(c - 40)
        elif 100 <= c <= 107: st['bg'] = str(c - 100 + 8)
        elif c == 49: st['bg'] = None
        elif c in (38, 48):
            key = 'fg' if c == 38 else 'bg'
            if i + 1 < len(p) and p[i + 1] == 5:
                st[key] = str(p[i + 2]); i += 2
            elif i + 1 < len(p) and p[i + 1] == 2:
                st[key] = '#%02x%02x%02x' % tuple(p[i + 2:i + 5]); i += 4
        i += 1

def parse(text, w=None, h=None):
    """Rows of (char, style-tuple). Style persists across lines like the terminal's does."""
    st = new_style()
    link = None
    rows = []
    for line in text.split('\n'):
        row = []
        pos = 0
        while pos < len(line):
            m = OSC8.match(line, pos)
            if m:
                link = link_of(m.group(1)); pos = m.end(); continue
            m = OTHER.match(line, pos)
            if m:
                pos = m.end(); continue
            m = SGR.match(line, pos)
            if m:
                apply(st, m.group(1)); pos = m.end(); continue
            ch = line[pos]; pos += 1
            cw = width(ch)
            if cw == 0:
                if row: row[-1] = (row[-1][0] + ch, row[-1][1])
                continue
            key = (st['fg'], st['bg'], st['bold'], st['dim'], st['italic'], st['underline'], st['inverse'], st['strike'], link)
            row.append((ch, key))
            if cw == 2: row.append(('', key))
        rows.append(row)
    if rows and not rows[-1]: rows.pop()
    blank = (' ', (None, None, False, False, False, False, False, False, None))
    if w:
        rows = [(r + [blank] * w)[:w] for r in rows]
    if h:
        rows = (rows + [[blank] * (w or 0)] * h)[:h]
    return rows

NAMES = ['bold', 'dim', 'italic', 'underline', 'inverse', 'strike']
def fmt(key):
    out = []
    if key[0] is not None: out.append('fg=' + key[0])
    if key[1] is not None: out.append('bg=' + key[1])
    out += [n for n, v in zip(NAMES, key[2:8]) if v]
    if len(key) > 8 and key[8]: out.append('link=' + key[8])
    return ' '.join(out) or 'plain'

def visible(key, ch):
    """Style as it matters for a cell: a blank cell only shows bg, inverse and underline."""
    if ch == ' ':
        return (None, key[1], False, False, False, key[5], key[6], key[7], None) if (key[6] or key[5] or key[7]) else (None, key[1], False, False, False, False, False, False, None)
    return key

if __name__ == '__main__':
    rows = parse(open(sys.argv[1], encoding='utf-8').read())
    for y, row in enumerate(rows):
        text = ''.join(c for c, _ in row).rstrip()
        if not text and not any(k[1] or k[6] for _, k in row): continue
        runs = []; cur = None; x0 = 0; last = 0
        for x, (ch, k) in enumerate(row):
            v = visible(k, ch)
            if ch == ' ' and cur is not None and v[1] == cur[1] and not (v[5] or v[6]) and not (cur[5] or cur[6]):
                continue
            if v != cur:
                if cur is not None and fmt(cur) != 'plain': runs.append('%d-%d %s' % (x0, last, fmt(cur)))
                cur = v; x0 = x
            last = x
        if cur is not None and fmt(cur) != 'plain': runs.append('%d-%d %s' % (x0, last, fmt(cur)))
        print('%2d|%s' % (y, text[:int(sys.argv[2]) if len(sys.argv) > 2 else 400]))
        if runs: print('    ' + ' | '.join(runs))
