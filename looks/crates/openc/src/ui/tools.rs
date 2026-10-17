// OWNER: tools
//! Tool cards: one summary row each, a body on `surface` when open, diffs with word-level
//! emphasis, merged read/search runs and the subagent tree.
//!
//! Submodules: `diff` (diff data and rows), `diffview` (the full-screen viewer), `kinds` (what
//! a refusal, an answer or a plan looks like in output text), `output` (output as rows) and
//! `tree` (subagent calls).

use std::time::{Duration, Instant};

use agent_core::{ToolCall, ToolKind, ToolStatus};
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use tuikit::width::{display_width, spans_width};

use super::row::{cut, fmt_dur, sp, spaces, two_col, wrap_plain, Cx, Row};

mod diff;
pub mod diffview;
pub mod kinds;
mod output;
pub mod rewind;
mod tree;
pub use diff::{diff_rows, DKind, DRow, DiffData, DiffOpts};
pub use kinds::Denial;

/// Body rows past this are cut with a `╌ n more lines` row.
pub const BODY_CAP: usize = 40;
/// An edit with this many changed rows or fewer opens by itself.
pub const AUTO_OPEN_DIFF: usize = 12;

#[derive(Clone, Debug)]
pub struct ToolEntry {
    pub call: ToolCall,
    pub started: Option<Instant>,
    pub took: Option<Duration>,
    /// When the call finished, so a turn that ends in an interrupt can tell which failures the
    /// interrupt caused.
    pub ended: Option<Instant>,
    /// The call was cut short by an interrupt: shown as `■ interrupted`, body suppressed.
    pub interrupted: bool,
    pub children: Vec<ToolEntry>,
    pub diff: Option<DiffData>,
    /// A subagent the person picked out: unfolded with every call listed, not eight.
    pub focus: bool,
    /// A permission request about this call is waiting for an answer.
    pub awaiting: bool,
}

impl ToolEntry {
    pub fn new(call: ToolCall, now: Instant, replay: bool) -> ToolEntry {
        // A replayed call that never finished reads as interrupted, like it did live.
        let cut = call.status == ToolStatus::Failed
            && call.output.as_deref() == Some(agent_core::INTERRUPTED_OUTPUT);
        let mut e = ToolEntry {
            call,
            started: (!replay).then_some(now),
            took: None,
            ended: None,
            interrupted: cut,
            children: Vec::new(),
            diff: None,
            focus: false,
            awaiting: false,
        };
        e.refresh_diff();
        e
    }

    /// Replace the call (same id) and fix the clock when it finishes.
    pub fn update(&mut self, call: ToolCall, now: Instant) {
        let was_running = self.running();
        self.call = call;
        if was_running && !self.running() {
            self.took = self.started.map(|s| now.saturating_duration_since(s));
            self.ended = Some(now);
        }
        self.refresh_diff();
    }

    pub fn running(&self) -> bool {
        matches!(self.call.status, ToolStatus::Pending | ToolStatus::Running)
    }

    pub fn failed(&self) -> bool {
        self.call.status == ToolStatus::Failed
    }

    pub fn done(&self) -> bool {
        self.call.status == ToolStatus::Completed
    }

    /// A person or a rule refused the call; it never ran.
    pub fn denial(&self) -> Option<Denial> {
        if !self.failed() {
            return None;
        }
        kinds::denial(self.call.output.as_deref()?)
    }

    fn refresh_diff(&mut self) {
        if self.call.kind != ToolKind::Edit {
            return;
        }
        if let Some(d) = &self.call.diff {
            let done = self.call.status == ToolStatus::Completed;
            let rebuild = match &self.diff {
                None => true,
                Some(old) => done && !old.from_disk,
            };
            if rebuild {
                let full = self.verb() == "Write";
                self.diff = Some(DiffData::build_with(
                    d.old.as_deref(),
                    &d.new,
                    &d.path,
                    done,
                    DiffOpts { context: 2, full },
                ));
            }
        }
    }

    pub fn elapsed(&self, now: Instant) -> Option<Duration> {
        match (self.took, self.started) {
            (Some(t), _) => Some(t),
            (None, Some(s)) => Some(now.saturating_duration_since(s)),
            _ => None,
        }
    }

    pub fn verb(&self) -> &'static str {
        verb_of(&self.call)
    }

    pub fn target(&self) -> String {
        target_of(&self.call)
    }

    /// Output lines that carry text. A file read comes as `   12→code` lines followed by
    /// reminders the CLI appends, so only the numbered ones count.
    fn lines(&self) -> usize {
        let Some(o) = self.call.output.as_deref().map(output::effective) else {
            return 0;
        };
        let text = o.lines().filter(|l| !l.trim().is_empty()).count();
        if self.verb() == "Read" {
            let nums: Vec<&str> = o.lines().filter(|l| output::numbered(l)).collect();
            let starts_numbered = o
                .lines()
                .find(|l| !l.trim().is_empty())
                .is_some_and(output::numbered);
            if starts_numbered && !nums.is_empty() {
                // A file that ends with a newline is listed with one empty last line.
                let trailing = nums
                    .last()
                    .is_some_and(|l| l.split_once('→').is_some_and(|(_, c)| c.is_empty()));
                return nums.len() - usize::from(trailing);
            }
        }
        text
    }
}

