// OWNER: history-cells
//! Exec cells (spec B.5): `Explored` groups of read, list and search calls and the `Ran` cell of
//! one command, ported from Codex's `exec_cell/{model,render}.rs`. Wizard hands a tool call over
//! whole, so there is no live output and no exit code: the caller maps a failed call to exit
//! code 1 and measures the duration itself.

use std::time::{Duration, Instant};

use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};

use super::HistoryCell;
use crate::highlight::highlight_bash_to_lines;
use crate::style::{ColorLevel, palette, shimmer_spans};
use crate::term::history_insert::wrap_history_line;
use crate::wrap::{WrapOpts, adaptive_wrap_line, line_width};

pub const TOOL_CALL_MAX_LINES: usize = 5;
const USER_SHELL_TOOL_CALL_MAX_LINES: usize = 50;
const TRANSCRIPT_HINT: &str = "ctrl + t to view transcript";
const COMMAND_CONTINUATION_MAX_LINES: usize = 2;

/// What a command does, as far as the `Explored` grouping cares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecKind {
    Read {
        name: String,
    },
    List {
        path: Option<String>,
    },
    Search {
        query: Option<String>,
        path: Option<String>,
    },
    Run,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecOutput {
    pub text: String,
    /// Wizard has no exit code: 0 for a completed call, 1 for a failed one.
    pub exit_code: i32,
}

#[derive(Clone, Debug)]
pub struct ExecCall {
    pub id: String,
    /// The script as shown after `Ran`.
    pub command: String,
    pub kind: ExecKind,
    pub output: Option<ExecOutput>,
    pub started: Instant,
    pub duration: Option<Duration>,
    pub user_shell: bool,
}

impl ExecCall {
    pub fn new(id: impl Into<String>, command: impl Into<String>) -> Self {
        let command = command.into();
        let kind = classify_command(&command);
        ExecCall {
            id: id.into(),
            command,
            kind,
            output: None,
            started: Instant::now(),
            duration: None,
            user_shell: false,
        }
    }

    fn is_exploring(&self) -> bool {
        !self.user_shell && self.kind != ExecKind::Run
    }
}

#[derive(Clone, Debug)]
pub struct ExecCell {
    pub calls: Vec<ExecCall>,
    /// Off gives the static dim bullet instead of the shimmer (tests, reduced motion).
    pub animations: bool,
}

impl ExecCell {
    pub fn new(call: ExecCall) -> Self {
        ExecCell {
            calls: vec![call],
            animations: true,
        }
    }

    /// Adds a call to an `Explored` group. Returns false, leaving the cell alone, when either
    /// side is not an explore call: the caller then starts a new cell.
    pub fn push_call(&mut self, call: ExecCall) -> bool {
        if self.is_exploring() && call.is_exploring() {
            self.calls.push(call);
            true
        } else {
            false
        }
    }

    /// Marks the latest call with this id finished. False means no such call.
    pub fn complete_call(&mut self, id: &str, output: ExecOutput, duration: Duration) -> bool {
        let Some(call) = self.calls.iter_mut().rev().find(|c| c.id == id) else {
            return false;
        };
        call.output = Some(output);
        call.duration = Some(duration);
        true
    }

    /// An interrupted turn: every unfinished call counts as failed.
    pub fn mark_failed(&mut self) {
        for call in &mut self.calls {
            if call.duration.is_none() {
                call.duration = Some(call.started.elapsed());
                call.output
                    .get_or_insert_with(ExecOutput::default)
                    .exit_code = 1;
            }
        }
    }

    pub fn is_exploring(&self) -> bool {
        self.calls.iter().all(ExecCall::is_exploring)
    }

    pub fn is_active(&self) -> bool {
        self.calls.iter().any(|c| c.duration.is_none())
    }

    /// A finished command cell can go to scrollback at once; an explore group stays open for more calls.
    pub fn should_flush(&self) -> bool {
        !self.is_exploring() && !self.is_active()
    }

    fn active_start(&self) -> Option<Instant> {
        self.calls
            .iter()
            .find(|c| c.duration.is_none())
            .map(|c| c.started)
    }
}

// ---- shared helpers ---------------------------------------------------------------------------

/// `1.50s`, `12ms`, `1m 05s`.
pub fn format_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.2}s", ms as f64 / 1000.0)
    } else {
        let secs = d.as_secs();
        format!("{}m {:02}s", secs / 60, secs % 60)
    }
}

fn activity_marker(start: Option<Instant>, animations: bool) -> Span<'static> {
    if !animations {
        return "•".dim();
    }
    if palette().level == ColorLevel::TrueColor {
        shimmer_spans("•")
            .into_iter()
            .next()
            .unwrap_or_else(|| "•".into())
    } else {
        let ms = start.map(|s| s.elapsed().as_millis()).unwrap_or(0);
        if (ms / 600).is_multiple_of(2) {
            "•".into()
        } else {
            "◦".dim()
        }
    }
}

fn plain_span(text: &str) -> Span<'static> {
    Span::from(text.to_string())
}

/// First line gets `first`, the rest `rest`.
fn prefix_lines(
    lines: Vec<Line<'static>>,
    first: Span<'static>,
    rest: Span<'static>,
) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .enumerate()
        .map(|(i, mut l)| {
            l.spans
                .insert(0, if i == 0 { first.clone() } else { rest.clone() });
            l
        })
        .collect()
}

