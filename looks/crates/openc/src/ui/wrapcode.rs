// OWNER: transcript
//! Soft wrap for one line of code, shared by code blocks, tool output and the three diff
//! renderers (round 2 findings 5 and 21).
//!
//! Rows break between tokens: after a space, or after `. , ( ) [ ] { } - / \ :` and friends when
//! what follows is not a closing mark, so `);` stays together and a row never starts with a
//! lone `;` or `)`. A token longer than a whole row is cut by cells. If the last row would hold
//! fewer than [`MIN_TAIL`] cells, whole tokens move down from the row above it, so the line
//! never ends in a `↳ ;` row of its own.
//!
//! That needs the end of the line, which a streaming answer does not have yet.
//! [`wrap_code_partial`] lays out a line that is still growing so that what it shows is only ever
//! appended to: the token being typed is held back until a delimiter confirms it, and the few
//! cells at the end of a row that a later short row could still pull down are held back with it.
//! A row drawn while the line grows is therefore a prefix of the row the finished line gets
//! (checklist 31), and `tests/stream.rs` holds both to it.

use ratatui::text::Span;
use tuikit::width::{grapheme_width, is_wrap_break};
use unicode_segmentation::UnicodeSegmentation;

/// Fewest cells the last row of a wrapped line may hold before whole tokens are pulled down to it.
pub const MIN_TAIL: usize = 4;

struct Cell {
    w: usize,
    space: bool,
    /// A row may end right after this cell, unless the next one is a closing mark.
    brk: bool,
    /// A row may not start with this cell (`) ] } ; , . : ? !`) unless a token is too long.
    closing: bool,
    span: usize,
    b0: usize,
    b1: usize,
}

fn cells(spans: &[Span<'static>]) -> Vec<Cell> {
    let mut out = Vec::new();
    for (si, sp) in spans.iter().enumerate() {
        for (b, g) in sp.content.grapheme_indices(true) {
            let first = g.chars().next().unwrap_or(' ');
            out.push(Cell {
                w: grapheme_width(g),
                space: first == ' ' || first == '\t',
                brk: is_wrap_break(g),
                closing: matches!(first, ')' | ']' | '}' | ';' | ',' | '.' | ':' | '?' | '!'),
                span: si,
                b0: b,
                b1: b + g.len(),
            });
        }
    }
    out
}

/// A token and the spaces after it. `[start, end)` is the token, `tail` the end of its spaces.
#[derive(Clone, Copy, Debug)]
struct Atom {
    start: usize,
    end: usize,
    tail: usize,
}

fn atoms(c: &[Cell]) -> Vec<Atom> {
    let n = c.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let start = i;
        if c[i].space {
            // Leading indentation is one token of its own and counts toward the row.
            while i < n && c[i].space {
                i += 1;
            }
            out.push(Atom {
                start,
                end: i,
                tail: i,
            });
            continue;
        }
        loop {
            i += 1;
            if i >= n || c[i].space || (c[i - 1].brk && !c[i].closing) {
                break;
            }
        }
        let end = i;
        while i < n && c[i].space {
            i += 1;
        }
        out.push(Atom {
            start,
            end,
            tail: i,
        });
    }
    out
}

/// One row: its cells `[start, end)` and where each token in it starts and ends.
#[derive(Clone, Debug)]
struct Row {
    start: usize,
    end: usize,
    toks: Vec<(usize, usize)>,
}