pub fn verb_of(c: &ToolCall) -> &'static str {
    let n = c.name.to_lowercase();
    if n == "task" || n == "agent" {
        return "Task";
    }
    if kinds::is_ask(c) {
        return "Ask";
    }
    if kinds::is_plan(c) {
        return "Plan";
    }
    match c.kind {
        ToolKind::Execute => "Bash",
        ToolKind::Read => "Read",
        ToolKind::Edit => {
            if n == "write"
                || n == "write_file"
                || (c.diff.as_ref().is_some_and(|d| d.old.is_none())
                    && n != "edit"
                    && n != "multiedit")
            {
                "Write"
            } else {
                "Edit"
            }
        }
        ToolKind::Search => "Search",
        ToolKind::Fetch => "Fetch",
        _ => "Tool",
    }
}

pub fn target_of(c: &ToolCall) -> String {
    let from_input = |k: &str| c.input.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let t = match c.kind {
        ToolKind::Execute => from_input("command").unwrap_or_else(|| c.title.clone()),
        _ if kinds::is_plan(c) => {
            if c.title.is_empty() {
                kinds::plan_text(c)
                    .map(kinds::plan_title)
                    .unwrap_or_default()
            } else {
                c.title.clone()
            }
        }
        _ if !c.title.is_empty() => c.title.clone(),
        _ => from_input("file_path")
            .or_else(|| from_input("path"))
            .or_else(|| from_input("pattern"))
            .or_else(|| from_input("url"))
            .or_else(|| from_input("description"))
            .unwrap_or_default(),
    };
    let t = match t.strip_prefix("select:") {
        Some(rest) if c.name == "ToolSearch" => format!("load {}", rest.replace(',', ", ")),
        _ => t,
    };
    let t = if matches!(verb_of(c), "Tool" | "Bash") && t.is_empty() {
        match c.name.strip_prefix("mcp__") {
            Some(rest) => rest.replacen("__", ":", 1),
            None => c.name.clone(),
        }
    } else {
        t
    };
    // One row, always: a multi-line command shows its first line.
    t.lines().next().unwrap_or("").trim().to_string()
}

/// Tools the transcript never shows: their state lives in the dock.
pub fn hidden(name: &str) -> bool {
    let n = name.to_lowercase();
    matches!(
        n.as_str(),
        "todowrite"
            | "taskcreate"
            | "taskupdate"
            | "tasklist"
            | "taskget"
            | "taskoutput"
            | "taskstop"
            | "todo_write"
            | "update_plan"
    )
}

/// Reads and searches that arrive back to back are drawn as one row.
pub fn mergeable(k: ToolKind) -> bool {
    matches!(k, ToolKind::Read | ToolKind::Search)
}

// ---- cards -------------------------------------------------------------------------------

