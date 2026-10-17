// OWNER: tools (tool blocks; the open bodies of read, list, search, web search and fetch are in `tools_body`)
//! Tool blocks. Wizard's tool names are mapped onto Grok's block kinds here: `execute` is a
//! `Run` row, `write_file` and `edit_file` an expanded `Edit` with a diff, and reads, searches,
//! listings, web calls and subagents fold into a verb group (`◈ Read 2 files, Searched 1
//! pattern`). `todo` calls draw nothing in the scrollback (the todo pane shows them).

use std::path::Path;

use agent_core::transcript::Part;
use agent_core::{ToolCall, ToolKind, ToolStatus};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use similar::{ChangeTag, TextDiff};
use tuikit::width::{display_width, wrap, wrap_spans, WrapMode};

use super::syntax;
use super::tools_body;
use super::transcript::{fmt_thought, Entry, EntryKey, Geo, Kind, Rail, Row};
use super::{bold, st};
use crate::theme::Theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Execute,
    Read,
    List,
    Search,
    WebSearch,
    Fetch,
    Edit,
    PlanExit,
    Subagent,
    /// A command started in the background: `Task started: ...`.
    Task,
    Generic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bucket {
    Read,
    List,
    Search,
    Web,
    Fetch,
    Subagent,
}

impl Class {
    pub fn group_bucket(self) -> Option<Bucket> {
        Some(match self {
            Class::Read => Bucket::Read,
            Class::List => Bucket::List,
            Class::Search => Bucket::Search,
            Class::WebSearch => Bucket::Web,
            Class::Fetch => Bucket::Fetch,
            Class::Subagent => Bucket::Subagent,
            _ => return None,
        })
    }
}

impl Bucket {
    fn words(self, present: bool, n: usize) -> String {
        let (past, pres, one, many) = match self {
            Bucket::Read => ("Read", "Reading", "file", "files"),
            Bucket::List => ("Listed", "Listing", "dir", "dirs"),
            Bucket::Search => ("Searched", "Searching", "pattern", "patterns"),
            Bucket::Web => ("Searched", "Searching", "website", "websites"),
            Bucket::Fetch => ("Fetched", "Fetching", "website", "websites"),
            Bucket::Subagent => ("Ran", "Running", "subagent", "subagents"),
        };
        format!(
            "{} {} {}",
            if present { pres } else { past },
            n,
            if n == 1 { one } else { many }
        )
    }
}

/// Map a wizard (or mock) tool call onto a block kind. `None` means draw nothing.
pub fn classify(c: &ToolCall) -> Option<Class> {
    let name = c.name.to_lowercase();
    Some(match name.as_str() {
        "todo" | "todo_write" | "todowrite" => return None,
        "execute" | "bash" | "shell" | "run" | "run_code" | "run_command" => {
            if c.input.get("run_in_background").and_then(|v| v.as_bool()) == Some(true) {
                Class::Task
            } else {
                Class::Execute
            }
        }
        "read_file" | "read" | "view" | "cat" => Class::Read,
        "list_files" | "ls" | "list" | "glob" | "list_dir" => Class::List,
        "search_files" | "grep" | "search" | "find" => Class::Search,
        "web_search" | "x_search" | "websearch" => Class::WebSearch,
        "web_fetch" | "webfetch" | "fetch" => Class::Fetch,
        "write_file" | "edit_file" | "edit" | "write" | "multiedit" | "create_file" => Class::Edit,
        "exit_plan" | "exitplanmode" | "exit_plan_mode" => Class::PlanExit,
        "spawn_subagent" | "task" | "agent" | "subagent" => Class::Subagent,
        _ => match c.kind {
            ToolKind::Execute => Class::Execute,
            ToolKind::Edit => Class::Edit,
            ToolKind::Fetch => Class::Fetch,
            ToolKind::Search => Class::Search,
            ToolKind::Read => Class::Generic,
            _ => Class::Generic,
        },
    })
}

/// A `!cmd` the user ran, as opposed to one the agent did.
pub fn is_user(c: &ToolCall) -> bool {
    c.input.get("user").and_then(|v| v.as_bool()) == Some(true)
}

pub(super) fn running(c: &ToolCall) -> bool {
    matches!(c.status, ToolStatus::Pending | ToolStatus::Running)
}

