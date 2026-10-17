//! Finished tool calls as jcode shows them: one `✓ name summary · N tok` row, and under an
//! edit the changed lines in a `┌─ diff` gutter. A running call is not a transcript row; the
//! status line names it.

use agent_core::{FileDiff, ToolCall, ToolKind, ToolStatus};
use ratatui::style::Style;
use ratatui::text::Span;
use serde_json::Value;
use similar::{ChangeTag, TextDiff};
use unicode_width::UnicodeWidthStr;

use super::line::{spans_width, truncate_spans, Hang, RLine};
use super::markdown::Highlighter;
use crate::theme;

/// How edit diffs are shown; `/diff` cycles it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DiffMode {
    Off,
    #[default]
    Inline,
    FullInline,
}

impl DiffMode {
    pub fn label(self) -> &'static str {
        match self {
            DiffMode::Off => "OFF",
            DiffMode::Inline => "Inline",
            DiffMode::FullInline => "Inline Full",
        }
    }

    pub fn next(self) -> Self {
        match self {
            DiffMode::Off => DiffMode::Inline,
            DiffMode::Inline => DiffMode::FullInline,
            DiffMode::FullInline => DiffMode::Off,
        }
    }
}

const MAX_INLINE_DIFF_LINES: usize = 12;

/// The name jcode's own tool of the same job has; anything else keeps wizard's name.
pub fn display_name(call: &ToolCall) -> String {
    match call.name.as_str() {
        "execute" | "bash" | "shell" | "shell_exec" | "Bash" => "bash",
        "read_file" | "file_read" | "Read" => "read",
        "write_file" | "file_write" | "Write" => "write",
        "edit_file" | "file_edit" | "Edit" => "edit",
        "list_dir" | "list_files" => "ls",
        "search" | "file_grep" | "Grep" => "grep",
        "file_glob" | "Glob" => "glob",
        "web_search" | "WebSearch" => "websearch",
        "web_fetch" | "fetch" | "WebFetch" => "webfetch",
        "todo" | "todo_write" | "todo_read" | "todowrite" | "todoread" | "TodoWrite" => "todo",
        "task" | "task_runner" | "Task" => "subagent",
        other => return other.to_string(),
    }
    .to_string()
}

pub fn is_todo(call: &ToolCall) -> bool {
    display_name(call) == "todo"
}

fn s<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| v.get(k).and_then(Value::as_str))
}

