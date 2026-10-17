// OWNER: bottom-pane (bang shell mode)
//! The `!cmd` result: run the command through the shell and draw it like Codex's user-shell exec
//! cell (spec B.5.3, C.2.4): `• You ran ls`, the output dim under `└`, then a plain rule.
//!
//! The history-cells owner builds the full exec cell; this one is the small piece `!` needs now
//! and is meant to be replaced by that cell once it takes a user-shell source.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};

use super::HistoryCell;
use crate::style::palette;
use crate::wrap::{WrapOpts, word_wrap_line};

/// What running a command produced.
#[derive(Clone, Debug, PartialEq)]
pub struct ShellOutput {
    pub command: String,
    /// stdout and stderr, interleaved as the shell wrote them.
    pub output: String,
    pub success: bool,
}

/// Output kept per command; the rest of a runaway stream is dropped.
const MAX_OUTPUT_BYTES: usize = 1 << 20;
/// A command that outlives this is killed so a stuck `!cmd` cannot hold the terminal forever.
const TIMEOUT: Duration = Duration::from_secs(120);

/// Run `command` with `bash -c` in `cwd`, stdin closed, stderr folded into stdout.
pub fn run_shell(cwd: &Path, command: &str) -> ShellOutput {
    let shell = if Command::new("bash").arg("--version").output().is_ok() {
        "bash"
    } else {
        "sh"
    };
    let script = format!("exec 2>&1\n{command}");
    let child = Command::new(shell)
        .arg("-c")
        .arg(&script)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return ShellOutput {
                command: command.to_string(),
                output: format!("failed to start {shell}: {e}"),
                success: false,
            };
        }
    };
    let mut out = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(o) = out.as_mut() {
            let mut chunk = [0u8; 8192];
            while let Ok(n) = o.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                if buf.len() < MAX_OUTPUT_BYTES {
                    buf.extend_from_slice(&chunk[..n.min(MAX_OUTPUT_BYTES - buf.len())]);
                }
            }
        }
        buf
    });
    let started = std::time::Instant::now();
    let success = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st.success(),
            Ok(None) if started.elapsed() > TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => break false,
        }
    };
    let bytes = reader.join().unwrap_or_default();
    ShellOutput {
        command: command.to_string(),
        output: String::from_utf8_lossy(&bytes).into_owned(),
        success,
    }
}

// ---- highlighting -----------------------------------------------------------------------------

struct Theme {
    text: Color,
    command: Color,
    flag: Color,
    punct: Color,
    op: Color,
    string: Color,
    dollar: Color,
}

fn theme() -> Theme {
    let p = palette();
    let rgb = |c: (u8, u8, u8)| p.best_color(c);
    if p.light_bg() {
        Theme {
            text: rgb((76, 79, 105)),
            command: rgb((30, 102, 245)),
            flag: rgb((230, 69, 83)),
            punct: rgb((124, 127, 147)),
            op: rgb((23, 146, 153)),
            string: rgb((64, 160, 43)),
            dollar: rgb((210, 15, 57)),
        }
    } else {
        Theme {
            text: rgb((205, 214, 244)),
            command: rgb((137, 180, 250)),
            flag: rgb((235, 160, 172)),
            punct: rgb((147, 153, 178)),
            op: rgb((148, 226, 213)),
            string: rgb((166, 227, 161)),
            dollar: rgb((243, 139, 168)),
        }
    }
}