fn failed(c: &ToolCall) -> bool {
    c.status == ToolStatus::Failed
}

/// The accent a block of this status is drawn in.
fn accent(c: &ToolCall, th: &Theme) -> Color {
    if running(c) {
        th.accent_running
    } else if failed(c) {
        th.accent_error
    } else {
        th.accent_success
    }
}

fn first_line(s: &str) -> &str {
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or("")
}

pub(super) fn input_str<'a>(c: &'a ToolCall, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| c.input.get(*k).and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())
}

pub fn command_of(c: &ToolCall) -> String {
    input_str(c, &["command", "cmd", "script", "code"])
        .map(str::to_string)
        .unwrap_or_else(|| c.title.clone())
}

pub(super) fn path_of(c: &ToolCall) -> String {
    input_str(c, &["path", "file_path", "file", "filePath"])
        .map(str::to_string)
        .unwrap_or_else(|| c.title.clone())
}

/// `path` relative to `cwd` when inside it.
pub fn rel_path(path: &str, cwd: &Path) -> String {
    let p = Path::new(path);
    match p.strip_prefix(cwd) {
        Ok(r) if !r.as_os_str().is_empty() => r.display().to_string(),
        _ => path.to_string(),
    }
}

fn basename(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
}

pub(super) fn titlecase(name: &str) -> String {
    let s = name.replace(['_', '-'], " ");
    let mut cs = s.chars();
    match cs.next() {
        Some(f) => f.to_uppercase().collect::<String>() + cs.as_str(),
        None => String::new(),
    }
}

pub fn tool_entry(
    key: EntryKey,
    call: &ToolCall,
    class: Class,
    expanded: Option<bool>,
    geo: &Geo,
    th: &Theme,
    cwd: &Path,
) -> Entry {
    match class {
        // a `!cmd` block opens when it starts; an agent's command stays one line
        Class::Execute => exec_entry(key, call, expanded.unwrap_or(is_user(call)), geo, th),
        Class::Edit => edit_entry(key, call, expanded.unwrap_or(true), geo, th, cwd),
        Class::PlanExit => generic_row(key, call, "Plan", "Exit", geo, th),
        Class::Task => {
            let what = input_str(call, &["description"])
                .map(str::to_string)
                .unwrap_or_else(|| first_line(&command_of(call)).to_string());
            generic_row(key, call, "Task", &format!("started: {what}"), geo, th)
        }
        // an open read, list, search, web search or fetch shows what it found
        Class::Read | Class::List | Class::Search | Class::WebSearch | Class::Fetch
            if expanded == Some(true) =>
        {
            tools_body::open_entry(key, call, class, geo, th, cwd)
        }
        _ => {
            let name = titlecase(&call.name);
            let detail = call.title.clone();
            generic_row(key, call, &name, &detail, geo, th)
        }
    }
}

/// A collapsed one-line block: bullet, bold label, muted detail.
fn generic_row(
    key: EntryKey,
    c: &ToolCall,
    label: &str,
    detail: &str,
    geo: &Geo,
    th: &Theme,
) -> Entry {
    let mut r = bullet_row(c, geo, th);
    let mut x = geo.cx + 2;
    r = r.seg(x, label.to_string(), bold(th.gray));
    x += display_width(label) as u16;
    if !detail.is_empty() {
        let room = (geo.cx as usize + geo.cw).saturating_sub(x as usize + 1);
        let d = tuikit::width::truncate(first_line(detail), room);
        r = r.seg(x, format!(" {d}"), st(th.gray));
    }
    Entry {
        key,
        kind: Kind::Tool,
        rows: vec![r],
        groupable: true,
        collapsed: true,
        selectable: true,
        foldable: false,
        running: running(c),
        members: Vec::new(),
    }
}

/// `◆ ` of a collapsed block: the glyph at half strength (or pulsing while it runs), the space
/// after it at full strength.
fn bullet_row(c: &ToolCall, geo: &Geo, th: &Theme) -> Row {
    let acc = accent(c, th);
    let r = Row::new();
    if running(c) {
        r.wave(geo.cx, "◆", st(acc), acc)
            .seg(geo.cx + 1, " ", st(acc))
    } else {
        r.seg(geo.cx, "◆", st(th.dim_accent(acc)))
            .seg(geo.cx + 1, " ", st(acc))
    }
}

