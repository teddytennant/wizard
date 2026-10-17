//! Display-width helpers. Everything here counts terminal cells, never bytes or chars.
//!
//! The wrapping core works on a flat list of [`Cell`]s (one per grapheme) so the plain-string,
//! styled-span and editor paths all break lines the same way.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::borrow::Cow;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Default columns a tab occupies wherever the toolkit has to pick one (editor, markdown, tool
/// output). A frontend can change it with [`set_tab_width`].
pub const TAB_WIDTH: usize = 4;

/// Whether widths follow grapheme clusters (a ZWJ family is one wide glyph, a heart with VS16
/// is two cells), which is what a terminal in mode 2027 does and what `unicode-width` computes.
/// A terminal without it adds up the width of each code point, so the same text takes other
/// column counts there and every cell after it on the row lands in the wrong place. Clusters are
/// the default; [`set_cluster_widths`] switches to per-code-point sums for such a terminal.
static CLUSTER_WIDTHS: AtomicBool = AtomicBool::new(true);

/// Choose cluster widths (`true`, the default) or per-code-point sums (`false`). Call before
/// anything is laid out.
pub fn set_cluster_widths(on: bool) {
    CLUSTER_WIDTHS.store(on, Ordering::Relaxed);
}

pub fn cluster_widths() -> bool {
    CLUSTER_WIDTHS.load(Ordering::Relaxed)
}

static TAB: AtomicUsize = AtomicUsize::new(TAB_WIDTH);

static SPACES_FIT: AtomicBool = AtomicBool::new(false);

/// OpenTUI wraps `Word` and `WordPunct` text greedily by cell ([`wrap_scan`]): a space that does
/// not fit overflows like any other cell, so a word that ends exactly at the edge still moves
/// down when a space follows, and a long word that fills a row leaves the space to start the
/// next one. The default keeps the older rule, where a trailing space hangs past the edge.
pub fn set_spaces_fit(on: bool) {
    SPACES_FIT.store(on, Ordering::Relaxed);
}

/// Columns a tab takes right now.
pub fn tab_width() -> usize {
    TAB.load(Ordering::Relaxed)
}

/// opencode draws a tab as two cells; openw sets that, other frontends keep four.
pub fn set_tab_width(n: usize) {
    TAB.store(n.max(1), Ordering::Relaxed);
}

/// Width of one grapheme cluster. Control characters count as zero so stray bytes in tool
/// output cannot push a line past its box.
pub fn grapheme_width(g: &str) -> usize {
    grapheme_width_in(g, cluster_widths())
}

/// [`grapheme_width`] under an explicit width model.
pub fn grapheme_width_in(g: &str, cluster: bool) -> usize {
    if g == "\t" {
        return tab_width();
    }
    let mut chars = g.chars();
    match (chars.next(), chars.next()) {
        (None, _) => 0,
        (Some(c), None) if c.is_control() => 0,
        // "\r\n" is one cluster; it must not reach a cell as a control.
        _ if g.chars().any(char::is_control) => 0,
        _ if cluster => UnicodeWidthStr::width(g),
        _ => g
            .chars()
            .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
            .sum(),
    }
}

/// Display width of a string in terminal cells. Tabs count as [`TAB_WIDTH`].
pub fn display_width(s: &str) -> usize {
    if s.is_ascii() {
        let tab = tab_width();
        return s
            .bytes()
            .map(|b| match b {
                b'\t' => tab,
                0x20..=0x7e => 1,
                _ => 0,
            })
            .sum();
    }
    s.graphemes(true).map(grapheme_width).sum()
}

/// Characters that reorder or hide text without drawing: bidi embeddings, overrides and
/// isolates, the marks that set direction, and the invisible operators. A terminal that
/// does bidi shows a different string than the one the model sent.
pub fn is_format_spoof(c: char) -> bool {
    matches!(c,
        '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
        | '\u{2060}'..='\u{2064}' | '\u{FEFF}')
}