fn no_hyphen(width: usize) -> WrapOpts {
    let mut o = WrapOpts::new(width).uax14(true);
    o.hyphen_split = false;
    o
}

fn dimmed(mut line: Line<'static>) -> Line<'static> {
    for s in &mut line.spans {
        s.style = s.style.add_modifier(Modifier::DIM);
    }
    line
}

/// One raw output line with its SGR sequences applied.
fn ansi_line(raw: &str) -> Line<'static> {
    tuikit::ansi::to_lines(raw, Style::default())
        .into_iter()
        .next()
        .unwrap_or_default()
}

fn output_ellipsis_text(omitted: usize) -> String {
    format!("… +{omitted} lines ({TRANSCRIPT_HINT})")
}

fn output_ellipsis_line(omitted: usize, prefix: Option<&Line<'static>>) -> Line<'static> {
    let mut line = prefix.cloned().unwrap_or_default();
    line.spans.push(output_ellipsis_text(omitted).dim());
    line
}

/// Rows a line takes once the history inserter wraps it at `width`.
fn row_count(line: &Line<'static>, width: u16) -> usize {
    let width = width.max(1) as usize;
    let blank = line
        .spans
        .iter()
        .all(|s| s.content.chars().all(char::is_whitespace));
    if blank {
        return line_width(line).div_ceil(width).max(1);
    }
    wrap_history_line(line, width).len().max(1)
}

struct OutputLines {
    lines: Vec<Line<'static>>,
    omitted: Option<usize>,
}

/// Up to `limit` head and `limit` tail lines of the output, dim, with the omitted count.
fn output_lines(output: &ExecOutput, limit: usize) -> OutputLines {
    let all: Vec<&str> = output.text.lines().collect();
    let total = all.len();
    let head_end = total.min(limit);
    let mut out: Vec<Line<'static>> = all[..head_end]
        .iter()
        .map(|raw| dimmed(ansi_line(raw)))
        .collect();
    let tail_len = (total - head_end).min(limit);
    let omitted = total - head_end - tail_len;
    let omitted = (omitted > 0).then_some(omitted);
    if let Some(n) = omitted {
        out.push(output_ellipsis_line(n, None));
    }
    for raw in &all[total - tail_len..] {
        out.push(dimmed(ansi_line(raw)));
    }
    OutputLines {
        lines: out,
        omitted,
    }
}

/// Keeps `max_rows` viewport rows: a head, an ellipsis row, a tail.
fn truncate_lines_middle(
    lines: &[Line<'static>],
    max_rows: usize,
    width: u16,
    omitted_hint: Option<usize>,
    ellipsis_prefix: Option<&Line<'static>>,
) -> Vec<Line<'static>> {
    let width = width.max(1);
    if max_rows == 0 {
        return Vec::new();
    }
    let rows: Vec<usize> = lines.iter().map(|l| row_count(l, width)).collect();
    if rows.iter().sum::<usize>() <= max_rows {
        return lines.to_vec();
    }
    let estimated = omitted_hint.unwrap_or(0)
        + lines
            .len()
            .saturating_sub(usize::from(omitted_hint.is_some()));
    let ellipsis_rows = row_count(&output_ellipsis_line(estimated, ellipsis_prefix), width);
    if ellipsis_rows >= max_rows {
        return vec![output_ellipsis_line(estimated, ellipsis_prefix)];
    }
    let available = max_rows - ellipsis_rows;
    let head_budget = available / 2;
    let tail_budget = available - head_budget;

    let mut head: Vec<Line<'static>> = Vec::new();
    let mut used = 0;
    let mut head_end = 0;
    while head_end < lines.len() && used + rows[head_end] <= head_budget {
        used += rows[head_end];
        head.push(lines[head_end].clone());
        head_end += 1;
    }
    let mut tail_rev: Vec<Line<'static>> = Vec::new();
    let mut used = 0;
    let mut tail_start = lines.len();
    while tail_start > head_end && used + rows[tail_start - 1] <= tail_budget {
        used += rows[tail_start - 1];
        tail_rev.push(lines[tail_start - 1].clone());
        tail_start -= 1;
    }
    let base = omitted_hint.unwrap_or(0);
    let additional = lines
        .len()
        .saturating_sub(head.len() + tail_rev.len())
        .saturating_sub(usize::from(omitted_hint.is_some()));
    head.push(output_ellipsis_line(base + additional, ellipsis_prefix));
    head.extend(tail_rev.into_iter().rev());
    head
}

fn limit_lines_from_start(lines: &[Line<'static>], keep: usize) -> Vec<Line<'static>> {
    let ellipsis = |n: usize| Line::from(vec![format!("… +{n} lines").dim()]);
    if lines.len() <= keep {
        return lines.to_vec();
    }
    if keep == 0 {
        return vec![ellipsis(lines.len())];
    }
    let mut out = lines[..keep].to_vec();
    out.push(ellipsis(lines.len() - keep));
    out
}

// ---- rendering --------------------------------------------------------------------------------

impl HistoryCell for ExecCell {
    fn raw_lines(&self) -> Vec<Line<'static>> {
        crate::ui::plain_lines(self.transcript_lines(crate::ui::RAW_WIDTH))
    }

    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if self.is_exploring() {
            self.exploring_lines(width)
        } else {
            self.command_lines(width)
        }
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();
        for (i, call) in self.calls.iter().enumerate() {
            if i > 0 {
                lines.push(Line::default());
            }
            let script = highlight_bash_to_lines(&call.command);
            let opts = WrapOpts::new(width as usize)
                .uax14(true)
                .initial_indent(Line::from("$ ".magenta()))
                .subsequent_indent(Line::from("    "));
            for l in &script {
                lines.extend(adaptive_wrap_line(l, &opts));
            }
            let Some(output) = &call.output else { continue };
            let opts = WrapOpts::new(width.max(1) as usize).uax14(true);
            for raw in output.text.lines() {
                lines.extend(adaptive_wrap_line(&ansi_line(raw), &opts));
            }
            if let Some(d) = call.duration {
                let mut result = if output.exit_code == 0 {
                    Line::from("✓".green().bold())
                } else {
                    Line::from(vec![
                        "✗".red().bold(),
                        format!(" ({})", output.exit_code).into(),
                    ])
                };
                result
                    .spans
                    .push(format!(" • {}", format_duration(d)).dim());
                lines.push(result);
            }
        }
        lines
    }
}

impl ExecCell {
    fn exploring_lines(&self, width: u16) -> Vec<Line<'static>> {
        let active = self.is_active();
        let mut out = vec![Line::from(vec![
            if active {
                activity_marker(self.active_start(), self.animations)
            } else {
                "•".dim()
            },
            " ".into(),
            if active {
                "Exploring".bold()
            } else {
                "Explored".bold()
            },
        ])];

        let is_read = |c: &ExecCall| matches!(c.kind, ExecKind::Read { .. });
        let mut body: Vec<Line<'static>> = Vec::new();
        let mut calls = self.calls.as_slice();
        while let Some((call, rest)) = calls.split_first() {
            let group_len = if is_read(call) {
                1 + rest.iter().take_while(|c| is_read(c)).count()
            } else {
                1
            };
            let (group, remaining) = calls.split_at(group_len);
            calls = remaining;

            let entry: (&str, Vec<Span<'static>>) = match &call.kind {
                ExecKind::Read { .. } => {
                    let mut names: Vec<&str> = Vec::new();
                    for c in group {
                        if let ExecKind::Read { name } = &c.kind {
                            if !names.contains(&name.as_str()) {
                                names.push(name);
                            }
                        }
                    }
                    let mut spans: Vec<Span<'static>> = Vec::new();
                    for (i, n) in names.iter().enumerate() {
                        if i > 0 {
                            spans.push(", ".dim());
                        }
                        spans.push(plain_span(n));
                    }
                    ("Read", spans)
                }
                ExecKind::List { path } => (
                    "List",
                    vec![plain_span(path.as_deref().unwrap_or(&call.command))],
                ),
                ExecKind::Search { query, path } => {
                    let spans = match (query, path) {
                        (Some(q), Some(p)) => vec![plain_span(q), " in ".dim(), plain_span(p)],
                        (Some(q), None) => vec![plain_span(q)],
                        _ => vec![plain_span(&call.command)],
                    };
                    ("Search", spans)
                }
                ExecKind::Run => ("Run", vec![plain_span(&call.command)]),
            };
            let (title, spans) = entry;
            let initial = Line::from(vec![title.cyan(), " ".into()]);
            let subsequent = Line::from(" ".repeat(line_width(&initial)));
            let opts = WrapOpts::new(width as usize)
                .uax14(true)
                .initial_indent(initial)
                .subsequent_indent(subsequent);
            body.extend(adaptive_wrap_line(&Line::from(spans), &opts));
        }
        out.extend(prefix_lines(body, "  └ ".dim(), plain_span("    ")));
        out
    }

    fn command_lines(&self, width: u16) -> Vec<Line<'static>> {
        // A command cell holds one call; if a caller packs more, show the first.
        let Some(call) = self.calls.first() else {
            return Vec::new();
        };
        let bullet = match call.output.as_ref().filter(|_| call.duration.is_some()) {
            Some(o) if o.exit_code == 0 => "•".green().bold(),
            Some(_) => "•".red().bold(),
            None => activity_marker(Some(call.started), self.animations),
        };
        let title = if call.duration.is_none() {
            "Running"
        } else if call.user_shell {
            "You ran"
        } else {
            "Ran"
        };
        let mut header = Line::from(vec![bullet, " ".into(), title.bold(), " ".into()]);
        let header_prefix = line_width(&header);

        let highlighted = highlight_bash_to_lines(&call.command);
        // Continuation rows sit behind `  │ `.
        let cont_width = (width as usize).saturating_sub(4).max(1);
        let mut continuation: Vec<Line<'static>> = Vec::new();
        if let Some((first, rest)) = highlighted.split_first() {
            let avail = (width as usize).saturating_sub(header_prefix).max(1);
            let mut wrapped = adaptive_wrap_line(first, &no_hyphen(avail)).into_iter();
            if let Some(seg) = wrapped.next() {
                header.spans.extend(seg.spans);
            }
            continuation.extend(wrapped);
            for l in rest {
                continuation.extend(adaptive_wrap_line(l, &no_hyphen(cont_width)));
            }
        }
        let mut lines = vec![header];
        let continuation = limit_lines_from_start(&continuation, COMMAND_CONTINUATION_MAX_LINES);
        if !continuation.is_empty() {
            lines.extend(prefix_lines(continuation, "  │ ".dim(), "  │ ".dim()));
        }

        let Some(output) = &call.output else {
            return lines;
        };
        let limit = if call.user_shell {
            USER_SHELL_TOOL_CALL_MAX_LINES
        } else {
            TOOL_CALL_MAX_LINES
        };
        let raw = output_lines(output, limit);
        if raw.lines.is_empty() {
            lines.extend(prefix_lines(
                vec![Line::from("(no output)".dim())],
                "  └ ".dim(),
                plain_span("    "),
            ));
            return lines;
        }
        // Wrap first so the cap counts screen rows, not logical lines.
        let out_width = (width as usize).saturating_sub(4).max(1);
        let mut wrapped: Vec<Line<'static>> = Vec::new();
        for l in &raw.lines {
            wrapped.extend(adaptive_wrap_line(l, &no_hyphen(out_width)));
        }
        let prefixed = prefix_lines(wrapped, "  └ ".dim(), plain_span("    "));
        let ellipsis_prefix = Line::from(vec!["    ".dim()]);
        lines.extend(truncate_lines_middle(
            &prefixed,
            limit,
            width,
            raw.omitted,
            Some(&ellipsis_prefix),
        ));
        lines
    }
}

