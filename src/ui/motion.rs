//! One indicator per kind of wait, and a thin bar where the numbers exist.
//!
//! The tick is about 100ms, which is the frame time for a tool or a thought.
//! Idle waits step more slowly. `WIZARD_REDUCED_MOTION` freezes each glyph on
//! its first frame. Nothing here paints a background.

use ratatui::text::{Line, Span};

use super::{accent, dim};
use crate::theme::{self, Token};

/// What the wait is. Each one has its own glyph.
#[derive(Clone, Copy)]
pub(super) enum Kind {
    /// The model is thinking, or text is arriving.
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

fn reduced() -> bool {
    matches!(
        std::env::var("WIZARD_REDUCED_MOTION").as_deref(),
        Ok("1") | Ok("true")
    )
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
}
