"""Parse a `tmux capture-pane -e -p` dump into a grid of cells."""

import re
import unicodedata

SGR = re.compile(r"\x1b\[([0-9;:]*)m")
OTHER = re.compile(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b\[[0-9;?]*[A-Za-ln-z]|\x1b[()][A-Z0-9]|[\x0e\x0f]")

BOLD, DIM, ITALIC, UNDER, REV, STRIKE = 1, 2, 4, 8, 16, 32
NAMES = {BOLD: "b", DIM: "d", ITALIC: "i", UNDER: "u", REV: "r", STRIKE: "s"}


def width(ch):
    if unicodedata.combining(ch) or ch in "‍️︎":
        return 0
    return 2 if unicodedata.east_asian_width(ch) in "WF" else 1


def parse(text, cols=None):
    """Rows of cells; a cell is (text, fg, bg, attrs). Colours are 'default', an int or '#rrggbb'."""
    rows = []
    fg = bg = "default"
    attrs = 0
    lines = text.split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    for line in lines:
        line = OTHER.sub("", line)
        cells = []
        pos = 0
        for m in SGR.finditer(line):
            for ch in line[pos : m.start()]:
                w = width(ch)
                if w == 0 and cells:
                    t, f, b, a = cells[-1]
                    cells[-1] = (t + ch, f, b, a)
                    continue
                cells.append((ch, fg, bg, attrs))
                if w == 2:
                    cells.append(("", fg, bg, attrs))
            pos = m.end()
            p = [int(x) if x else 0 for x in re.split("[;:]", m.group(1))] if m.group(1) else [0]
            k = 0
            while k < len(p):
                c = p[k]
                if c == 0:
                    fg = bg = "default"
                    attrs = 0
                elif c == 1:
                    attrs |= BOLD
                elif c == 2:
                    attrs |= DIM
                elif c == 3:
                    attrs |= ITALIC
                elif c == 4:
                    attrs |= UNDER
                elif c == 7:
                    attrs |= REV
                elif c == 9:
                    attrs |= STRIKE
                elif c == 22:
                    attrs &= ~(BOLD | DIM)
                elif c == 23:
                    attrs &= ~ITALIC
                elif c == 24:
                    attrs &= ~UNDER
                elif c == 27:
                    attrs &= ~REV
                elif c == 29:
                    attrs &= ~STRIKE
                elif c == 39:
                    fg = "default"
                elif c == 49:
                    bg = "default"
                elif c in (38, 48) and k + 1 < len(p) and p[k + 1] == 2:
                    v = "#%02x%02x%02x" % tuple(p[k + 2 : k + 5])
                    if c == 38:
                        fg = v
                    else:
                        bg = v
                    k += 4
                elif c in (38, 48) and k + 1 < len(p) and p[k + 1] == 5:
                    if c == 38:
                        fg = p[k + 2]
                    else:
                        bg = p[k + 2]
                    k += 2
                elif 30 <= c <= 37:
                    fg = c - 30
                elif 90 <= c <= 97:
                    fg = c - 90 + 8
                elif 40 <= c <= 47:
                    bg = c - 40
                elif 100 <= c <= 107:
                    bg = c - 100 + 8
                k += 1
        for ch in line[pos:]:
            w = width(ch)
            if w == 0 and cells:
                t, f, b, a = cells[-1]
                cells[-1] = (t + ch, f, b, a)
                continue
            cells.append((ch, fg, bg, attrs))
            if w == 2:
                cells.append(("", fg, bg, attrs))
        if cols is not None:
            cells = cells[:cols]
            cells += [(" ", "default", "default", 0)] * (cols - len(cells))
        rows.append(cells)
    return rows


def attr_names(a):
    return "".join(n for bit, n in NAMES.items() if a & bit)


def describe(rows):
    """One line per non-blank row: the text, then its style runs."""
    out = []
    for y, row in enumerate(rows):
        text = "".join(c[0] for c in row).rstrip()
        if not text:
            continue
        out.append("%3d %s" % (y, text))
        runs = []
        start = 0
        for x in range(1, len(row) + 1):
            if x == len(row) or row[x][1:] != row[start][1:]:
                seg = "".join(c[0] for c in row[start:x])
                if seg.strip() or row[start][2] != "default":
                    _, f, b, a = row[start]
                    tag = str(f) if f != "default" else "-"
                    if b != "default":
                        tag += "/" + str(b)
                    if a:
                        tag += " " + attr_names(a)
                    runs.append("%d:%s[%s]" % (start, tag, seg.strip()[:24]))
                start = x
        out.append("      " + "  ".join(runs))
    return "\n".join(out)


if __name__ == "__main__":
    import sys

    print(describe(parse(open(sys.argv[1], encoding="utf-8", errors="replace").read())))