/// A small bash tokenizer in Catppuccin colours: command words, flags, operators and strings
/// (spec B.3.6). Anything else is plain text.
pub fn highlight_bash(line: &str) -> Vec<Span<'static>> {
    let t = theme();
    let fg = |c: Color| Style::default().fg(c);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    let mut expect_command = true;
    let push = |spans: &mut Vec<Span<'static>>, s: String, st: Style| {
        if !s.is_empty() {
            spans.push(Span::styled(s, st));
        }
    };
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            let start = i;
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            push(&mut spans, chars[start..i].iter().collect(), fg(t.text));
        } else if matches!(c, '"' | '\'') {
            let start = i;
            i += 1;
            while i < chars.len() && chars[i] != c {
                if chars[i] == '\\' && c == '"' {
                    i += 1;
                }
                i += 1;
            }
            i = (i + 1).min(chars.len());
            push(&mut spans, chars[start..i].iter().collect(), fg(t.string));
            expect_command = false;
        } else if matches!(c, '&' | '|' | ';') {
            let start = i;
            while i < chars.len() && matches!(chars[i], '&' | '|' | ';') {
                i += 1;
            }
            push(&mut spans, chars[start..i].iter().collect(), fg(t.op));
            expect_command = true;
        } else if c == '$' {
            push(&mut spans, "$".into(), fg(t.dollar));
            i += 1;
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            push(&mut spans, chars[start..i].iter().collect(), fg(t.text));
        } else {
            let start = i;
            while i < chars.len()
                && !chars[i].is_whitespace()
                && !matches!(chars[i], '"' | '\'' | '&' | '|' | ';' | '$')
            {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            if expect_command {
                push(&mut spans, word, fg(t.command));
                expect_command = false;
            } else if word.starts_with('-') && word.len() > 1 {
                let dashes = word.chars().take_while(|c| *c == '-').count();
                push(&mut spans, word[..dashes].to_string(), fg(t.punct));
                push(&mut spans, word[dashes..].to_string(), fg(t.flag));
            } else {
                push(&mut spans, word, fg(t.text));
            }
        }
    }
    spans
}

// ---- the cell ---------------------------------------------------------------------------------

/// Continuation rows of a long command before `… +N lines`.
const COMMAND_CONTINUATION_ROWS: usize = 2;
/// Output rows shown for a user shell command before the middle is cut.
const OUTPUT_MAX_ROWS: usize = 50;
const HEAD_TAIL_LINES: usize = 5;

#[derive(Clone, Debug)]
pub struct UserShellCell {
    pub result: ShellOutput,
}

fn dim_line(spans: Vec<Span<'static>>) -> Line<'static> {
    Line::from(spans)
}

/// Strip terminal escape sequences from command output; colour never reaches the cell.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            match it.peek() {
                Some('[') => {
                    it.next();
                    for n in it.by_ref() {
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    it.next();
                    while let Some(n) = it.next() {
                        if n == '\x07' {
                            break;
                        }
                        if n == '\x1b' {
                            it.next();
                            break;
                        }
                    }
                }
                _ => {
                    it.next();
                }
            }
        } else if c == '\r' {
            // A carriage return rewinds the line; keep what is written after it.
            if it.peek() != Some(&'\n') {
                out.push('\n');
            }
        } else if c == '\t' {
            out.push_str("    ");
        } else if !c.is_control() || c == '\n' {
            out.push(c);
        }
    }
    out
}

