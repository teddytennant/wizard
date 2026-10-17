//! The bottom of the main screen: spinner, notification row or menu, the prompt box, and the
//! footer line or the shortcut table under it.

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier, Style};

use crate::app::{App, Hint, Menu, SPINNER_FRAMES, VERBS};
use crate::text::{blit, commas, put, truncate, width, wrap_str, Row};

const MENU_ROWS: usize = 5;

/// Draws the bottom region and returns its first row.
pub fn draw(buf: &mut Buffer, app: &mut App) -> u16 {
    let (w, h) = (buf.area.width, buf.area.height);
    let th = app.theme;
    let footer = if app.shortcuts {
        shortcut_rows(app, w as usize)
    } else {
        vec![footer_row(app)]
    };
    let input_w = w.saturating_sub(3).max(1);
    let layout = app.editor.layout(input_w as usize);
    let max_input = (h as usize / 2).max(1);
    let n = layout.len().clamp(1, max_input);
    let menu = menu_rows(app, w as usize);
    let above = menu.as_ref().map_or(1, |m| m.len().max(1));
    let tip = tip(app);
    let spinner = if app.busy.is_some() && menu.is_none() && !app.streaming() {
        2 + usize::from(tip.is_some())
    } else {
        0
    };
    let cap = if app.shortcuts {
        h as usize / 2
    } else {
        h as usize
    };
    let fixed = n + 2 + above + spinner;
    let footer_h = footer.len().min(cap.saturating_sub(fixed).max(1));
    let total = (fixed + footer_h).min(h as usize);
    let top = h - total as u16;
    let mut y = top;

    if spinner > 0 {
        blit(buf, 0, y + 1, &spinner_row(app));
        if let Some(t) = &tip {
            blit(buf, 0, y + 2, t);
        }
        y += spinner as u16;
    }
    match &menu {
        Some(rows) => {
            for row in rows {
                blit(buf, 0, y, row);
                y += 1;
            }
            if rows.is_empty() {
                y += 1;
            }
        }
        None => {
            let esc =
                matches!(app.hint, Some((Hint::Esc, _))).then(|| "Esc again to clear".to_string());
            let editor = (app.editor.line_count() > 1)
                .then(|| format!("ctrl+g to edit in {}", crate::app::editor_display()));
            let search = app
                .search_hint
                .then(|| "ctrl+r to search history".to_string());
            let shown = esc.or(search).or(editor).or_else(|| app.notice.clone());
            if let Some(n) = shown.as_ref() {
                let x = (w as usize).saturating_sub(width(n) + 2);
                put(buf, x as u16, y, n, th.dim());
            }
            y += 1;
        }
    }

    let edge = if app.bash_mode { th.bash } else { th.border };
    put(buf, 0, y, &"─".repeat(w as usize), th.fg(edge));
    if let Some(i) = app.history_at {
        let label = format!(" History {}/{} ", i + 1, app.arrow_history().len());
        put(
            buf,
            3,
            y,
            &label,
            Style::new().fg(Color::Reset).add_modifier(Modifier::DIM),
        );
    }
    y += 1;
    let prefix_style = if app.bash_mode {
        th.fg(th.bash)
    } else if app.busy.is_some() {
        th.dim()
    } else {
        Style::new()
    };
    put(
        buf,
        0,
        y,
        if app.bash_mode { "!" } else { "❯" },
        prefix_style,
    );
    put(buf, 1, y, "\u{a0}", prefix_style);
    let text = app.editor.text().to_string();
    if text.is_empty() {
        if !app.queued.is_empty() {
            put(
                buf,
                2,
                y,
                "Press up to edit queued messages",
                Style::new().add_modifier(Modifier::DIM),
            );
        } else if app.blocks.is_empty() && app.busy.is_none() && !app.submitted {
            let ph = crate::app::PLACEHOLDERS[app.placeholder % crate::app::PLACEHOLDERS.len()];
            put(buf, 2, y, ph, Style::new().add_modifier(Modifier::DIM));
        }
        app.cursor = Some((2, y));
    } else {
        let command_end = command_token(app, &text);
        let cursor = app.editor.cursor();
        let cur_row = layout
            .iter()
            .position(|r| {
                cursor >= r.start && cursor <= r.next && (cursor < r.next || r.last_in_line)
            })
            .unwrap_or(layout.len() - 1);
        let first = cur_row.saturating_sub(n - 1).min(layout.len() - n);
        for (i, r) in layout.iter().enumerate().skip(first).take(n) {
            let ry = y + (i - first) as u16;
            let mut x = 2;
            let line = &text[r.start..r.end];
            if r.start < command_end {
                let cut = command_end.min(r.end) - r.start;
                x = put(buf, x, ry, &line[..cut], th.fg(th.suggestion));
                put(buf, x, ry, &line[cut..], Style::new());
            } else {
                put(buf, x, ry, line, Style::new());
            }
            if i == cur_row {
                let col = width(&text[r.start..cursor.clamp(r.start, r.end)]);
                app.cursor = Some(((2 + col as u16).min(w - 1), ry));
            }
        }
    }
    if let Some(hint) = arg_hint(app, &text) {
        if let Some((cx, cy)) = app.cursor {
            put(buf, cx + 1, cy, hint, th.dim());
        }
    }
    y += n as u16;
    put(buf, 0, y, &"─".repeat(w as usize), th.fg(edge));
    y += 1;
    for row in footer.iter().take(footer_h) {
        if y >= h {
            break;
        }
        blit(buf, 0, y, row);
        y += 1;
    }
    top
}