// ---- execute --------------------------------------------------------------------------------

fn exec_entry(key: EntryKey, c: &ToolCall, expanded: bool, geo: &Geo, th: &Theme) -> Entry {
    let cmd = command_of(c);
    // a description, when the call has one, stands in for the command on the header row
    let head =
        input_str(c, &["description"]).map_or_else(|| first_line(&cmd).to_string(), str::to_string);
    let user = is_user(c);
    // `(user) ` stands between the label and the title of a `!cmd` block
    let tag = if user { "(user) " } else { "" };
    let title_x = geo.cx + 6 + tag.len() as u16;
    if !expanded {
        let mut r = bullet_row(c, geo, th);
        r = r.seg(geo.cx + 2, "Run", bold(th.gray));
        if user {
            r = r.seg(geo.cx + 6, tag, st(th.gray));
        }
        let room = (geo.cx as usize + geo.cw).saturating_sub(title_x as usize + 1);
        r = r.seg(title_x, tuikit::width::truncate(&head, room), st(th.gray));
        return Entry {
            key,
            kind: Kind::Tool,
            rows: vec![r],
            groupable: true,
            collapsed: true,
            selectable: true,
            foldable: true,
            running: running(c),
            members: Vec::new(),
        };
    }
    let acc = accent(c, th);
    let rail = if running(c) {
        Rail::Wave(acc)
    } else {
        Rail::Solid(acc)
    };
    let mut rows: Vec<Row> = Vec::new();
    let mut r = Row::new().rail(rail);
    r = if running(c) {
        r.wave(geo.cx, "◆", st(acc), acc)
            .seg(geo.cx + 1, " ", st(acc))
    } else {
        r.seg(geo.cx, "◆ ", st(acc))
    };
    r = r.seg(geo.cx + 2, "Run", bold(th.text_primary));
    if user {
        r = r.seg(geo.cx + 6, tag, st(th.gray));
    }
    let room = geo.cw.saturating_sub(6 + tag.len());
    r = r.seg(
        title_x,
        tuikit::width::truncate(&head, room),
        st(th.text_primary),
    );
    rows.push(r);
    // `$ command`, highlighted as shell, wrapped under the prompt
    let lines = syntax::highlight(th.syntax(), "bash", &cmd, th.accent_skill);
    for (i, spans) in lines.iter().enumerate() {
        let spans = if spans.is_empty() {
            vec![Span::raw("")]
        } else {
            spans.clone()
        };
        let w = geo.cw.saturating_sub(2).max(10);
        for (j, row_spans) in wrap_spans(&spans, w, WrapMode::Word)
            .into_iter()
            .enumerate()
        {
            let mut rr = Row::new().rail(rail);
            if i == 0 && j == 0 {
                rr = rr.seg(geo.cx, "$", st(th.gray_dim));
            }
            rr = rr.spans(geo.cx + 2, &row_spans);
            rows.push(rr);
        }
    }
    rows.push(Row::new().rail(rail));
    let out = c.output.clone().unwrap_or_default();
    let w = geo.cw.saturating_sub(2).max(20);
    let mut shown = 0usize;
    let all: Vec<String> = out
        .trim_end_matches('\n')
        .split('\n')
        .map(|l| l.replace('\t', "    "))
        .collect();
    let cap = 400usize;
    for line in all.iter().take(cap) {
        for l in wrap(line, w) {
            rows.push(
                Row::new()
                    .rail(rail)
                    .fill(geo.cx, geo.cx + geo.cw as u16, th.bg_dark)
                    .seg(geo.cx, l, st(th.text_primary).bg(th.bg_dark)),
            );
            shown += 1;
        }
    }
    if all.len() > cap {
        rows.push(
            Row::new()
                .rail(rail)
                .fill(geo.cx, geo.cx + geo.cw as u16, th.bg_dark)
                .seg(
                    geo.cx,
                    format!("… +{} lines", all.len() - cap),
                    st(th.gray).bg(th.bg_dark),
                ),
        );
    }
    if out.trim().is_empty() && !running(c) && shown == 0 {
        // no output: the block ends after the blank row
        rows.pop();
    }
    for (i, r) in rows.iter_mut().enumerate() {
        r.brow = i;
    }
    Entry {
        key,
        kind: Kind::Tool,
        rows,
        groupable: true,
        collapsed: false,
        selectable: true,
        foldable: true,
        running: running(c),
        members: Vec::new(),
    }
}