impl HistoryCell for UserShellCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let w = (width as usize).max(1);
        let bullet = Span::styled(
            "•",
            Style::default()
                .fg(if self.result.success {
                    Color::Green
                } else {
                    Color::Red
                })
                .add_modifier(Modifier::BOLD),
        );
        let mut out: Vec<Line<'static>> = Vec::new();

        // Header: `• You ran ` then the first command line; the rest continue under `  │ `.
        let header_prefix = Line::from(vec![
            bullet,
            Span::raw(" "),
            Span::styled("You ran", Style::default().bold()),
            Span::raw(" "),
        ]);
        let prefix_w = crate::wrap::line_width(&header_prefix);
        let cmd_lines: Vec<&str> = self.result.command.split('\n').collect();
        let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
        for (li, l) in cmd_lines.iter().enumerate() {
            let hl = Line::from(highlight_bash(l));
            let first_w = if li == 0 {
                w.saturating_sub(prefix_w).max(1)
            } else {
                w.saturating_sub(4).max(1)
            };
            let wrapped = wrap_with_first_width(&hl, first_w, w.saturating_sub(4).max(1));
            for r in wrapped {
                rows.push(r.spans);
            }
        }
        let mut cmd_rows = rows.into_iter();
        let first = cmd_rows.next().unwrap_or_default();
        let mut header = header_prefix.spans.clone();
        header.extend(first);
        out.push(Line::from(header));
        let rest: Vec<Vec<Span<'static>>> = cmd_rows.collect();
        let dim = Style::default().add_modifier(Modifier::DIM);
        for (i, r) in rest.iter().enumerate() {
            if i >= COMMAND_CONTINUATION_ROWS {
                let hidden = rest.len() - COMMAND_CONTINUATION_ROWS;
                out.push(dim_line(vec![Span::styled(
                    format!("  │ … +{hidden} lines"),
                    dim,
                )]));
                break;
            }
            let mut spans = vec![Span::styled("  │ ", dim)];
            spans.extend(r.clone());
            out.push(Line::from(spans));
        }

        // Output.
        let text = strip_ansi(&self.result.output);
        let text = text.trim_end_matches('\n');
        if text.is_empty() {
            out.push(Line::from(vec![
                Span::styled("  └ ", dim),
                Span::styled("(no output)", dim),
            ]));
        } else {
            let logical: Vec<&str> = text.split('\n').collect();
            let mut shown: Vec<String> = Vec::new();
            let mut omitted_marker = false;
            if logical.len() > HEAD_TAIL_LINES * 2 {
                shown.extend(logical[..HEAD_TAIL_LINES].iter().map(|s| s.to_string()));
                shown.push(format!(
                    "… +{} lines (ctrl + t to view transcript)",
                    logical.len() - HEAD_TAIL_LINES * 2
                ));
                omitted_marker = true;
                shown.extend(
                    logical[logical.len() - HEAD_TAIL_LINES..]
                        .iter()
                        .map(|s| s.to_string()),
                );
            } else {
                shown.extend(logical.iter().map(|s| s.to_string()));
            }
            let carried = if omitted_marker {
                logical.len() - HEAD_TAIL_LINES * 2
            } else {
                0
            };
            let mut body: Vec<Line<'static>> = Vec::new();
            for l in &shown {
                let line = Line::from(Span::styled(l.clone(), dim));
                let mut opts = WrapOpts::new(w.saturating_sub(4).max(1));
                opts.hyphen_split = false;
                body.extend(word_wrap_line(&line, &opts));
            }
            let body = truncate_middle(body, OUTPUT_MAX_ROWS, carried);
            for (i, l) in body.into_iter().enumerate() {
                let prefix = if i == 0 { "  └ " } else { "    " };
                let mut spans = vec![Span::styled(prefix, dim)];
                spans.extend(l.spans);
                out.push(Line::from(spans));
            }
        }
        out
    }
}