/// `+3 -1` for an edit's diff.
fn diff_counts(d: &DiffData) -> String {
    if d.removed == 0 {
        format!("+{}", d.added)
    } else {
        format!("+{} -{}", d.added, d.removed)
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The right-hand part of the summary row. `room` is the most it may take: a running call
/// gives up its detail, last part first, before the title on the left loses its first words.
fn outcome(e: &ToolEntry, cx: &Cx, room: usize) -> Vec<Span<'static>> {
    let p = cx.p;
    let g = cx.g;
    let sep = format!(" {} ", g.sep);
    if e.running() && e.awaiting {
        return vec![sp("waiting for you", p.s_warn())];
    }
    if e.running() {
        let t = e.elapsed(cx.now).map(fmt_dur).unwrap_or_default();
        let mut parts = vec![t];
        // What the subagent is doing now is the last row of the tree under this one (running
        // and folded it trails the last three calls, unfolded it lists them), so the row says
        // how long and how many and no more (round 2 finding 6 saw it twice, then crowd out
        // the title at 44 columns).
        if !e.children.is_empty() {
            parts.push(tree::count_text(e, g.sep));
        }
        parts.retain(|s| !s.is_empty());
        while parts.len() > 1 && display_width(&parts.join(&sep)) > room {
            parts.pop();
        }
        return vec![sp(parts.join(&sep), p.s_faint())];
    }
    let took = e.took.map(fmt_dur);
    let long = e.took.is_some_and(|t| t.as_secs_f64() >= 1.0);
    let out = output::effective(e.call.output.as_deref().unwrap_or(""));
    let lines = e.lines();
    if e.interrupted {
        let mut v = vec![sp(format!("{} ", g.stop), p.s_warn())];
        v.push(sp(
            format!(
                "interrupted{}",
                took.map(|t| format!("{sep}{t}")).unwrap_or_default()
            ),
            p.s_faint(),
        ));
        return v;
    }
    if e.failed() {
        let mut parts = Vec::new();
        if let Some(d) = e.denial() {
            parts.push(match d {
                Denial::User { .. } => "denied".to_string(),
                Denial::Rule { .. } => "denied by rule".to_string(),
            });
            if let Some(dd) = &e.diff {
                parts.push(diff_counts(dd));
            }
        } else if kinds::is_ask(&e.call) {
            parts.push("skipped".into());
        } else {
            parts.push(match kinds::exit_code(out) {
                Some(c) => format!("exit {c}"),
                None => "failed".to_string(),
            });
            parts.extend(took);
        }
        return vec![
            sp(format!("{} ", g.fail), p.s_err()),
            sp(parts.join(&sep), p.s_faint()),
        ];
    }
    let mut parts: Vec<String> = Vec::new();
    match e.verb() {
        "Edit" | "Write" => {
            // A write is instant; the wall time of one includes the wait for an answer.
            if let Some(d) = &e.diff {
                parts.push(diff_counts(d));
            }
        }
        "Bash" => {
            parts.extend(took);
            if output::is_binary(out) {
                parts.push(format!("binary {}", output::human_bytes(out.len())));
            } else if lines == 0 {
                parts.push("no output".into());
            } else {
                parts.push(plural(lines, "line", "lines"));
            }
        }
        "Read" => {
            if long {
                parts.extend(took.clone());
            }
            if output::is_binary(out) {
                parts.push(format!("binary {}", output::human_bytes(out.len())));
            } else if lines > 0 {
                parts.push(plural(lines, "line", "lines"));
            } else {
                parts.push("empty".into());
            }
        }
        "Search" => {
            if long {
                parts.extend(took);
            }
            parts.push(if lines > 0 {
                plural(lines, "result", "results")
            } else {
                "no results".into()
            });
        }
        "Fetch" => {
            parts.extend(took);
            if !out.is_empty() {
                parts.push(output::human_bytes(out.len()));
            }
        }
        "Task" => {
            parts.extend(took);
            if !e.children.is_empty() {
                parts.push(tree::count_text(e, g.sep));
            }
        }
        "Plan" => parts.push("approved".into()),
        "Ask" => {
            let a = kinds::answers(&e.call);
            let joined = a
                .iter()
                .map(|x| x.answer.as_str())
                .collect::<Vec<_>>()
                .join(&sep);
            if a.is_empty() {
            } else if joined.chars().count() <= 40 {
                parts.push(joined);
            } else {
                parts.push(format!("{} answers", a.len()));
            }
        }
        _ => {
            parts.extend(took);
            if lines > 1 {
                parts.push(plural(lines, "line", "lines"));
            }
        }
    }
    let mut v = vec![sp(g.ok, p.s_ok())];
    if !parts.is_empty() {
        v.push(sp(format!(" {}", parts.join(&sep)), p.s_faint()));
    }
    v
}

fn glyph_for(e: &ToolEntry, open: bool, cx: &Cx) -> Span<'static> {
    let p = cx.p;
    if e.running() {
        sp(cx.spinner(), p.s_accent())
    } else if open {
        sp(cx.g.unfolded, p.s_dim())
    } else {
        sp(cx.g.folded, p.s_dim())
    }
}

fn verb_style(e: &ToolEntry, cx: &Cx) -> Style {
    if e.running() {
        cx.p.s_dim().add_modifier(Modifier::BOLD)
    } else {
        cx.p.s_dim()
    }
}

fn is_path_verb(v: &str) -> bool {
    matches!(v, "Read" | "Edit" | "Write")
}

/// The one summary row.
fn head_row(e: &ToolEntry, open: bool, cx: &Cx) -> Row {
    let p = cx.p;
    let verb = e.verb();
    let target = e.target();
    let target = if is_path_verb(verb) && target.chars().count() > 48 {
        tuikit::width::truncate_left(&target, 48)
    } else {
        target
    };
    let left = vec![
        spaces(2),
        glyph_for(e, open, cx),
        spaces(1),
        sp(format!("{verb:<6}  "), verb_style(e, cx)),
        sp(target, p.s_text()),
    ];
    // The title keeps its first 24 cells (or all of it, if shorter) before the right side may
    // take more than what is left over.
    let identity = spans_width(&left).min(2 + 1 + 1 + 8 + 24);
    let room = cx.width.saturating_sub(identity + 2);
    // Two cells between a title and its outcome when the title is cut: one read as `render… ✓`
    // (round 2 finding 10).
    let mut right = vec![spaces(1)];
    right.extend(outcome(e, cx, room));
    Row::new(two_col(left, right, cx.width, Style::default())).head()
}