// ---- command classification -------------------------------------------------------------------

/// Split a script into words the way a POSIX shell would for plain quoting. `None` when
/// the quoting does not close.
fn shell_words(s: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                have = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        ch => cur.push(ch),
                    }
                }
            }
            '"' => {
                have = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            n @ ('"' | '\\' | '$' | '`') => cur.push(n),
                            n => {
                                cur.push('\\');
                                cur.push(n);
                            }
                        },
                        ch => cur.push(ch),
                    }
                }
            }
            '\\' => {
                have = true;
                cur.push(chars.next()?);
            }
            c if c.is_whitespace() => {
                if have {
                    words.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            c => {
                have = true;
                cur.push(c);
            }
        }
    }
    if have {
        words.push(cur);
    }
    Some(words)
}

/// Segments of a script split at `&&`, `||`, `;` and `|`, with a flag for "piped from the
/// previous segment". Quoted text never splits. `None` for scripts with substitutions,
/// redirections or subshells, which are not read-only explores.
fn split_segments(script: &str) -> Option<Vec<(String, bool)>> {
    let mut segs: Vec<(String, bool)> = Vec::new();
    let mut cur = String::new();
    let mut piped = false;
    let mut quote: Option<char> = None;
    let mut chars = script.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                quote = None;
            } else if c == '\\' && q == '"' {
                cur.push(chars.next()?);
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                cur.push(c);
            }
            '\\' => {
                cur.push(c);
                cur.push(chars.next()?);
            }
            '$' | '`' | '>' | '<' | '(' | ')' | '{' | '}' | '\n' => return None,
            '&' => {
                if chars.peek() == Some(&'&') {
                    chars.next();
                    segs.push((std::mem::take(&mut cur), piped));
                    piped = false;
                } else {
                    return None;
                }
            }
            ';' => {
                segs.push((std::mem::take(&mut cur), piped));
                piped = false;
            }
            '|' => {
                let or = chars.peek() == Some(&'|');
                if or {
                    chars.next();
                }
                segs.push((std::mem::take(&mut cur), piped));
                piped = !or;
            }
            c => cur.push(c),
        }
    }
    if quote.is_some() {
        return None;
    }
    segs.push((cur, piped));
    Some(
        segs.into_iter()
            .filter(|(s, _)| !s.trim().is_empty())
            .collect(),
    )
}

