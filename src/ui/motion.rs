//! One indicator per kind of wait, and a thin bar where the numbers exist.
//!
//! The tick is about 100ms, which is the frame time for a tool or a thought.
//! Idle waits step more slowly. `WIZARD_REDUCED_MOTION` freezes each glyph on
//! its first frame. The determinate bar paints no background. The half-block
//! wave is the exception: two colors in one cell, on a small swatch.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::{accent, dim};
use crate::theme::{self, Token};

/// What the wait is. Each one has its own glyph.
#[derive(Clone, Copy)]
pub(super) enum Kind {
    /// The model is thinking, or text is arriving. Braille, for the house
    /// skins that do not roll their own cycle. Wizard does; see [`THINKING`].
    Thinking,
    /// A tool or a shell command is running.
    Tool,
    /// One subagent, on the rail.
    Subagent,
    /// Nothing is moving yet: a connect, a rebuild.
    Waiting,
    /// A command left in the background.
    Background,
}

pub(super) fn glyph(kind: Kind, tick: u64) -> String {
    glyph_at(kind, tick, reduced())
}

pub(super) fn glyph_at(kind: Kind, tick: u64, reduced: bool) -> String {
    let frames: &[&str] = match kind {
        Kind::Thinking => &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
        Kind::Tool => &["│", "╱", "─", "╲"],
        Kind::Subagent => &["●", "◉", "○", "◉"],
        Kind::Waiting => &[".  ", ".. ", "..."],
        Kind::Background => &["◜", "◠", "◝", "◞", "◡", "◟"],
    };
    let period = match kind {
        Kind::Waiting => 4,
        Kind::Subagent => 2,
        _ => 1,
    };
    let index = if reduced {
        0
    } else {
        ((tick / period) as usize) % frames.len()
    };
    let frame = frames[index];
    if reduced && matches!(kind, Kind::Waiting) {
        "…".to_string()
    } else if reduced && matches!(kind, Kind::Subagent) {
        "●".to_string()
    } else {
        frame.to_string()
    }
}

/// Glyph cycles, then the gallery. The first [`CORE`] are the original
/// one-cell deck. A turn draws one and holds it; the next turn draws another,
/// from a shuffled deck, so it is not the same wheel twice in a row.
#[derive(Clone, Copy)]
enum Cycle {
    Glyphs(&'static [&'static str]),
    Spark,
    Wand,
    Fill,
    Heart,
    Breathe,
    Chase,
    Scanner,
    Needle,
    Sweep,
    Ring,
    Wave,
    Glider,
}

/// Original one-cell deck. Dashboard rows deal from this prefix so a session
/// mark stays one cell, and so a session keeps the cycle it had before the
/// gallery was added.
const CORE: usize = 14;