fn surface_row(cx: &Cx, spans: Vec<Span<'static>>) -> Row {
    let mut v = vec![spaces(4)];
    v.extend(spans);
    Row::new(v).bg(cx.p.surface)
}

/// `you said: use a flag instead`, wrapped.
fn denial_rows(d: &Denial, w: usize, cx: &Cx) -> Vec<Row> {
    let p = cx.p;
    match d {
        Denial::User { note } if note.is_empty() => {
            vec![surface_row(cx, vec![sp("you said no", p.s_faint())])]
        }
        Denial::User { note } => wrap_plain(&format!("you said: {note}"), w)
            .into_iter()
            .map(|l| surface_row(cx, vec![sp(l, p.s_dim())]))
            .collect(),
        Denial::Rule { why } => wrap_plain(why, w)
            .into_iter()
            .map(|l| surface_row(cx, vec![sp(l, p.s_dim())]))
            .collect(),
    }
}

/// `key  value` rows for a call's input, one line each.
fn param_rows(e: &ToolEntry, w: usize, cx: &Cx) -> Vec<Row> {
    let p = cx.p;
    let Some(obj) = e.call.input.as_object() else {
        return Vec::new();
    };
    let key_w = obj
        .keys()
        .map(|k| k.chars().count())
        .max()
        .unwrap_or(0)
        .min(14);
    obj.iter()
        .take(6)
        .map(|(k, v)| {
            let val = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let one: String = val.split_whitespace().collect::<Vec<_>>().join(" ");
            surface_row(
                cx,
                vec![
                    sp(format!("{k:<key_w$}  "), p.s_faint()),
                    sp(cut(&one, w.saturating_sub(key_w + 2)), p.s_dim()),
                ],
            )
        })
        .collect()
}

fn md_rows(text: &str, w: usize, cx: &Cx) -> Vec<Row> {
    let inner = Cx {
        p: cx.p,
        theme: cx.theme,
        g: cx.g,
        width: w.saturating_sub(2).max(10),
        detail: cx.detail,
        now: cx.now,
        spin: cx.spin,
    };
    super::md::render(text, &inner)
        .into_iter()
        .map(|mut r| {
            r.spans.insert(0, spaces(2));
            r.bg = Some(cx.p.surface);
            r
        })
        .collect()
}

/// Rows drawn under the head, on `surface`, at column 4.
fn body_rows(e: &ToolEntry, cx: &Cx) -> Vec<Row> {
    let p = cx.p;
    let w = cx.width.saturating_sub(5).max(8);
    let mut rows: Vec<Row> = Vec::new();
    let mut more = 0usize;
    let verb = e.verb();
    let out = e.call.output.as_deref().unwrap_or("");
    if let Some(d) = e.denial() {
        rows.extend(denial_rows(&d, w, cx));
    }
    if let Some(d) = &e.diff {
        rows.extend(diff_rows(d, cx, BODY_CAP + 1));
    } else if verb == "Task" {
        let limit = if e.focus { BODY_CAP } else { tree::TREE_ROWS };
        rows.extend(tree::rows(e, cx, limit));
        if !out.trim().is_empty() {
            if !rows.is_empty() {
                rows.push(Row::new(Vec::new()).bg(p.surface));
            }
            let report = md_rows(out, w, cx);
            let total = report.len();
            rows.extend(report.into_iter().take(10));
            if total > 10 {
                more += total - 10;
            }
        }
    } else if verb == "Plan" {
        if let Some(plan) = kinds::plan_text(&e.call) {
            rows.extend(md_rows(plan, w, cx));
        }
    } else if verb == "Ask" {
        let answers = kinds::answers(&e.call);
        if answers.is_empty() && e.denial().is_none() {
            rows.extend(
                wrap_plain(out, w)
                    .into_iter()
                    .map(|l| surface_row(cx, vec![sp(l, p.s_dim())])),
            );
        }
        for a in answers {
            rows.push(surface_row(
                cx,
                vec![
                    sp(format!("{:<10} ", cut(&a.header, 10)), p.s_faint()),
                    sp(cut(&a.answer, w.saturating_sub(11)), p.s_text()),
                ],
            ));
        }
    } else {
        if verb == "Bash" {
            let cmd = e
                .call
                .input
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let full = if cmd.is_empty() {
                e.target()
            } else {
                cmd.to_string()
            };
            if full.contains('\n') || full.chars().count() > w {
                for l in full
                    .lines()
                    .flat_map(|l| wrap_plain(l, w.saturating_sub(2)))
                {
                    rows.push(surface_row(
                        cx,
                        vec![sp("$ ", p.s_faint()), sp(l, p.s_text())],
                    ));
                }
            }
        }
        if verb == "Tool" || verb == "Fetch" {
            rows.extend(param_rows(e, w, cx));
        }
        if e.denial().is_none() && !(e.running() && out.is_empty()) {
            let room = BODY_CAP.saturating_sub(rows.len());
            let skip = e.failed() && kinds::exit_code(out).is_some();
            let b = match verb {
                "Read" if e.done() => output::read_rows(out, &e.target(), w, cx, room),
                "Search" if e.done() => output::search_rows(out, w, cx, room),
                _ => output::rows(out, w, cx, room, skip),
            };
            more = b.more;
            rows.extend(b.rows);
        }
    }
    if rows.len() > BODY_CAP {
        more += rows.len() - BODY_CAP;
        rows.truncate(BODY_CAP);
    }
    if more > 0 {
        rows.push(surface_row(
            cx,
            vec![sp(
                format!(
                    "{} {more} more {}  e opens in pager",
                    cx.g.dashed,
                    if more == 1 { "line" } else { "lines" }
                ),
                p.s_faint(),
            )],
        ));
    }
    rows
}