fn short_display_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let trimmed = normalized.trim_end_matches('/');
    trimmed
        .split('/')
        .rev()
        .find(|p| !p.is_empty() && !matches!(*p, "build" | "dist" | "node_modules" | "src"))
        .map(str::to_string)
        .unwrap_or_else(|| trimmed.to_string())
}

/// Positional operands: arguments that are not flags, skipping the value of any flag in
/// `with_vals`, and everything after `--`.
fn operands<'a>(args: &'a [String], with_vals: &[&str]) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut skip = false;
    for (i, a) in args.iter().enumerate() {
        if skip {
            skip = false;
            continue;
        }
        if a == "--" {
            out.extend(args[i + 1..].iter().map(String::as_str));
            break;
        }
        if a.starts_with("--") && a.contains('=') {
            continue;
        }
        if a.starts_with('-') && a.len() > 1 {
            if with_vals.contains(&a.as_str()) {
                skip = true;
            }
            continue;
        }
        out.push(a);
    }
    out
}

/// `head -n 50 file`, `sed -n 1,40p file`, `cat file`: the file that was read.
fn read_target(head: &str, args: &[String]) -> Option<String> {
    let ops = match head {
        "cat" | "more" | "less" | "nl" | "bat" | "batcat" => operands(args, &["-s", "--style"]),
        "head" | "tail" => operands(args, &["-n", "-c", "--lines", "--bytes"]),
        "sed" => {
            // `sed -n 1,40p file`: the script is the first operand.
            if !args.iter().any(|a| a == "-n") {
                return None;
            }
            let ops = operands(args, &["-e", "-f"]);
            if ops.len() != 2 {
                return None;
            }
            return Some(ops[1].to_string());
        }
        _ => return None,
    };
    (ops.len() == 1).then(|| ops[0].to_string())
}