/// Replace tabs with spaces and drop control characters other than `\n`, plus the bidi and
/// invisible format characters. Borrowed when the input is already clean, which is the common
/// case. Escape *bodies* stay as visible text here (`ESC` itself is gone); text that goes to
/// the real terminal instead of through ratatui needs [`plain_text`].
pub fn sanitize(s: &str) -> Cow<'_, str> {
    let bad = |c: char| (c.is_control() && c != '\n') || is_format_spoof(c);
    if !s.chars().any(bad) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\t' => out.extend(std::iter::repeat_n(' ', tab_width())),
            '\n' => out.push('\n'),
            c if bad(c) => {}
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// Text that will be written to the real terminal outside ratatui (window title, the summary
/// printed after the alternate screen closes). Removes whole escape sequences, bodies
/// included (CSI, OSC, DCS, SOS, PM, APC, two-byte escapes, and their C1 forms), then every
/// remaining control character except `\n` and the bidi and invisible format characters.
/// Tabs become spaces. Nothing in the result can start or continue a control sequence.
pub fn plain_text(s: &str) -> Cow<'_, str> {
    if !s
        .chars()
        .any(|c| (c.is_control() && c != '\n') || is_format_spoof(c))
    {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\x1b' => match it.peek().copied() {
                Some('[') => {
                    it.next();
                    skip_csi(&mut it);
                }
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    it.next();
                    skip_string(&mut it);
                }
                Some(n) if ('\u{20}'..='\u{2f}').contains(&n) => {
                    // nF escape: intermediates then one final byte.
                    while it.next_if(|n| ('\u{20}'..='\u{2f}').contains(n)).is_some() {}
                    it.next_if(|n| ('\u{30}'..='\u{7e}').contains(n));
                }
                Some(n) if ('\u{30}'..='\u{7e}').contains(&n) => {
                    it.next();
                }
                _ => {}
            },
            '\u{9b}' => skip_csi(&mut it),
            '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => skip_string(&mut it),
            '\t' => out.push_str("    "),
            '\n' => out.push('\n'),
            c if c.is_control() || is_format_spoof(c) => {}
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

fn skip_csi(it: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    // parameters and intermediates, then a final byte; anything else aborts the sequence the
    // way a terminal does (the offending char is then handled as ordinary input).
    while it.next_if(|c| ('\u{20}'..='\u{3f}').contains(c)).is_some() {}
    it.next_if(|c| ('\u{40}'..='\u{7e}').contains(c));
}

fn skip_string(it: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    // up to BEL, ST (ESC \ or U+009C) or the end of the input. A bare ESC that does not start
    // ST ends the string; it is consumed here and what follows is plain text.
    while let Some(&c) = it.peek() {
        match c {
            '\x07' | '\u{9c}' => {
                it.next();
                return;
            }
            '\x1b' => {
                it.next();
                if it.peek() == Some(&'\\') {
                    it.next();
                }
                return;
            }
            _ => {
                it.next();
            }
        }
    }
}

/// `s` as one shell word: bare when it is plain, else single-quoted, so a `--session <id>` hint
/// printed for the user to paste cannot run anything the id spells. Control sequences are
/// removed first ([`plain_text`]).
pub fn shell_word(s: &str) -> String {
    let id = plain_text(s).replace('\n', " ");
    if !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
    {
        id
    } else {
        format!("'{}'", id.replace('\'', "'\\''"))
    }
}

/// Normalise `\r\n` and lone `\r` to `\n`.
pub fn normalize_newlines(s: &str) -> Cow<'_, str> {
    if !s.contains('\r') {
        return Cow::Borrowed(s);
    }
    Cow::Owned(s.replace("\r\n", "\n").replace('\r', "\n"))
}

thread_local! {
    static ASCII_ELLIPSIS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Draw truncation as `...` instead of `…` on this thread, for a terminal that cannot show the
/// one-cell character. The UI draws on one thread, so a per-thread switch is enough and keeps
/// tests that set it apart.
pub fn set_ascii_ellipsis(on: bool) {
    ASCII_ELLIPSIS.with(|c| c.set(on));
}

/// What a cut string ends in and how many cells that takes.
fn ellipsis() -> (&'static str, usize) {
    if ASCII_ELLIPSIS.with(std::cell::Cell::get) {
        ("...", 3)
    } else {
        ("…", 1)
    }
}

/// Truncate to `max` cells, ending with `…` (or `...`, see [`set_ascii_ellipsis`]) when
/// something was cut.
pub fn truncate(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }
    let (dots, dw) = ellipsis();
    if max <= dw {
        // Not even room for the marker and a letter: the marker, cut to fit.
        return dots.chars().take(max).collect();
    }
    let budget = max - dw;
    let mut out = String::new();
    let mut w = 0;
    for g in s.graphemes(true) {
        let gw = grapheme_width(g);
        if w + gw > budget {
            break;
        }
        out.push_str(g);
        w += gw;
    }
    out.push_str(dots);
    out
}

/// Like [`truncate`] but keeps the end of the string (for paths).
pub fn truncate_left(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }
    let (dots, dw) = ellipsis();
    if max <= dw {
        return dots.chars().take(max).collect();
    }
    let budget = max - dw;
    let mut kept: Vec<&str> = Vec::new();
    let mut w = 0;
    for g in s.graphemes(true).rev() {
        let gw = grapheme_width(g);
        if w + gw > budget {
            break;
        }
        kept.push(g);
        w += gw;
    }
    kept.reverse();
    format!("{dots}{}", kept.concat())
}