fn child_summary(c: &ToolEntry) -> String {
    let lines = c.lines();
    match c.verb() {
        "Read" | "Bash" if lines > 0 => plural(lines, "line", "lines"),
        "Search" if lines > 0 => plural(lines, "result", "results"),
        "Edit" | "Write" => c.diff.as_ref().map(diff_counts).unwrap_or_default(),
        _ => String::new(),
    }
}

/// Default fold state for a card when the user has not touched it.
pub fn default_open(entries: &[ToolEntry]) -> bool {
    if entries.len() != 1 {
        return false;
    }
    let e = &entries[0];
    if let Some(d) = &e.diff {
        if e.done() && d.changed() <= AUTO_OPEN_DIFF && !d.rows.is_empty() {
            return true;
        }
    }
    if e.failed() && !e.interrupted {
        if e.denial().is_some() {
            // The collapsed card already says what you told it.
            return false;
        }
        let n = e.call.output.as_deref().map_or(0, |o| o.lines().count());
        return n > 0 && n <= 8;
    }
    false
}

/// Lay out one tool block: a single card, or a merged run of reads or searches.
pub fn layout(entries: &[ToolEntry], user_open: Option<bool>, cx: &Cx) -> Vec<Row> {
    let p = cx.p;
    let open = cx.detail || user_open.unwrap_or_else(|| default_open(entries));
    if entries.len() == 1 {
        let e = &entries[0];
        if e.running() && e.awaiting {
            // The permission panel under the transcript names this call and shows what it
            // would do; a card above it said the same thing again (finding 7).
            return Vec::new();
        }
        let open = open || (e.focus && e.verb() == "Task");
        let mut rows = vec![head_row(e, open, cx)];
        let tail_w = cx.width.saturating_sub(6);
        if open {
            rows.extend(body_rows(e, cx));
        } else if e.running() && e.verb() == "Bash" {
            // The last three output lines while it runs; they vanish when it finishes.
            rows.extend(output::tail(
                e.call.output.as_deref().unwrap_or(""),
                3,
                tail_w,
                cx,
            ));
        } else if e.failed() && !e.interrupted {
            // Why it failed, without opening the card: the reason, or the last lines.
            if let Some(d) = e.denial() {
                if !d.note().is_empty() {
                    rows.push(Row::new(vec![
                        spaces(4),
                        sp(cut(&format!("you said: {}", d.note()), tail_w), p.s_faint()),
                    ]));
                }
            } else {
                let out = output::without_exit_line(e.call.output.as_deref().unwrap_or(""));
                rows.extend(output::tail(out, 3, tail_w, cx));
            }
        } else if e.running() && !e.children.is_empty() {
            rows.extend(tree::tail(e, cx, 3));
        }
        return rows;
    }
    // Merged run.
    let verb = entries[0].verb();
    let any_running = entries.iter().any(ToolEntry::running);
    let any_failed = entries.iter().any(ToolEntry::failed);
    let targets: Vec<String> = entries.iter().map(ToolEntry::target).collect();
    let names: Vec<String> = if verb == "Read" {
        distinct_names(&targets)
    } else {
        targets.iter().map(|t| short_target(t)).collect()
    };
    let noun = if verb == "Read" { "files" } else { "searches" };
    let shown: Vec<&str> = names.iter().take(2).map(String::as_str).collect();
    let more = names.len().saturating_sub(2);
    let mut target = format!("{} {noun}  {}", entries.len(), shown.join(", "));
    if more > 0 {
        target.push_str(&format!(", +{more}"));
    }
    let lines: usize = entries.iter().map(ToolEntry::lines).sum();
    let right = if any_running {
        let done = entries.iter().filter(|e| !e.running()).count();
        vec![sp(format!("{done}/{}", entries.len()), p.s_faint())]
    } else if any_failed {
        let n = entries.iter().filter(|e| e.failed()).count();
        vec![
            sp(format!("{} ", cx.g.fail), p.s_err()),
            sp(format!("{n} failed"), p.s_faint()),
        ]
    } else {
        let tail = if lines > 0 {
            format!(" {lines} lines")
        } else {
            String::new()
        };
        vec![sp(cx.g.ok, p.s_ok()), sp(tail, p.s_faint())]
    };
    let glyph = if any_running {
        sp(cx.spinner(), p.s_accent())
    } else if open {
        sp(cx.g.unfolded, p.s_dim())
    } else {
        sp(cx.g.folded, p.s_dim())
    };
    let left = vec![
        spaces(2),
        glyph,
        spaces(1),
        sp(
            format!("{verb:<6}  "),
            if any_running {
                p.s_dim().add_modifier(Modifier::BOLD)
            } else {
                p.s_dim()
            },
        ),
        sp(target, p.s_text()),
    ];
    let mut padded = vec![spaces(1)];
    padded.extend(right);
    let mut rows = vec![Row::new(two_col(left, padded, cx.width, Style::default())).head()];
    if open {
        for e in entries {
            let g = if e.running() {
                sp(cx.spinner(), p.s_accent())
            } else if e.failed() {
                sp(cx.g.fail, p.s_err())
            } else {
                sp(cx.g.ok, p.s_ok())
            };
            let left = vec![spaces(4), g, spaces(1), sp(e.target(), p.s_text())];
            let right = vec![sp(child_summary(e), p.s_faint())];
            rows.push(Row::new(two_col(left, right, cx.width, Style::default())).bg(p.surface));
        }
    }
    rows
}

