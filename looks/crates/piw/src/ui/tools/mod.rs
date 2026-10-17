// OWNER: tools (tool execution blocks, diffs, user bash blocks)
//! Tool calls as Pi's boxed blocks (spec 7, 6.7, 14.2). A block is a `Box` with padding 1x1 whose
//! background follows the call's state: `toolPendingBg` while it runs, `toolSuccessBg` after
//! success, `toolErrorBg` after a failure. Inside: the call row, then the result, rendered the way
//! Pi's `core/tools/renderers/*.js` do.
//!
//! Pi picks the renderer by tool name; wizard's names differ, so [`classify`] maps
//! `read_file`/`list_files`/`search_files`/`write_file`/`edit_file`/`execute` onto Pi's
//! `read`/`ls`/`grep`/`write`/`edit`/`bash`. Everything else is the generic
//! `formatToolCallWithArgs` form.

mod diff;
mod user;
pub mod util;

use std::cell::RefCell;
use std::time::Duration;

use agent_core::{ToolCall, ToolKind, ToolStatus};
use ratatui::style::Style;
use ratatui::text::Span;

use super::{blank, boxed, indent, span, truncate_dots, Cx, Lines};
use crate::theme::Tok;
use crate::ui::markdown_theme;
use util::*;

pub use user::{render_user_bash, UserBash};

/// How long a call has been running, and whether it has finished (bash shows `Elapsed 3.0s`
/// while running and `Took 3.0s` after).
#[derive(Clone, Copy, Debug, Default)]
pub struct ToolTime {
    pub elapsed: Duration,
    pub done: bool,
}

pub struct ToolCx<'a> {
    pub cx: &'a Cx,
    pub time: ToolTime,
    /// Set when the owning message was aborted or errored while this call was still pending: the
    /// block turns `toolErrorBg` and shows this text as its result.
    pub abort_message: Option<String>,
}

impl<'a> ToolCx<'a> {
    pub fn new(cx: &'a Cx, time: ToolTime) -> Self {
        ToolCx {
            cx,
            time,
            abort_message: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PiTool {
    Read,
    Bash,
    Edit,
    Write,
    Grep,
    Find,
    Ls,
    Generic,
}

pub fn classify(c: &ToolCall) -> PiTool {
    match c.name.to_lowercase().as_str() {
        "read_file" | "read" => PiTool::Read,
        "execute" | "bash" | "shell" => PiTool::Bash,
        "edit_file" | "edit" => PiTool::Edit,
        "write_file" | "write" => PiTool::Write,
        "search_files" | "grep" => PiTool::Grep,
        "find" | "glob" => PiTool::Find,
        "list_files" | "ls" => PiTool::Ls,
        "git_status" | "git_diff" | "web_search" | "x_search" | "web_fetch" | "memory" | "todo"
        | "generate_image" => PiTool::Generic,
        _ => match c.kind {
            ToolKind::Execute => PiTool::Bash,
            ToolKind::Read => PiTool::Read,
            ToolKind::Search => PiTool::Grep,
            ToolKind::Edit if c.diff.as_ref().is_some_and(|d| d.old.is_none()) => PiTool::Write,
            ToolKind::Edit => PiTool::Edit,
            _ => PiTool::Generic,
        },
    }
}

/// What the result part of a block has to show.
struct Eff {
    /// Result text as the tool gave it (cleaned), `None` while nothing has arrived.
    text: Option<String>,
    error: bool,
    running: bool,
    /// The call is over (success, failure or abort).
    done: bool,
}

fn effective(call: &ToolCall, tcx: &ToolCx) -> Eff {
    let out = call.output.as_deref().map(clean);
    if let Some(msg) = &tcx.abort_message {
        let text = match out.as_deref().map(str::trim_end).filter(|o| !o.is_empty()) {
            Some(o) => format!("{o}\n\n\n{msg}"),
            None => msg.clone(),
        };
        return Eff {
            text: Some(text),
            error: true,
            running: false,
            done: true,
        };
    }
    let failed = call.status == ToolStatus::Failed;
    let done = matches!(call.status, ToolStatus::Completed | ToolStatus::Failed);
    Eff {
        text: out,
        error: failed,
        running: matches!(call.status, ToolStatus::Pending | ToolStatus::Running),
        done,
    }
}

struct Ctx<'a> {
    cx: &'a Cx,
    /// Width of the box content.
    inner: u16,
    rows: Lines,
}

impl Ctx<'_> {
    fn th(&self) -> &crate::theme::PiTheme {
        self.cx.th()
    }

    fn push_spans(&mut self, spans: Vec<Span<'static>>) {
        let rows = wrap_row(spans, self.inner);
        self.rows.extend(rows);
    }

    fn blank(&mut self) {
        self.rows.push(blank());
    }

    /// Plain-styled text, `\n` separated.
    fn text(&mut self, t: &str, style: Style) {
        let rows = wrap_plain(t, self.inner, style);
        self.rows.extend(rows);
    }

    fn hint(&mut self, remaining: usize, total: Option<usize>, tail: &str) {
        let l = more_hint(self.cx, remaining, total, tail);
        self.rows.push(truncate_dots(
            l,
            self.inner as usize,
            self.cx.th().fg(Tok::Muted),
        ));
    }
}

fn title(th: &crate::theme::PiTheme, name: &str) -> Span<'static> {
    span(name.to_string(), th.bold(Tok::ToolTitle))
}