/// Keep both ends, drop the middle.
pub fn truncate_middle(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }
    let (dots, dw) = ellipsis();
    if max <= dw + 1 {
        return truncate(s, max);
    }
    let budget = max - dw;
    let head_budget = budget.div_ceil(2);
    let tail_budget = budget - head_budget;
    let mut head = String::new();
    let mut hw = 0;
    for g in s.graphemes(true) {
        let gw = grapheme_width(g);
        if hw + gw > head_budget {
            break;
        }
        head.push_str(g);
        hw += gw;
    }
    let mut tail: Vec<&str> = Vec::new();
    let mut tw = 0;
    for g in s.graphemes(true).rev() {
        let gw = grapheme_width(g);
        if tw + gw > tail_budget {
            break;
        }
        tail.push(g);
        tw += gw;
    }
    tail.reverse();
    format!("{head}{dots}{}", tail.concat())
}

/// Pad with spaces on the right up to `width` cells. Longer strings are returned unchanged.
pub fn pad_right(s: &str, width: usize) -> String {
    let w = display_width(s);
    let mut out = s.to_string();
    out.extend(std::iter::repeat_n(' ', width.saturating_sub(w)));
    out
}

pub fn pad_left(s: &str, width: usize) -> String {
    let w = display_width(s);
    let mut out = String::new();
    out.extend(std::iter::repeat_n(' ', width.saturating_sub(w)));
    out.push_str(s);
    out
}

pub fn center(s: &str, width: usize) -> String {
    let w = display_width(s);
    let total = width.saturating_sub(w);
    let left = total / 2;
    let mut out = String::new();
    out.extend(std::iter::repeat_n(' ', left));
    out.push_str(s);
    out.extend(std::iter::repeat_n(' ', total - left));
    out
}

/// One grapheme as the wrapper sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub width: u8,
    pub space: bool,
    /// A row may end right after this cell even inside a word (`WrapMode::WordPunct`).
    pub brk_after: bool,
}

pub fn cell_for(g: &str) -> Cell {
    let w = grapheme_width(g).min(255) as u8;
    let space = g.chars().next().is_some_and(|c| c == ' ' || c == '\t');
    Cell {
        width: w,
        space,
        brk_after: false,
    }
}

/// The characters opentui's word wrap breaks after, measured in opencode 1.18.34 by typing a
/// token that overflows the prompt: `. , ; : ? ( ) [ ] { } - / \`. Others (`!`, `_`, `|`, `=`,
/// `&`, quotes, ...) do not break.
pub fn is_wrap_break(g: &str) -> bool {
    matches!(
        g,
        "." | "," | ";" | ":" | "?" | "(" | ")" | "[" | "]" | "{" | "}" | "-" | "/" | "\\"
    )
}