/// Wrap `line` so its first row is `first` wide and the rest are `rest` wide.
fn wrap_with_first_width(line: &Line<'static>, first: usize, rest: usize) -> Vec<Line<'static>> {
    let mut opts = WrapOpts::new(first);
    opts.hyphen_split = false;
    let wrapped = word_wrap_line(line, &opts);
    if first == rest || wrapped.len() <= 1 {
        return wrapped;
    }
    // Re-wrap everything after the first row at the wider width.
    let mut out = vec![wrapped[0].clone()];
    let mut tail_spans: Vec<Span<'static>> = Vec::new();
    for (i, l) in wrapped.iter().enumerate().skip(1) {
        if i > 1 {
            tail_spans.push(Span::raw(" "));
        }
        tail_spans.extend(l.spans.clone());
    }
    let mut opts = WrapOpts::new(rest);
    opts.hyphen_split = false;
    out.extend(word_wrap_line(&Line::from(tail_spans), &opts));
    out
}

/// Cut the middle of `lines` to `max` rows: half the rest from the top, half from the bottom and
/// an ellipsis row counting what went (`carried` is what was dropped before).
fn truncate_middle(lines: Vec<Line<'static>>, max: usize, carried: usize) -> Vec<Line<'static>> {
    if lines.len() <= max {
        return lines;
    }
    let ellipsis_rows = 1;
    let available = max.saturating_sub(ellipsis_rows);
    let head = available / 2;
    let tail = available - head;
    let hidden = lines.len() - head - tail;
    let n = hidden + carried.saturating_sub(1);
    let mut out: Vec<Line<'static>> = lines[..head].to_vec();
    out.push(Line::from(Span::styled(
        format!("… +{n} lines (ctrl + t to view transcript)"),
        Style::default().add_modifier(Modifier::DIM),
    )));
    out.extend_from_slice(&lines[lines.len() - tail..]);
    out
}

/// A full-width dim rule: the separator Codex prints after a finished command.
#[derive(Clone, Debug)]
pub struct RuleCell;

impl HistoryCell for RuleCell {
    fn raw_lines(&self) -> Vec<Line<'static>> {
        Vec::new()
    }

    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        vec![Line::from("─".repeat(width as usize)).dim()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::{ColorLevel, Palette, set_palette};

    fn pal() {
        set_palette(Palette::new(
            Some((230, 230, 230)),
            Some((0, 0, 0)),
            ColorLevel::TrueColor,
        ));
    }

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn cell(cmd: &str, out: &str, ok: bool) -> UserShellCell {
        pal();
        UserShellCell {
            result: ShellOutput {
                command: cmd.into(),
                output: out.into(),
                success: ok,
            },
        }
    }

    #[test]
    fn matches_the_bang_ls_capture() {
        let c = cell(
            "ls",
            "Cargo.toml\nimg.png\nnotes.txt\nREADME.md\nsrc\n",
            true,
        );
        let lines = c.display_lines(120);
        assert_eq!(
            text(&lines),
            vec![
                "• You ran ls",
                "  └ Cargo.toml",
                "    img.png",
                "    notes.txt",
                "    README.md",
                "    src",
            ]
        );
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Green));
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            lines[0].spans[2]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        // `ls` is the command word in Catppuccin blue.
        assert_eq!(lines[0].spans[4].style.fg, Some(Color::Rgb(137, 180, 250)));
        assert!(lines[1].spans[1].style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn failure_is_a_red_bullet_and_empty_output_says_so() {
        let c = cell("false", "", false);
        let lines = c.display_lines(120);
        assert_eq!(text(&lines), vec!["• You ran false", "  └ (no output)"]);
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Red));
    }

    #[test]
    fn long_command_continues_under_a_branch_and_caps_at_two_rows() {
        let cmd = format!("echo {}", "word ".repeat(40));
        let c = cell(cmd.trim_end(), "", true);
        let t = text(&c.display_lines(30));
        assert!(t[0].starts_with("• You ran echo "));
        assert!(t[1].starts_with("  │ "));
        assert!(t[2].starts_with("  │ "));
        assert!(t[3].starts_with("  │ … +"), "{t:?}");
        assert_eq!(t.last().unwrap(), "  └ (no output)");
    }

    #[test]
    fn long_output_keeps_head_and_tail_with_the_ellipsis_row() {
        let out: String = (1..=300).map(|n| format!("{n}\n")).collect();
        let c = cell("seq 1 300", &out, true);
        let t = text(&c.display_lines(80));
        assert_eq!(t[1], "  └ 1");
        assert!(
            t.iter()
                .any(|l| l.starts_with("    … +") && l.ends_with("(ctrl + t to view transcript)"))
        );
        assert_eq!(t.last().unwrap(), "    300");
        assert!(t.len() <= 1 + OUTPUT_MAX_ROWS);
    }

    #[test]
    fn highlighter_colours_flags_operators_and_strings() {
        pal();
        let spans = highlight_bash("git log --oneline | head -3 && echo 'hi'");
        let fg = |needle: &str| {
            spans
                .iter()
                .find(|s| s.content == needle)
                .and_then(|s| s.style.fg)
        };
        assert_eq!(fg("git"), Some(Color::Rgb(137, 180, 250)));
        assert_eq!(fg("log"), Some(Color::Rgb(205, 214, 244)));
        assert_eq!(fg("oneline"), Some(Color::Rgb(235, 160, 172)));
        assert_eq!(fg("|"), Some(Color::Rgb(148, 226, 213)));
        assert_eq!(fg("head"), Some(Color::Rgb(137, 180, 250)));
        assert_eq!(fg("'hi'"), Some(Color::Rgb(166, 227, 161)));
        assert_eq!(fg("--"), Some(Color::Rgb(147, 153, 178)));
    }

    #[test]
    fn rule_is_dim_and_full_width() {
        let l = RuleCell.display_lines(30);
        assert_eq!(l[0].spans[0].content, "─".repeat(30));
    }

    #[test]
    fn run_shell_captures_both_streams_and_the_exit_status() {
        let dir = std::env::temp_dir();
        let r = run_shell(&dir, "echo out; echo err >&2; exit 3");
        assert!(!r.success);
        assert_eq!(r.output, "out\nerr\n");
        let r = run_shell(&dir, "true");
        assert!(r.success);
        assert_eq!(r.output, "");
    }

    #[test]
    fn ansi_and_carriage_returns_are_cleaned() {
        assert_eq!(
            strip_ansi("\x1b[31mred\x1b[0m\r\nnext\ta"),
            "red\nnext    a"
        );
    }
}