const CYCLES: &[Cycle] = &[
    Cycle::Glyphs(&["◐", "◓", "◑", "◒"]),
    Cycle::Glyphs(&["▖", "▘", "▝", "▗"]),
    Cycle::Glyphs(&[
        "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█", "▇", "▆", "▅", "▄", "▃", "▂",
    ]),
    Cycle::Glyphs(&["·", "∘", "○", "●", "○", "∘"]),
    Cycle::Glyphs(&["⌜", "⌝", "⌟", "⌞"]),
    Cycle::Glyphs(&["◢", "◣", "◤", "◥"]),
    Cycle::Glyphs(&["░", "▒", "▓", "█", "▓", "▒"]),
    Cycle::Glyphs(&["◇", "◈", "◆", "◈"]),
    Cycle::Glyphs(&["┤", "┘", "┴", "└", "├", "┌", "┬", "┐"]),
    Cycle::Glyphs(&["←", "↖", "↑", "↗", "→", "↘", "↓", "↙"]),
    Cycle::Glyphs(&["○", "◔", "◑", "◕", "●", "◕", "◑", "◔"]),
    Cycle::Glyphs(&["▢", "▣", "■", "▣"]),
    Cycle::Glyphs(&["╭", "╮", "╯", "╰"]),
    Cycle::Glyphs(&["⋮", "⋯", "⋰", "⋱"]),
    Cycle::Glyphs(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    Cycle::Glyphs(&["|", "/", "-", "\\"]),
    Cycle::Glyphs(&["│", "╱", "─", "╲"]),
    Cycle::Glyphs(&["●", "◉", "○", "◉"]),
    Cycle::Glyphs(&[".  ", ".. ", "..."]),
    Cycle::Glyphs(&["◜", "◠", "◝", "◞", "◡", "◟"]),
    Cycle::Glyphs(&["·", "✢", "✳", "∗", "✳", "✢"]),
    Cycle::Glyphs(&["·", "✢", "*", "✶", "✻", "✽"]),
    Cycle::Spark,
    Cycle::Wand,
    Cycle::Fill,
    Cycle::Heart,
    Cycle::Breathe,
    Cycle::Chase,
    Cycle::Scanner,
    Cycle::Needle,
    Cycle::Sweep,
    Cycle::Ring,
    Cycle::Wave,
    Cycle::Glider,
];

pub(crate) fn thinking_count() -> usize {
    CYCLES.len()
}

fn cycle_at(ix: usize) -> Cycle {
    CYCLES[ix % CYCLES.len()]
}

fn frame_index(len: usize, phase: u64, tick: u64) -> usize {
    if reduced() || len == 0 {
        0
    } else {
        tick.wrapping_add(phase) as usize % len
    }
}

/// Plain text of the first line. Tests and the rolled-cycle check use this.
/// Multi-row cycles return only the top row.
pub(crate) fn thinking_frame(ix: usize, phase: u64, tick: u64) -> String {
    thinking_spans(ix, phase, tick, "thinking")
        .into_iter()
        .map(|span| span.content.into_owned())
        .collect()
}

/// First cell of the cycle. The streaming tail and the dashboard use this so
/// a three-cell or two-row mark does not land in a paragraph or a session row.
pub(crate) fn thinking_cell(ix: usize, phase: u64, tick: u64) -> String {
    if matches!(cycle_at(ix), Cycle::Sweep) {
        return "·".to_string();
    }
    thinking_frame(ix, phase, tick)
        .chars()
        .next()
        .unwrap_or('·')
        .to_string()
}

/// True when the cycle is the color sweep, which replaces the waiting word
/// instead of sitting in front of it.
pub(crate) fn cycle_sweeps(ix: usize) -> bool {
    matches!(cycle_at(ix), Cycle::Sweep)
}

/// Styled first line of the rolled cycle. `word` is the waiting label, used
/// only by the sweep.
pub(crate) fn thinking_spans(ix: usize, phase: u64, tick: u64, word: &str) -> Vec<Span<'static>> {
    let tick = if reduced() { 0 } else { tick };
    match cycle_at(ix) {
        Cycle::Glyphs(frames) => {
            let at = frame_index(frames.len(), phase, tick);
            vec![Span::styled(frames[at], accent())]
        }
        Cycle::Spark => vec![Span::styled(spark(frame_index(8, phase, tick)), accent())],
        Cycle::Wand => vec![Span::styled(wand(frame_index(8, phase, tick)), accent())],
        Cycle::Fill => vec![Span::styled(fill(frame_index(16, phase, tick)), accent())],
        Cycle::Heart => vec![Span::styled(heart(frame_index(6, phase, tick)), accent())],
        Cycle::Breathe => vec![Span::styled("◆", breathe_style(tick.wrapping_add(phase)))],
        Cycle::Chase => chase_spans(frame_index(4, phase, tick)),
        Cycle::Scanner => vec![Span::styled(scanner(frame_index(8, phase, tick)), accent())],
        Cycle::Needle => vec![Span::styled(needle(frame_index(12, phase, tick)), accent())],
        Cycle::Sweep => {
            let word = if word.is_empty() { "thinking" } else { word };
            crate::skin::motion::shimmer(word, tick.wrapping_add(phase))
        }
        Cycle::Ring => braille_spans(&ring_points(frame_index(16, phase, tick)), 4, 0),
        Cycle::Wave => wave_spans(tick.wrapping_add(phase), 0),
        Cycle::Glider => braille_spans(&glider_points(life_gen(phase, tick)), 4, 0),
    }
}

/// Second row of a two-row cycle. Empty for everything else.
pub(crate) fn thinking_extra(ix: usize, phase: u64, tick: u64) -> Vec<Line<'static>> {
    let tick = if reduced() { 0 } else { tick };
    let spans = match cycle_at(ix) {
        Cycle::Ring => braille_spans(&ring_points(frame_index(16, phase, tick)), 4, 4),
        Cycle::Wave => wave_spans(tick.wrapping_add(phase), 2),
        Cycle::Glider => braille_spans(&glider_points(life_gen(phase, tick)), 4, 4),
        _ => return Vec::new(),
    };
    vec![Line::from(spans)]
}