/// Path in `accent`, `...` in `toolOutput` when empty.
fn path_span(cx: &Cx, p: Option<&str>) -> Span<'static> {
    let th = cx.th();
    match p.filter(|p| !p.is_empty()) {
        Some(p) => span(shorten_path(p, &cx.home), th.fg(Tok::Accent)),
        None => span("...", th.fg(Tok::ToolOutput)),
    }
}

fn path_of(call: &ToolCall) -> Option<String> {
    arg_str(&call.input, &["file_path", "path"])
        .or_else(|| call.diff.as_ref().map(|d| d.path.clone()))
        .or_else(|| (!call.title.is_empty()).then(|| call.title.clone()))
}

fn range_span(cx: &Cx, input: &serde_json::Value) -> Option<Span<'static>> {
    let offset = arg_u64(input, &["offset"]);
    let limit = arg_u64(input, &["limit"]);
    if offset.is_none() && limit.is_none() {
        return None;
    }
    let start = offset.unwrap_or(1);
    // a model can send `limit: 0` or a limit near u64::MAX
    let end = limit
        .map(
            |l| match start.checked_add(l).and_then(|e| e.checked_sub(1)) {
                Some(e) if l > 0 => format!("-{e}"),
                _ => String::new(),
            },
        )
        .unwrap_or_default();
    Some(span(format!(":{start}{end}"), cx.th().fg(Tok::Warning)))
}

/// One tool call as a block, including the blank row it owns.
pub fn render(call: &ToolCall, tcx: &ToolCx) -> Lines {
    let nested = call.parent_id.is_some();
    let width = if nested {
        tcx.cx.width.saturating_sub(2).max(3)
    } else {
        tcx.cx.width
    };
    let mut cx = tcx.cx.clone();
    cx.width = width;
    let inner_tcx = ToolCx {
        cx: &cx,
        time: tcx.time,
        abort_message: tcx.abort_message.clone(),
    };
    let rows = render_block(call, &inner_tcx);
    if nested {
        rows.into_iter()
            .map(|l| if l.spans.is_empty() { l } else { indent(l, 2) })
            .collect()
    } else {
        rows
    }
}