/// Byte length of the leading `/command` when it names a command the backend has.
fn command_token(app: &App, text: &str) -> usize {
    let Some(rest) = text.strip_prefix('/') else {
        return 0;
    };
    let name = rest.split(char::is_whitespace).next().unwrap_or("");
    let local = ["help", "clear", "model", "status", "resume", "exit", "quit"];
    if app.commands.iter().any(|c| c.name == name) || local.contains(&name) {
        name.len() + 1
    } else {
        0
    }
}

pub fn mode_label(name: &str) -> String {
    if name.is_empty() {
        "default mode".to_string()
    } else if name.contains(' ') {
        name.to_string()
    } else {
        format!("{name} mode")
    }
}

fn footer_row(app: &App) -> Row {
    let th = app.theme;
    let mut row = Row::of("  ", Style::new());
    match app.hint {
        Some((Hint::CtrlC, _)) => return row.with("Press Ctrl-C again to exit", th.dim()),
        Some((Hint::CtrlD, _)) => return row.with("Press Ctrl-D again to exit", th.dim()),
        _ => {}
    }
    if app.bash_mode {
        return row.with("! for shell mode", th.fg(th.bash));
    }
    let cycle = " (shift+tab to cycle)";
    // no session yet, so no modes to name
    // the mode pressed for, at once as the reference shows it: nothing is sent until wizard
    // confirms it (prompts wait in the queue), so the footer cannot run ahead of a prompt
    let (shown, plan) = app.shown_mode();
    if app.cfg.modes.is_empty() && !plan {
        return row.with("? for shortcuts", th.dim());
    }
    let first = match app.shown_index() {
        None => {
            row.push("⏸ plan mode on", th.fg(th.plan));
            row.push(cycle, th.dim());
            false
        }
        Some(0) => {
            row.push(format!("⏸ {} on", mode_label(&shown)), th.dim());
            true
        }
        // each of wizard's other modes has its own mark: sovereign runs on its own like
        // Claude Code's auto mode, chat has no project tools at all
        Some(_) => {
            let (mark, color) = match shown.as_str() {
                "sovereign" | "auto" => ("⏵⏵", th.warning),
                "chat" => ("◇", th.accept),
                _ => ("⏵⏵", th.accept),
            };
            row.push(format!("{mark} {} on", mode_label(&shown)), th.fg(color));
            row.push(cycle, th.dim());
            false
        }
    };
    // typing during a turn takes the hint away; Esc would clear the text first
    if app.busy.is_some() && app.editor.is_empty() {
        row.push(" · esc to interrupt", th.dim());
    } else if first && app.editor.is_empty() && app.menu == Menu::None {
        row.push(" · ? for shortcuts", th.dim());
    }
    row
}

fn has_command(app: &App, name: &str) -> bool {
    app.commands.iter().any(|c| c.name == name)
}