// ---- edit -----------------------------------------------------------------------------------

/// One diff row before layout.
#[derive(Clone, Debug)]
struct DiffLine {
    tag: ChangeTag,
    no: usize,
    spans: Vec<Span<'static>>,
}

enum DiffItem {
    Line(DiffLine),
    Gap(usize),
}

fn line_spans(
    hl: &[Vec<Span<'static>>],
    i: usize,
    text: &str,
    fallback: Color,
) -> Vec<Span<'static>> {
    match hl.get(i) {
        Some(s) if !s.is_empty() => s.clone(),
        _ if text.is_empty() => Vec::new(),
        _ => vec![Span::styled(text.to_string(), Style::new().fg(fallback))],
    }
}

fn diff_items(old: Option<&str>, new: &str, lang: &str, base: usize, th: &Theme) -> Vec<DiffItem> {
    let old_t = old.unwrap_or("");
    let hl_old = syntax::highlight(th.syntax(), lang, old_t, th.md_text);
    let hl_new = syntax::highlight(th.syntax(), lang, new, th.md_text);
    let diff = TextDiff::from_lines(old_t, new);
    let mut out = Vec::new();
    let groups = diff.grouped_ops(3);
    let mut prev_end: Option<usize> = None;
    for g in &groups {
        // unchanged lines between hunks are folded into a separator
        if let Some(first) = g.first() {
            if let Some(pe) = prev_end {
                let skipped = first.new_range().start.saturating_sub(pe);
                if skipped > 0 {
                    out.push(DiffItem::Gap(skipped));
                }
            }
        }
        for op in g {
            for ch in diff.iter_changes(op) {
                let text = ch.value().trim_end_matches('\n');
                match ch.tag() {
                    ChangeTag::Equal => {
                        let i = ch.new_index().unwrap_or(0);
                        out.push(DiffItem::Line(DiffLine {
                            tag: ChangeTag::Equal,
                            no: i + base,
                            spans: line_spans(&hl_new, i, text, th.md_text),
                        }));
                    }
                    ChangeTag::Delete => {
                        let i = ch.old_index().unwrap_or(0);
                        out.push(DiffItem::Line(DiffLine {
                            tag: ChangeTag::Delete,
                            no: i + base,
                            spans: line_spans(&hl_old, i, text, th.md_text),
                        }));
                    }
                    ChangeTag::Insert => {
                        let i = ch.new_index().unwrap_or(0);
                        out.push(DiffItem::Line(DiffLine {
                            tag: ChangeTag::Insert,
                            no: i + base,
                            spans: line_spans(&hl_new, i, text, th.md_text),
                        }));
                    }
                }
            }
        }
        prev_end = g.last().map(|o| o.new_range().end);
    }
    out
}

/// A snippet edit (`old` to `new`) widened to whole lines with three lines of context either
/// side, read from the file as it is on disk now, and the number of the first line shown.
/// Wizard sends only the replaced text, so without this a diff is numbered from 1 and has no
/// context. `None` when the file cannot be read or does not contain `new`.
fn with_context(path: &str, old: &str, new: &str, cwd: &Path) -> Option<(String, String, usize)> {
    let p = if Path::new(path).is_absolute() {
        Path::new(path).to_path_buf()
    } else {
        cwd.join(path)
    };
    let text = std::fs::read_to_string(p).ok()?;
    let needle = new.trim_end_matches('\n');
    if needle.is_empty() {
        return None;
    }
    let at = text.find(needle)?;
    let end = at + needle.len();
    // widen to whole lines
    let line_start = text[..at].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[end..].find('\n').map_or(text.len(), |i| end + i);
    let first_line = text[..line_start].matches('\n').count();
    // three lines of context before and after
    let before: Vec<&str> = text[..line_start].lines().collect();
    let take_b = before.len().min(3);
    let ctx_before = before[before.len() - take_b..].join("\n");
    let after: Vec<&str> = if line_end < text.len() {
        text[line_end + 1..].lines().take(3).collect()
    } else {
        Vec::new()
    };
    let ctx_after = after.join("\n");
    let pre = &text[line_start..at];
    let post = &text[end..line_end];
    let join = |mid: &str| {
        let mut out = String::new();
        if take_b > 0 {
            out.push_str(&ctx_before);
            out.push('\n');
        }
        out.push_str(pre);
        out.push_str(mid.trim_end_matches('\n'));
        out.push_str(post);
        out.push('\n');
        if !after.is_empty() {
            out.push_str(&ctx_after);
            out.push('\n');
        }
        out
    };
    Some((join(old), join(new), first_line - take_b + 1))
}