fn render_block(call: &ToolCall, tcx: &ToolCx) -> Lines {
    let cx = tcx.cx;
    let th = cx.th();
    let eff = effective(call, tcx);
    let mut c = Ctx {
        cx,
        inner: cx.width.saturating_sub(2).max(1),
        rows: Vec::new(),
    };
    let kind = classify(call);
    let mut bg = if eff.error {
        Tok::ToolErrorBg
    } else if eff.done {
        Tok::ToolSuccessBg
    } else {
        Tok::ToolPendingBg
    };
    match kind {
        PiTool::Read => read(call, &eff, &mut c),
        PiTool::Bash => bash(call, &eff, tcx, &mut c),
        PiTool::Edit => edit(call, &eff, &mut c),
        PiTool::Write => write(call, &eff, &mut c),
        PiTool::Grep => grep_like(call, &eff, &mut c, "grep", 15, "matches limit"),
        PiTool::Find => grep_like(call, &eff, &mut c, "find", 20, "results limit"),
        PiTool::Ls => grep_like(call, &eff, &mut c, "ls", 20, "entries limit"),
        PiTool::Generic => generic(call, &eff, &mut c),
    }
    // a failed call keeps its error background even when the renderer found nothing to show
    if eff.error {
        bg = Tok::ToolErrorBg;
    }
    let mut out = vec![blank()];
    out.extend(boxed(c.rows, cx.width, 1, 1, th.bg(bg)));
    out
}

// ---- read ------------------------------------------------------------------------------------

fn read(call: &ToolCall, eff: &Eff, c: &mut Ctx) {
    let cx = c.cx;
    let th = cx.th();
    let path = path_of(call);
    let range = range_span(cx, &call.input);
    let expanded = cx.expanded;
    // compact forms while collapsed: skills and project instruction files
    if !expanded {
        if let Some(p) = path.as_deref() {
            let base = p.rsplit('/').next().unwrap_or(p);
            let hint = |c: &Ctx| span(format!(" ({EXPAND_KEY} to expand)"), c.th().fg(Tok::Dim));
            if base == "SKILL.md" {
                let dir = p
                    .trim_end_matches("/SKILL.md")
                    .rsplit('/')
                    .next()
                    .filter(|d| !d.is_empty())
                    .unwrap_or(base);
                let mut v = vec![
                    span("[skill] ", th.bold(Tok::CustomMessageLabel)),
                    span(dir.to_string(), th.fg(Tok::CustomMessageText)),
                ];
                v.extend(range.clone());
                v.push(hint(c));
                c.push_spans(v);
                return;
            }
            if matches!(
                base,
                "AGENTS.md" | "AGENTS.override.md" | "AGENTS.MD" | "CLAUDE.md" | "CLAUDE.MD"
            ) {
                let rel = p
                    .strip_prefix(&format!("{}/", cx.cwd.trim_end_matches('/')))
                    .map_or_else(|| shorten_path(p, &cx.home), String::from);
                let mut v = vec![
                    span("read resource", th.bold(Tok::ToolTitle)),
                    Span::raw(" "),
                    span(rel, th.fg(Tok::Accent)),
                ];
                v.extend(range.clone());
                v.push(hint(c));
                c.push_spans(v);
                return;
            }
        }
    }
    let mut v = vec![
        title(th, "read"),
        Span::raw(" "),
        path_span(cx, path.as_deref()),
    ];
    v.extend(range);
    c.push_spans(v);
    if !(expanded || eff.error) {
        return;
    }
    let Some(text) = eff.text.as_deref() else {
        return;
    };
    c.blank();
    let lang = if eff.error {
        None
    } else {
        path.as_deref().and_then(markdown_theme::lang_for_path)
    };
    let lines: Vec<&str> = text.split('\n').collect();
    let lines = trim_trailing_empty(lines);
    let max = if expanded { lines.len() } else { 10 };
    show_code(c, &lines[..max.min(lines.len())], lang);
    if lines.len() > max {
        c.hint(lines.len() - max, None, "more lines");
    }
}

/// Rows of code: highlighted when the language is known, else `toolOutput`.
fn show_code(c: &mut Ctx, lines: &[&str], lang: Option<&str>) {
    let th = c.cx.th().clone();
    let hl = lang.and_then(|l| markdown_theme::highlight_code(Some(l), &lines.join("\n"), &th));
    for (i, l) in lines.iter().enumerate() {
        match hl.as_ref().and_then(|h| h.get(i)) {
            Some(spans) => c.push_spans(spans.clone()),
            None => c.text(l, th.fg(Tok::ToolOutput)),
        }
    }
}