/// The three columns `?` opens under the prompt. Each column wraps on its own.
pub fn shortcut_columns(app: &App) -> [Vec<&'static str>; 3] {
    let mut a = vec!["! for shell mode", "/ for commands", "@ for file paths"];
    if has_command(app, "btw") {
        a.push("/btw for side question");
    }
    let b = vec![
        "double tap esc to clear input",
        "shift + tab to cycle agent modes",
        "ctrl + o for verbose output",
        "ctrl + t to toggle tasks",
        "\\⏎ for newline",
    ];
    let c = vec![
        "ctrl + shift + _ to undo",
        "ctrl + z to suspend",
        "alt + p to switch model",
        "ctrl + s to stash prompt",
        "ctrl + g to edit in $EDITOR",
    ];
    [a, b, c]
}

/// Column starts and wrap widths. Each column is its longest entry plus a gap: two and three
/// columns under the prompt, four and five in the help panel. When that does not fit, the
/// prompt's table shrinks whole columns in proportion and wraps in what is left, gap included;
/// the panel's shrinks only the text and keeps the gaps.
pub fn shortcut_layout(cols: &[Vec<&str>; 3], w: usize, panel: bool) -> [(usize, usize); 3] {
    let (gaps, margin) = if panel {
        ([4.0, 5.0, 0.0], 3)
    } else {
        ([2.0, 3.0, 0.0], 2)
    };
    let text: Vec<f64> = cols
        .iter()
        .map(|c| c.iter().map(|e| width(e)).max().unwrap_or(0) as f64)
        .collect();
    let avail = w.saturating_sub(2 * margin) as f64;
    let natural: f64 = text.iter().sum::<f64>() + gaps.iter().sum::<f64>();
    let deficit = (natural - avail).max(0.0);
    let mut out = [(0, 0); 3];
    let mut acc = 0.0;
    for i in 0..3 {
        let start = acc;
        let (wrap_at, step) = if panel {
            let inner = text[i] - deficit * text[i] / text.iter().sum::<f64>().max(1.0);
            (inner.round(), inner + gaps[i])
        } else {
            let whole = text[i] + gaps[i];
            let shrunk = whole - deficit * whole / natural.max(1.0);
            ((start + shrunk).round() - start.round(), shrunk)
        };
        acc += step;
        let x = margin + start.round() as usize;
        out[i] = (x, wrap_at.max(1.0) as usize);
    }
    // the last column runs to the margin; in the panel the first two keep two columns clear
    out[2].1 = w.saturating_sub(margin + out[2].0).max(1);
    if panel {
        out[0].1 = (out[1].0 - out[0].0).saturating_sub(2).max(1);
        out[1].1 = (out[2].0 - out[1].0).saturating_sub(2).max(1);
    }
    out
}

pub fn shortcut_table(app: &App, w: usize, panel: bool, style: Style) -> Vec<Row> {
    let cols = shortcut_columns(app);
    let layout = shortcut_layout(&cols, w, panel);
    let mut cells: Vec<Vec<String>> = Vec::new();
    for (i, col) in cols.iter().enumerate() {
        let mut lines = Vec::new();
        for entry in col {
            for r in wrap_str(entry, style, layout[i].1.max(1)) {
                lines.push(r.text());
            }
        }
        cells.push(lines);
    }
    let height = cells.iter().map(Vec::len).max().unwrap_or(0);
    let mut rows = Vec::new();
    for y in 0..height {
        let mut line = String::new();
        for (i, col) in cells.iter().enumerate() {
            if let Some(t) = col.get(y) {
                while width(&line) < layout[i].0 {
                    line.push(' ');
                }
                line.push_str(t);
            }
        }
        rows.push(Row::of(line, style));
    }
    rows
}

fn shortcut_rows(app: &App, w: usize) -> Vec<Row> {
    shortcut_table(app, w, false, app.theme.dim())
}

