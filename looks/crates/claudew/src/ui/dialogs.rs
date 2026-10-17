//! The panels that take the prompt's place: help, the model picker, status and the session
//! picker. A permission prompt is not one of them; it flows at the end of the transcript.

use agent_core::{PermissionRequest, ToolKind};
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier, Style};

use crate::app::{effort_glyph, permission_options, App, Dialog};
use crate::text::{blit, put, truncate, width, wrap, wrap_str, Row, Span};
use crate::ui::{diff, markdown, prompt};

const MODEL_ROWS: usize = 10;
/// Rows from which the help panel spaces its parts out, as the reference does at 50 rows and
/// not at 40.
const ROOMY_HELP: usize = 48;

struct Panel {
    top: Vec<Row>,
    /// Rows held against the bottom edge.
    bottom: Vec<Row>,
    /// Rows left for the transcript when the panel takes all the height it can.
    fill: Option<u16>,
}

/// Draws the dialog against the bottom edge and returns its first row.
pub fn draw(buf: &mut Buffer, app: &mut App, dialog: &Dialog) -> u16 {
    let (w, h) = (buf.area.width, buf.area.height);
    let th = app.theme;
    let panel = match dialog {
        Dialog::Help { tab, sel, focus } => help(app, *tab, *sel, *focus, w as usize, h as usize),
        Dialog::Model { sel } => model(app, *sel, w as usize, h as usize),
        Dialog::Status => status(app, w as usize),
        Dialog::Mode { sel } => mode(app, *sel, w as usize),
        Dialog::Effort { sel } => effort(app, *sel, w as usize),
        Dialog::Resume { sel, query } => resume(app, *sel, query, w as usize, h as usize),
        Dialog::Rewind { sel } => rewind(app, *sel, w as usize, h as usize),
        Dialog::RewindConfirm { nth, sel } => rewind_confirm(app, *nth, *sel, w as usize),
        Dialog::Btw {
            question,
            answer,
            scroll,
        } => btw(
            app,
            question,
            answer.as_deref(),
            *scroll,
            w as usize,
            h as usize,
        ),
        Dialog::Search { query, sel } => search(app, query, *sel, w as usize),
        Dialog::Permission { .. } => return h,
    };
    let need = (panel.top.len() + panel.bottom.len() + 1) as u16;
    let height = match panel.fill {
        Some(keep) => h.saturating_sub(keep),
        None => need.min(h.saturating_sub(2)),
    }
    .max(2)
    .min(h);
    let top = h - height;
    let edge = if matches!(dialog, Dialog::Search { .. }) {
        "─"
    } else {
        "▔"
    };
    put(buf, 0, top, &edge.repeat(w as usize), th.fg(th.suggestion));
    let notice = match dialog {
        Dialog::Model { .. } => crate::app::effort_notice_text(&app.cfg.effort),
        // the side question's panel carries no notice on its edge
        Dialog::Btw { .. }
        | Dialog::Search { .. }
        | Dialog::Rewind { .. }
        | Dialog::RewindConfirm { .. } => None,
        _ => app.notice.clone(),
    };
    if let Some(n) = &notice {
        let x = (w as usize).saturating_sub(width(n) + 3);
        put(buf, x as u16, top, &format!(" {n} "), th.dim());
    }
    let room = height as usize - 1;
    if panel.fill.is_some() {
        let bottom_n = panel.bottom.len().min(room);
        for (i, row) in panel.top.iter().take(room - bottom_n).enumerate() {
            blit(buf, 0, top + 1 + i as u16, row);
        }
        for (i, row) in panel.bottom.iter().rev().take(bottom_n).enumerate() {
            blit(buf, 0, h - 1 - i as u16, row);
        }
    } else {
        // a panel taller than the room it gets is cut at the bottom
        for (i, row) in panel.top.iter().chain(&panel.bottom).take(room).enumerate() {
            blit(buf, 0, top + 1 + i as u16, row);
        }
    }
    // the history search keeps the cursor in its filter, after what is typed
    if let Dialog::Search { query, .. } = dialog {
        let row = top + 1 + panel.top.len().saturating_sub(2) as u16;
        app.cursor = Some((6 + width(query) as u16, row));
    }
    // the history search sits under a blank row the transcript keeps
    if matches!(dialog, Dialog::Search { .. }) {
        top.saturating_sub(1)
    } else {
        top
    }
}

fn indent(row: Row, n: usize) -> Row {
    let mut out = Row::of(" ".repeat(n), Style::new());
    out.extend(row);
    out
}

fn tabs(app: &App, title: &str, names: &[&str], on: usize, color: Color, focus: bool) -> Row {
    let th = app.theme;
    let mut row =
        Row::of("   ", Style::new()).with(title, th.fg(color).add_modifier(Modifier::BOLD));
    for (i, name) in names.iter().enumerate() {
        row.push(" ", Style::new());
        if i == on {
            let st = if focus {
                Style::new().add_modifier(Modifier::REVERSED)
            } else {
                Style::new().bg(color)
            };
            let text = if focus {
                st.add_modifier(Modifier::BOLD)
            } else {
                st.fg(th.tab_fg).add_modifier(Modifier::BOLD)
            };
            row.push(" ", st);
            row.push(*name, text);
            row.push(" ", st);
        } else {
            row.push(format!(" {name} "), Style::new());
        }
    }
    row
}