/// [`cell_for`] with the break marks the mode asks for.
pub fn cell_for_mode(g: &str, mode: WrapMode) -> Cell {
    let mut c = cell_for(g);
    if mode == WrapMode::WordPunct && is_wrap_break(g) {
        c.brk_after = true;
    }
    c
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum WrapMode {
    /// Break at spaces, hard-break words longer than a row.
    #[default]
    Word,
    /// Break anywhere.
    Char,
    /// [`Word`](WrapMode::Word), and also right after the characters in [`is_wrap_break`], as
    /// opencode wraps (a long URL or path fills the row up to its last `/`).
    WordPunct,
    /// Pi's editor (`wordWrapLine` in pi-tui): spaces count toward the row width, so a word
    /// moves down unless the word and the space after it both fit.
    PiEditor,
}

/// Partition `cells` into rows no wider than `width`. Returns the start index of every row
/// (always at least `[0]` for non-empty input, empty for empty input or `width == 0`).
/// Spaces that would overflow a row hang on it instead of starting the next one, so callers
/// that draw rows should trim trailing spaces. A grapheme wider than `width` gets a row of
/// its own so the loop always makes progress.
pub fn wrap_cells(cells: &[Cell], width: usize, mode: WrapMode) -> Vec<usize> {
    wrap_cells_with(cells, width, mode, SPACES_FIT.load(Ordering::Relaxed))
}

/// [`wrap_cells`] with the spaces rule given instead of read from [`set_spaces_fit`].
pub fn wrap_cells_with(
    cells: &[Cell],
    width: usize,
    mode: WrapMode,
    spaces_fit: bool,
) -> Vec<usize> {
    if cells.is_empty() || width == 0 {
        return Vec::new();
    }
    if mode == WrapMode::PiEditor {
        return pi_editor_starts(cells, width);
    }
    if spaces_fit && mode != WrapMode::Char {
        return wrap_scan(cells, width);
    }
    let n = cells.len();
    let mut starts = vec![0usize];
    let mut i = 0;
    while i < n {
        let row_start = i;
        let mut w = 0usize;
        let mut has_word = false;
        loop {
            if i >= n {
                return starts;
            }
            if mode != WrapMode::Char && cells[i].space {
                // Whole space run hangs on the current row.
                while i < n && cells[i].space {
                    w += cells[i].width as usize;
                    i += 1;
                }
                continue;
            }
            // Token: a word (Word mode) or a single cell (Char mode).
            let mut k = i;
            let mut tw = 0usize;
            if mode != WrapMode::Char {
                while k < n && !cells[k].space {
                    tw += cells[k].width as usize;
                    k += 1;
                    if cells[k - 1].brk_after || tw > width {
                        // Past `width` the token is hard-broken anyway, and every test below
                        // only asks whether it fits; scanning to its end once per row made a
                        // single long token quadratic.
                        break;
                    }
                }
            } else {
                tw = cells[i].width as usize;
                k = i + 1;
            }
            if w + tw <= width {
                w += tw;
                i = k;
                has_word = true;
                continue;
            }
            // Does not fit on this row.
            if has_word && tw <= width {
                break; // word moves to the next row
            }
            if has_word && mode != WrapMode::Char {
                // Long word after content: start it on a fresh row, then hard-break it there.
                break;
            }
            // Hard break: fill what fits, at least one cell.
            let mut taken = 0;
            while i < k {
                let cw = cells[i].width as usize;
                if w + cw > width && taken > 0 {
                    break;
                }
                w += cw;
                i += 1;
                taken += 1;
                if w >= width {
                    break;
                }
            }
            break;
        }
        if i < n && i > row_start {
            starts.push(i);
        } else if i == row_start {
            // Defensive: never loop without progress.
            i += 1;
        }
    }
    starts
}

/// OpenTUI's greedy wrap, character by character. A break opportunity is recorded after every
/// space and every cell marked `brk_after`, provided that cell itself fit. When a cell does not
/// fit the row ends at the last opportunity, or just before that cell if there is none, so a
/// space never hangs past the edge: a word that ends exactly at the edge still moves down when a
/// space follows it (the space is what overflowed, and the last opportunity is before the
/// word), and a long word that fills the row leaves the space to start the next one.
fn wrap_scan(cells: &[Cell], width: usize) -> Vec<usize> {
    let n = cells.len();
    let mut starts = vec![0usize];
    let mut row = 0usize;
    let mut w = 0usize;
    let mut last_break: Option<usize> = None;
    let mut j = 0usize;
    while j < n {
        let cw = cells[j].width as usize;
        if w + cw > width && j > row {
            let at = last_break.filter(|b| *b > row).unwrap_or(j);
            starts.push(at);
            row = at;
            w = 0;
            last_break = None;
            j = at;
            continue;
        }
        w += cw;
        if cells[j].space || cells[j].brk_after {
            last_break = Some(j + 1);
        }
        j += 1;
    }
    starts
}

/// Port of pi-tui's `wordWrapLine`. A break is allowed after the last space before a word; when
/// any cell (a space included) overflows, the row ends at that break if what follows it still fits.
fn pi_editor_starts(cells: &[Cell], width: usize) -> Vec<usize> {
    let mut starts = vec![0usize];
    let mut cur = 0usize;
    let mut chunk_start = 0usize;
    let mut opp: Option<(usize, usize)> = None;
    for i in 0..cells.len() {
        let w = cells[i].width as usize;
        if cur + w > width {
            match opp {
                Some((oi, ow)) if cur - ow + w <= width => {
                    starts.push(oi);
                    chunk_start = oi;
                    cur -= ow;
                }
                _ if chunk_start < i => {
                    starts.push(i);
                    chunk_start = i;
                    cur = 0;
                }
                _ => {}
            }
            opp = None;
        }
        cur += w;
        if cells[i].space && cells.get(i + 1).is_some_and(|n| !n.space) {
            opp = Some((i + 1, cur));
        }
    }
    starts
}

/// Word-wrap `s` (no newlines expected; they are treated as zero-width) into byte ranges.
/// Ranges partition the input, so trailing hanging spaces are included in a row's range.
pub fn wrap_ranges(s: &str, width: usize, mode: WrapMode) -> Vec<Range<usize>> {
    let gs: Vec<(usize, &str)> = s.grapheme_indices(true).collect();
    let cells: Vec<Cell> = gs.iter().map(|(_, g)| cell_for_mode(g, mode)).collect();
    let starts = wrap_cells(&cells, width, mode);
    let mut out = Vec::with_capacity(starts.len());
    for (r, &st) in starts.iter().enumerate() {
        let b0 = gs[st].0;
        let b1 = starts.get(r + 1).map_or(s.len(), |&e| gs[e].0);
        out.push(b0..b1);
    }
    out
}

/// Wrap plain text on `\n` and width. Empty input gives one empty line.
pub fn wrap(s: &str, width: usize) -> Vec<String> {
    wrap_mode(s, width, WrapMode::Word)
}

/// [`wrap`] with a chosen mode.
pub fn wrap_mode(s: &str, width: usize, mode: WrapMode) -> Vec<String> {
    let mut out = Vec::new();
    for line in normalize_newlines(&sanitize(s)).split('\n') {
        if line.is_empty() {
            out.push(String::new());
            continue;
        }
        let ranges = wrap_ranges(line, width, mode);
        if ranges.is_empty() {
            out.push(String::new());
        }
        for r in ranges {
            out.push(line[r].trim_end_matches(' ').to_string());
        }
    }
    out
}

/// Wrap a run of styled spans. Spans must not contain newlines. Each returned line has
/// trailing spaces trimmed. Empty input gives one empty line.
pub fn wrap_spans(
    spans: &[Span<'static>],
    width: usize,
    mode: WrapMode,
) -> Vec<Vec<Span<'static>>> {
    struct Seg {
        span: usize,
        start: usize,
        end: usize,
    }
    let mut cells = Vec::new();
    let mut segs = Vec::new();
    for (si, sp) in spans.iter().enumerate() {
        for (b, g) in sp.content.grapheme_indices(true) {
            cells.push(cell_for_mode(g, mode));
            segs.push(Seg {
                span: si,
                start: b,
                end: b + g.len(),
            });
        }
    }
    let starts = wrap_cells(&cells, width, mode);
    if starts.is_empty() {
        return vec![Vec::new()];
    }
    let mut rows = Vec::with_capacity(starts.len());
    for (r, &st) in starts.iter().enumerate() {
        let mut en = starts.get(r + 1).copied().unwrap_or(cells.len());
        // Trim trailing spaces from the drawn row.
        while en > st + 1 && cells[en - 1].space {
            en -= 1;
        }
        if en == st + 1 && cells[st].space && r + 1 < starts.len() {
            en = st; // a row of only hanging spaces draws as empty
        }
        let mut row: Vec<Span<'static>> = Vec::new();
        let mut cur: Option<(usize, usize, usize)> = None; // span, start, end
        for seg in &segs[st..en] {
            match &mut cur {
                Some((sp, _, e)) if *sp == seg.span && *e == seg.start => *e = seg.end,
                _ => {
                    if let Some((sp, a, b)) = cur.take() {
                        row.push(Span::styled(
                            spans[sp].content[a..b].to_string(),
                            spans[sp].style,
                        ));
                    }
                    cur = Some((seg.span, seg.start, seg.end));
                }
            }
        }
        if let Some((sp, a, b)) = cur {
            row.push(Span::styled(
                spans[sp].content[a..b].to_string(),
                spans[sp].style,
            ));
        }
        rows.push(row);
    }
    rows
}

/// Split spans on embedded `\n` into logical lines, keeping styles.
pub fn split_spans_on_newline(spans: Vec<Span<'static>>) -> Vec<Vec<Span<'static>>> {
    let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    for sp in spans {
        if !sp.content.contains('\n') {
            lines.last_mut().unwrap().push(sp);
            continue;
        }
        let mut parts = sp.content.split('\n').peekable();
        while let Some(p) = parts.next() {
            if !p.is_empty() {
                lines
                    .last_mut()
                    .unwrap()
                    .push(Span::styled(p.to_string(), sp.style));
            }
            if parts.peek().is_some() {
                lines.push(Vec::new());
            }
        }
    }
    lines
}

/// Hard-cut spans to `max` cells (no ellipsis). Used as a last safety net so a row can never
/// spill past its box on absurdly small widths.
pub fn clip_spans(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    if spans_width(&spans) <= max {
        return spans;
    }
    let mut out = Vec::new();
    let mut w = 0;
    for sp in spans {
        let mut piece = String::new();
        let mut full = false;
        for g in sp.content.graphemes(true) {
            let gw = grapheme_width(g);
            if w + gw > max {
                full = true;
                break;
            }
            piece.push_str(g);
            w += gw;
        }
        if !piece.is_empty() {
            out.push(Span::styled(piece, sp.style));
        }
        if full {
            break;
        }
    }
    out
}

/// Display width of a span list.
pub fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| display_width(&s.content)).sum()
}