/// Stable cycle for a dashboard row. Dealt from the original one-cell deck so
/// a session keeps its animation and the row stays one cell.
pub(crate) fn session_cycle(id: &str) -> usize {
    let mut h = 0xcbf29ce484222325u64;
    for b in id.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    (h as usize) % CORE
}

fn reduced() -> bool {
    matches!(
        std::env::var("WIZARD_REDUCED_MOTION").as_deref(),
        Ok("1") | Ok("true")
    )
}

/// Braille bits, left to right and top to bottom. A point outside the cell is
/// left unset, so a circle can clip a corner without a special case.
const BRAILLE_BIT: [u8; 8] = [0x01, 0x02, 0x04, 0x40, 0x08, 0x10, 0x20, 0x80];

fn braille_cell(points: &[(i32, i32)], ox: i32, oy: i32) -> char {
    let mut bits = 0u8;
    for &(x, y) in points {
        let dx = x - ox;
        let dy = y - oy;
        if (0..2).contains(&dx) && (0..4).contains(&dy) {
            bits |= BRAILLE_BIT[(dy as usize) * 2 + dx as usize];
        }
    }
    char::from_u32(0x2800 + u32::from(bits)).unwrap_or('⠀')
}

/// `cols` braille cells, reading `points` from row `row0`.
fn braille_spans(points: &[(i32, i32)], cols: i32, row0: i32) -> Vec<Span<'static>> {
    (0..cols)
        .map(|col| Span::styled(braille_cell(points, col * 2, row0).to_string(), accent()))
        .collect()
}

/// Eight sparks. Two of them are the same mark on purpose: that is the beat
/// where the previous one has gone and the next has not arrived.
fn spark(frame: usize) -> String {
    const FRAMES: [&str; 8] = ["·", "✦", "·", "✧", "·", "✶", "·", "✹"];
    FRAMES[frame % FRAMES.len()].to_string()
}

/// A two-cell wand. The tip is the only cell that changes.
fn wand(frame: usize) -> String {
    const TIPS: [&str; 8] = ["·", "∘", "○", "✺", "✹", "✺", "○", "∘"];
    format!("╱{}", TIPS[frame % TIPS.len()])
}

/// A two-cell eighths bar, filling and then emptying.
fn fill(frame: usize) -> String {
    const LEFT: [&str; 9] = ["▏", "▎", "▍", "▌", "▋", "▊", "▉", "█", "█"];
    const RIGHT: [&str; 9] = [" ", " ", " ", " ", " ", " ", " ", "▏", "█"];
    let n = frame % 16;
    let (l, r) = if n < 9 { (n, 0) } else { (8, 16 - n) };
    format!("{}{}", LEFT[l], RIGHT[r])
}

/// Two hearts trading weight. The space is the second cell, so the row does
/// not jump when the mark changes width.
fn heart(frame: usize) -> String {
    const FRAMES: [&str; 6] = ["♡ ", "♥ ", "♡ ", "♥♡", "♡♥", " ♡"];
    FRAMES[frame % FRAMES.len()].to_string()
}

/// One diamond, brightening and dimming. Truecolor blends toward the
/// background; without it the glyph steps through dim, plain, and bold.
fn breathe_style(tick: u64) -> Style {
    let intensity = 0.5 * (1.0 + (tick as f32 * 0.35).sin());
    if theme::active().depth() == theme::ColorDepth::TrueColor {
        if let ratatui::style::Color::Rgb(r, g, b) = theme::color(Token::Accent) {
            let (r, g, b) = crate::skin::blend::blend((r, g, b), (r / 4, g / 4, b / 4), intensity);
            return Style::default().fg(ratatui::style::Color::Rgb(r, g, b));
        }
    }
    if intensity < 0.35 {
        dim()
    } else if intensity < 0.7 {
        accent()
    } else {
        accent().add_modifier(ratatui::style::Modifier::BOLD)
    }
}

/// A three-cell chase: the lit block walks, the other two stay dim.
fn chase_spans(frame: usize) -> Vec<Span<'static>> {
    (0..3)
        .map(|i| {
            let on = i == frame % 3;
            Span::styled("■", if on { accent() } else { dim() })
        })
        .collect()
}