const HELP_BLUE: Color = Color::Indexed(110);

fn help(app: &App, tab: usize, sel: usize, focus: bool, w: usize, h: usize) -> Panel {
    let th = app.theme;
    let mut top = vec![
        tabs(
            app,
            "Help",
            &["General", "Commands", "Custom commands"],
            tab,
            HELP_BLUE,
            focus,
        ),
        Row::new(),
    ];
    let docs = "https://github.com/teddytennant/wizard";
    if crate::ui::markdown::hyperlinks() {
        app.links
            .borrow_mut()
            .push((docs.to_string(), docs.to_string()));
    }
    let bottom = vec![
        Row::of(format!("   For more help: {docs}"), Style::new()),
        Row::new(),
        Row::of("   Esc to cancel", th.dim().add_modifier(Modifier::ITALIC)),
    ];
    if tab == 0 {
        // a tall terminal gets a blank row around each part
        let roomy = h >= ROOMY_HELP;
        if roomy {
            top.push(Row::new());
        }
        let about = "Wizard understands your codebase, makes edits with your permission, and \
                     executes commands — right from your terminal.";
        for r in wrap_str(about, Style::new(), w.saturating_sub(6)) {
            top.push(indent(r, 3));
        }
        if roomy {
            top.push(Row::new());
            top.push(Row::new());
        }
        top.push(Row::of("   ", Style::new()).with("Shortcuts", th.bold()));
        top.extend(prompt::shortcut_table(app, w, true, Style::new()));
        top.push(Row::new());
        let mut bottom = bottom;
        if roomy {
            top.push(Row::new());
            bottom.insert(1, Row::new());
        }
        return Panel {
            top,
            bottom,
            fill: None,
        };
    }
    let commands: Vec<_> = sorted_commands(app)
        .into_iter()
        .filter(|c| app.custom.contains(&c.name) == (tab == 2))
        .collect();
    top.push(Row::new());
    if tab == 2 && commands.is_empty() {
        top.push(Row::of("   No custom commands found", th.dim()));
        return Panel {
            top,
            bottom,
            fill: None,
        };
    }
    let label = if tab == 2 { "custom" } else { "default" };
    top.push(Row::of(format!("   Browse {label} commands"), Style::new()));
    top.push(Row::new());
    let room = h.saturating_sub(3 + 1 + top.len() + bottom.len() + 4);
    let shown = (room / 2).max(1);
    let n = commands.len();
    let first = if sel >= shown { sel + 1 - shown } else { 0 };
    for (i, cmd) in commands.iter().enumerate().skip(first).take(shown) {
        let on = focus && i == sel;
        let name = format!("/{}", cmd.name);
        let row = if on {
            Row::of("   ❯ ", th.fg(th.suggestion)).with(name, th.fg(th.suggestion))
        } else if i == sel {
            Row::of("   ❯ ", th.dim()).with(name, Style::new())
        } else {
            Row::of("     ", Style::new()).with(name, Style::new())
        };
        top.push(row);
        top.push(Row::of(
            format!("     {}", truncate(&cmd.description, w.saturating_sub(10))),
            th.dim(),
        ));
    }
    top.push(Row::new());
    let below = n.saturating_sub(first + shown);
    if below > 0 {
        top.push(Row::of(format!("     ↓ {below} more"), th.dim()));
    } else {
        top.push(Row::new());
    }
    top.push(Row::new());
    top.push(Row::new());
    Panel {
        top,
        bottom,
        fill: Some(3),
    }
}

