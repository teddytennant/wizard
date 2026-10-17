//! The transcript as rows: the header, then one block per message, tool call or notice with a
//! blank row before each. Finished reads, searches and commands fold into one summary row; the
//! detailed view (ctrl+o) shows every call with its output.

use agent_core::{NoticeLevel, TodoStatus, ToolCall, ToolKind, ToolStatus};
use ratatui::style::{Modifier, Style};

use crate::app::{clock_text, App, Block, DONE_VERBS};
use crate::text::{wrap, wrap_str, Row, Span};
use crate::theme::Theme;
use crate::ui::prompt::fmt_secs;
use crate::ui::{diff, markdown};

/// The Wizard mark: a hat, in the nine columns Claude Code's mascot takes.
pub const MARK: [&str; 3] = ["    ▟▙   ", "   ▟██▙  ", " ▝▀▀▀▀▀▀▘"];

pub fn rows(app: &App, w: usize, detail: bool) -> Vec<Row> {
    app.links.borrow_mut().clear();
    let th = app.theme;
    let mut out = header(app, w);
    let blocks = &app.blocks;
    let mut i = 0;
    while i < blocks.len() {
        match &blocks[i] {
            Block::User { text, bash } => {
                out.push(Row::new());
                let mut rows = user(&th, text, *bash, w);
                // a prompt that waited in the queue stays grey until its turn shows something
                if app.from_queue.contains(&i) && app.busy.is_some() && i + 1 == blocks.len() {
                    for r in &mut rows {
                        for (t, st) in r.0.iter_mut() {
                            if !t.trim().is_empty() && !t.starts_with('❯') {
                                st.fg = Some(th.secondary);
                            }
                        }
                    }
                }
                out.extend(rows);
            }
            Block::Output {
                text,
                accent,
                dim,
                muted,
            } => out.extend(output(&th, text, accent, *dim, *muted, w)),
            Block::Assistant { text, time } => {
                if text.trim().is_empty() {
                    i += 1;
                    continue;
                }
                out.push(Row::new());
                if detail {
                    let stamp = format!("{} {}", clock_text(*time, true), app.cfg.model);
                    let pad = w.saturating_sub(stamp.chars().count() + 8);
                    out.push(Row::of(" ".repeat(pad), Style::new()).with(stamp, th.dim()));
                }
                out.extend(assistant(app, text, w));
            }
            Block::Thought(text) => {
                if detail && !text.trim().is_empty() {
                    out.push(Row::new());
                    let st = th.dim().add_modifier(Modifier::ITALIC);
                    out.push(Row::of("✻ Thinking…", st));
                    for r in wrap_str(text.trim(), st, w.saturating_sub(4)) {
                        out.push(Row::of("  ", Style::new()).with(r.text(), st));
                    }
                }
            }
            Block::Tool(_) => {
                let mut j = i;
                let mut group: Vec<&ToolCall> = Vec::new();
                while let Some(Block::Tool(t)) = blocks.get(j) {
                    if detail || !folds(t) {
                        break;
                    }
                    group.push(t);
                    j += 1;
                }
                if group.is_empty() {
                    let Block::Tool(t) = &blocks[i] else {
                        unreachable!()
                    };
                    out.push(Row::new());
                    out.extend(tool(app, t, w, detail));
                } else {
                    out.push(Row::new());
                    out.extend(folded(&th, &group, w));
                    i = j - 1;
                }
            }
            Block::Todos(todos) => {
                if app.show_todos && !todos.is_empty() {
                    out.push(Row::new());
                    out.push(Row::of("● ", th.fg(th.success)).with("Update Todos", th.bold()));
                    for (n, t) in todos.iter().enumerate() {
                        let lead = if n == 0 { ELBOW } else { "     " };
                        let (mark, st) = match t.status {
                            TodoStatus::Completed => {
                                ("☒ ", th.dim().add_modifier(Modifier::CROSSED_OUT))
                            }
                            TodoStatus::InProgress => ("☐ ", th.bold()),
                            TodoStatus::Pending => ("☐ ", Style::new()),
                        };
                        out.push(
                            Row::of(lead, th.dim())
                                .with(mark, Style::new())
                                .with(t.text.clone(), st),
                        );
                    }
                }
            }
            Block::TurnEnd { verb, secs, time } => {
                out.push(Row::new());
                out.push(Row::of(
                    format!(
                        "✻ {} for {} · done {}",
                        DONE_VERBS[*verb % DONE_VERBS.len()],
                        fmt_secs(*secs),
                        clock_text(*time, false)
                    ),
                    th.dim(),
                ));
            }
            Block::Notice { level, text } => {
                let color = match level {
                    NoticeLevel::Info => th.secondary,
                    NoticeLevel::Warn => th.warning,
                    NoticeLevel::Error => th.error,
                };
                // a command's answer with fenced blocks (/diff) is markdown, drawn under ⎿
                if *level == NoticeLevel::Info && text.contains("```") {
                    let mut links = app.links.borrow_mut();
                    for (k, r) in markdown::render(&th, text, w.saturating_sub(5), &mut links)
                        .into_iter()
                        .enumerate()
                    {
                        let mut row = if k == 0 {
                            Row::of(ELBOW, th.dim())
                        } else {
                            Row::of("     ", Style::new())
                        };
                        row.extend(r);
                        out.push(row);
                    }
                } else {
                    out.extend(hanging(&th, text, th.fg(color), w));
                }
            }
        }
        i += 1;
    }
    // prompts typed during a turn wait under it, greyed, until it ends
    for (n, q) in app.queued.iter().enumerate() {
        out.push(Row::new());
        let st = Style::new().bg(th.user_bg).fg(th.secondary);
        for (i, r) in wrap_str(q, st, w.saturating_sub(4)).into_iter().enumerate() {
            let mut row = Row::of(if i == 0 { "❯ " } else { "  " }, st);
            row.extend(r);
            out.push(row);
        }
        // only a turn can be cut short for them; a mode change or a side question cannot
        if n + 1 == app.queued.len() && app.busy.is_some() {
            out.push(Row::of("  ctrl+enter to send now", th.dim()));
        }
    }
    out
}