/// Truncate a styled line to `max` cells with an ellipsis (taking the style of the last span
/// that survives).
pub fn truncate_line(line: &Line<'static>, max: usize) -> Line<'static> {
    if spans_width(&line.spans) <= max {
        return line.clone();
    }
    let (dots, dw) = ellipsis();
    if max <= dw {
        return Line::from(dots.chars().take(max).collect::<String>()).style(line.style);
    }
    let budget = max - dw;
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut w = 0;
    let mut last_style = Style::default();
    'outer: for sp in &line.spans {
        let mut piece = String::new();
        for g in sp.content.graphemes(true) {
            let gw = grapheme_width(g);
            if w + gw > budget {
                if !piece.is_empty() {
                    out.push(Span::styled(piece, sp.style));
                }
                last_style = sp.style;
                break 'outer;
            }
            piece.push_str(g);
            w += gw;
        }
        last_style = sp.style;
        if !piece.is_empty() {
            out.push(Span::styled(piece, sp.style));
        }
    }
    out.push(Span::styled(dots, last_style));
    Line::from(out).style(line.style)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_handles_wide_combining_and_controls() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("日本語"), 6);
        assert_eq!(display_width("e\u{301}"), 1); // e + combining acute
        assert_eq!(display_width("a\tb"), 2 + tab_width());
        assert_eq!(display_width("a\r\x07b"), 2);
        assert_eq!(display_width("👨‍👩‍👧"), 2); // ZWJ family is one emoji cell pair
    }

    #[test]
    fn truncate_variants() {
        assert_eq!(truncate("hello world", 20), "hello world");
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate("日本語です", 5), "日本…");
        assert_eq!(truncate("abc", 0), "");
        assert_eq!(truncate_left("/a/b/c/file.rs", 8), "…file.rs");
        assert_eq!(truncate_middle("abcdefghij", 6), "abc…ij");
        assert!(display_width(&truncate_middle("日本語日本語日本語", 7)) <= 7);
    }

    #[test]
    fn pad_and_center() {
        assert_eq!(pad_right("日", 4), "日  ");
        assert_eq!(pad_left("ab", 4), "  ab");
        assert_eq!(center("ab", 5), " ab  ");
        assert_eq!(pad_right("toolong", 3), "toolong");
    }

    #[test]
    fn wrap_words_and_hard_breaks() {
        assert_eq!(wrap("hello world foo", 11), vec!["hello world", "foo"]);
        assert_eq!(wrap("hello world", 5), vec!["hello", "world"]);
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap("ab abcdefghij", 4), vec!["ab", "abcd", "efgh", "ij"]);
        assert_eq!(wrap("a\n\nb", 10), vec!["a", "", "b"]);
        assert_eq!(wrap("", 10), vec![""]);
    }

    #[test]
    fn wrap_never_exceeds_width_with_wide_chars() {
        for w in 1..12 {
            for line in wrap("日本語のテキストを折り返す test データ", w) {
                // A single wide grapheme in a 1-column box is allowed to overflow.
                if w >= 2 {
                    assert!(display_width(&line) <= w, "{line:?} wider than {w}");
                }
            }
        }
    }

    #[test]
    fn punct_wrap_breaks_after_the_characters_opentui_does() {
        // 66 x, "ab", the break character, then 8 more: a 76 cell token in 70 columns
        let x = "x".repeat(66);
        for (p, breaks) in [
            ('.', true),
            ('/', true),
            ('-', true),
            ('(', true),
            ('?', true),
            ('!', false),
            ('_', false),
            ('=', false),
            ('"', false),
        ] {
            let s = format!("{x}ab{p}cdefghij");
            let rows = wrap_mode(&s, 70, WrapMode::WordPunct);
            let first = rows[0].chars().count();
            assert_eq!(first, if breaks { 69 } else { 70 }, "{p}: {rows:?}");
        }
        // plain word mode never breaks inside the token
        assert_eq!(
            wrap_mode(&format!("{x}ab/cdefghij"), 70, WrapMode::Word)[0]
                .chars()
                .count(),
            70
        );
        // a URL near the edge fills the row up to its last slash
        let rows = wrap_mode(
            "see the link (https://example.com/a/very/long/url/that/goes/on) end",
            41,
            WrapMode::WordPunct,
        );
        assert_eq!(rows[0], "see the link (https://example.com/a/very/");
        assert_eq!(rows[1], "long/url/that/goes/on) end");
    }

    #[test]
    fn wrap_zero_width_is_empty_not_a_hang() {
        assert!(wrap_cells(
            &[Cell {
                width: 1,
                space: false,
                brk_after: false
            }],
            0,
            WrapMode::Word
        )
        .is_empty());
        assert_eq!(wrap("abc", 0), vec![String::new()]);
        assert_eq!(wrap("日", 1), vec!["日"]);
    }

    /// Rows as strings under the OpenTUI spaces rule, for the cases measured in real opencode
    /// (`tools/critic/fid/f05_wrap.py`) at 113 columns.
    fn rows_fit(text: &str, width: usize, mode: WrapMode) -> Vec<String> {
        let cells: Vec<Cell> = text
            .graphemes(true)
            .map(|g| cell_for_mode(g, mode))
            .collect();
        let starts = wrap_cells_with(&cells, width, mode, true);
        let chars: Vec<&str> = text.graphemes(true).collect();
        starts
            .iter()
            .enumerate()
            .map(|(r, &st)| {
                let en = starts.get(r + 1).copied().unwrap_or(chars.len());
                chars[st..en].concat().trim_end().to_string()
            })
            .collect()
    }

    #[test]
    fn a_word_and_its_spaces_have_to_fit_together_like_opentui() {
        let w = 113;
        for mode in [WrapMode::Word, WrapMode::WordPunct] {
            // a long word that fills the row hands the space to the next row
            let r = rows_fit(&format!("{} tail words here", "w".repeat(w)), w, mode);
            assert_eq!(r, vec!["w".repeat(w), " tail words here".into()]);
            // the same after punctuation
            let r = rows_fit(&format!("{}; tail", "w".repeat(w - 1)), w, mode);
            assert_eq!(r, vec![format!("{};", "w".repeat(w - 1)), " tail".into()]);
            // `abc` would end at column 113, but not with the space after it
            let src = format!("{}abc next words here", "word ".repeat(22));
            let r = rows_fit(&src, w, mode);
            assert_eq!(r[0], "word ".repeat(22).trim_end());
            assert_eq!(r[1], "abc next words here");
            // a punctuation break keeps the `;` of `")` on the row, the space starts the next
            let src = format!("{}\"); }} }} }}", "x".repeat(w - 3));
            let r = rows_fit(&src, w, mode);
            if mode == WrapMode::WordPunct {
                assert_eq!(
                    r,
                    vec![format!("{}\");", "x".repeat(w - 3)), " } } }".into()]
                );
            }
            // `c` and its space end exactly at 113, so `end` is the only thing that moves
            let src = format!("{}c end", "ab ".repeat(37));
            let r = rows_fit(&src, w, mode);
            assert_eq!(r, vec![format!("{}c", "ab ".repeat(37)), "end".into()]);
        }
        // with the rule off a trailing space hangs, as before
        let cells: Vec<Cell> = "aaaa b"
            .graphemes(true)
            .map(|g| cell_for_mode(g, WrapMode::Word))
            .collect();
        assert_eq!(
            wrap_cells_with(&cells, 4, WrapMode::Word, false),
            vec![0, 5]
        );
        assert_eq!(wrap_cells_with(&cells, 4, WrapMode::Word, true), vec![0, 4]);
    }

    #[test]
    fn wrap_spans_keeps_styles_across_breaks() {
        let spans = vec![
            Span::raw("hello "),
            Span::styled("bold word", Style::new().bold()),
            Span::raw(" tail"),
        ];
        let rows = wrap_spans(&spans, 10, WrapMode::Word);
        let text: Vec<String> = rows
            .iter()
            .map(|r| r.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(text, vec!["hello bold", "word tail"]);
        assert!(rows[0][1]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD));
        assert_eq!(rows[0][1].content, "bold");
    }

    #[test]
    fn wrap_ranges_partition_the_input() {
        let s = "the quick brown fox jumps";
        let rs = wrap_ranges(s, 8, WrapMode::Word);
        assert_eq!(rs.first().unwrap().start, 0);
        assert_eq!(rs.last().unwrap().end, s.len());
        for w in rs.windows(2) {
            assert_eq!(w[0].end, w[1].start);
        }
    }

    #[test]
    fn sanitize_strips_control_bytes() {
        assert_eq!(sanitize("a\tb\x07c"), "a    bc");
        assert!(matches!(sanitize("plain"), Cow::Borrowed(_)));
        assert_eq!(normalize_newlines("a\r\nb\rc"), "a\nb\nc");
    }

    /// Pieces that, glued together in any order, form every kind of sequence a hostile
    /// model, tool or file name could use against the real terminal.
    const HOSTILE: &[&str] = &[
        "\x1b]52;c;cHduZWQ=\x07",
        "\x1b]0;pwn\x1b\\",
        "\x1b]8;;http://evil\x07link\x1b]8;;\x07",
        "\x1b[2J",
        "\x1b[?1049h",
        "\x1b[?1000h",
        "\x1b[6n",
        "\x1bc",
        "\x1bP1$r\x1b\\",
        "\x1b_Gi=1;AAAA\x1b\\",
        "\x1b^pm\x1b\\",
        "\x1bXsos\x07",
        "\x1b(0",
        "\x1b#8",
        "\u{9b}2J",
        "\u{9d}52;c;x\u{9c}",
        "\u{90}q\u{9c}",
        "\u{202e}gnp.exe",
        "\u{2066}x\u{2069}",
        "\u{feff}",
        "\x00\x07\x08\x7f\r\n",
        "\x1b",
        "\x1b[",
        "\x1b]",
        "\x1b[31;",
        "plain ",
        "caf\u{e9} ",
        "日本語",
        "\n",
        "\t",
    ];

    fn assert_inert(out: &str, input: &str) {
        for c in out.chars() {
            assert!(
                (!c.is_control() || c == '\n') && !is_format_spoof(c),
                "{c:?} survived in {out:?} from {input:?}"
            );
        }
    }

    #[test]
    fn plain_text_removes_every_sequence_in_the_hostile_set() {
        for h in HOSTILE {
            let out = plain_text(h);
            assert_inert(&out, h);
        }
        // whole bodies go, not only the introducer
        assert_eq!(plain_text("a\x1b]52;c;cHduZWQ=\x07b"), "ab");
        assert_eq!(plain_text("a\x1b]0;t\x1b\\b"), "ab");
        assert_eq!(plain_text("a\x1b[2Jb\x1b[?1000hc"), "abc");
        assert_eq!(plain_text("a\u{9d}52;c;x\u{9c}b"), "ab");
        assert_eq!(plain_text("a\u{202e}b"), "ab");
        assert_eq!(plain_text("x\x1bcy"), "xy");
        assert!(matches!(plain_text("plain\ntext"), Cow::Borrowed(_)));
    }

    #[test]
    fn plain_text_survives_random_concatenations_of_hostile_pieces() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..4000 {
            let mut s = String::new();
            for _ in 0..(next() % 12) {
                s.push_str(HOSTILE[(next() % HOSTILE.len() as u64) as usize]);
            }
            let out = plain_text(&s);
            assert_inert(&out, &s);
            // idempotent: a second pass finds nothing to remove
            assert_eq!(plain_text(&out), out);
            assert_inert(&sanitize(&s), &s);
        }
    }

    #[test]
    fn a_crlf_cluster_has_no_width() {
        assert_eq!(grapheme_width("\r\n"), 0);
        assert_eq!(display_width("a\r\nb"), 2);
    }

    #[test]
    fn truncate_line_adds_ellipsis_in_last_style() {
        let line = Line::from(vec![
            Span::raw("hello "),
            Span::styled("world", Style::new().bold()),
        ]);
        let t = truncate_line(&line, 8);
        let text: String = t.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "hello w…");
    }

    /// Finding 11: the scan for the end of a token ran to the end of an unbroken blob once per
    /// row it cut, so 400 KB took 0.7 s and 5 MB did not finish.
    #[test]
    fn one_long_token_wraps_in_linear_time() {
        let run = |n: usize| {
            let s = "x".repeat(n);
            let t = std::time::Instant::now();
            let rows = wrap(&s, 40);
            assert_eq!(rows.len(), n.div_ceil(40));
            assert!(rows.iter().all(|r| r.len() <= 40));
            t.elapsed().as_secs_f64()
        };
        let small = run(50_000);
        let big = run(400_000);
        // 8x the text costs about 8x; quadratic costs about 64x
        assert!(
            big < small * 24.0 + 0.2,
            "50 KB {small:.3}s, 400 KB {big:.3}s"
        );
    }

    #[test]
    fn a_long_word_hard_breaks_the_way_it_did() {
        assert_eq!(wrap("ab cdefghij", 5), ["ab", "cdefg", "hij"]);
        assert_eq!(wrap("abcdefgh-ij", 5), ["abcde", "fgh-i", "j"]);
    }

    #[test]
    fn the_ascii_ellipsis_is_three_cells_and_every_truncator_uses_it() {
        set_ascii_ellipsis(true);
        assert_eq!(truncate("abcdefghij", 6), "abc...");
        assert_eq!(truncate_left("abcdefghij", 6), "...hij");
        assert_eq!(truncate_middle("abcdefghij", 7), "ab...ij");
        assert_eq!(truncate("abcdefghij", 2), "..");
        for s in [
            truncate("日本語のテキスト", 7),
            truncate_left("日本語のテキスト", 7),
            truncate_middle("日本語のテキスト", 7),
        ] {
            assert!(display_width(&s) <= 7 && !s.contains('…'), "{s}");
        }
        let line = Line::from(vec![Span::raw("abcdefghij")]);
        assert_eq!(
            truncate_line(&line, 6)
                .spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>(),
            "abc..."
        );
        set_ascii_ellipsis(false);
        assert_eq!(truncate("abcdefghij", 6), "abcde…");
    }

    #[test]
    fn per_code_point_widths_match_a_terminal_without_mode_2027() {
        // (text, cluster width, per-code-point width): the table from the code review.
        for (g, cluster, legacy) in [
            ("👨\u{200d}👩\u{200d}👧", 2, 6),
            ("👍🏽", 2, 4),
            ("1\u{fe0f}\u{20e3}", 2, 1),
            ("❤\u{fe0f}", 2, 1),
            ("⚠\u{fe0f}", 2, 1),
            ("e\u{301}", 1, 1),
            ("字", 2, 2),
            ("a", 1, 1),
        ] {
            assert_eq!(grapheme_width_in(g, true), cluster, "{g:?} as a cluster");
            assert_eq!(grapheme_width_in(g, false), legacy, "{g:?} per code point");
        }
        assert_eq!(grapheme_width_in("\r\n", false), 0);
        assert_eq!(grapheme_width_in("\t", false), TAB_WIDTH);
    }
}