// ---- bash ------------------------------------------------------------------------------------

fn command_of(call: &ToolCall) -> Option<String> {
    arg_str(&call.input, &["command", "cmd"])
        .or_else(|| (!call.title.is_empty() && call.input.is_null()).then(|| call.title.clone()))
}

/// Visual rows of the bash output, the last `keep` of them with the hint on top when cut.
fn bash(call: &ToolCall, eff: &Eff, tcx: &ToolCx, c: &mut Ctx) {
    let cx = c.cx;
    let th = cx.th();
    let cmd = command_of(call);
    let mut head = vec![match cmd.as_deref().filter(|s| !s.is_empty()) {
        Some(cmd) => span(format!("$ {cmd}"), th.bold(Tok::ToolTitle)),
        None => span("$ ", th.bold(Tok::ToolTitle)),
    }];
    if cmd.as_deref().is_none_or(str::is_empty) {
        head.push(span("...", th.fg(Tok::ToolOutput)));
    }
    if let Some(t) = arg_u64(&call.input, &["timeout"]) {
        head.push(span(format!(" (timeout {t}s)"), th.fg(Tok::Muted)));
    }
    c.push_spans(head);
    let out = eff.text.clone().unwrap_or_default().trim().to_string();
    if !out.is_empty() {
        let style = th.fg(Tok::ToolOutput);
        c.blank();
        if cx.expanded {
            c.text(&out, style);
        } else {
            let visual: Lines = wrap_plain(&out, c.inner, style);
            if visual.len() <= 5 {
                c.rows.extend(visual);
            } else {
                let skipped = visual.len() - 5;
                c.hint(skipped, None, "earlier lines");
                c.rows.extend(visual[skipped..].iter().cloned());
            }
        }
    }
    if eff.error
        && eff.done
        && tcx.abort_message.is_none()
        && !out.contains("Command exited with code")
        && !out.contains("Command aborted")
        && !out.contains("Command timed out")
    {
        c.blank();
        c.push_spans(vec![span("(failed)", th.fg(Tok::Error))]);
    }
    if matches!(call.status, ToolStatus::Pending) && tcx.abort_message.is_none() {
        return;
    }
    let label = if eff.running { "Elapsed" } else { "Took" };
    c.blank();
    c.push_spans(vec![span(
        format!("{label} {}", duration(tcx.time.elapsed)),
        th.fg(Tok::Muted),
    )]);
}

// ---- edit and write ----------------------------------------------------------------------------

type EditKey = (String, bool, u64);

/// Most diffs kept; the least recently made go first.
const EDIT_CACHE_MAX: usize = 64;

/// Largest file an edit diff will read for its context. Past this the fragment alone is shown.
const EDIT_FILE_MAX: u64 = 1 << 20;

thread_local! {
    /// `(call id, settled, content hash)` to the finished diff text of the edit, so a block does
    /// not read its file and diff it again at every frame. The diff, not the file: a cache of
    /// whole-file before and after strings held 5.5 MB per call of a 2.8 MB file, for good.
    static EDIT_CACHE: RefCell<Vec<(EditKey, String)>> = const { RefCell::new(Vec::new()) };
}

/// The diff text of an edit whose fragments are `old` and `new`. When the file is on disk (a
/// regular file under the cwd, at most a megabyte) it holds `new` once the edit ran and `old`
/// before, and the diff is of the whole file with line numbers; otherwise of the fragments.
fn edit_diff(call: &ToolCall, cwd: &str, old: &str, new: &str, settled: bool) -> String {
    let key = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (path_of(call), old, new).hash(&mut h);
        (call.id.clone(), settled, h.finish())
    };
    if let Some(t) = EDIT_CACHE.with(|c| {
        c.borrow()
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, t)| t.clone())
    }) {
        return t;
    }
    let found = (|| {
        let path = path_of(call)?;
        let full = if path.starts_with('/') {
            path
        } else {
            format!("{}/{}", cwd.trim_end_matches('/'), path)
        };
        let file = tuikit::fsread::read_regular_under(&full, cwd, EDIT_FILE_MAX)?;
        let after = |f: &str| {
            (!new.is_empty() && f.contains(new)).then(|| (f.replacen(new, old, 1), f.to_string()))
        };
        let before = |f: &str| {
            (!old.is_empty() && f.contains(old)).then(|| (f.to_string(), f.replacen(old, new, 1)))
        };
        if settled {
            after(&file).or_else(|| before(&file))
        } else {
            before(&file).or_else(|| after(&file))
        }
    })();
    let text = match found {
        Some((before, after)) => diff::generate(&before, &after, 4, true),
        None => diff::generate(old, new, 4, false),
    };
    EDIT_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= EDIT_CACHE_MAX {
            c.remove(0);
        }
        c.push((key, text.clone()));
    });
    text
}