fn truncate_end(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_string();
    }
    let mut out = String::new();
    for c in text.chars() {
        if out.width() + unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) + 1 > max {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

pub fn truncate_middle(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max || max < 3 {
        return truncate_end(text, max);
    }
    let rem = max - 1;
    let head = rem / 2 + rem % 2;
    let tail = rem / 2;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(&chars[chars.len() - tail..]);
    out
}

/// `first second … last`, falling back to cutting the middle.
fn truncate_command(cmd: &str, max: usize) -> String {
    let one_line = cmd.split('\n').next().unwrap_or("");
    let cmd = if one_line.len() < cmd.trim_end().len() {
        format!("{one_line} …")
    } else {
        one_line.to_string()
    };
    if cmd.width() <= max {
        return cmd;
    }
    let t: Vec<&str> = cmd.split_whitespace().collect();
    if t.len() >= 4 {
        let n = t.len();
        for cand in [
            format!("{} {} … {} {}", t[0], t[1], t[n - 2], t[n - 1]),
            format!("{} {} … {}", t[0], t[1], t[n - 1]),
            format!("{} … {}", t[0], t[n - 1]),
        ] {
            if cand.width() <= max {
                return cand;
            }
        }
    }
    truncate_middle(&cmd, max)
}

/// Keep the end of a path behind a `…/` marker.
fn truncate_path(path: &str, max: usize) -> String {
    if path.width() <= max {
        return path.to_string();
    }
    let marker = if path.starts_with("~/") {
        "~/…/"
    } else if path.starts_with("./") {
        "./…/"
    } else if path.starts_with('/') {
        "/…/"
    } else {
        "…/"
    };
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let mut tail = String::new();
    for p in parts.iter().rev() {
        let cand = if tail.is_empty() {
            (*p).to_string()
        } else {
            format!("{p}/{tail}")
        };
        if marker.width() + cand.width() > max {
            break;
        }
        tail = cand;
    }
    if tail.is_empty() {
        truncate_middle(path, max)
    } else {
        format!("{marker}{tail}")
    }
}

/// The technical summary after the name: the command, the path, the pattern.
pub fn summary(call: &ToolCall, name: &str, budget: usize) -> String {
    let i = &call.input;
    let title = call.title.trim();
    match name {
        "bash" => {
            let cmd = s(i, &["command", "cmd"]).unwrap_or(title);
            if cmd.is_empty() {
                String::new()
            } else {
                format!("$ {}", truncate_command(cmd, budget.saturating_sub(2)))
            }
        }
        "read" => {
            let path = s(i, &["path", "file_path", "file"]).unwrap_or(title);
            let n = |k: &str| i.get(k).and_then(Value::as_u64);
            let range = match (
                n("start_line").or(n("start")),
                n("end_line").or(n("end")),
                n("offset"),
                n("limit"),
            ) {
                (Some(a), Some(b), _, _) => format!(":{a}-{b}"),
                (Some(a), None, _, _) => format!(":{a}-"),
                (None, Some(b), _, _) => format!(":1-{b}"),
                (None, None, Some(o), Some(l)) => format!(":{o}-{}", o + l),
                (None, None, Some(o), None) => format!(":{o}"),
                _ => String::new(),
            };
            format!(
                "{}{range}",
                truncate_path(path, budget.saturating_sub(range.width()))
            )
        }
        "write" | "edit" => {
            let path = s(i, &["path", "file_path", "file"]).unwrap_or(title);
            truncate_path(path, budget)
        }
        "glob" => s(i, &["pattern"]).map_or(title.to_string(), |p| {
            format!("'{}'", truncate_middle(p, 38))
        }),
        "grep" => match (s(i, &["pattern", "query"]), s(i, &["path"])) {
            (Some(p), Some(path)) => format!("'{p}' in {path}"),
            (Some(p), None) => format!("'{p}'"),
            _ => title.to_string(),
        },
        "ls" => s(i, &["path"]).unwrap_or(".").to_string(),
        "webfetch" => s(i, &["url"]).unwrap_or(title).to_string(),
        "websearch" => s(i, &["query"]).map_or(title.to_string(), |q| format!("'{q}'")),
        "todo" => match i.get("todos").and_then(Value::as_array) {
            Some(a) => format!("{} items", a.len()),
            None => "todos".to_string(),
        },
        "subagent" => format!(
            "{} ({})",
            s(i, &["description", "prompt"]).unwrap_or("task"),
            s(i, &["subagent_type", "agent"]).unwrap_or("agent")
        ),
        _ => s(i, &["action"]).unwrap_or(title).to_string(),
    }
}

/// `exit 3`, or `error: <first line>`.
fn error_summary(output: &str) -> String {
    let lower = output.to_lowercase();
    for key in [
        "exit code: ",
        "exit code ",
        "exit status: ",
        "exit status ",
        "exited with ",
    ] {
        if let Some(at) = lower.find(key) {
            let digits: String = lower[at + key.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if !digits.is_empty() && digits != "0" {
                return format!("exit {digits}");
            }
        }
    }
    let first = output
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("failed");
    let detail = first
        .strip_prefix("error:")
        .or_else(|| first.strip_prefix("Error:"))
        .unwrap_or(first)
        .trim();
    format!("error: {}", truncate_middle(detail, 80))
}

/// `87 tok`, `1.2k tok`, `14k tok`: a quarter of the output's bytes.
pub fn token_label(bytes: usize) -> String {
    let t = bytes / 4;
    match t {
        0..=999 => format!("{t} tok"),
        1000..=9999 => {
            if (t % 1000) / 100 == 0 {
                format!("{}k tok", t / 1000)
            } else {
                format!("{}.{}k tok", t / 1000, (t % 1000) / 100)
            }
        }
        _ => format!("{}k tok", t / 1000),
    }
}

struct Change {
    add: bool,
    text: String,
}

fn changes(diff: &FileDiff) -> Vec<Change> {
    let old = diff.old.as_deref().unwrap_or("");
    let d = TextDiff::from_lines(old, &diff.new);
    d.iter_all_changes()
        .filter_map(|c| {
            let add = match c.tag() {
                ChangeTag::Insert => true,
                ChangeTag::Delete => false,
                ChangeTag::Equal => return None,
            };
            let text = c.value().trim().to_string();
            (!text.is_empty()).then_some(Change { add, text })
        })
        .collect()
}

fn ext_of(path: &str) -> &str {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("txt")
}

fn diff_block(diff: &FileDiff, width: usize, mode: DiffMode, out: &mut Vec<RLine>) {
    let dim = Style::default().fg(theme::dim());
    let ch = changes(diff);
    let (adds, dels) = (
        ch.iter().filter(|c| c.add).count(),
        ch.iter().filter(|c| !c.add).count(),
    );
    out.push(RLine::styled(format!("┌─ diff · {}", diff.path), dim).hang(Hang::None));
    let truncated = mode != DiffMode::FullInline && ch.len() > MAX_INLINE_DIFF_LINES;
    let mut hl = Highlighter::new(ext_of(&diff.path));
    let mut row = |c: &Change, out: &mut Vec<RLine>| {
        let (sign, col) = if c.add {
            ("+", (100u16, 200u16, 100u16))
        } else {
            ("-", (200, 100, 100))
        };
        let sign_style = Style::default().fg(theme::rgb(col.0 as u8, col.1 as u8, col.2 as u8));
        let mut spans = vec![Span::styled("│ ", dim), Span::styled(sign, sign_style)];
        for (text, (r, g, b)) in hl.line(&c.text) {
            let mix = |a: u8, b: u16| ((a as u16 * 70 + b * 30) / 100) as u8;
            spans.push(Span::styled(
                text,
                Style::default().fg(theme::rgb(mix(r, col.0), mix(g, col.1), mix(b, col.2))),
            ));
        }
        if mode == DiffMode::FullInline {
            out.push(RLine::new(spans).hang(Hang::Spans(vec![Span::styled("│ ", dim)])));
        } else {
            let max = width.saturating_sub(1);
            if spans_width(&spans) > max {
                let mut cut = truncate_spans(&spans, max);
                if let Some(last) = cut.last_mut() {
                    *last = Span::styled("…", dim);
                }
                spans = cut;
            }
            out.push(RLine::new(spans).nowrap());
        }
    };
    if truncated {
        for c in &ch[..6] {
            row(c, out);
        }
        out.push(
            RLine::styled(
                format!(
                    "│ ... {} more changes ...",
                    ch.len() - MAX_INLINE_DIFF_LINES
                ),
                dim,
            )
            .nowrap(),
        );
        for c in &ch[ch.len() - 6..] {
            row(c, out);
        }
        out.push(RLine::styled(format!("└─ (+{adds} -{dels} total)"), dim));
    } else {
        for c in &ch {
            row(c, out);
        }
        out.push(RLine::styled("└─", dim));
    }
}

/// The rows of one finished call. `width` is the text width of the transcript.
pub fn render(call: &ToolCall, width: usize, mode: DiffMode, out: &mut Vec<RLine>) {
    let dim = Style::default().fg(theme::dim());
    let tool = Style::default().fg(theme::tool());
    let failed = call.status == ToolStatus::Failed;
    let name = display_name(call);
    let output = call.output.as_deref().unwrap_or("");
    let diff = call
        .diff
        .as_ref()
        .filter(|_| mode != DiffMode::Off && !failed);
    let (icon, icon_color) = if failed {
        ("✗", theme::rgb(220, 100, 100))
    } else {
        ("✓", theme::rgb(100, 180, 100))
    };
    let row_width = width.saturating_sub(1);
    let mut sum = if failed {
        error_summary(output)
    } else if diff.is_some() {
        String::new()
    } else {
        summary(
            call,
            &name,
            row_width.saturating_sub(name.width() + 14).max(20),
        )
    };
    if call.kind == ToolKind::Think && sum.is_empty() {
        sum = call.title.clone();
    }
    let mut spans = vec![
        Span::styled(format!("  {icon} "), Style::default().fg(icon_color)),
        Span::styled(name.clone(), tool),
    ];
    if !sum.is_empty() {
        spans.push(Span::styled(format!(" {sum}"), dim));
    }
    if let Some(d) = call.diff.as_ref().filter(|_| !failed) {
        let ch = changes(d);
        let (a, r) = (
            ch.iter().filter(|c| c.add).count(),
            ch.iter().filter(|c| !c.add).count(),
        );
        if a > 0 || r > 0 {
            spans.push(Span::styled(" (", dim));
            spans.push(Span::styled(
                format!("+{a}"),
                Style::default().fg(theme::diff_add()),
            ));
            spans.push(Span::styled(" ", dim));
            spans.push(Span::styled(
                format!("-{r}"),
                Style::default().fg(theme::diff_del()),
            ));
            spans.push(Span::styled(")", dim));
        }
    }
    let bytes = output.len();
    let badge_color = match bytes / 4 {
        0..=3999 => theme::rgb(118, 118, 118),
        4000..=11999 => theme::rgb(214, 184, 92),
        _ => theme::rgb(224, 118, 118),
    };
    let suffix = vec![
        Span::styled(" · ", dim),
        Span::styled(token_label(bytes), Style::default().fg(badge_color)),
    ];
    let keep = row_width.saturating_sub(spans_width(&suffix));
    let mut row = truncate_spans(&spans, keep);
    row.extend(suffix);
    out.push(RLine::new(row).nowrap());

    if name == "bash" && !sum.starts_with('$') {
        let cmd = summary(call, &name, 82);
        if !cmd.is_empty() {
            out.push(RLine::styled(format!("    {cmd}"), dim).nowrap());
        }
    }
    if let Some(d) = diff {
        diff_block(d, width, mode, out);
    }
}

/// What the status line says about a running call: `write · ./notes.txt`.
pub fn running_detail(call: &ToolCall) -> (String, String) {
    let name = display_name(call);
    let detail = match name.as_str() {
        "bash" => summary(call, &name, 40),
        _ => summary(call, &name, 40),
    };
    (name, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, input: Value, output: &str) -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: name.into(),
            input,
            status: ToolStatus::Completed,
            output: Some(output.into()),
            ..Default::default()
        }
    }

    fn rows(c: &ToolCall) -> Vec<String> {
        let mut out = Vec::new();
        render(c, 100, DiffMode::Inline, &mut out);
        out.iter().map(RLine::text).collect()
    }

    #[test]
    fn a_command_row_names_the_command_and_the_output_size() {
        let c = call(
            "execute",
            serde_json::json!({"command": "ls -la"}),
            &"x".repeat(160),
        );
        assert_eq!(rows(&c), ["  ✓ bash $ ls -la · 40 tok"]);
    }

    #[test]
    fn a_failed_command_shows_the_exit_code_and_the_command_under_it() {
        let mut c = call(
            "execute",
            serde_json::json!({"command": "false"}),
            "boom\nExit code: 3",
        );
        c.status = ToolStatus::Failed;
        assert_eq!(rows(&c), ["  ✗ bash exit 3 · 4 tok", "    $ false"]);
    }

    #[test]
    fn an_edit_row_counts_lines_and_shows_only_the_changes() {
        let mut c = call("edit_file", serde_json::json!({"path": "a.txt"}), "ok");
        c.diff = Some(FileDiff {
            path: "a.txt".into(),
            old: Some("one\ntwo\nthree\n".into()),
            new: "one\nTWO\nthree\n".into(),
        });
        assert_eq!(
            rows(&c),
            [
                "  ✓ edit (+1 -1) · 0 tok",
                "┌─ diff · a.txt",
                "│ -two",
                "│ +TWO",
                "└─"
            ]
        );
    }

    #[test]
    fn long_diffs_keep_six_lines_at_each_end() {
        let new: String = (0..20).map(|i| format!("line {i}\n")).collect();
        let mut c = call("write_file", serde_json::json!({"path": "b.txt"}), "ok");
        c.diff = Some(FileDiff {
            path: "b.txt".into(),
            old: None,
            new,
        });
        let r = rows(&c);
        assert_eq!(r.len(), 1 + 1 + 6 + 1 + 6 + 1);
        assert_eq!(r[8], "│ ... 8 more changes ...");
        assert_eq!(r[15], "└─ (+20 -0 total)");
    }

    #[test]
    fn token_labels() {
        assert_eq!(token_label(348), "87 tok");
        assert_eq!(token_label(4000), "1k tok");
        assert_eq!(token_label(5000), "1.2k tok");
        assert_eq!(token_label(80_000), "20k tok");
    }

    #[test]
    fn paths_keep_their_tail() {
        assert_eq!(truncate_path("/a/b/c/d/file.rs", 12), "/…/d/file.rs");
        assert_eq!(truncate_middle("abcdefghij", 7), "abc…hij");
    }
}