fn parse_search(head: &str, args: &[String]) -> ExecKind {
    let with_vals: &[&str] = match head {
        "rg" | "rga" => &[
            "-g",
            "--glob",
            "--iglob",
            "-t",
            "--type",
            "--type-add",
            "--type-not",
            "-m",
            "--max-count",
            "-A",
            "-B",
            "-C",
            "--context",
            "--max-depth",
        ],
        "ag" | "ack" | "pt" => &[
            "-G",
            "-g",
            "--file-search-regex",
            "--ignore-dir",
            "--ignore-file",
            "--path-to-ignore",
        ],
        _ => &[
            "-e",
            "-f",
            "-m",
            "-A",
            "-B",
            "-C",
            "--include",
            "--exclude",
            "--exclude-dir",
            "--max-count",
            "--context",
        ],
    };
    if matches!(head, "rg" | "rga") && args.iter().any(|a| a == "--files") {
        let ops = operands(args, with_vals);
        return ExecKind::List {
            path: ops.first().map(|p| short_display_path(p)),
        };
    }
    let ops = operands(args, with_vals);
    ExecKind::Search {
        query: ops.first().map(|s| s.to_string()),
        path: ops.get(1).map(|p| short_display_path(p)),
    }
}

fn classify_words(words: &[String]) -> ExecKind {
    let Some((head, args)) = words.split_first() else {
        return ExecKind::Run;
    };
    match head.as_str() {
        "ls" | "eza" | "exa" | "tree" | "du" => {
            let with_vals: &[&str] = match head.as_str() {
                "ls" => &[
                    "-I",
                    "-w",
                    "--block-size",
                    "--format",
                    "--time-style",
                    "--color",
                    "--quoting-style",
                ],
                "tree" => &["-L", "-P", "-I", "--charset", "--filelimit", "--sort"],
                "du" => &["-d", "--max-depth", "-B", "--block-size", "--exclude"],
                _ => &[
                    "-I",
                    "--ignore-glob",
                    "--color",
                    "--sort",
                    "--time-style",
                    "--time",
                ],
            };
            ExecKind::List {
                path: operands(args, with_vals)
                    .first()
                    .map(|p| short_display_path(p)),
            }
        }
        "rg" | "rga" | "ag" | "ack" | "pt" | "grep" | "egrep" | "fgrep" => parse_search(head, args),
        "git" => match args.split_first() {
            Some((sub, rest)) if sub == "grep" => parse_search("grep", rest),
            Some((sub, rest)) if sub == "ls-files" => ExecKind::List {
                path: operands(
                    rest,
                    &["--exclude", "--exclude-from", "--pathspec-from-file"],
                )
                .first()
                .map(|p| short_display_path(p)),
            },
            _ => ExecKind::Run,
        },
        "find" | "fd" => {
            let ops = operands(
                args,
                &[
                    "-name",
                    "-iname",
                    "-path",
                    "-type",
                    "-maxdepth",
                    "-mindepth",
                    "-t",
                    "-e",
                    "-d",
                    "-E",
                ],
            );
            let query = if head == "fd" {
                ops.first().map(|s| s.to_string())
            } else {
                args.iter()
                    .position(|a| a == "-name" || a == "-iname" || a == "-path")
                    .and_then(|i| args.get(i + 1))
                    .cloned()
            };
            let path = if head == "fd" {
                ops.get(1).map(|p| short_display_path(p))
            } else {
                ops.first().map(|p| short_display_path(p))
            };
            if query.is_some() {
                ExecKind::Search { query, path }
            } else {
                ExecKind::List { path }
            }
        }
        "cat" | "more" | "less" | "nl" | "bat" | "batcat" | "head" | "tail" | "sed" => {
            match read_target(head, args) {
                Some(p) => ExecKind::Read {
                    name: short_display_path(&p),
                },
                None => ExecKind::Run,
            }
        }
        _ => ExecKind::Run,
    }
}

/// Commands that only reshape what the previous pipe stage printed.
fn is_pipe_filter(words: &[String]) -> bool {
    matches!(
        words.first().map(String::as_str),
        Some("head" | "tail" | "wc" | "sort" | "uniq" | "cut" | "tr" | "nl" | "column" | "xargs")
    ) || (words.first().map(String::as_str) == Some("sed") && words.iter().any(|w| w == "-n"))
}