fn edit(call: &ToolCall, eff: &Eff, c: &mut Ctx) {
    let cx = c.cx;
    let th = cx.th();
    let path = path_of(call);
    c.push_spans(vec![
        title(th, "edit"),
        Span::raw(" "),
        path_span(cx, path.as_deref()),
    ]);
    if eff.error {
        if let Some(t) = eff.text.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            c.blank();
            c.text(t, th.fg(Tok::Error));
        }
        return;
    }
    let Some(d) = &call.diff else { return };
    let old = d.old.clone().unwrap_or_default();
    let text = edit_diff(call, &cx.cwd, &old, &d.new, eff.done);
    if text.is_empty() {
        return;
    }
    c.blank();
    for row in diff::render(&text, th) {
        c.push_spans(row.spans);
    }
}

fn write(call: &ToolCall, eff: &Eff, c: &mut Ctx) {
    let cx = c.cx;
    let th = cx.th();
    let path = path_of(call);
    c.push_spans(vec![
        title(th, "write"),
        Span::raw(" "),
        path_span(cx, path.as_deref()),
    ]);
    let content = arg_str(&call.input, &["content"])
        .or_else(|| call.diff.as_ref().map(|d| d.new.clone()))
        .unwrap_or_default();
    if !content.is_empty() {
        let content = clean(&content);
        let lang = path.as_deref().and_then(markdown_theme::lang_for_path);
        let lines: Vec<&str> = trim_trailing_empty(content.split('\n').collect());
        let total = lines.len();
        let max = if cx.expanded { total } else { 10 };
        c.blank();
        show_code(c, &lines[..max.min(total)], lang);
        if total > max {
            c.hint(total - max, Some(total), "");
        }
    }
    if eff.error {
        if let Some(t) = eff.text.as_deref().filter(|t| !t.is_empty()) {
            c.blank();
            c.text(t, th.fg(Tok::Error));
        }
    }
}

// ---- grep, find, ls ------------------------------------------------------------------------------