fn header(app: &App, w: usize) -> Vec<Row> {
    let th = app.theme;
    let brand = th.fg(th.brand);
    let mut model = app.model_name();
    if !matches!(app.cfg.effort.as_str(), "" | "medium" | "default") {
        model.push_str(&format!(" with {} effort", app.cfg.effort));
    }
    let provider = app.provider();
    if !provider.is_empty() {
        model.push_str(&format!(" · {provider}"));
    }
    let room = w.saturating_sub(11);
    vec![
        Row::new(),
        Row::of(MARK[0], brand)
            .with("  ", Style::new())
            .with("Wizard", th.bold())
            .with(" ", Style::new())
            .with(
                match app.wizard_version() {
                    v if v.is_empty() => String::new(),
                    v => format!("v{v}"),
                },
                th.dim(),
            ),
        Row::of(MARK[1], brand)
            .with("  ", Style::new())
            .with(crate::text::truncate(&model, room), th.dim()),
        Row::of(MARK[2], brand)
            .with("  ", Style::new())
            .with(short_cwd(&app.opts.cwd.to_string_lossy(), w), th.dim()),
        Row::new(),
    ]
}

/// The working directory for the header: `~` for home, and leading components dropped behind
/// `/…/` until it fits.
pub fn short_cwd(cwd: &str, w: usize) -> String {
    let mut path = cwd.to_string();
    if let Some(home) = std::env::var_os("HOME").and_then(|h| h.into_string().ok()) {
        if !home.is_empty() && (path == home || path.starts_with(&format!("{home}/"))) {
            path = format!("~{}", &path[home.len()..]);
        }
    }
    let limit = w.saturating_sub(14);
    if path.chars().count() <= limit {
        return path;
    }
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    for skip in 1..parts.len() {
        let cand = format!("/…/{}", parts[skip..].join("/"));
        if cand.chars().count() <= limit {
            return cand;
        }
    }
    format!("/…/{}", parts.last().copied().unwrap_or(""))
}