fn greedy(c: &[Cell], pw: &[usize], atoms: &[Atom], w: usize) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let mut cur: Option<Row> = None;
    for a in atoms {
        let fits = cur.as_ref().is_some_and(|r| pw[a.end] - pw[r.start] <= w);
        if fits {
            let r = cur.as_mut().expect("fits implies a row");
            r.toks.push((a.start, a.end));
            r.end = a.end;
            continue;
        }
        if let Some(r) = cur.take() {
            rows.push(r);
        }
        if pw[a.end] - pw[a.start] <= w {
            cur = Some(Row {
                start: a.start,
                end: a.end,
                toks: vec![(a.start, a.end)],
            });
            continue;
        }
        // Longer than a whole row: cut by cells; the last piece can take the tokens after it.
        let mut j = a.start;
        while j < a.end {
            let mut k = j;
            let mut used = 0;
            while k < a.end && used + c[k].w <= w {
                used += c[k].w;
                k += 1;
            }
            if k == j {
                k = j + 1;
            }
            let piece = Row {
                start: j,
                end: k,
                toks: vec![(j, k)],
            };
            if k < a.end {
                rows.push(piece);
            } else {
                cur = Some(piece);
            }
            j = k;
        }
    }
    rows.extend(cur);
    rows
}

/// Move whole tokens from the end of row `n - 2` to row `n - 1` until the last row has `want`
/// cells, while the result still fits. Returns how many tokens moved.
fn pull(rows: &mut [Row], pw: &[usize], w: usize, want: usize) -> usize {
    let n = rows.len();
    if n < 2 {
        return 0;
    }
    let mut moved = 0;
    loop {
        let (head, tail) = rows.split_at_mut(n - 1);
        let (prev, last) = (&mut head[n - 2], &mut tail[0]);
        if pw[last.end] - pw[last.start] >= want || prev.toks.len() < 2 {
            break;
        }
        let (s, e) = *prev.toks.last().expect("two tokens");
        if pw[last.end] - pw[s] > w {
            break;
        }
        prev.toks.pop();
        prev.end = prev.toks.last().map_or(prev.start, |t| t.1);
        last.toks.insert(0, (s, e));
        last.start = s;
        moved += 1;
    }
    moved
}

/// How many tokens at the end of `row` a later short row could pull down, if it needed `need`
/// cells. At most all but the first token: that one is never pulled.
fn hide_tokens(row: &Row, pw: &[usize], need: usize) -> usize {
    let mut hidden = 0;
    let mut cells_hidden = 0;
    let mut toks = row.toks.len();
    while cells_hidden < need && toks >= 2 {
        let (s, e) = row.toks[toks - 1];
        cells_hidden += pw[e] - pw[s];
        toks -= 1;
        hidden += 1;
    }
    hidden
}