/// The shortest tail of each path that tells it from the others: `render.rs` alone when it is
/// the only one, `src/render.rs` and `tests/render.rs` when two share a basename (round 2
/// finding 10: `Read 2 files  render.rs, render.rs`).
fn distinct_names(paths: &[String]) -> Vec<String> {
    let tail = |p: &str, n: usize| -> String {
        let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
        let from = parts.len().saturating_sub(n);
        parts[from..].join("/")
    };
    paths
        .iter()
        .map(|p| {
            let depth = p.split('/').filter(|s| !s.is_empty()).count().max(1);
            (1..=depth)
                .map(|n| tail(p, n))
                .find(|t| {
                    paths
                        .iter()
                        .filter(|q| *q != p)
                        .all(|q| tail(q, t.split('/').count()) != *t)
                })
                .unwrap_or_else(|| p.clone())
        })
        .collect()
}

fn short_target(t: &str) -> String {
    match t.rsplit_once('/') {
        Some((_, name)) if !name.is_empty() => name.to_string(),
        _ => t.to_string(),
    }
}

/// Plain text of an entry for `y` (copy block).
pub fn copy_text(entries: &[ToolEntry]) -> String {
    let mut s = String::new();
    for e in entries {
        s.push_str(&format!("{} {}\n", e.verb(), e.target()));
        if let Some(d) = &e.diff {
            for r in &d.rows {
                let sign = match r.kind {
                    DKind::Add => '+',
                    DKind::Del => '-',
                    _ => ' ',
                };
                if !matches!(r.kind, DKind::Gap(_)) {
                    s.push_str(&format!("{sign}{}\n", r.text));
                }
            }
        } else if let Some(o) = &e.call.output {
            s.push_str(&tuikit::ansi::strip(o));
            s.push('\n');
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::{Depth, Kind, Palette, UNICODE};
    use agent_core::FileDiff;

    fn call(name: &str, kind: ToolKind, title: &str) -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: name.into(),
            kind,
            title: title.into(),
            status: ToolStatus::Completed,
            ..Default::default()
        }
    }

    fn cx_run<R>(width: usize, f: impl FnOnce(&Cx) -> R) -> R {
        let p = Palette::new(Kind::Hearth, Depth::True);
        let t = p.theme();
        let cx = Cx {
            p: &p,
            theme: &t,
            g: &UNICODE,
            width,
            detail: false,
            now: Instant::now(),
            spin: 0,
        };
        f(&cx)
    }

    fn texts(rows: &[Row]) -> Vec<String> {
        rows.iter().map(Row::text).collect()
    }

    #[test]
    fn changed_words_keep_text_at_4_5_in_truecolor_and_256() {
        // Round 2 finding 17: a `,` in the syntax dim colour on the word tint was 3.5:1.
        for depth in [Depth::True, Depth::Ansi256] {
            let p = Palette::new(Kind::Hearth, depth);
            let t = p.theme();
            let cx = Cx {
                p: &p,
                theme: &t,
                g: &UNICODE,
                width: 80,
                detail: false,
                now: Instant::now(),
                spin: 0,
            };
            let d = DiffData::build(
                Some("let v = compute(&input, 0);\n"),
                "let v = compute(&input, 0, opts.strict);\n",
                "x.rs",
                false,
            );
            let hl = diff::highlight_rows(&d, &cx);
            let mut checked = 0;
            for (r, spans) in d.rows.iter().zip(hl) {
                for s in diff::styled_code(r, spans, &cx) {
                    let Some(bg) = s.style.bg else { continue };
                    if bg != p.add_word && bg != p.del_word {
                        continue;
                    }
                    let c =
                        crate::palette::contrast(p.seen(s.style.fg.unwrap_or(p.text)), p.seen(bg));
                    assert!(c >= 4.5, "{depth:?} {:?}: {c:.2}", s.content);
                    checked += 1;
                }
            }
            assert!(checked > 0, "{depth:?}: no changed word was marked");
        }
    }

    #[test]
    fn edit_diff_counts_and_marks_changed_words() {
        let d = DiffData::build(
            Some("a\nlet x = foo(1);\nb\n"),
            "a\nlet x = bar(1);\nb\n",
            "/nonexistent/f.rs",
            true,
        );
        assert_eq!((d.added, d.removed), (1, 1));
        let del = d.rows.iter().find(|r| r.kind == DKind::Del).unwrap();
        assert_eq!(&del.text[del.emph[0].clone()], "foo");
    }

    #[test]
    fn card_is_one_row_with_verb_target_and_outcome() {
        let mut c = call("Read", ToolKind::Read, "src/a.rs");
        c.output = Some("1 a\n2 b\n3 c\n".into());
        let e = ToolEntry::new(c, Instant::now(), true);
        let rows = cx_run(80, |cx| layout(std::slice::from_ref(&e), None, cx));
        assert_eq!(rows.len(), 1);
        let t = rows[0].text();
        // Verbs sit in an 8 cell column, so `Search` has the two spaces `Bash` has after it.
        assert!(t.starts_with("  ▸ Read    src/a.rs"), "{t:?}");
        assert!(t.ends_with("✓ 3 lines"), "{t:?}");
        assert_eq!(tuikit::width::display_width(&t), 80);
    }

    #[test]
    fn small_edit_opens_and_big_one_stays_folded() {
        let mut c = call("Edit", ToolKind::Edit, "x.rs");
        c.diff = Some(FileDiff {
            path: "/nonexistent/x.rs".into(),
            old: Some("a\n".into()),
            new: "b\n".into(),
        });
        let e = ToolEntry::new(c, Instant::now(), true);
        let rows = cx_run(80, |cx| layout(std::slice::from_ref(&e), None, cx));
        assert!(rows.len() >= 3, "{:?}", texts(&rows));
        assert!(rows[0].text().ends_with("✓ +1 -1"));
        let big_new: String = (0..30).map(|i| format!("line {i}\n")).collect();
        let mut c = call("Write", ToolKind::Edit, "big.txt");
        c.diff = Some(FileDiff {
            path: "/nonexistent/big.txt".into(),
            old: None,
            new: big_new,
        });
        let e = ToolEntry::new(c, Instant::now(), true);
        let rows = cx_run(80, |cx| layout(std::slice::from_ref(&e), None, cx));
        assert_eq!(rows.len(), 1);
        assert!(rows[0].text().ends_with("✓ +30"), "{}", rows[0].text());
    }

    #[test]
    fn running_bash_shows_three_tail_lines_and_they_go_away() {
        let mut c = call("Bash", ToolKind::Execute, "cargo test");
        c.status = ToolStatus::Running;
        c.output = Some("a\nb\nc\nd\ne\n".into());
        let mut e = ToolEntry::new(c.clone(), Instant::now(), false);
        let rows = cx_run(80, |cx| layout(std::slice::from_ref(&e), None, cx));
        assert_eq!(rows.len(), 4);
        c.status = ToolStatus::Completed;
        e.update(c, Instant::now());
        let rows = cx_run(80, |cx| layout(std::slice::from_ref(&e), None, cx));
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn merged_reads_make_one_row() {
        let mk = |t: &str| ToolEntry::new(call("Read", ToolKind::Read, t), Instant::now(), true);
        let es = vec![mk("src/a.rs"), mk("src/b.rs"), mk("src/c.rs")];
        let rows = cx_run(100, |cx| layout(&es, None, cx));
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0].text().contains("3 files  a.rs, b.rs, +1"),
            "{}",
            rows[0].text()
        );
    }

    #[test]
    fn failed_call_shows_exit_and_last_lines() {
        let mut c = call("Bash", ToolKind::Execute, "cargo test");
        c.status = ToolStatus::Failed;
        c.output = Some("Exit code 101\nerror: boom".into());
        let e = ToolEntry::new(c, Instant::now(), true);
        let rows = cx_run(80, |cx| layout(std::slice::from_ref(&e), None, cx));
        assert!(rows[0].text().contains("✗ exit 101"), "{}", rows[0].text());
        assert!(rows.len() > 1, "short failures open by themselves");
    }

    #[test]
    fn a_read_counts_the_numbered_lines_and_not_the_empty_last_one_or_the_reminders() {
        let mut c = call("Read", ToolKind::Read, "main.rs");
        c.output = Some(
            "     1→fn main() {\n     2→}\n     3→\n\n<system-reminder>\nlong reminder\n</system-reminder>"
                .into(),
        );
        let e = ToolEntry::new(c, Instant::now(), true);
        let rows = cx_run(80, |cx| layout(std::slice::from_ref(&e), None, cx));
        assert!(rows[0].text().ends_with("✓ 2 lines"), "{}", rows[0].text());
    }

    #[test]
    fn a_folded_failure_shows_the_reason_not_the_exit_line_it_already_says() {
        let mut c = call("Bash", ToolKind::Execute, "cargo test");
        c.status = ToolStatus::Failed;
        c.output = Some("Exit code 101\nerror: boom\nnote: more".into());
        let e = ToolEntry::new(c, Instant::now(), true);
        let rows = cx_run(80, |cx| layout(std::slice::from_ref(&e), Some(false), cx));
        let t = texts(&rows);
        assert!(t[0].contains("✗ exit 101"), "{t:?}");
        assert_eq!(t.len(), 3, "{t:?}");
        assert!(
            t[1].trim() == "error: boom" && !t.iter().any(|l| l.contains("Exit code")),
            "{t:?}"
        );
    }

    #[test]
    fn a_running_task_keeps_its_title_at_44_columns_and_says_the_current_tool_once() {
        // Round 2 finding 6: at 44 columns the title was `Task…` and the right side
        // `2.6s · 2 tools · Task Check tests for wr`; at 120 the child's name was printed
        // on the row and again in the tree under it.
        let mut call = call("Task", ToolKind::Other, "Check tests for wrap");
        call.status = ToolStatus::Running;
        let mut parent = ToolEntry::new(call, Instant::now(), false);
        for t in ["a.rs", "b.rs"] {
            let mut c = self::call("Read", ToolKind::Read, t);
            c.status = ToolStatus::Running;
            parent
                .children
                .push(ToolEntry::new(c, Instant::now(), false));
        }
        for w in [44usize, 60, 80, 120] {
            let rows = cx_run(w, |cx| {
                layout(std::slice::from_ref(&parent), Some(false), cx)
            });
            let t = texts(&rows);
            assert!(display_width(&t[0]) <= w, "{w}: {t:?}");
            let shown = t[0].split("Task").nth(1).unwrap_or_default().trim_start();
            // At least 24 cells of the title, or all of it.
            assert!(
                shown.starts_with("Check tests for wra")
                    || shown.starts_with("Check tests for wrap"),
                "{w}: title lost: {:?}",
                t[0]
            );
            assert_eq!(
                t.iter().filter(|l| l.contains("b.rs")).count(),
                1,
                "{w}: the current call is named once: {t:?}"
            );
        }
    }

    #[test]
    fn two_reads_with_the_same_basename_say_which_directory() {
        let reads: Vec<ToolEntry> = ["/p/src/render.rs", "/p/tests/render.rs", "/p/src/main.rs"]
            .iter()
            .map(|t| ToolEntry::new(call("Read", ToolKind::Read, t), Instant::now(), true))
            .collect();
        let t = texts(&cx_run(100, |cx| layout(&reads, Some(false), cx)));
        assert!(t[0].contains("src/render.rs, tests/render.rs"), "{t:?}");
        assert_eq!(
            distinct_names(&["/a/x.rs".into(), "/b/y.rs".into()]),
            ["x.rs", "y.rs"]
        );
        assert_eq!(
            distinct_names(&["/a/b/x.rs".into(), "/c/b/x.rs".into()]),
            ["a/b/x.rs", "c/b/x.rs"]
        );
    }

    #[test]
    fn subagent_tree_has_branches() {
        let mut parent = ToolEntry::new(
            call("Task", ToolKind::Other, "survey"),
            Instant::now(),
            true,
        );
        for t in ["a.rs", "b.rs"] {
            parent.children.push(ToolEntry::new(
                call("Read", ToolKind::Read, t),
                Instant::now(),
                true,
            ));
        }
        let rows = cx_run(80, |cx| layout(&[parent.clone()], Some(true), cx));
        let t = texts(&rows);
        assert!(t[1].contains("├─") && t[2].contains("└─"), "{t:?}");
        assert!(t[0].contains("2 tools"), "{t:?}");
    }

    #[test]
    fn todo_tools_are_hidden() {
        assert!(hidden("TodoWrite") && hidden("TaskUpdate") && !hidden("Bash"));
    }
}