fn user(th: &Theme, text: &str, bash: bool, w: usize) -> Vec<Row> {
    let bg = Style::new().bg(if bash { th.bash_bg } else { th.user_bg });
    let body = bg.fg(th.user_fg);
    let spans = if bash {
        vec![(text.trim_end().to_string(), body)]
    } else {
        markdown::inline_with(th, text.trim_end(), body, None)
    };
    // the first row of what you sent runs one column further than the rest
    let lines = crate::text::wrap_first(&spans, w.saturating_sub(3), w.saturating_sub(4));
    let last = lines.len() - 1;
    lines
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            let mut row = match (i, bash) {
                (0, true) => Row::of("! ", bg.fg(th.bash)),
                (0, false) => Row::of("❯", bg.fg(th.secondary)).with(" ", bg),
                _ => Row::of("  ", bg),
            };
            row.extend(line);
            if i == last && !bash && text.ends_with(' ') && text.starts_with('/') {
                row.push(" ", bg);
            }
            // a wrapped row keeps the space it broke at
            if i < last {
                row.push(" ", bg);
            }
            row
        })
        .collect()
}

fn assistant(app: &App, text: &str, w: usize) -> Vec<Row> {
    let th = app.theme;
    let mut links = app.links.borrow_mut();
    markdown::render(&th, text, w.saturating_sub(3), &mut links)
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            let mut row = if i == 0 {
                Row::of("●", th.fg(th.bullet)).with(" ", Style::new())
            } else {
                Row::of("  ", Style::new())
            };
            row.extend(r);
            row
        })
        .collect()
}

/// The elbow a tool's result hangs from. The last cell is a no-break space, as Claude Code
/// writes it.
pub const ELBOW: &str = "  ⎿ \u{a0}";

/// `  ⎿  text`, later lines under the text.
fn hanging(th: &Theme, text: &str, style: Style, w: usize) -> Vec<Row> {
    wrap_str(text.trim_end(), style, w.saturating_sub(10))
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            let mut row = if i == 0 {
                Row::of(ELBOW, th.dim())
            } else {
                Row::of("     ", Style::new())
            };
            row.extend(r);
            row
        })
        .collect()
}

fn output(th: &Theme, text: &str, accent: &str, dim: bool, muted: bool, w: usize) -> Vec<Row> {
    let base = if muted {
        th.dim()
    } else if dim {
        Style::new().add_modifier(Modifier::DIM)
    } else {
        Style::new()
    };
    let mut spans: Vec<Span> = Vec::new();
    match text.find(accent) {
        Some(at) if !accent.is_empty() => {
            spans.push((text[..at].to_string(), base));
            spans.push((accent.to_string(), base.fg(th.suggestion)));
            spans.push((text[at + accent.len()..].to_string(), base));
        }
        _ => spans.push((text.to_string(), base)),
    }
    wrap(&spans, w.saturating_sub(7))
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            let mut row = if i == 0 && muted {
                Row::of(ELBOW, th.dim())
            } else if i == 0 {
                Row::of("  ⎿", th.dim()).with("  ", Style::new())
            } else {
                Row::of("     ", Style::new())
            };
            row.extend(r);
            row
        })
        .collect()
}

/// Reads, searches, shell commands and fetches fold into one summary row once done, as the
/// reference folds them. Any other tool keeps its own row: there is no "used N tools".
fn folds(t: &ToolCall) -> bool {
    let done = matches!(t.status, ToolStatus::Completed | ToolStatus::Failed);
    let local = t.input.get("local").is_some();
    done && !local
        && t.kind != ToolKind::Edit
        && t.diff.is_none()
        && !is_plan_call(t)
        && bucket(t).0 != "used"
}

pub fn is_plan_call(t: &ToolCall) -> bool {
    t.name == "exit_plan" || t.title.starts_with("exit_plan")
}

/// What a shell command does, for the folded summary: `find` and `grep` search, `cat` reads.
fn shell_kind(t: &ToolCall) -> ToolKind {
    let cmd = input_str(t, &["command", "cmd", "script"]).unwrap_or("");
    let first = cmd.split_whitespace().next().unwrap_or("");
    let first = first.rsplit('/').next().unwrap_or(first);
    match first {
        "find" | "grep" | "rg" | "ag" | "ack" | "fd" | "egrep" | "fgrep" => ToolKind::Search,
        "cat" | "head" | "tail" | "less" | "more" | "bat" => ToolKind::Read,
        _ => ToolKind::Execute,
    }
}