/// The commands as the help panel lists them: by name.
pub fn sorted_commands(app: &App) -> Vec<&agent_core::SlashCommand> {
    let mut v: Vec<_> = app.commands.iter().collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// `/mode`: laid out like the model picker, one row per mode.
fn mode(app: &App, sel: usize, w: usize) -> Panel {
    let (now, _) = app.shown_mode();
    let about = "Switch between Wizard's modes for this session. shift+tab also cycles them, \
                 with plan mode after the last.";
    choices(app, "Select mode", about, &app.cfg.modes, &now, sel, w)
}

/// `/effort`: the same list for the model's reasoning efforts.
fn effort(app: &App, sel: usize, w: usize) -> Panel {
    let about = "Set how hard the model thinks, for this session. The model picker adjusts it \
                 too, with ←/→.";
    choices(
        app,
        "Select effort",
        about,
        &app.cfg.efforts,
        &app.cfg.effort,
        sel,
        w,
    )
}

/// A picker of `items`, the one in use ticked, laid out like the model picker.
fn choices(
    app: &App,
    title: &str,
    about: &str,
    items: &[String],
    now: &str,
    sel: usize,
    w: usize,
) -> Panel {
    let th = app.theme;
    let mut top = vec![Row::of("   ", Style::new()).with(
        title.to_string(),
        th.fg(th.title).add_modifier(Modifier::BOLD),
    )];
    for r in wrap_str(about, th.dim(), w.saturating_sub(6)) {
        top.push(indent(r, 3));
    }
    top.push(Row::new());
    for (i, m) in items.iter().enumerate() {
        let current = *m == now;
        let mut row = Row::new();
        if i == sel {
            row.push("   ❯ ", th.fg(th.suggestion));
        } else {
            row.push("     ", Style::new());
        }
        row.push(format!("{:<4}", format!("{}.", i + 1)), th.dim());
        let style = if current {
            th.fg(th.success)
        } else if i == sel {
            th.fg(th.suggestion)
        } else {
            Style::new()
        };
        row.push(
            if current {
                format!("{m} ✔")
            } else {
                m.clone()
            },
            style,
        );
        top.push(row);
    }
    top.push(Row::new());
    Panel {
        top,
        bottom: vec![Row::of("   Enter to confirm · Esc to cancel", th.dim())],
        fill: None,
    }
}

fn model(app: &App, sel: usize, w: usize, h: usize) -> Panel {
    let th = app.theme;
    let models = &app.cfg.models;
    let mut top = vec![Row::of("   ", Style::new())
        .with("Select model", th.fg(th.title).add_modifier(Modifier::BOLD))];
    let about = "Switch between Wizard models. Your pick applies to this session only. For \
                 other/previous model names, specify with /model <name>.";
    for r in wrap_str(about, th.dim(), w.saturating_sub(6)) {
        top.push(indent(r, 3));
    }
    top.push(Row::new());
    // the panel takes at most half the screen and a row; the list gives way first
    let effort_rows = if app.cfg.effort.is_empty() { 0 } else { 2 };
    let fixed = 1 + top.len() + 1 + 1 + effort_rows + 1;
    let rows = (h / 2 + 1).saturating_sub(fixed).clamp(1, MODEL_ROWS);
    let shown = rows;
    // the widest name, the current one counted with its ` ✔`, then two columns
    let name_w = models
        .iter()
        .map(|m| width(&m.name) + if m.id == app.cfg.model { 2 } else { 0 })
        .max()
        .unwrap_or(0)
        + 2;
    let first = if sel >= shown { sel + 1 - shown } else { 0 };
    let last = (first + shown).min(models.len());
    for (i, m) in models.iter().enumerate().take(last).skip(first) {
        let current = m.id == app.cfg.model;
        let more_below = i + 1 == last && last < models.len();
        let more_above = i == first && first > 0;
        let mut row = Row::new();
        if i == sel {
            row.push("   ❯ ", th.fg(th.suggestion));
        } else if more_below {
            row.push("   ↓ ", th.dim());
        } else if more_above {
            row.push("   ↑ ", th.dim());
        } else {
            row.push("     ", Style::new());
        }
        row.push(format!("{:<4}", format!("{}.", i + 1)), th.dim());
        let label = if current {
            format!("{} ✔", m.name)
        } else {
            m.name.clone()
        };
        let style = if current {
            th.fg(th.success)
        } else if i == sel {
            th.fg(th.suggestion)
        } else {
            Style::new()
        };
        let pad = name_w.saturating_sub(width(&label));
        row.push(label, style);
        if !m.provider.is_empty() {
            row.push(" ".repeat(pad), Style::new());
            row.push(
                truncate(&m.provider, w.saturating_sub(9 + name_w + 2)),
                th.dim(),
            );
        }
        top.push(row);
    }
    if last < models.len() {
        top.push(Row::of(
            format!("      … +{} models", models.len() - last),
            th.dim(),
        ));
    }
    top.push(Row::new());
    if !app.cfg.effort.is_empty() {
        let e = app.effort_preview.as_ref().unwrap_or(&app.cfg.effort);
        // `xhigh` reads `xHigh`, as the reference spells it
        let mut name = e.clone();
        let at = usize::from(name.starts_with('x') && name.len() > 1);
        name[at..at + 1].make_ascii_uppercase();
        let mut row = Row::of("   ", Style::new())
            .with(effort_glyph(e), th.fg(th.brand))
            .with(format!(" {name} effort"), th.dim());
        if app.cfg.efforts.len() > 1 {
            row.push(" ←/→ to adjust", th.fg(th.subtle));
        }
        top.push(row);
        top.push(Row::new());
    }
    Panel {
        top,
        bottom: vec![Row::of("   Enter to confirm · Esc to cancel", th.dim())],
        fill: None,
    }
}

fn status(app: &App, w: usize) -> Panel {
    let th = app.theme;
    let mut top = vec![
        tabs(app, "Settings", &["Status"], 0, th.suggestion, false),
        Row::new(),
    ];
    let model = if app.cfg.model.is_empty() {
        app.model_name()
    } else {
        format!("{} ({})", app.model_name(), app.cfg.model)
    };
    let fields: Vec<(&str, String)> = vec![
        ("Version:", app.wizard_version()),
        ("Session ID:", app.session_id.clone()),
        ("cwd:", app.opts.cwd.to_string_lossy().into_owned()),
        ("", String::new()),
        ("Model:", model),
        ("Effort:", app.cfg.effort.clone()),
        ("Mode:", app.cfg.mode.clone()),
        ("Backend:", app.cfg.backend.clone()),
    ];
    for (label, value) in fields {
        // the empty label is the blank row Claude Code keeps before the model
        if label.is_empty() {
            top.push(Row::new());
            continue;
        }
        if value.is_empty() {
            continue;
        }
        // a long value breaks three columns short of the edge and carries on under itself
        let room = w.saturating_sub(25).max(10);
        let chars: Vec<char> = value.chars().collect();
        for (i, part) in chars.chunks(room).enumerate() {
            let lead = if i == 0 {
                format!("{label:<19}")
            } else {
                " ".repeat(19)
            };
            top.push(
                Row::of("   ", Style::new())
                    .with(lead, th.bold())
                    .with(part.iter().collect::<String>(), Style::new()),
            );
        }
    }
    top.push(Row::new());
    top.push(Row::of("   Esc to cancel", th.dim()));
    Panel {
        top,
        bottom: Vec::new(),
        fill: Some(2),
    }
}

/// `3 minutes ago · main · 203.2KB`: when, the branch its directory is on, and the size of
/// wizard's session file.
fn session_meta(now: i64, s: &agent_core::SessionInfo) -> String {
    let mut out = ago(now, s.updated);
    let branch = git_branch(std::path::Path::new(&s.cwd)).unwrap_or_else(|| "HEAD".into());
    out.push_str(&format!(" · {branch}"));
    let file = std::env::var_os("HOME").map(|h| {
        std::path::PathBuf::from(h)
            .join(".wizard/sessions")
            .join(format!("{}.jsonl", s.id))
    });
    if let Some(len) = file
        .and_then(|f| std::fs::metadata(f).ok())
        .map(|m| m.len())
    {
        out.push_str(&format!(" · {}", human_size(len)));
    }
    out
}

/// `3.2KB`, `192KB`: one decimal, dropped when it is zero.
fn human_size(n: u64) -> String {
    let (v, unit) = match n {
        0..1024 => return format!("{n}B"),
        1024..1_048_576 => (n as f64 / 1024.0, "KB"),
        _ => (n as f64 / 1_048_576.0, "MB"),
    };
    let s = format!("{v:.1}");
    format!("{}{unit}", s.strip_suffix(".0").unwrap_or(&s))
}

/// The branch `.git/HEAD` names, walking up from `dir`.
fn git_branch(dir: &std::path::Path) -> Option<String> {
    for d in dir.ancestors() {
        let head = d.join(".git/HEAD");
        if let Ok(t) = std::fs::read_to_string(&head) {
            return t
                .trim()
                .strip_prefix("ref: refs/heads/")
                .map(str::to_string);
        }
    }
    None
}

pub fn ago(now: i64, then: i64) -> String {
    let s = (now - then).max(0);
    let plural = |n: i64, unit: &str| format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" });
    if then <= 0 {
        String::new()
    } else if s < 60 {
        plural(s.max(1), "second")
    } else if s < 3600 {
        plural(s / 60, "minute")
    } else if s < 86400 {
        plural(s / 3600, "hour")
    } else {
        plural(s / 86400, "day")
    }
}

fn resume(app: &App, sel: usize, query: &str, w: usize, h: usize) -> Panel {
    let th = app.theme;
    let dim = Style::new().add_modifier(Modifier::DIM);
    let inner = w.saturating_sub(8);
    let list = app.resume_list(query);
    let footer: Vec<Row> = wrap_str(
        "Ctrl+A to show all projects · Type to search · Esc to cancel",
        th.dim(),
        w.saturating_sub(8),
    )
    .into_iter()
    .map(|r| indent(r, 5))
    .collect();
    // the transcript keeps six rows; the list gets what is left, in whole entries
    let room = h.saturating_sub(6 + 7 + footer.len());
    let shown = (room / 3).max(1);
    let mut title = Row::of("   ", Style::new()).with(
        "Resume session",
        th.fg(th.title).add_modifier(Modifier::BOLD),
    );
    if list.len() > shown {
        title.push(
            format!(" ({} of {})", sel.min(list.len() - 1) + 1, list.len()),
            th.dim().add_modifier(Modifier::BOLD),
        );
    }
    let mut top = vec![title, Row::of(format!("   ╭{}╮", "─".repeat(inner)), dim)];
    let mut search = Row::of("   │", dim).with(" ", Style::new());
    if query.is_empty() {
        search.push("⌕ Search…", th.dim());
    } else {
        search.push(format!("⌕ {query}"), Style::new());
    }
    let pad = (inner + 4).saturating_sub(search.width());
    search.push(" ".repeat(pad), Style::new());
    search.push("│", dim);
    top.push(search);
    top.push(Row::of(format!("   ╰{}╯", "─".repeat(inner)), dim));
    let scope = if app.resume_all {
        "all projects".to_string()
    } else {
        app.opts
            .cwd
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    top.push(Row::of(format!("     {scope}"), th.dim()));
    top.push(Row::new());
    if list.is_empty() {
        top.push(Row::of(
            "    No conversations found in this project.",
            th.dim(),
        ));
        top.push(Row::of("    Ctrl+A to show all projects", th.dim()));
        top.push(Row::new());
        return Panel {
            top,
            bottom: footer,
            fill: None,
        };
    }
    let sel = sel.min(list.len() - 1);
    let first = if sel >= shown { sel + 1 - shown } else { 0 };
    let last = (first + shown).min(list.len());
    let now = app.now_unix;
    for (i, s) in list.iter().enumerate().take(last).skip(first) {
        let on = i == sel;
        let title = if s.title.is_empty() {
            s.id.chars().take(8).collect()
        } else {
            s.title.clone()
        };
        let title = truncate(&title, w.saturating_sub(10));
        let lead = if on {
            Row::of("   ❯ ", th.fg(th.suggestion))
        } else if i + 1 == last && last < list.len() {
            Row::of("   ↓ ", th.dim())
        } else if i == first && first > 0 {
            Row::of("   ↑ ", th.dim())
        } else {
            Row::of("     ", Style::new())
        };
        let style = if on {
            th.fg(th.suggestion)
        } else {
            Style::new()
        };
        top.push(lead.with(title, style));
        top.push(Row::of(format!("     {}", session_meta(now, s)), th.dim()));
        top.push(Row::new());
    }
    let height = top.len() + footer.len();
    top.truncate(height.min(h).saturating_sub(footer.len()));
    Panel {
        top,
        bottom: footer,
        fill: None,
    }
}

/// A finished plan waiting for approval: a pane over all but the last two transcript rows,
/// the plan at its top and the question pinned to its bottom. Returns the rows left above it.
pub fn plan_review(buf: &mut Buffer, app: &App, req: &PermissionRequest, sel: usize) -> u16 {
    let th = app.theme;
    let area = *buf.area();
    let (w, h) = (area.width as usize, area.height);
    if h < 8 {
        return 0;
    }
    let accent = th.fg(th.suggestion);
    let rule = Row::of("  ", Style::new()).with("─".repeat(w.saturating_sub(4)), th.fg(th.plan));
    let dashes =
        Row::of("  ", Style::new()).with("╌".repeat(w.saturating_sub(4)), th.fg(th.subtle));
    let plan = req.input.get("plan").and_then(|p| p.as_str()).unwrap_or("");
    let mut top = vec![
        Row::of("▔".repeat(w), accent),
        Row::new(),
        rule.clone(),
        Row::of("   ", Style::new()).with("Ready to code?", accent.add_modifier(Modifier::BOLD)),
        Row::new(),
        Row::of("   Here is Wizard's plan:", Style::new()),
        dashes.clone(),
    ];
    let mut links = app.links.borrow_mut();
    for r in markdown::render(&th, plan, w.saturating_sub(6), &mut links) {
        top.push(indent(r, 3));
    }
    drop(links);
    top.push(dashes);
    let mut bottom = vec![rule];
    let question =
        "Wizard has written up a plan and is ready to execute. Would you like to proceed?";
    for r in wrap_str(question, th.dim(), w.saturating_sub(6)) {
        bottom.push(indent(r, 3));
    }
    bottom.push(Row::new());
    for (i, (label, _)) in permission_options(req).iter().enumerate() {
        let on = i == sel;
        bottom.push(
            Row::of(if on { "   ❯ " } else { "     " }, accent)
                .with(format!("{}.", i + 1), th.dim())
                .with(" ", Style::new())
                .with(label.clone(), if on { accent } else { Style::new() }),
        );
    }
    bottom.push(Row::new());
    let start = 2u16;
    let room = (h - start) as usize;
    // a plan longer than the pane is cut, its closing rule with it, and an arrow says so
    let avail = room.saturating_sub(bottom.len());
    if top.len() > avail {
        top.truncate(avail);
        if let Some(last) = top.last_mut() {
            let pad = w.saturating_sub(2).saturating_sub(last.width());
            last.push(" ".repeat(pad), Style::new());
            last.push("↓", th.dim());
        }
    }
    for y in start..h {
        for x in 0..area.width {
            buf[(x, y)].reset();
        }
    }
    for (i, r) in top.iter().enumerate() {
        blit(buf, 0, start + i as u16, r);
    }
    let y0 = h - bottom.len() as u16;
    for (i, r) in bottom.iter().enumerate() {
        blit(buf, 0, y0 + i as u16, r);
    }
    start
}

/// A permission prompt as rows for the end of the transcript.
pub fn permission(app: &App, req: &PermissionRequest, sel: usize, w: usize) -> Vec<Row> {
    let th = app.theme;
    let accent = th.fg(th.suggestion);
    let dashes = Row::of("╌".repeat(w), th.fg(th.subtle));
    let mut rows = vec![Row::of("─".repeat(w), accent)];
    let name = req
        .diff
        .as_ref()
        .map(|d| d.path.clone())
        .unwrap_or_else(|| req.title.clone());
    let cwd = app.opts.cwd.to_string_lossy();
    let short = name
        .strip_prefix(&format!("{cwd}/"))
        .unwrap_or(&name)
        .to_string();
    let question: Vec<Span> = match (&req.diff, req.kind) {
        (Some(d), _) if d.old.is_none() => {
            rows.push(
                Row::of(" ", Style::new()).with("Create file", accent.add_modifier(Modifier::BOLD)),
            );
            rows.push(Row::of(format!(" {short}"), th.dim()));
            rows.push(dashes.clone());
            let lines: Vec<&str> = d.new.lines().collect();
            let nw = lines.len().max(1).to_string().len();
            for (i, l) in lines.iter().enumerate() {
                rows.push(
                    Row::of(
                        format!("  {:>nw$}", i + 1),
                        th.fg(th.code[0]).add_modifier(Modifier::DIM),
                    )
                    .with(" ", Style::new())
                    .with(l.to_string(), th.fg(th.code[0])),
                );
            }
            rows.push(dashes);
            let base = short.rsplit('/').next().unwrap_or(&short).to_string();
            vec![
                (" Do you want to create ".into(), Style::new()),
                (base, th.bold()),
                ("?".into(), Style::new()),
            ]
        }
        (Some(d), _) => {
            rows.push(
                Row::of(" ", Style::new()).with("Edit file", accent.add_modifier(Modifier::BOLD)),
            );
            rows.push(Row::of(format!(" {short}"), th.dim()));
            rows.push(dashes.clone());
            rows.extend(diff::render(&th, d, 0, w).rows);
            rows.push(dashes);
            vec![
                (" Do you want to make this edit to ".into(), Style::new()),
                (short, th.bold()),
                ("?".into(), Style::new()),
            ]
        }
        (None, kind) => {
            let title = match kind {
                ToolKind::Execute => "Bash command".to_string(),
                ToolKind::Fetch => "Fetch".to_string(),
                _ => req.tool.clone(),
            };
            rows.push(Row::of(" ", Style::new()).with(title, accent.add_modifier(Modifier::BOLD)));
            let command = req
                .input
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or(&req.title)
                .to_string();
            let about = req
                .input
                .get("description")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| {
                    (req.title != command && !req.title.is_empty()).then(|| req.title.clone())
                });
            if let Some(about) = about {
                for r in wrap_str(&about, th.dim(), w.saturating_sub(2)) {
                    rows.push(indent(r, 1));
                }
            }
            rows.push(dashes.clone());
            for r in wrap_str(&command, Style::new(), w.saturating_sub(2)) {
                rows.push(indent(r, 1));
            }
            rows.push(dashes);
            rows.push(Row::of(" This command requires approval", Style::new()));
            rows.push(Row::new());
            vec![(" Do you want to proceed?".into(), Style::new())]
        }
    };
    rows.extend(wrap(&question, w.saturating_sub(1)));
    for (i, (label, _)) in permission_options(req).iter().enumerate() {
        let on = i == sel;
        let st = if on { accent } else { Style::new() };
        for (k, r) in wrap_str(label, st, w.saturating_sub(8))
            .into_iter()
            .enumerate()
        {
            let mut row = Row::new();
            if k > 0 {
                row.push("      ", Style::new());
            } else {
                row.push(if on { " ❯ " } else { "   " }, accent);
                row.push(format!("{}.", i + 1), th.dim());
                row.push(" ", Style::new());
            }
            row.extend(r);
            rows.push(row);
        }
    }
    rows.push(Row::new());
    match &app.amend {
        Some(note) => rows.push(
            Row::of(" ", Style::new())
                .with("Tell Wizard what to do differently: ", th.dim())
                .with(note.clone(), Style::new()),
        ),
        None if permission_options(req)[sel.min(permission_options(req).len() - 1)].1
            == crate::app::Choice::Always =>
        {
            rows.push(Row::of(" Esc to cancel", th.dim()))
        }
        None => rows.push(Row::of(" Esc to cancel · Tab to amend", th.dim())),
    }
    rows
}

/// `/btw`'s panel: the question, wizard's answer, and the keys. Copy and fork are left out:
/// the look has no clipboard path and wizard cannot fork a session.
fn btw(
    app: &App,
    question: &str,
    answer: Option<&str>,
    scroll: usize,
    w: usize,
    h: usize,
) -> Panel {
    let th = app.theme;
    let mut top = vec![
        Row::new(),
        Row::of("    ", Style::new())
            .with("/btw", th.fg(th.warning).add_modifier(Modifier::BOLD))
            .with(format!(" {question}"), th.dim()),
        Row::new(),
    ];
    match answer {
        Some(text) => {
            let mut links = app.links.borrow_mut();
            let rows = markdown::render(&th, text.trim(), w.saturating_sub(12), &mut links);
            let room = (h / 2).saturating_sub(6).max(3);
            let scroll = scroll.min(rows.len().saturating_sub(room));
            for r in rows.into_iter().skip(scroll).take(room) {
                top.push(indent(r, 6));
            }
        }
        None => top.push(Row::of("      Thinking…", th.dim())),
    }
    top.push(Row::new());
    Panel {
        top,
        bottom: vec![Row::of("    ↑/↓ to scroll · Esc to close", th.dim())],
        fill: None,
    }
}

/// The rewind's confirm step: the prompt being gone back to, what happens, and what to do.
fn rewind_confirm(app: &App, nth: usize, sel: usize, w: usize) -> Panel {
    let th = app.theme;
    let accent = th.fg(th.suggestion);
    let targets = app.rewind_targets();
    let prompt = targets.get(nth).cloned().unwrap_or_default();
    let at = app
        .prompts
        .iter()
        .rposition(|p| *p == prompt)
        .and_then(|i| app.prompt_times.get(i).copied())
        .unwrap_or(app.now_unix);
    let bar = Style::new().add_modifier(Modifier::DIM);
    let mut top = vec![
        Row::of("   ", Style::new()).with("Rewind", th.fg(th.title).add_modifier(Modifier::BOLD)),
        Row::new(),
        Row::of(
            "   Confirm you want to restore to the point before you sent this message:",
            Style::new(),
        ),
        Row::new(),
    ];
    for line in wrap_str(&prompt, Style::new(), w.saturating_sub(8)) {
        top.push(
            Row::of("   ", Style::new())
                .with("│", bar)
                .with(" ", Style::new())
                .with(line.text(), th.fg(th.user_fg)),
        );
    }
    top.push(
        Row::of("   ", Style::new())
            .with("│", bar)
            .with(" ", Style::new())
            .with(format!("({})", ago_short(app.now_unix, at)), th.dim()),
    );
    top.push(Row::new());
    // wizard's /rewind truncates the conversation and puts back the files later turns changed
    top.push(Row::of(
        "   The conversation will be truncated here.",
        th.dim(),
    ));
    // what going back undoes in the files: the later turns' added lines go, removed ones return
    let files = app.changes_after(nth);
    match files.as_slice() {
        [] => top.push(Row::of("   The code will be unchanged.", th.dim())),
        [(file, added, removed)] => {
            let mut row = Row::of("   The code will be restored ", th.dim());
            if *added > 0 {
                row.push(format!("-{added}"), th.fg(Color::Indexed(174)));
            }
            if *removed > 0 {
                if *added > 0 {
                    row.push(" ", th.dim());
                }
                row.push(format!("+{removed}"), th.fg(Color::Indexed(72)));
            }
            row.push(format!(" in {file}."), th.dim());
            top.push(row);
        }
        many => top.push(Row::of(
            format!("   The code will be restored in {} files.", many.len()),
            th.dim(),
        )),
    }
    top.push(Row::new());
    let restores_code = !app.changes_after(nth).is_empty();
    for (i, label) in crate::app::REWIND_CHOICES.iter().enumerate() {
        let label = if i == 0 && restores_code {
            "Restore code and conversation"
        } else {
            label
        };
        let on = i == sel;
        top.push(
            Row::of(if on { "   ❯ " } else { "     " }, accent)
                .with(format!("{}.", i + 1), th.dim())
                .with(" ", Style::new())
                .with(label.to_string(), if on { accent } else { Style::new() }),
        );
    }
    // wizard's checkpoints hold what its own tools changed, as the reference's do
    if restores_code {
        top.push(Row::new());
        top.push(Row::of(
            "   ⚠ Rewinding does not affect files edited manually or via bash.",
            th.dim(),
        ));
    }
    Panel {
        top,
        bottom: Vec::new(),
        fill: None,
    }
}

fn rewind(app: &App, sel: usize, w: usize, h: usize) -> Panel {
    let th = app.theme;
    let targets = app.rewind_targets();
    let n = targets.len() + 1;
    let sel = sel.min(n - 1);
    let mut top = vec![
        Row::of("   ", Style::new()).with("Rewind", th.fg(th.title).add_modifier(Modifier::BOLD)),
        Row::new(),
        Row::of(
            "   Restore the code and/or conversation to the point before…",
            Style::new(),
        ),
        Row::new(),
    ];
    // each prompt takes three rows (itself, what it changed, a gap); the panel's height is
    // fixed by how many fit, and a row above and below is kept for the overflow counts
    let room = ((h / 2).saturating_sub(12) / 3).max(1);
    let first = (sel + 1).saturating_sub(room).min(n.saturating_sub(room));
    top.push(if first > 0 {
        Row::of(format!("     ↑ {first} more"), th.dim())
    } else {
        Row::new()
    });
    #[allow(clippy::needless_range_loop)]
    for i in first..n.min(first + room) {
        let on = i == sel;
        let lead = if on {
            Row::of("   ❯ ", th.fg(th.suggestion))
        } else {
            Row::of("     ", Style::new())
        };
        let row = if i + 1 == n {
            let st = if on {
                th.fg(th.suggestion)
            } else {
                Style::new()
            };
            lead.with("(current)", st.add_modifier(Modifier::ITALIC))
        } else {
            let line = targets[i].lines().next().unwrap_or("");
            let st = if on {
                th.fg(th.suggestion)
            } else {
                Style::new()
            };
            lead.with(truncate(line, w.saturating_sub(10)), st)
        };
        top.push(row);
        if i + 1 < n {
            let mut changed = Row::of("     ", Style::new());
            let files = app.changes_after(i);
            if files.is_empty() {
                changed.push("No code changes", th.dim());
            }
            for (k, (file, added, removed)) in files.iter().enumerate() {
                if k > 0 {
                    changed.push(", ", th.dim());
                }
                changed.push(file.clone(), th.dim());
                if *added > 0 {
                    changed.push(format!(" +{added}"), th.fg(Color::Indexed(72)));
                }
                if *removed > 0 {
                    changed.push(format!(" -{removed}"), th.fg(th.removed_fg));
                }
            }
            top.push(changed);
        }
        top.push(Row::new());
    }
    let below = n.saturating_sub(first + room);
    let more = if below > 0 {
        Row::of(format!("     ↓ {below} more"), th.dim())
    } else {
        Row::new()
    };
    Panel {
        top,
        bottom: vec![
            more,
            Row::new(),
            Row::of(
                "   Enter to continue · Esc to cancel",
                th.dim().add_modifier(Modifier::ITALIC),
            ),
        ],
        fill: Some(h.saturating_sub(9 + 3 * room) as u16),
    }
}

fn search(app: &App, query: &str, sel: usize, w: usize) -> Panel {
    let th = app.theme;
    let accent = th.fg(th.suggestion);
    let hits = app.search_hits(query);
    const ROWS: usize = 8;
    // a 48 column list, then the preview box out to three columns from the edge
    let list_w = 48.min(w.saturating_sub(15) / 2);
    let scope = if app.search_here {
        "this project"
    } else {
        "everywhere"
    };
    let mut top = vec![Row::of("  ", Style::new()).with(
        format!("Search prompts · {scope}"),
        accent.add_modifier(Modifier::BOLD),
    )];
    let shown: Vec<usize> = hits
        .iter()
        .copied()
        .take(ROWS.max(sel + 1))
        .skip((sel + 1).saturating_sub(ROWS))
        .collect();
    let preview_w = w.saturating_sub(list_w + 17).max(10);
    let selected = hits
        .get(sel)
        .map(|&i| app.prompts[i].clone())
        .unwrap_or_default();
    let mut preview: Vec<String> = wrap_str(&selected, Style::new(), preview_w.saturating_sub(4))
        .iter()
        .map(Row::text)
        .collect();
    preview.resize(ROWS.saturating_sub(2), String::new());
    let dim = Style::new().add_modifier(Modifier::DIM);
    for slot in 0..ROWS {
        let mut row = Row::new();
        let pos = ROWS - 1 - slot;
        match shown.get(pos) {
            Some(&i) => {
                let on = hits.get(sel) == Some(&i);
                let more = slot == 0 && hits.len() > shown.len();
                row.push(
                    if on {
                        "  ❯ "
                    } else if more {
                        "  ↑ "
                    } else {
                        "    "
                    },
                    if on { accent } else { th.dim() },
                );
                let at = app.prompt_times.get(i).copied().unwrap_or(app.now_unix);
                row.push(format!("{:<7}", ago_short(app.now_unix, at)), th.dim());
                row.push("  ", Style::new());
                let text = truncate(app.prompts[i].lines().next().unwrap_or(""), list_w);
                let pad = list_w.saturating_sub(width(&text)) + 2;
                row.push(text, if on { accent } else { Style::new() });
                row.push(" ".repeat(pad), Style::new());
            }
            None => {
                row.push(" ".repeat(list_w + 15), Style::new());
            }
        }
        let inner = preview_w.saturating_sub(2);
        if slot == 0 {
            row.push(format!("╭{}╮", "─".repeat(inner)), dim);
        } else if slot + 1 == ROWS {
            row.push(format!("╰{}╯", "─".repeat(inner)), dim);
        } else {
            let t = &preview[slot - 1];
            row.push("│", dim);
            row.push(format!(" {t}"), th.dim());
            row.push(" ".repeat(inner.saturating_sub(width(t) + 1)), Style::new());
            row.push("│", dim);
        }
        top.push(row);
    }
    let inner = w.saturating_sub(6);
    top.push(Row::of(format!("  ╭{}╮", "─".repeat(inner)), accent));
    let mut field = Row::of("  │", accent).with(" ", Style::new());
    if query.is_empty() {
        field.push("⌕ ", Style::new());
        field.push("Filter history…", th.dim());
    } else {
        field.push(format!("⌕ {query}"), Style::new());
    }
    let pad = (inner + 3).saturating_sub(field.width());
    field.push(" ".repeat(pad), Style::new());
    field.push("│", accent);
    top.push(field);
    top.push(Row::of(format!("  ╰{}╯", "─".repeat(inner)), accent));
    Panel {
        top,
        bottom: vec![Row::of(
            "  ↑/↓ to navigate · Enter to use · Esc to cancel · ctrl+s to scope",
            th.dim(),
        )],
        fill: None,
    }
}

fn ago_short(now: i64, then: i64) -> String {
    let s = (now - then).max(0);
    match s {
        0..=59 => format!("{s}s ago"),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}