fn spinner_row(app: &App) -> Row {
    let th = app.theme;
    let Some(busy) = &app.busy else {
        return Row::new();
    };
    let elapsed = app.clock.elapsed().saturating_sub(busy.since);
    let ms = elapsed.as_millis() as usize;
    let frame = {
        let n = SPINNER_FRAMES.len();
        let i = (ms / 120) % (2 * n - 2);
        SPINNER_FRAMES[if i < n { i } else { 2 * n - 2 - i }]
    };
    let verb = format!("{}…", VERBS[busy.verb % VERBS.len()]);
    let glow = (ms / 100) % (verb.chars().count() + 12);
    let mut row = Row::of(format!("{frame} "), th.fg(th.brand));
    for (i, ch) in verb.chars().enumerate() {
        let near = i + 1 >= glow && i <= glow + 1;
        let color = if near && ms >= 100 {
            th.shimmer
        } else {
            th.brand
        };
        let s = ch.to_string();
        match row.0.last_mut() {
            Some((t, st)) if st.fg == Some(color) => t.push_str(&s),
            _ => row.0.push((s, th.fg(color))),
        }
    }
    // the seconds come with the first tokens or thinking, not before
    if elapsed.as_secs() >= 1 && (busy.chars > 0 || busy.thinking) {
        let mut info = format!(" ({}", fmt_secs(elapsed.as_secs()));
        if busy.chars > 0 {
            info.push_str(&format!(
                " · ↓ {} tokens",
                commas((busy.chars / 4).max(1) as u64)
            ));
        }
        if busy.thinking {
            info.push_str(" · thinking");
            if matches!(app.cfg.effort.as_str(), "low" | "high" | "xhigh" | "max") {
                info.push_str(&format!(" with {} effort", app.cfg.effort));
            }
        }
        info.push(')');
        row.push(info, th.dim());
    }
    row
}

pub fn fmt_secs(s: u64) -> String {
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{}h {}m {}s", s / 3600, s % 3600 / 60, s % 60)
    }
}

/// `text` as spans with the first case-insensitive occurrence of `needle` in bold.
fn mark_match(text: &str, needle: &str, base: Style, hit: Style) -> Vec<(String, Style)> {
    if !needle.is_empty() {
        if let Some(at) = text.to_lowercase().find(&needle.to_lowercase()) {
            let end = at + needle.len();
            if text.is_char_boundary(at) && text.is_char_boundary(end) {
                return vec![
                    (text[..at].to_string(), base),
                    (text[at..end].to_string(), hit),
                    (text[end..].to_string(), base),
                ];
            }
        }
    }
    vec![(text.to_string(), base)]
}

fn pad_to(row: &mut Row, col: usize) {
    let w = row.width();
    if w < col {
        row.push(" ".repeat(col - w), Style::new());
    }
}

/// Rows of the open menu, bottom aligned by the caller: whole entries only, at most five rows,
/// scrolled just far enough to show the selection. `None` when no menu is open.
fn menu_rows(app: &App, w: usize) -> Option<Vec<Row>> {
    let th = app.theme;
    match &app.menu {
        Menu::None => None,
        Menu::Slash { items, sel } => {
            let query = app.editor.text().trim_start_matches('/').to_string();
            if items.is_empty() {
                let msg = format!("No commands match \"/{query}\"");
                return Some(vec![Row::of("  ", Style::new()).with(msg, th.dim())]);
            }
            let longest = app
                .commands
                .iter()
                .map(|c| width(&c.name) + 1)
                .max()
                .unwrap_or(0);
            let name_w = (longest + 3).min(w * 2 / 5).max(8);
            let desc_x = 4 + name_w;
            let desc_w = w.saturating_sub(desc_x + 2).max(4);
            let entry = |i: usize, selected: bool| -> Vec<Row> {
                let cmd = &app.commands[items[i]];
                let color = if selected {
                    th.suggestion
                } else {
                    th.secondary
                };
                let base = th.fg(color);
                let hit = if selected {
                    base.add_modifier(Modifier::BOLD)
                } else {
                    Style::new().add_modifier(Modifier::BOLD)
                };
                let mut first = Row::of(if selected { "  ❯ " } else { "    " }, base);
                let name = truncate(&format!("/{}", cmd.name), name_w.saturating_sub(1));
                first.push("/", base);
                first.0.extend(
                    mark_match(&name[1..], &query, base, hit)
                        .into_iter()
                        .filter(|s| !s.0.is_empty()),
                );
                let lines = wrap_str(&cmd.description, base, desc_w);
                let mut out = Vec::new();
                for (li, line) in lines.iter().take(2).enumerate() {
                    let mut text = line.text();
                    if li == 1 && lines.len() > 2 {
                        let rest: Vec<String> = lines[1..].iter().map(Row::text).collect();
                        text = truncate(&rest.join(" "), desc_w);
                    }
                    let mut row = if li == 0 {
                        std::mem::take(&mut first)
                    } else {
                        Row::new()
                    };
                    pad_to(&mut row, desc_x);
                    let marked = if li == 0 {
                        mark_match(&text, &query, base, hit)
                    } else {
                        vec![(text, base)]
                    };
                    row.0.extend(marked.into_iter().filter(|s| !s.0.is_empty()));
                    out.push(row);
                }
                if out.is_empty() {
                    out.push(first);
                }
                out
            };
            Some(window(items.len(), *sel, |i| entry(i, i == *sel)))
        }
        Menu::Mention { items, sel } => {
            if items.is_empty() {
                return None;
            }
            let entry = |i: usize| -> Vec<Row> {
                let selected = *sel == Some(i);
                let style = th.fg(if selected {
                    th.suggestion
                } else {
                    th.secondary
                });
                let lead = if selected { "  ❯ " } else { "    " };
                let text = match items[i].strip_prefix(crate::app::AGENT) {
                    Some(a) => {
                        let (name, desc) = a.split_once('\t').unwrap_or((a, ""));
                        let head = format!("* {name} (agent) – ");
                        let room = w.saturating_sub(4 + width(&head) + 2).min(60);
                        format!("{head}{}", truncate(desc, room))
                    }
                    None => truncate(&format!("+ {}", items[i]), w.saturating_sub(6)),
                };
                vec![Row::of(lead, style).with(text, style)]
            };
            Some(window(items.len(), sel.unwrap_or(0), entry))
        }
    }
}