/// A simplified `parse_command` for the scripts wizard's `execute` tool runs: a script made only
/// of read, list and search commands is an explore call, anything else is `Run`. A leading
/// `cd dir &&` is ignored and trailing pipe filters (`| head -3`) do not count.
pub fn classify_command(script: &str) -> ExecKind {
    let Some(segments) = split_segments(script) else {
        return ExecKind::Run;
    };
    let mut found: Vec<ExecKind> = Vec::new();
    for (i, (seg, piped)) in segments.iter().enumerate() {
        let Some(words) = shell_words(seg) else {
            return ExecKind::Run;
        };
        if words.is_empty() {
            continue;
        }
        if words[0] == "cd" && i + 1 < segments.len() {
            continue;
        }
        if *piped && is_pipe_filter(&words) {
            continue;
        }
        match classify_words(&words) {
            ExecKind::Run => return ExecKind::Run,
            k => found.push(k),
        }
    }
    if found.len() == 1 {
        found.remove(0)
    } else {
        // Several explore commands in one script: Codex lists each, this API has one kind.
        ExecKind::Run
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::{ColorLevel, Palette, set_palette};
    use ratatui::style::Color;
    use std::path::PathBuf;

    fn dark() {
        set_palette(Palette::new(
            Some((230, 230, 230)),
            Some((0, 0, 0)),
            ColorLevel::TrueColor,
        ));
    }

    fn text(l: &Line<'_>) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn texts(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(text).collect()
    }

    fn cell(cmd: &str, out: Option<(i32, &str)>) -> ExecCell {
        let mut c = ExecCell::new(ExecCall::new("c1", cmd));
        c.animations = false;
        if let Some((code, text)) = out {
            c.complete_call(
                "c1",
                ExecOutput {
                    text: text.into(),
                    exit_code: code,
                },
                Duration::from_millis(1),
            );
        }
        c
    }

    fn done(cmd: &str) -> ExecCell {
        cell(cmd, Some((0, "")))
    }

    fn render(c: &ExecCell, w: u16) -> String {
        dark();
        texts(&c.display_lines(w)).join("\n")
    }

    // Snapshots of Codex's history_cell/tests.rs.

    #[test]
    fn single_line_command_compact_when_fits() {
        assert_eq!(
            render(&done("echo ok"), 80),
            "• Ran echo ok\n  └ (no output)"
        );
    }

    #[test]
    fn single_line_command_wraps_with_four_space_continuation() {
        assert_eq!(
            render(
                &done("a_very_long_token_without_spaces_to_force_wrapping"),
                24
            ),
            "• Ran a_very_long_token_\n  │ without_spaces_to_\n  │ force_wrapping\n  └ (no output)"
        );
    }

    #[test]
    fn multiline_command_without_wrap_uses_branch_then_eight_spaces() {
        assert_eq!(
            render(&done("echo one\necho two"), 80),
            "• Ran echo one\n  │ echo two\n  └ (no output)"
        );
    }

    #[test]
    fn multiline_command_wraps_with_extra_indent_on_subsequent_lines() {
        assert_eq!(
            render(
                &done("set -o pipefail\ncargo test -p codex-tui --quiet"),
                28
            ),
            "• Ran set -o pipefail\n  │ cargo test -p codex-tui\n  │ --quiet\n  └ (no output)"
        );
    }

    #[test]
    fn multiline_command_both_lines_wrap_with_correct_prefixes() {
        assert_eq!(
            render(
                &done(
                    "first_token_is_long_enough_to_wrap\nsecond_token_is_also_long_enough_to_wrap"
                ),
                28
            ),
            "• Ran first_token_is_long_en\n  │ ough_to_wrap\n  │ second_token_is_also_lon\n  │ … +1 lines\n  └ (no output)"
        );
    }

    #[test]
    fn stderr_tail_more_than_five_lines() {
        let out: String = (1..=10)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let c = cell("seq 1 10 1>&2 && false", Some((1, &out)));
        assert_eq!(
            render(&c, 80),
            "• Ran seq 1 10 1>&2 && false\n  └ 1\n    2\n    … +6 lines (ctrl + t to view transcript)\n    9\n    10"
        );
    }

    #[test]
    fn ran_cell_multiline_with_stderr() {
        let c = cell(
            "echo this_is_a_very_long_single_token_that_will_wrap_across_the_available_width",
            Some((
                1,
                "error: first line on stderr\nerror: second line on stderr",
            )),
        );
        assert_eq!(
            render(&c, 28),
            "• Ran echo\n  │ this_is_a_very_long_si\n  │ ngle_token_that_will_w\n  │ … +2 lines\n  └ error: first line on\n    stderr\n    error: second line on\n    stderr"
        );
    }

    #[test]
    fn coalesced_reads_dedupe_names() {
        dark();
        let mut c = ExecCell::new(ExecCall::new("a", "cat auth.rs"));
        for (i, f) in ["auth.rs", "shimmer.rs"].iter().enumerate() {
            assert!(c.push_call(ExecCall::new(format!("b{i}"), format!("cat {f}"))));
        }
        for id in ["a", "b0", "b1"] {
            c.complete_call(id, ExecOutput::default(), Duration::from_millis(1));
        }
        assert_eq!(
            texts(&c.display_lines(80)),
            vec!["• Explored", "  └ Read auth.rs, shimmer.rs"]
        );
    }

    #[test]
    fn explored_lists_each_search_and_merges_adjacent_reads() {
        dark();
        let mut c = ExecCell::new(ExecCall::new("1", "rg -n shimmer_spans"));
        assert!(c.push_call(ExecCall::new("2", "cat shimmer.rs")));
        assert!(c.push_call(ExecCall::new("3", "cat status_indicator_widget.rs")));
        for id in ["1", "2", "3"] {
            c.complete_call(id, ExecOutput::default(), Duration::from_millis(1));
        }
        assert_eq!(
            texts(&c.display_lines(80)),
            vec![
                "• Explored",
                "  └ Search shimmer_spans",
                "    Read shimmer.rs, status_indicator_widget.rs"
            ]
        );
    }

    #[test]
    fn push_call_refuses_a_command_run() {
        let mut c = ExecCell::new(ExecCall::new("1", "ls -la"));
        assert!(!c.push_call(ExecCall::new("2", "cargo test")));
        let mut run = ExecCell::new(ExecCall::new("1", "cargo test"));
        assert!(!run.push_call(ExecCall::new("2", "ls")));
    }

    #[test]
    fn running_cells_say_so() {
        let c = cell("sleep 5", None);
        assert_eq!(render(&c, 80), "• Running sleep 5");
        let e = {
            let mut e = ExecCell::new(ExecCall::new("1", "ls -la"));
            e.animations = false;
            e
        };
        assert_eq!(render(&e, 80), "• Exploring\n  └ List ls -la");
    }

    #[test]
    fn user_shell_cell_says_you_ran() {
        let mut c = cell("ls", Some((0, "a\nb")));
        c.calls[0].user_shell = true;
        assert_eq!(render(&c, 80), "• You ran ls\n  └ a\n    b");
    }

    #[test]
    fn user_shell_output_cap_is_fifty_rows() {
        let out: String = (1..=100).map(|n| format!("{n}\n")).collect();
        let mut c = cell("seq 1 100", Some((0, &out)));
        c.calls[0].user_shell = true;
        let rows = texts(&c.display_lines(120));
        assert_eq!(rows.len(), 1 + 50);
        assert_eq!(rows[1], "  └ 1");
        assert_eq!(rows[24], "    24");
        assert_eq!(rows[25], "    … +51 lines (ctrl + t to view transcript)");
        assert_eq!(rows[26], "    76");
        assert_eq!(rows[50], "    100");
    }

    #[test]
    fn styles_match_the_capture() {
        dark();
        let c = cell("echo hi", Some((0, "hi")));
        let rows = c.display_lines(80);
        assert_eq!(
            rows[0].spans[0].style,
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        );
        assert_eq!(
            rows[0].spans[2].style,
            Style::default().add_modifier(Modifier::BOLD)
        );
        assert_eq!(
            rows[1].spans[0].style,
            Style::default().add_modifier(Modifier::DIM)
        );
        assert_eq!(
            rows[1].spans[1].style,
            Style::default().add_modifier(Modifier::DIM)
        );
        let failed = cell("false", Some((1, "")));
        assert_eq!(
            failed.display_lines(80)[0].spans[0].style,
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        );
        let mut e = ExecCell::new(ExecCall::new("1", "ls -la"));
        e.complete_call("1", ExecOutput::default(), Duration::ZERO);
        let rows = e.display_lines(80);
        assert_eq!(
            rows[0].spans[0].style,
            Style::default().add_modifier(Modifier::DIM)
        );
        assert_eq!(rows[1].spans[1].style, Style::default().fg(Color::Cyan));
    }

    #[test]
    fn ansi_colour_in_output_survives_dimmed() {
        let c = cell("printf x", Some((0, "\x1b[31mred\x1b[0m ok")));
        let rows = c.display_lines(80);
        assert_eq!(
            rows[1].spans[1].style,
            Style::default().fg(Color::Red).add_modifier(Modifier::DIM)
        );
        assert_eq!(text(&rows[1]), "  └ red ok");
    }

    #[test]
    fn transcript_form() {
        dark();
        let c = cell("cat src/lib.rs", Some((0, "fn a() {}")));
        let rows = c.transcript_lines(80);
        assert_eq!(
            texts(&rows),
            vec!["$ cat src/lib.rs", "fn a() {}", "✓ • 1ms"]
        );
        assert_eq!(rows[0].spans[0].style, Style::default().fg(Color::Magenta));
        assert_eq!(
            rows[2].spans[0].style,
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        );
        let f = cell("false", Some((101, "")));
        assert_eq!(
            texts(&f.transcript_lines(80)),
            vec!["$ false", "✗ (101) • 1ms"]
        );
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(Duration::from_millis(0)), "0ms");
        assert_eq!(format_duration(Duration::from_millis(1070)), "1.07s");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1.50s");
        assert_eq!(format_duration(Duration::from_secs(65)), "1m 05s");
    }

    // classify_command

    fn kind(s: &str) -> ExecKind {
        classify_command(s)
    }

    #[test]
    fn classifies_explore_commands() {
        assert_eq!(kind("ls -la"), ExecKind::List { path: None });
        assert_eq!(
            kind("ls src"),
            ExecKind::List {
                path: Some("src".into())
            }
        );
        assert_eq!(
            kind("cat src/lib.rs"),
            ExecKind::Read {
                name: "lib.rs".into()
            }
        );
        assert_eq!(
            kind("rg -n add src"),
            ExecKind::Search {
                query: Some("add".into()),
                path: Some("src".into())
            }
        );
        assert_eq!(
            kind("grep -rn 'fn main' ."),
            ExecKind::Search {
                query: Some("fn main".into()),
                path: Some(".".into())
            }
        );
        assert_eq!(
            kind("sed -n 1,40p src/main.rs"),
            ExecKind::Read {
                name: "main.rs".into()
            }
        );
        assert_eq!(
            kind("head -n 20 README.md"),
            ExecKind::Read {
                name: "README.md".into()
            }
        );
        assert_eq!(
            kind("cd sub && cat a.rs"),
            ExecKind::Read {
                name: "a.rs".into()
            }
        );
        assert_eq!(
            kind("rg foo | head -3"),
            ExecKind::Search {
                query: Some("foo".into()),
                path: None
            }
        );
    }

    #[test]
    fn everything_else_is_a_run() {
        assert_eq!(kind("echo hello from the shell"), ExecKind::Run);
        assert_eq!(kind("cargo test"), ExecKind::Run);
        assert_eq!(kind("ls && rm -rf x"), ExecKind::Run);
        assert_eq!(kind("cat a > b"), ExecKind::Run);
        assert_eq!(kind("seq 1 120"), ExecKind::Run);
        assert_eq!(kind("sleep 2; echo late"), ExecKind::Run);
        assert_eq!(kind("git status --short"), ExecKind::Run);
        assert_eq!(kind("set -e\necho one"), ExecKind::Run);
        assert_eq!(kind("cat a b"), ExecKind::Run);
    }

    // The reference captures.

    fn reference(size: &str, name: &str) -> Option<Vec<String>> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../reference/codex");
        let full = dir.join(size).join(format!("{name}.full.txt"));
        let plain = dir.join(size).join(format!("{name}.txt"));
        let raw = std::fs::read_to_string(&full)
            .or_else(|_| std::fs::read_to_string(&plain))
            .ok()?;
        let rows: Vec<String> = raw.lines().map(|l| l.trim_end().to_string()).collect();
        // From the first `• ` cell row after the user message to the rule.
        let user = rows.iter().rposition(|l| l.starts_with("› go fake:"))?;
        let mut start = user + 1;
        while rows[start].is_empty() {
            start += 1;
        }
        let end = rows[start..]
            .iter()
            .position(|l| !l.is_empty() && l.chars().all(|c| c == '─'))?
            + start;
        let mut block: Vec<String> = rows[start..end].to_vec();
        while block.last().is_some_and(|l| l.is_empty()) {
            block.pop();
        }
        Some(block)
    }

    fn norm(rows: Vec<String>) -> Vec<String> {
        // The fixture repo's commit hash differs per run.
        rows.into_iter()
            .map(|l| {
                let t = l.trim_start();
                if t.len() == 12
                    && t.ends_with(" init")
                    && t[..7].chars().all(|c| c.is_ascii_hexdigit())
                {
                    format!("{}HASH init", &l[..l.len() - t.len()])
                } else {
                    l
                }
            })
            .collect()
    }

    fn exec_scenario(w: u16) -> Vec<String> {
        dark();
        let long = "a very long line ".repeat(30);
        let cases: Vec<(&str, String, i32)> = vec![
            (
                "echo hello from the shell",
                "hello from the shell\n".into(),
                0,
            ),
            (
                "sh -c 'echo running 4 tests; echo \"test add ... ok\"; echo \"test sub ... FAILED\" >&2; exit 101'",
                "running 4 tests\ntest add ... ok\ntest sub ... FAILED\n".into(),
                101,
            ),
            (
                "seq 1 120",
                (1..=120).map(|n| format!("{n}\n")).collect(),
                0,
            ),
            (
                "git status --short && printf 'a very long line %.0s' $(seq 1 30); echo; git log --oneline | head -3",
                format!("{long}\nHASH init\n").replacen("line \nHASH", "line\nHASH", 1),
                0,
            ),
        ];
        let mut rows: Vec<String> = Vec::new();
        for (i, (cmd, out, code)) in cases.into_iter().enumerate() {
            let c = cell(cmd, Some((code, &out)));
            if i > 0 {
                rows.push(String::new());
            }
            // The history inserter wraps what the cell leaves overlong (the ellipsis row).
            for l in c.display_lines(w) {
                rows.extend(texts(&wrap_history_line(&l, w as usize)));
            }
        }
        rows
    }

    #[test]
    fn exec_scenario_matches_the_captures() {
        for (size, w) in [
            ("120x36", 120u16),
            ("150x42", 150),
            ("200x50", 200),
            ("80x24", 80),
            ("60x24", 60),
            ("40x24", 40),
            ("30x24", 30),
        ] {
            let Some(want) = reference(size, "turns-05-exec-done") else {
                continue;
            };
            let got = exec_scenario(w);
            assert_eq!(norm(got), norm(want), "width {w}");
        }
    }

    #[test]
    fn exec_edge_matches_the_capture() {
        dark();
        let Some(want) = reference("120x36", "xe-04-exec-edge-done") else {
            return;
        };
        let cases: Vec<(&str, &str, i32)> = vec![
            (
                "set -e\necho one\necho two\necho three\necho four",
                "one\ntwo\nthree\nfour\n",
                0,
            ),
            (
                "printf '\\033[31mred\\033[0m ok\\n'",
                "\x1b[31mred\x1b[0m ok\n",
                0,
            ),
            ("true", "", 0),
            ("false", "", 1),
            ("sleep 2; echo late", "late\n", 0),
        ];
        let mut rows: Vec<String> = Vec::new();
        for (i, (cmd, out, code)) in cases.into_iter().enumerate() {
            if i > 0 {
                rows.push(String::new());
            }
            rows.extend(texts(&cell(cmd, Some((code, out))).display_lines(120)));
        }
        assert_eq!(rows, want);
    }

    #[test]
    fn explore_matches_the_capture() {
        dark();
        for (size, w) in [
            ("120x36", 120u16),
            ("60x24", 60),
            ("40x24", 40),
            ("30x24", 30),
        ] {
            let Some(want) = reference(size, "turns-04-explore-done") else {
                continue;
            };
            // Arrival order differs between runs; the capture at this size has the order below.
            let mut order: Vec<&str> = Vec::new();
            for l in &want {
                let t = l.trim_start_matches(['•', ' ', '└']).trim_start();
                if t.starts_with("List") {
                    order.push("ls -la");
                } else if t.starts_with("Read") {
                    order.push("cat src/lib.rs");
                } else if t.starts_with("Search") {
                    order.push("rg -n add src");
                }
            }
            let mut e = ExecCell::new(ExecCall::new("0", order[0]));
            for (i, cmd) in order.iter().enumerate().skip(1) {
                assert!(e.push_call(ExecCall::new(i.to_string(), *cmd)));
            }
            for i in 0..order.len() {
                e.complete_call(&i.to_string(), ExecOutput::default(), Duration::ZERO);
            }
            assert_eq!(texts(&e.display_lines(w)), want, "width {w}");
        }
    }
}