fn edit_entry(
    key: EntryKey,
    c: &ToolCall,
    expanded: bool,
    geo: &Geo,
    th: &Theme,
    cwd: &Path,
) -> Entry {
    let path = c
        .diff
        .as_ref()
        .map(|d| d.path.clone())
        .unwrap_or_else(|| path_of(c));
    let shown = if expanded {
        rel_path(&path, cwd)
    } else {
        basename(&path).to_string()
    };
    let bullet = if failed(c) {
        th.accent_error
    } else {
        th.gray_bright
    };
    let mut head = Row::new().seg(
        geo.cx,
        "◆ ",
        st(if failed(c) {
            th.dim_accent(bullet)
        } else {
            bullet
        }),
    );
    head = head.seg(geo.cx + 2, "Edit", bold(th.text_primary));
    let room = (geo.cx as usize + geo.cw).saturating_sub(geo.cx as usize + 8);
    let mut rows = vec![head];
    // long paths break after a `/` and continue seven columns in
    let chunks = wrap_path(&shown, room.max(8), (geo.cw).saturating_sub(7).max(8));
    for (i, chunk) in chunks.iter().enumerate() {
        if i == 0 {
            rows[0] = std::mem::take(&mut rows[0]).seg(geo.cx + 7, chunk.clone(), st(th.path));
        } else {
            rows.push(Row::new().seg(geo.cx + 7, chunk.clone(), st(th.path)));
        }
    }
    if failed(c) || !expanded {
        if failed(c) {
            let out = c.output.clone().unwrap_or_default();
            for l in out.lines().take(6) {
                for w in wrap(l, geo.cw.saturating_sub(2)) {
                    rows.push(Row::new().seg(geo.cx + 2, w, st(th.gray)));
                }
            }
        }
        return Entry {
            key,
            kind: Kind::Tool,
            rows,
            groupable: true,
            collapsed: true,
            selectable: true,
            foldable: !failed(c),
            running: false,
            members: Vec::new(),
        };
    }
    rows.push(Row::new());
    if let Some(d) = &c.diff {
        let lang = syntax::lang_for_path(&d.path);
        let widened = d
            .old
            .as_deref()
            .and_then(|old| with_context(&d.path, old, &d.new, cwd));
        let items = match &widened {
            Some((old, new, base)) => diff_items(Some(old), new, lang, *base, th),
            None => diff_items(d.old.as_deref(), &d.new, lang, 1, th),
        };
        let maxno = items
            .iter()
            .filter_map(|i| match i {
                DiffItem::Line(l) => Some(l.no),
                _ => None,
            })
            .max()
            .unwrap_or(1);
        let nw = maxno.to_string().len();
        let gutter = (2 + nw + 2) as u16;
        let body_x = geo.cx + gutter;
        let body_w = (geo.cw as u16).saturating_sub(gutter).max(8) as usize;
        for item in items {
            match item {
                DiffItem::Gap(n) => {
                    let t = if n == 1 {
                        "… 1 unchanged line".to_string()
                    } else {
                        format!("… {n} unchanged lines")
                    };
                    rows.push(Row::new().seg(geo.cx + 2, t, st(th.gray)));
                }
                DiffItem::Line(l) => {
                    let (num_c, bg) = match l.tag {
                        ChangeTag::Delete => (th.diff_delete_fg, Some(th.diff_delete_bg)),
                        ChangeTag::Insert => (th.diff_insert_fg, Some(th.diff_insert_bg)),
                        ChangeTag::Equal => (th.diff_gutter_fg, None),
                    };
                    let spans: Vec<Span<'static>> = l
                        .spans
                        .iter()
                        .map(|s| {
                            let t = s.content.replace('\t', "    ");
                            let mut stl = s.style;
                            if th.bandless && l.tag != ChangeTag::Equal {
                                // no bands to carry the change: the whole line takes its colour
                                stl = stl.fg(num_c);
                            }
                            if let Some(b) = bg {
                                stl = stl.bg(b);
                            }
                            Span::styled(t, stl)
                        })
                        .collect();
                    let wrapped = if spans.is_empty() {
                        vec![Vec::new()]
                    } else {
                        wrap_spans(&spans, body_w, WrapMode::Word)
                    };
                    for (i, rs) in wrapped.into_iter().enumerate() {
                        let mut r = Row::new();
                        if let Some(b) = bg {
                            r = r.fill(body_x, geo.cx + geo.cw as u16, b);
                        }
                        if i == 0 {
                            let num = format!("{:>w$}", l.no, w = nw);
                            r = r.seg(geo.cx + 2, num, st(num_c));
                        }
                        r = r.spans(body_x, &rs);
                        rows.push(r);
                    }
                }
            }
        }
    } else if let Some(out) = &c.output {
        for l in out.lines().take(20) {
            rows.push(Row::new().seg(geo.cx + 2, l.to_string(), st(th.gray)));
        }
    }
    Entry {
        key,
        kind: Kind::Tool,
        rows,
        groupable: true,
        collapsed: false,
        selectable: true,
        foldable: true,
        running: running(c),
        members: Vec::new(),
    }
}