fn bucket(t: &ToolCall) -> (&'static str, &'static str, &'static str) {
    // wizard marks some tools as reads that the reference would fold otherwise: a listing is
    // a listing, git status and git diff are commands, and a tool search finds tools
    let tool = if t.name.is_empty() {
        t.title.as_str()
    } else {
        t.name.as_str()
    };
    let tool = tool.split([':', '(', ' ']).next().unwrap_or("");
    match tool {
        "list_files" => return ("listed", "directory", "directories"),
        "git_status" | "git_diff" => return ("ran", "shell command", "shell commands"),
        "tool_search" => return ("searched for", "tool", "tools"),
        _ => {}
    }
    let kind = if t.kind == ToolKind::Execute {
        shell_kind(t)
    } else {
        t.kind
    };
    match kind {
        ToolKind::Execute => ("ran", "shell command", "shell commands"),
        ToolKind::Search => ("searched for", "pattern", "patterns"),
        ToolKind::Read => ("read", "file", "files"),
        ToolKind::Fetch => ("fetched", "URL", "URLs"),
        _ => ("used", "tool", "tools"),
    }
}

/// `  Searched for 1 pattern, read 1 file`: one phrase per kind, in the order first used.
fn folded(th: &Theme, group: &[&ToolCall], w: usize) -> Vec<Row> {
    let mut counts: Vec<((&str, &str, &str), usize)> = Vec::new();
    for t in group {
        let b = bucket(t);
        match counts.iter_mut().find(|(k, _)| *k == b) {
            Some((_, n)) => *n += 1,
            None => counts.push((b, 1)),
        }
    }
    let st = th.dim();
    let mut spans: Vec<Span> = Vec::new();
    for (i, ((verb, one, many), n)) in counts.iter().enumerate() {
        let mut verb = verb.to_string();
        if i == 0 {
            verb[..1].make_ascii_uppercase();
        } else {
            spans.push((", ".into(), st));
        }
        spans.push((format!("{verb} "), st));
        spans.push((n.to_string(), st.add_modifier(Modifier::BOLD)));
        spans.push((format!(" {}", if *n == 1 { one } else { many }), st));
    }
    wrap(&spans, w.saturating_sub(4))
        .into_iter()
        .map(|r| {
            let mut row = Row::of("  ", Style::new());
            row.extend(r);
            row
        })
        .collect()
}

/// A plan put up for approval, as the transcript keeps it: `Updated plan`, then once answered
/// the plan itself, under `User approved` or in a box under `User rejected`.
fn plan_rows(app: &App, t: &ToolCall, w: usize) -> Vec<Row> {
    let th = app.theme;
    let bullet = |text: &str, color| {
        Row::of("●", th.fg(color))
            .with(" ", Style::new())
            .with(text.to_string(), th.bold())
    };
    let mut out = vec![bullet("Updated plan", th.success)];
    let plan = input_str(t, &["plan"]).unwrap_or("");
    let mut links = Vec::new();
    match t.status {
        ToolStatus::Completed => {
            out.push(Row::new());
            out.push(bullet("User approved Wizard's plan", th.success));
            out.push(Row::of(ELBOW, th.dim()).with("Plan saved to: .wizard/plan.md", th.dim()));
            for r in markdown::render(&th, plan, w.saturating_sub(5), &mut links) {
                let mut row = Row::of("     ", Style::new());
                row.extend(r);
                out.push(row);
            }
        }
        ToolStatus::Failed => {
            out.push(
                Row::of(ELBOW, th.dim()).with("User rejected Wizard's plan:", th.fg(th.subtle)),
            );
            let border = th.fg(th.plan);
            // the box is as wide as the plan, up to the screen
            let rows = markdown::render(&th, plan, w.saturating_sub(9), &mut links);
            let inner = rows.iter().map(|r| r.width()).max().unwrap_or(0);
            out.push(
                Row::of("     ", Style::new()).with(format!("╭{}╮", "─".repeat(inner + 2)), border),
            );
            for r in rows {
                let pad = inner.saturating_sub(r.width());
                let mut row = Row::of("     ", Style::new()).with("│ ", border);
                row.extend(r);
                row.push(" ".repeat(pad), Style::new());
                row.push(" │", border);
                out.push(row);
            }
            out.push(
                Row::of("     ", Style::new()).with(format!("╰{}╯", "─".repeat(inner + 2)), border),
            );
        }
        _ => {}
    }
    out
}