/// A three-cell window on a sine. The middle cell is the brightest sample.
fn scanner(frame: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let at = |shift: usize| {
        let n = ((frame + shift) as f32 * 0.7).sin().abs();
        BARS[(n * 7.0).round() as usize]
    };
    format!("{}{}{}", at(0), at(2), at(4))
}

/// A one-cell needle. Twelve marks, so it does not repeat the four-frame line.
fn needle(frame: usize) -> String {
    const FRAMES: [&str; 12] = ["│", "╱", "─", "╲", "│", "╱", "─", "╲", "┼", "╳", "┼", "│"];
    FRAMES[frame % FRAMES.len()].to_string()
}

/// A 4 by 4 ring, two braille rows. `frame` rotates which dots are lit.
fn ring_points(frame: usize) -> Vec<(i32, i32)> {
    const RING: [(i32, i32); 12] = [
        (1, 0),
        (2, 0),
        (3, 0),
        (4, 0),
        (5, 1),
        (6, 2),
        (6, 3),
        (5, 4),
        (4, 5),
        (3, 6),
        (2, 6),
        (1, 5),
    ];
    let n = frame % RING.len();
    (0..4).map(|i| RING[(n + i * 3) % RING.len()]).collect()
}

/// Four half-block columns. `row` is 0 for the top pair, 2 for the bottom,
/// so the two rows of the mark share one phase.
fn wave_spans(tick: u64, row: i32) -> Vec<Span<'static>> {
    const UPPER: [char; 8] = ['█', '▇', '▆', '▅', '▄', '▃', '▂', '▁'];
    let mut spans = Vec::with_capacity(4);
    for col in 0..4 {
        let phase = tick as f32 * 0.45 + col as f32 * 0.8;
        let height = 1.0 + phase.sin();
        let top = height > 1.0;
        let sample = if row == 0 { top } else { !top };
        let level = ((height.fract()) * 7.0).round() as usize;
        let ch = if sample { UPPER[7 - level] } else { ' ' };
        spans.push(Span::styled(ch.to_string(), accent()));
    }
    spans
}

/// Conway's glider, 8 by 8, one generation per few ticks. The phase picks
/// which generation the freeze lands on.
fn life_gen(phase: u64, tick: u64) -> usize {
    if reduced() {
        return (phase as usize) % 4;
    }
    ((tick.wrapping_add(phase) / 3) as usize) % 4
}

fn glider_points(generation: usize) -> Vec<(i32, i32)> {
    const GLIDER: [[(i32, i32); 5]; 4] = [
        [(1, 0), (2, 1), (0, 2), (1, 2), (2, 2)],
        [(0, 1), (2, 1), (1, 2), (2, 2), (1, 3)],
        [(2, 1), (0, 2), (2, 2), (1, 3), (2, 3)],
        [(1, 1), (2, 2), (3, 2), (1, 3), (2, 3)],
    ];
    GLIDER[generation % GLIDER.len()].to_vec()
}

/// `182000` → `182k`. Under a thousand the number is left whole.
pub(crate) fn fmt_tokens(n: u64) -> String {
    if n >= 1000 {
        format!("{}k", n / 1000)
    } else {
        n.to_string()
    }
}

/// A measured bar: `━` in the accent over a dim `─` track. `width` is the
/// track, not the label.
pub(super) fn determinate(done: u64, total: u64, width: usize) -> Line<'static> {
    let width = width.max(1);
    let total = total.max(1);
    let filled = ((done.min(total) as usize) * width) / total as usize;
    Line::from(vec![
        Span::styled("━".repeat(filled), accent()),
        Span::styled("─".repeat(width - filled), dim()),
    ])
}

/// A one-cell window walking a dim track. Frozen at the left edge when
/// motion is reduced, because there is no number to stand still on.
pub(super) fn sweep(width: usize, tick: u64, reduced: bool) -> Line<'static> {
    let width = width.max(1);
    let offset = if reduced { 0 } else { (tick as usize) % width };
    let mut spans = Vec::new();
    if offset > 0 {
        spans.push(Span::styled("─".repeat(offset), dim()));
    }
    spans.push(Span::styled("━", accent()));
    if offset + 1 < width {
        spans.push(Span::styled("─".repeat(width - offset - 1), dim()));
    }
    Line::from(spans)
}