/// Split a path into rows: the first `first` wide, the rest `rest`, breaking after a `/`.
fn wrap_path(p: &str, first: usize, rest: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut limit = first;
    for seg in p.split_inclusive('/') {
        if display_width(&cur) + display_width(seg) > limit && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            limit = rest;
        }
        cur.push_str(seg);
        while display_width(&cur) > limit {
            let mut cut = String::new();
            for ch in cur.chars() {
                if display_width(&cut) + 1 > limit {
                    break;
                }
                cut.push(ch);
            }
            let tail = cur[cut.len()..].to_string();
            out.push(cut);
            cur = tail;
            limit = rest;
        }
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

// ---- verb groups ----------------------------------------------------------------------------

fn bucket_of(c: &ToolCall) -> Option<Bucket> {
    classify(c).and_then(Class::group_bucket)
}

pub fn group_label(members: &[&Part], th: &Theme) -> (Vec<Span<'static>>, bool, bool) {
    let mut order: Vec<(Bucket, usize)> = Vec::new();
    let mut any_running = false;
    let mut failures = 0usize;
    for p in members {
        if let Part::Tool(c) = p {
            if let Some(b) = bucket_of(c) {
                match order.iter_mut().find(|(x, _)| *x == b) {
                    Some((_, n)) => *n += 1,
                    None => order.push((b, 1)),
                }
            }
            any_running |= running(c);
            failures += failed(c) as usize;
        }
    }
    // websites are the distinct URLs the searches returned, when the output says
    let web: Vec<&ToolCall> = members
        .iter()
        .filter_map(|p| match p {
            Part::Tool(c) if bucket_of(c) == Some(Bucket::Web) => Some(c),
            _ => None,
        })
        .collect();
    let sites = tools_body::distinct_sites(&web);
    if sites > 0 {
        if let Some((_, n)) = order.iter_mut().find(|(b, _)| *b == Bucket::Web) {
            *n = sites;
        }
    }
    let text = order
        .iter()
        .map(|(b, n)| b.words(any_running, *n))
        .collect::<Vec<_>>()
        .join(", ");
    let mut spans = vec![Span::styled(
        text,
        Style::new().fg(th.gray_bright).add_modifier(Modifier::BOLD),
    )];
    if failures > 0 {
        spans.push(Span::styled(
            format!(" · {failures} failed"),
            st(th.accent_error),
        ));
    }
    (spans, any_running, failures > 0)
}

pub fn group_entry(
    key: EntryKey,
    members: &[(EntryKey, &Part)],
    expanded: bool,
    geo: &Geo,
    th: &Theme,
    cwd: &Path,
    opened: &dyn Fn(EntryKey) -> bool,
) -> Entry {
    let parts: Vec<&Part> = members.iter().map(|m| m.1).collect();
    let (label, any_running, any_failed) = group_label(&parts, th);
    let mut head = Row::new();
    let acc = if any_failed {
        th.accent_error
    } else {
        th.accent_running
    };
    head = if any_running {
        head.wave(geo.cx, "◈", st(acc), acc)
            .wave(geo.cx + 1, " ", st(acc), acc)
    } else {
        head.seg(
            geo.cx,
            "◈ ",
            st(if any_failed {
                th.accent_error
            } else {
                th.gray_solid()
            }),
        )
    };
    head = head.spans(geo.cx + 2, &label);
    let mut rows = vec![head];
    let mut mem = Vec::new();
    if expanded {
        for (k, p) in members {
            mem.push(super::transcript::Member {
                key: *k,
                row: rows.len(),
                sel: member_row(p, geo, th, true),
                open: opened(*k),
            });
            match (opened(*k), p) {
                // a member opened with `→` shows its body in place
                (true, Part::Tool(c))
                    if classify(c).is_some_and(|cl| cl.group_bucket().is_some()) =>
                {
                    let class = classify(c).unwrap_or(Class::Generic);
                    rows.extend(tools_body::open_entry(*k, c, class, geo, th, cwd).rows);
                }
                _ => rows.push(member_row(p, geo, th, false)),
            }
        }
    }
    Entry {
        key,
        kind: Kind::Group,
        rows,
        groupable: true,
        collapsed: !expanded,
        selectable: true,
        foldable: true,
        running: any_running,
        members: mem,
    }
}

fn member_row(p: &Part, geo: &Geo, th: &Theme, selected: bool) -> Row {
    let bg = |s: Style| if selected { s.bg(th.bg_dark) } else { s };
    match p {
        Part::Thought { took, .. } => {
            let glyph = if selected { "› " } else { "◆ " };
            let label = if selected { th.text_primary } else { th.gray };
            let mut r = Row::new().seg(geo.cx, glyph, bg(st(th.gray))).seg(
                geo.cx + 2,
                "Thought",
                bg(bold(label)),
            );
            if let Some(d) = took {
                if d.as_millis() > 0 {
                    r = r.seg(
                        geo.cx + 9,
                        format!(" for {}", fmt_thought(*d)),
                        bg(st(label)),
                    );
                }
            }
            r
        }
        Part::Tool(c) => match classify(c) {
            Some(class) => tools_body::tool_member_row(c, class, geo, th, selected),
            None => Row::new(),
        },
        Part::Text(_) => Row::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, kind: ToolKind) -> ToolCall {
        ToolCall {
            name: name.into(),
            kind,
            ..Default::default()
        }
    }

    #[test]
    fn wizard_tool_names_map_onto_block_kinds() {
        assert_eq!(
            classify(&call("execute", ToolKind::Execute)),
            Some(Class::Execute)
        );
        assert_eq!(
            classify(&call("read_file", ToolKind::Read)),
            Some(Class::Read)
        );
        assert_eq!(
            classify(&call("list_files", ToolKind::Read)),
            Some(Class::List)
        );
        assert_eq!(
            classify(&call("search_files", ToolKind::Search)),
            Some(Class::Search)
        );
        assert_eq!(
            classify(&call("web_search", ToolKind::Search)),
            Some(Class::WebSearch)
        );
        assert_eq!(
            classify(&call("edit_file", ToolKind::Edit)),
            Some(Class::Edit)
        );
        assert_eq!(
            classify(&call("write_file", ToolKind::Edit)),
            Some(Class::Edit)
        );
        assert_eq!(classify(&call("todo", ToolKind::Other)), None);
        assert_eq!(
            classify(&call("exit_plan", ToolKind::Other)),
            Some(Class::PlanExit)
        );
        assert_eq!(
            classify(&call("memory", ToolKind::Other)),
            Some(Class::Generic)
        );
    }

    #[test]
    fn long_paths_break_after_a_slash() {
        let r = wrap_path("/home/nixos/.grok/sessions/abc/plan.md", 20, 20);
        assert!(r.len() >= 2);
        assert!(r[0].ends_with('/'), "{r:?}");
        assert_eq!(r.concat(), "/home/nixos/.grok/sessions/abc/plan.md");
    }
}