fn slice(spans: &[Span<'static>], c: &[Cell], start: usize, end: usize) -> Vec<Span<'static>> {
    let mut row: Vec<Span<'static>> = Vec::new();
    let mut cur: Option<(usize, usize, usize)> = None;
    for cell in &c[start..end] {
        match &mut cur {
            Some((sp, _, e)) if *sp == cell.span && *e == cell.b0 => *e = cell.b1,
            _ => {
                if let Some((sp, a, b)) = cur.take() {
                    row.push(Span::styled(
                        spans[sp].content[a..b].to_string(),
                        spans[sp].style,
                    ));
                }
                cur = Some((cell.span, cell.b0, cell.b1));
            }
        }
    }
    if let Some((sp, a, b)) = cur {
        row.push(Span::styled(
            spans[sp].content[a..b].to_string(),
            spans[sp].style,
        ));
    }
    row
}

fn prefix_widths(c: &[Cell]) -> Vec<usize> {
    let mut pw = Vec::with_capacity(c.len() + 1);
    pw.push(0);
    for cell in c {
        pw.push(pw.last().copied().unwrap_or(0) + cell.w);
    }
    pw
}

/// Wrap a finished line to `width` cells.
pub fn wrap_code(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    let c = cells(spans);
    if c.is_empty() || width == 0 {
        return vec![Vec::new()];
    }
    let pw = prefix_widths(&c);
    let at = atoms(&c);
    let mut rows = greedy(&c, &pw, &at, width);
    pull(&mut rows, &pw, width, MIN_TAIL);
    rows.iter()
        .map(|r| slice(spans, &c, r.start, r.end))
        .collect()
}

/// Wrap a line that is still being written. Every row but the last is final once drawn, and the
/// last only ever gains cells at its end, however the line goes on. May return no rows while
/// there is nothing safe to show yet.
pub fn wrap_code_partial(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    let c = cells(spans);
    if c.is_empty() || width == 0 {
        return Vec::new();
    }
    let pw = prefix_widths(&c);
    let mut at = atoms(&c);
    // The token being typed may still grow, or take a closing mark that glues it to the one
    // before: it waits for a delimiter. A token followed by a space is settled.
    if at
        .last()
        .is_some_and(|a| a.tail == a.end && !c[a.start].space)
    {
        at.pop();
    }
    if at.is_empty() {
        return Vec::new();
    }
    let mut rows = greedy(&c, &pw, &at, width);
    let n = rows.len();
    let last_w = pw[rows[n - 1].end] - pw[rows[n - 1].start];
    let trim = |row: &mut Row, hide: usize| {
        for _ in 0..hide {
            row.toks.pop();
        }
        row.end = row.toks.last().map_or(row.start, |t| t.1);
    };
    if n >= 2 && last_w < MIN_TAIL {
        // A short last row pulls tokens down from the row above when the line ends. Show
        // neither it nor the tokens it could take.
        let hide = hide_tokens(&rows[n - 2], &pw, MIN_TAIL - last_w);
        rows.pop();
        trim(rows.last_mut().expect("n >= 2"), hide);
    } else {
        // The line may go on past the end of this row. If it does, the row after it holds at
        // least one cell and takes at most `MIN_TAIL - 1` cells from this one, so those stay
        // out of sight until the line says otherwise. What is held back only ever shrinks.
        let hide = hide_tokens(&rows[n - 1], &pw, MIN_TAIL - 1);
        trim(rows.last_mut().expect("one row"), hide);
    }
    rows.iter()
        .map(|r| slice(spans, &c, r.start, r.end))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;

    fn line(s: &str) -> Vec<Span<'static>> {
        vec![Span::styled(s.to_string(), Style::new())]
    }

    fn texts(rows: &[Vec<Span<'static>>]) -> Vec<String> {
        rows.iter()
            .map(|r| r.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect()
    }

    #[test]
    fn a_closing_mark_never_lands_alone_on_the_next_row() {
        // The row fills exactly at `)`: greedy wrap puts `;` on a row of its own.
        let src = "    let value = compute_input(&input, 0);";
        for w in 8..src.len() {
            let rows = texts(&wrap_code(&line(src), w));
            let last = rows.last().expect("a row");
            // Short is fine only when no token could move down: from 16 cells up there is
            // always one that can.
            assert!(
                rows.len() == 1 || w < 16 || last.trim().chars().count() >= MIN_TAIL,
                "width {w}: {rows:?}"
            );
            assert!(
                rows.len() == 1
                    || !last
                        .trim()
                        .chars()
                        .all(|c| matches!(c, ')' | ';' | ',' | '.')),
                "width {w}: a row of closing marks only: {rows:?}"
            );
            for r in &rows[1..] {
                let t = r.trim_start();
                assert!(
                    !t.starts_with([')', ';', ',', '.', ']', '}']) || t.len() > w,
                    "width {w}: row starts with a closing mark: {rows:?}"
                );
            }
            assert_eq!(rows.concat().replace(' ', ""), src.replace(' ', ""));
        }
    }

    #[test]
    fn rows_break_between_tokens_not_inside_them() {
        let src = "        let opts = Options::strict(input.len(), true);";
        let rows = texts(&wrap_code(&line(src), 30));
        assert_eq!(rows.len(), 2, "{rows:?}");
        // `opts.stri` / `ct);` is what character wrapping made of this in the split view.
        for r in &rows {
            assert!(r.chars().count() <= 30);
        }
        assert!(!rows[0].ends_with("Opt"), "{rows:?}");
        let joined = rows.join("");
        assert_eq!(joined.replace(' ', ""), src.replace(' ', ""));
    }

    #[test]
    fn a_token_longer_than_the_row_is_cut_by_cells() {
        let src = format!("x {}", "a".repeat(50));
        let rows = texts(&wrap_code(&line(&src), 20));
        assert!(rows.iter().all(|r| r.chars().count() <= 20), "{rows:?}");
        assert_eq!(rows.concat().replace(' ', ""), src.replace(' ', ""));
    }

    #[test]
    fn indentation_counts_and_wide_cells_fit() {
        let src = "    犬犬犬犬犬犬犬犬犬犬犬犬";
        for w in [10, 11, 16] {
            let rows = wrap_code(&line(src), w);
            for r in texts(&rows) {
                assert!(tuikit::width::display_width(&r) <= w, "{w}: {r:?}");
            }
        }
    }

    /// Feed a line one char at a time. Every row but the last must stay as it was and the
    /// last may only gain cells; the finished line may add rows and extend the last.
    fn assert_grows_only(src: &str, w: usize) {
        let mut prev: Vec<String> = Vec::new();
        let chars: Vec<(usize, char)> = src.char_indices().collect();
        for (k, _) in chars.iter().enumerate() {
            let end = chars.get(k + 1).map_or(src.len(), |c| c.0);
            let now = texts(&wrap_code_partial(&line(&src[..end]), w));
            if !prev.is_empty() {
                let keep = prev.len() - 1;
                assert!(
                    now.len() >= prev.len(),
                    "w {w} {:?}: rows shrank {prev:?} -> {now:?}",
                    &src[..end]
                );
                for i in 0..keep {
                    assert_eq!(now[i], prev[i], "w {w} {:?}: row {i}", &src[..end]);
                }
                assert!(
                    now[keep].starts_with(&prev[keep]),
                    "w {w} {:?}: tail {:?} -> {:?}",
                    &src[..end],
                    prev[keep],
                    now[keep]
                );
            }
            prev = now;
        }
        // The finished line starts from what was shown.
        let fin = texts(&wrap_code(&line(src), w));
        if !prev.is_empty() {
            let keep = prev.len() - 1;
            assert!(fin.len() >= prev.len(), "w {w}: {prev:?} -> {fin:?}");
            for i in 0..keep {
                assert_eq!(fin[i], prev[i], "w {w} final row {i}: {prev:?} -> {fin:?}");
            }
            assert!(
                fin[keep].starts_with(&prev[keep]),
                "w {w}: {prev:?} -> {fin:?}"
            );
        }
    }

    #[test]
    fn a_growing_line_only_appends_and_the_finished_line_extends_it() {
        let lines = [
            "    let value_1 = compute_1(&input, 1) + other(2);",
            "pub fn wrap(line: &str, w: usize) -> Vec<String> { line.split(' ').collect() }",
            "x = aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; y();",
            "        .map(|r| r.trim_end().to_string()).collect::<Vec<_>>());",
            "a.b.c.d.e.f.g.h.i.j.k.l.m.n.o.p.q.r.s.t.u.v.w.x.y.z.aa.bb.cc",
            "    犬犬 犬犬犬 犬犬犬犬 犬犬 犬犬犬犬犬犬犬犬犬犬犬犬犬犬犬犬",
            "))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))",
            "a ; b ; c ; d ; e ; f ; g ; h ; i ; j ; k ; l ; m ; n ; o ; p",
        ];
        let mut seed = 0x9e3779b97f4a7c15u64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut all: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
        // Random punctuation-heavy lines.
        let alphabet: Vec<char> = "ab c(),.;:)]} _-/\\\"'&|=!?犬".chars().collect();
        for _ in 0..200 {
            let n = (rnd() % 70) as usize + 1;
            all.push(
                (0..n)
                    .map(|_| alphabet[(rnd() as usize) % alphabet.len()])
                    .collect(),
            );
        }
        for src in &all {
            for w in [4usize, 5, 6, 7, 8, 9, 10, 12, 17, 24, 31, 40, 76] {
                assert_grows_only(src, w);
            }
        }
    }
}