/// What a command's output has admitted so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CommandProgress {
    pub done: u64,
    pub total: u64,
}

/// `Compiling [3/40]`, `45%`, and cargo's `running N tests` plus the `ok`
/// lines under it. The last figure in the output wins. Ordinary prose
/// (`1/2`) is not a bar.
pub(super) fn parse_command_progress(text: &str) -> Option<CommandProgress> {
    let mut running: Option<u64> = None;
    let mut tests = 0u64;
    let mut fraction: Option<CommandProgress> = None;
    let mut percent: Option<CommandProgress> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(n) = running_tests(line) {
            running = Some(n);
            tests = 0;
        }
        if is_test_result(line) {
            tests = tests.saturating_add(1);
        }
        if let Some(found) = fraction_in(line) {
            fraction = Some(found);
        }
        if let Some(found) = percent_in(line) {
            percent = Some(found);
        }
    }
    if let Some(total) = running
        && total > 0
        && tests > 0
    {
        return Some(CommandProgress {
            done: tests.min(total),
            total,
        });
    }
    fraction.or(percent)
}

fn running_tests(line: &str) -> Option<u64> {
    let rest = line.strip_prefix("running ")?;
    let (n, tail) = rest.split_once(' ')?;
    if tail.starts_with("test") {
        n.parse().ok()
    } else {
        None
    }
}

fn is_test_result(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("test ") else {
        return false;
    };
    rest.ends_with("... ok") || rest.ends_with("... FAILED") || rest.ends_with("... ignored")
}

fn fraction_in(line: &str) -> Option<CommandProgress> {
    // Prefer a bracketed or parenthesised `n/m`, then a bare one on a line
    // that already looks like a build.
    let interesting = line.contains('[')
        || line.contains('(')
        || line.contains("Compil")
        || line.contains("Build")
        || line.contains("Download")
        || line.contains("Install");
    if !interesting {
        return None;
    }
    let bytes = line.as_bytes();
    let mut best = None;
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i < bytes.len()
                && bytes[i] == b'/'
                && i + 1 < bytes.len()
                && bytes[i + 1].is_ascii_digit()
            {
                let done: u64 = line[start..i].parse().ok()?;
                let mut j = i + 1;
                let end = j;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                let total: u64 = line[end..j].parse().ok()?;
                if total > 0 && done <= total && total < 1_000_000 {
                    best = Some(CommandProgress { done, total });
                }
                i = j;
                continue;
            }
        }
        i += 1;
    }
    best
}

fn percent_in(line: &str) -> Option<CommandProgress> {
    let interesting = line.contains('%')
        && (line.contains("Build")
            || line.contains("Compil")
            || line.contains("Download")
            || line.contains("Install")
            || line.contains("bundl")
            || line.contains("webpack"));
    if !interesting {
        return None;
    }
    let (head, _) = line.split_once('%')?;
    let digits: String = head
        .chars()
        .rev()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    let pct: u64 = digits.parse().ok()?;
    if pct > 100 {
        return None;
    }
    Some(CommandProgress {
        done: pct,
        total: 100,
    })
}

/// Cargo, npm, and the other commands whose output is a build rather than
/// a sentence. Used when the output has not admitted a fraction yet.
pub(super) fn is_long_command(command: &str) -> bool {
    let command = command.to_ascii_lowercase();
    const MARKS: &[&str] = &[
        "cargo", "npm", "pnpm", "yarn", "make", "just ", "go test", "pytest", "jest", "mvn",
        "gradle", "cmake", "ninja", "tsc", "webpack", "bun ", "ctest", "meson",
    ];
    MARKS.iter().any(|mark| command.contains(mark))
}