fn grep_like(
    call: &ToolCall,
    eff: &Eff,
    c: &mut Ctx,
    name: &str,
    max_collapsed: usize,
    limit_word: &str,
) {
    let cx = c.cx;
    let th = cx.th();
    let input = &call.input;
    let path = arg_str(input, &["path"]);
    let limit = arg(input, &["limit"]).map(|v| match v {
        serde_json::Value::String(s) => s.clone(),
        v => v.to_string(),
    });
    let out_style = th.fg(Tok::ToolOutput);
    let mut v = vec![title(th, name), Span::raw(" ")];
    match name {
        "grep" => {
            let pat = arg_str(input, &["pattern", "query"]).unwrap_or_default();
            v.push(span(format!("/{pat}/"), th.fg(Tok::Accent)));
            let p = path.as_deref().filter(|p| !p.is_empty()).unwrap_or(".");
            v.push(span(
                format!(" in {}", shorten_path(p, &cx.home)),
                out_style,
            ));
            if let Some(g) = arg_str(input, &["glob", "include"]).filter(|g| !g.is_empty()) {
                v.push(span(format!(" ({g})"), out_style));
            }
            if let Some(l) = &limit {
                v.push(span(format!(" limit {l}"), out_style));
            }
        }
        "find" => {
            let pat = arg_str(input, &["pattern", "glob"]).unwrap_or_default();
            v.push(span(pat, th.fg(Tok::Accent)));
            let p = path.as_deref().filter(|p| !p.is_empty()).unwrap_or(".");
            v.push(span(
                format!(" in {}", shorten_path(p, &cx.home)),
                out_style,
            ));
            if let Some(l) = &limit {
                v.push(span(format!(" (limit {l})"), out_style));
            }
        }
        _ => {
            v.push(path_span(
                cx,
                Some(path.as_deref().filter(|p| !p.is_empty()).unwrap_or(".")),
            ));
            if let Some(l) = &limit {
                v.push(span(format!(" (limit {l})"), out_style));
            }
        }
    }
    c.push_spans(v);
    let Some(text) = eff.text.as_deref() else {
        return;
    };
    let text = text.trim();
    if eff.error {
        if !text.is_empty() {
            c.blank();
            c.text(text, th.fg(Tok::Error));
        }
        return;
    }
    if !text.is_empty() {
        let lines: Vec<&str> = text.split('\n').collect();
        let max = if cx.expanded {
            lines.len()
        } else {
            max_collapsed
        };
        c.blank();
        for l in &lines[..max.min(lines.len())] {
            c.text(l, out_style);
        }
        if lines.len() > max {
            c.hint(lines.len() - max, None, "more lines");
        }
        // the tool's own notice names the limit it hit: `[5 matches limit reached. ...]`
        let needle = format!(" {limit_word} reached");
        if let Some(n) = text.lines().find_map(|l| {
            let l = l.strip_prefix('[')?;
            let i = l.find(&needle)?;
            l[..i].trim().parse::<u64>().ok()
        }) {
            c.push_spans(vec![span(
                format!("[Truncated: {n} {limit_word}]"),
                th.fg(Tok::Warning),
            )]);
        }
    }
}

// ---- generic ---------------------------------------------------------------------------------

fn generic(call: &ToolCall, eff: &Eff, c: &mut Ctx) {
    let cx = c.cx;
    let th = cx.th();
    let name = if call.name.is_empty() {
        &call.title
    } else {
        &call.name
    };
    let muted = th.fg(Tok::Muted);
    let entries: Vec<(String, serde_json::Value)> = match &call.input {
        serde_json::Value::Object(m) => m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        serde_json::Value::Null => Vec::new(),
        v => vec![("args".into(), v.clone())],
    };
    if entries.is_empty() {
        c.push_spans(vec![title(th, name)]);
    } else if cx.expanded {
        c.push_spans(vec![title(th, name)]);
        let lines: Vec<String> = entries
            .iter()
            .map(|(k, v)| {
                let t = match v {
                    serde_json::Value::String(s) => s.clone(),
                    v => serde_json::to_string_pretty(v).unwrap_or_default(),
                };
                format!(
                    "  {k}: {}",
                    clean(&t).split('\n').collect::<Vec<_>>().join("\n    ")
                )
            })
            .collect();
        c.text(&lines.join("\n"), muted);
    } else {
        let pairs = entries
            .iter()
            .map(|(k, v)| format!("{k}={}", serde_json::to_string(v).unwrap_or_default()))
            .collect::<Vec<_>>()
            .join(" ");
        let preview = if pairs.chars().count() > 100 {
            format!("{}...", pairs.chars().take(97).collect::<String>())
        } else {
            pairs
        };
        c.push_spans(vec![title(th, name), Span::raw(" "), span(preview, muted)]);
    }
    let Some(text) = eff.text.as_deref().filter(|t| !t.is_empty()) else {
        return;
    };
    let lines: Vec<&str> = text.split('\n').collect();
    let max = if cx.expanded { lines.len() } else { 10 };
    let style = if eff.error {
        th.fg(Tok::Error)
    } else {
        th.fg(Tok::ToolOutput)
    };
    for l in &lines[..max.min(lines.len())] {
        c.text(l, style);
    }
    if lines.len() > max {
        c.hint(lines.len() - max, None, "more lines");
    }
}