fn input_str<'a>(t: &'a ToolCall, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| t.input.get(*k).and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())
}

/// The tool's display name and argument, as in `Bash(echo hi)`.
pub fn call_label(app: &App, t: &ToolCall, absolute: bool) -> (String, String) {
    let path = |p: &str| -> String {
        if absolute {
            return p.to_string();
        }
        let cwd = app.opts.cwd.to_string_lossy();
        p.strip_prefix(&format!("{cwd}/")).unwrap_or(p).to_string()
    };
    let file = t
        .diff
        .as_ref()
        .map(|d| d.path.as_str())
        .or_else(|| input_str(t, &["path", "file_path", "file", "filePath"]));
    match t.kind {
        ToolKind::Execute => (
            "Bash".into(),
            input_str(t, &["command", "cmd", "script"])
                .unwrap_or(&t.title)
                .to_string(),
        ),
        ToolKind::Read => ("Read".into(), path(file.unwrap_or(&t.title))),
        ToolKind::Edit => {
            // a failed call has lost its diff; the tool's name still says which it was
            let new_file = match &t.diff {
                Some(d) => d.old.is_none(),
                None => t.name == "write_file",
            };
            (
                if new_file { "Write" } else { "Update" }.into(),
                path(file.unwrap_or(&t.title)),
            )
        }
        ToolKind::Search => (
            "Search".into(),
            input_str(t, &["pattern", "query", "regex"])
                .unwrap_or(&t.title)
                .to_string(),
        ),
        ToolKind::Fetch => (
            "Fetch".into(),
            input_str(t, &["url", "query"])
                .unwrap_or(&t.title)
                .to_string(),
        ),
        _ => (t.name.clone(), t.title.clone()),
    }
}