/// One line for a running build: the bar, the count, and how long it has
/// been going. `None` when this command is not a build and has no numbers.
pub(super) fn command_line(
    command: &str,
    output: &str,
    elapsed: &str,
    width: usize,
    tick: u64,
) -> Option<Line<'static>> {
    let progress = parse_command_progress(output);
    if progress.is_none() && !is_long_command(command) {
        return None;
    }
    let detail = match &progress {
        Some(progress) => format!("{}/{}", progress.done, progress.total),
        None => String::new(),
    };
    let tail = if elapsed.is_empty() {
        detail.clone()
    } else if detail.is_empty() {
        elapsed.to_string()
    } else {
        format!("{detail}  {elapsed}")
    };
    let bar_width = width.saturating_sub(tail.chars().count() + 2).clamp(4, 16);
    let mut bar = match &progress {
        Some(progress) => determinate(progress.done, progress.total, bar_width),
        None => sweep(bar_width, tick, reduced()),
    };
    if !tail.is_empty() {
        bar.spans.push(Span::styled(
            format!("  {tail}"),
            theme::style(Token::Faint),
        ));
    }
    Some(bar)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_kind_moves_on_its_own_clock_and_freezes() {
        assert_ne!(
            glyph_at(Kind::Thinking, 0, false),
            glyph_at(Kind::Thinking, 1, false)
        );
        assert_ne!(
            glyph_at(Kind::Tool, 0, false),
            glyph_at(Kind::Thinking, 0, false),
            "a tool is not the thinking braille"
        );
        assert_eq!(glyph_at(Kind::Tool, 0, false), "│");
        assert_eq!(glyph_at(Kind::Waiting, 0, false), ".  ");
        assert_eq!(glyph_at(Kind::Waiting, 4, false), ".. ");
        assert_eq!(glyph_at(Kind::Waiting, 0, true), "…");
        assert_eq!(glyph_at(Kind::Subagent, 0, true), "●");
        assert_eq!(glyph_at(Kind::Tool, 3, true), "│");
        assert_eq!(glyph_at(Kind::Background, 0, false).chars().count(), 1);
    }

    #[test]
    fn a_determinate_bar_fills_with_the_count_and_paints_no_background() {
        let line = determinate(1, 4, 8);
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(text, "━━──────");
        assert!(
            line.spans
                .iter()
                .all(|span| { matches!(span.style.bg, Some(ratatui::style::Color::Reset) | None) })
        );
        let frozen = sweep(6, 3, true);
        let frozen_text: String = frozen.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(frozen_text.starts_with('━'), "{frozen_text}");
    }

    #[test]
    fn cargo_fractions_percents_and_test_counts_parse() {
        assert_eq!(
            parse_command_progress("   Compiling wizard (3/40)\n"),
            Some(CommandProgress { done: 3, total: 40 })
        );
        assert_eq!(
            parse_command_progress("webpack bundled 45%\n"),
            Some(CommandProgress {
                done: 45,
                total: 100
            })
        );
        let tests = "\
running 24 tests
test fold::a ... ok
test fold::b ... ok
test fold::c ... FAILED
";
        assert_eq!(
            parse_command_progress(tests),
            Some(CommandProgress { done: 3, total: 24 })
        );
        assert_eq!(parse_command_progress("see section 1/2 of the note"), None);
        assert!(is_long_command("cargo test -p wizard"));
        assert!(!is_long_command("printf hello"));
    }

    #[test]
    fn tokens_read_as_thousands() {
        assert_eq!(fmt_tokens(182_000), "182k");
        assert_eq!(fmt_tokens(41_000), "41k");
        assert_eq!(fmt_tokens(12), "12");
    }

    #[test]
    fn thinking_cycles_move_and_the_dashboard_stays_one_cell() {
        assert!(CYCLES.len() > CORE);
        assert_eq!(thinking_count(), CYCLES.len());
        // The original deck is still the prefix, and each of those stays one
        // cell, so a session row dealt from it cannot widen.
        for ix in 0..CORE {
            let frame = thinking_frame(ix, 0, 0);
            assert_eq!(frame.chars().count(), 1, "{ix}: {frame}");
            assert_eq!(unicode_width::UnicodeWidthStr::width(frame.as_str()), 1, "{frame}");
            assert_ne!(thinking_frame(ix, 0, 0), thinking_frame(ix, 0, 1), "{ix}");
        }
        // A cycle past the deck still moves, and a frozen tick repeats.
        assert_ne!(thinking_frame(CORE, 0, 0), thinking_frame(CORE, 0, 1));
        assert_eq!(thinking_frame(0, 0, 0), thinking_frame(0, 0, 0));
        // The two-row marks have a second line; the one-cell deck does not.
        assert!(thinking_extra(CYCLES.len() - 1, 0, 1).len() == 1);
        assert!(thinking_extra(0, 0, 1).is_empty());
        // A dashboard deal stays inside the one-cell prefix.
        assert!(session_cycle("session-a") < CORE);
        assert!(session_cycle("session-b") < CORE);
    }
}