/// The entries that fit in the menu's rows with `sel` among them, starting as early as possible.
fn window(n: usize, sel: usize, entry: impl Fn(usize) -> Vec<Row>) -> Vec<Row> {
    let fits = |top: usize| -> (Vec<Row>, usize) {
        let mut rows = Vec::new();
        let mut last = top;
        for i in top..n {
            let e = entry(i);
            if rows.len() + e.len() > MENU_ROWS && !rows.is_empty() {
                break;
            }
            rows.extend(e.into_iter().take(MENU_ROWS));
            last = i;
        }
        (rows, last)
    };
    let mut top = 0;
    loop {
        let (rows, last) = fits(top);
        if sel <= last || top >= sel {
            return rows;
        }
        top += 1;
    }
}

pub fn color_or(c: Option<Color>, d: Color) -> Color {
    c.unwrap_or(d)
}

/// The argument hint a command takes, drawn after it once it is completed with nothing typed
/// after it: `/model  [model]`.
fn arg_hint<'a>(app: &'a App, text: &str) -> Option<&'a str> {
    let name = text.strip_prefix('/')?.strip_suffix(' ')?;
    if name.contains(' ') {
        return None;
    }
    let cmd = app.commands.iter().find(|c| c.name == name)?;
    (!cmd.input_hint.is_empty()).then_some(cmd.input_hint.as_str())
}

/// Things worth knowing about this look, one shown under the spinner once a turn has run a
/// few seconds.
const TIPS: [&str; 5] = [
    "Tip: Press ctrl+o to see every tool call in full",
    "Tip: Press shift+tab to switch between Wizard's modes",
    "Tip: Press ctrl+r to search the prompts you sent this session",
    "Tip: Press esc twice on an empty prompt to go back to an earlier one",
    "Tip: Press ctrl+g to write the prompt in your editor",
];

fn tip(app: &App) -> Option<Row> {
    let busy = app.busy.as_ref()?;
    // a turn for a prompt that waited in the queue carries on from the one before: no wait
    let queued = app
        .blocks
        .iter()
        .rposition(|b| matches!(b, crate::app::Block::User { .. }))
        .is_some_and(|i| app.from_queue.contains(&i));
    if !queued && app.clock.elapsed().saturating_sub(busy.since).as_secs() < 3 {
        return None;
    }
    let text = TIPS[app.turns % TIPS.len()];
    Some(Row::of("  ⎿ \u{a0}", app.theme.dim()).with(text, app.theme.dim()))
}