fn tool(app: &App, t: &ToolCall, w: usize, detail: bool) -> Vec<Row> {
    let th = app.theme;
    let running = matches!(t.status, ToolStatus::Pending | ToolStatus::Running);
    let dot = match t.status {
        ToolStatus::Completed => th.success,
        ToolStatus::Failed => th.error,
        _ => th.secondary,
    };
    if is_plan_call(t) {
        return plan_rows(app, t, w);
    }
    let (name, arg) = call_label(app, t, detail);
    let command = input_str(t, &["command", "cmd", "script"]);
    let mut out: Vec<Row> = Vec::new();
    let editing = t.kind == ToolKind::Edit || t.diff.is_some();

    if running && !editing && !detail {
        let title = if t.title.is_empty() {
            format!("{name}({arg})")
        } else {
            t.title.clone()
        };
        // the bullet of a call that is still out blinks
        let on = (app.clock.elapsed().as_millis() / 500).is_multiple_of(2);
        out.push(
            Row::of(if on { "●" } else { " " }, th.fg(dot))
                .with(" ", Style::new())
                .with(title, Style::new()),
        );
        if let Some(c) = command {
            out.push(Row::of(format!("  ⎿  $ {c}"), th.dim()));
        }
        return out;
    }

    // the arguments wrap in a column that starts right after the name
    let col = 2 + crate::text::width(&name);
    let args = if arg.is_empty() {
        Vec::new()
    } else {
        wrap_str(&format!("({arg})"), Style::new(), w.saturating_sub(col))
    };
    // the file an edit or write touched opens from its row, as an OSC 8 file:// link
    if matches!(name.as_str(), "Update" | "Write" | "Read")
        && !arg.is_empty()
        && markdown::hyperlinks()
    {
        let path = t
            .diff
            .as_ref()
            .map(|d| d.path.clone())
            .or_else(|| {
                input_str(t, &["path", "file_path", "file", "filePath"]).map(str::to_string)
            })
            .unwrap_or_default();
        let abs = if std::path::Path::new(&path).is_absolute() {
            std::path::PathBuf::from(&path)
        } else {
            app.opts.cwd.join(&path)
        };
        app.links.borrow_mut().push((
            format!("{name}(\0{arg}"),
            format!("file://{}", abs.display()),
        ));
    }
    let mut first = Row::of("●", th.fg(dot))
        .with(" ", Style::new())
        .with(name.clone(), th.bold());
    let mut args = args.into_iter();
    if let Some(r) = args.next() {
        first.extend(r);
    }
    out.push(first);
    for r in args {
        let mut row = Row::of(" ".repeat(col), Style::new());
        row.extend(r);
        out.push(row);
    }
    if running {
        return out;
    }
    if t.status == ToolStatus::Failed {
        let text = t.output.as_deref().unwrap_or("failed").trim();
        let text = if text == agent_core::INTERRUPTED_OUTPUT {
            "Interrupted"
        } else {
            text
        };
        // a resumed result already carries the prefix wizard gave it
        let text = text.strip_prefix("Error: ").unwrap_or(text);
        // a failed edit or write says only that, as the reference does; ctrl+o has the detail.
        // A call wizard refused never reached the file, so the reason is what is shown
        let refused = [
            "is not available in this turn",
            "blocked by plan mode",
            "blocked by pre_tool_use hook",
        ]
        .iter()
        .any(|r| text.contains(r));
        let short = match name.as_str() {
            _ if detail || refused || text == "Interrupted" => None,
            "Update" => Some("Error editing file"),
            "Write" => Some("Error writing file"),
            _ => None,
        };
        if let Some(short) = short {
            out.push(Row::of(ELBOW, th.dim()).with(short, th.fg(th.error)));
            return out;
        }
        out.extend(hanging(&th, &format!("Error: {text}"), th.fg(th.error), w));
        return out;
    }
    if let Some(d) = t.diff.as_ref().filter(|d| d.old.is_none()) {
        let lines: Vec<&str> = d.new.lines().collect();
        let nw = lines.len().max(1).to_string().len();
        let rel = relative(&app.opts.cwd, &d.path);
        out.push(
            Row::of(ELBOW, th.dim())
                .with("Wrote ", Style::new())
                .with(lines.len().to_string(), th.bold())
                .with(" lines to ", Style::new())
                .with(rel, th.bold()),
        );
        let shown = if detail {
            lines.len()
        } else {
            lines.len().min(10)
        };
        for (i, l) in lines.iter().take(shown).enumerate() {
            out.push(
                Row::of(
                    format!("      {:>nw$}", i + 1),
                    th.fg(th.code[0]).add_modifier(Modifier::DIM),
                )
                .with(" ", Style::new())
                .with(l.to_string(), th.fg(th.code[0])),
            );
        }
        if shown < lines.len() {
            out.push(Row::of(
                format!("     … +{} lines", lines.len() - shown),
                th.dim(),
            ));
        }
        return out;
    }
    if let Some(d) = &t.diff {
        let rendered = diff::render(&th, d, 5, w);
        let mut row = Row::of(ELBOW, th.dim());
        row.0.extend(diff::summary(&rendered, d.old.is_none(), &th));
        out.push(row);
        out.extend(rendered.rows);
        return out;
    }
    let text = t.output.as_deref().unwrap_or("").trim_end();
    match t.kind {
        ToolKind::Read => {
            // counted the way Claude Code counts: a final newline starts one more line
            let n = t.output.as_deref().unwrap_or("").split('\n').count();
            out.push(
                Row::of(ELBOW, th.dim())
                    .with("Read ", Style::new())
                    .with(n.to_string(), th.bold())
                    .with(if n == 1 { " line" } else { " lines" }, Style::new()),
            );
        }
        _ if text.is_empty() => out.extend(hanging(&th, "(No output)", Style::new(), w)),
        _ => {
            let lines: Vec<&str> = text.lines().collect();
            let shown = if detail {
                lines.len()
            } else {
                lines.len().min(3)
            };
            let mut body = lines[..shown].join("\n");
            if shown < lines.len() {
                body.push_str(&format!(
                    "\n… +{} lines (ctrl+o to expand)",
                    lines.len() - shown
                ));
            }
            out.extend(hanging(&th, &body, Style::new(), w));
        }
    }
    out
}

/// `path` relative to `from`, with `..` for each directory it climbs, as Claude Code writes
/// where a new file went.
pub fn relative(from: &std::path::Path, path: &str) -> String {
    let to = std::path::Path::new(path);
    if !to.is_absolute() {
        return path.to_string();
    }
    let a: Vec<_> = from.components().collect();
    let b: Vec<_> = to.components().collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut out: Vec<String> = vec!["..".to_string(); a.len() - common];
    out.extend(
        b[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    out.join("/")
}
