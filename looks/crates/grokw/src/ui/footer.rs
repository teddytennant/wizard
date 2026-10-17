// OWNER: footer (header chips, status line, dock sections other than the queue are not built)
//! The rows around the transcript: the header (branch, cwd, context counter), the turn status
//! row with its spinner and timers, the banner row, the queue dock, the shortcuts bar and the
//! toast. Row positions come from `Layout`; the strings and colours are spec 4.10 to 4.16.

use agent_core::transcript::{Part, Role};
use agent_core::ToolStatus;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use tuikit::paint::put_str;
use tuikit::width::display_width;

use super::transcript::fmt_duration;
use super::{anim, bold, dim, put, put_right, st, tools, Layout};
use crate::app::{App, Focus, Screen};
use crate::theme::blend;

/// `1.5K`, `23K`, `1.2M`: the context counter's number format.
pub fn fmt_count(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else if n < 10_000 {
        format!("{:.1}K", n as f64 / 1000.0)
    } else if n < 1_000_000 {
        format!("{}K", n / 1000)
    } else if n < 10_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else {
        format!("{}M", n / 1_000_000)
    }
}

/// `⇣` token counter: `812`, `1.48k`, `24.6k`, `100k`, `1.23m`.
pub fn fmt_tokens(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else if n < 10_000 {
        format!("{:.2}k", n as f64 / 1000.0)
    } else if n < 100_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else if n < 1_000_000 {
        format!("{}k", n / 1000)
    } else if n < 10_000_000 {
        format!("{:.2}m", n as f64 / 1e6)
    } else {
        format!("{:.1}m", n as f64 / 1e6)
    }
}

pub fn draw_header(buf: &mut Buffer, app: &App, lay: &Layout) {
    let th = &app.theme;
    let y = lay.header_y;
    let x0 = lay.hpad;
    let right = lay.w.saturating_sub(lay.hpad);
    let clip = Rect::new(x0, y, right.saturating_sub(x0 + 2), 1);
    let mut x = x0;
    if let Some(b) = &app.branch {
        x = put_str(
            buf,
            x,
            y,
            "\u{e0a0} ",
            Style::new().fg(th.text_primary).add_modifier(Modifier::DIM),
            clip,
        );
        x = put_str(
            buf,
            x,
            y,
            b,
            Style::new().fg(th.text_primary).add_modifier(Modifier::DIM),
            clip,
        );
        x = put_str(buf, x, y, " ", st(th.gray), clip);
    }
    let cwd = app.cwd_label();
    let room = clip.right().saturating_sub(x) as usize;
    let shown = tuikit::width::truncate(&cwd, room);
    put_str(buf, x, y, &shown, st(th.gray_dim), clip);
    if app.screen == Screen::Session {
        if let Some((used, total)) = app.context() {
            let pct = used as f32 / total.max(1) as f32 * 100.0;
            let s = format!("{} / {}", fmt_count(used), fmt_count(total));
            put_right(buf, right, y, &s, st(th.context_color(pct)));
        }
    }
}

// ---- turn status row ------------------------------------------------------------------------

struct Phase {
    key: String,
    /// Spinner and label colour.
    color: Color,
    spans: Vec<(String, Style)>,
}

fn open_assistant(app: &App) -> Option<&agent_core::transcript::Message> {
    app.tr
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant && m.took.is_none())
}

fn phase(app: &App) -> Phase {
    let th = &app.theme;
    let plain = |key: &str, s: &str| Phase {
        key: key.to_string(),
        color: th.accent_user,
        spans: vec![(s.to_string(), st(th.accent_user))],
    };
    // a stream retry shows until the retried reply produces something new
    if let Some((n, size)) = app.retry {
        if app.reply_size() == size {
            return Phase {
                key: format!("retry{n}"),
                color: th.warning,
                spans: vec![(
                    format!("Connection failed | Retrying (attempt {n})..."),
                    st(th.warning),
                )],
            };
        }
    }
    let Some(m) = open_assistant(app) else {
        return plain("wait", "Waiting for response…");
    };
    let n = m.parts.len();
    match m.parts.last() {
        None => plain("wait", "Waiting for response…"),
        Some(Part::Thought { took: None, .. }) => plain("think", "Thinking…"),
        Some(Part::Thought { .. }) => plain(&format!("wait{n}"), "Waiting for response…"),
        Some(Part::Text(_)) => plain("respond", "Responding…"),
        Some(Part::Tool(c)) => {
            if matches!(c.status, ToolStatus::Completed | ToolStatus::Failed) {
                return plain(&format!("wait{n}"), "Waiting for response…");
            }
            let class = tools::classify(c);
            let arg = |keys: &[&str]| {
                keys.iter()
                    .find_map(|k| c.input.get(*k).and_then(|v| v.as_str()))
                    .map(str::to_string)
                    .unwrap_or_else(|| c.title.clone())
            };
            let mut spans = vec![("Run ".to_string(), st(th.gray))];
            let name_style = st(th.fuzzy_accent);
            let tick = st(th.backtick);
            match class {
                Some(tools::Class::Read) => {
                    spans.push(("Read ".into(), name_style));
                    spans.push(("`".into(), tick));
                    spans.push((arg(&["path", "file_path"]), name_style));
                    spans.push(("`".into(), tick));
                }
                Some(tools::Class::Edit) => {
                    spans.push(("Edit ".into(), name_style));
                    spans.push(("`".into(), tick));
                    spans.push((arg(&["path", "file_path"]), name_style));
                    spans.push(("`".into(), tick));
                }
                Some(tools::Class::Execute) => {
                    spans.push((tools::command_of(c), st(th.accent_user)))
                }
                Some(tools::Class::Search) => {
                    spans.push((arg(&["pattern", "query"]), st(th.accent_user)))
                }
                Some(tools::Class::WebSearch) => {
                    spans.push(("Web search: ".into(), st(th.accent_user)));
                    spans.push((arg(&["query"]), st(th.accent_user)));
                }
                _ => spans.push((c.title.clone(), st(th.accent_user))),
            }
            Phase {
                key: format!("tool{}", c.id),
                color: th.accent_success,
                spans,
            }
        }
    }
}

/// Spans of `p` cut to `room` cells with an ellipsis.
fn clip_phase(p: &[(String, Style)], room: usize) -> Vec<(String, Style)> {
    let total: usize = p.iter().map(|(t, _)| display_width(t)).sum();
    if total <= room {
        return p.to_vec();
    }
    let mut out = Vec::new();
    let mut left = room.saturating_sub(1);
    for (t, s) in p {
        let w = display_width(t);
        if w <= left {
            out.push((t.clone(), *s));
            left -= w;
        } else {
            out.push((tuikit::width::truncate(t, left + 1), *s));
            return out;
        }
    }
    out
}

pub fn draw_strips(buf: &mut Buffer, app: &mut App, lay: &Layout) {
    let th = app.theme.clone();
    if let Some(y) = lay.turn_y {
        let p = phase(app);
        let now = app.now();
        if app.turn.phase != p.key {
            app.turn.phase = p.key.clone();
            app.turn.phase_started = now;
        }
        let x0 = lay.hpad + 1;
        let right = lay.w.saturating_sub(lay.hpad);
        let tick = app.tick();
        // spinner
        let spin = anim::spinner(tick);
        let mut sbuf = [0u8; 4];
        put(buf, x0, y, spin.encode_utf8(&mut sbuf), st(p.color));
        // right block: turn timer, tokens, [stop]
        let elapsed = app
            .turn
            .started
            .map(|s| now.saturating_sub(s))
            .unwrap_or_default();
        let mut right_text = fmt_duration(elapsed);
        let tokens = app.tr.usage.context_tokens;
        if tokens > 0 {
            right_text.push_str(&format!(" ⇣{}", fmt_tokens(tokens)));
        }
        right_text.push(' ');
        let stop = "[stop]";
        let rw = display_width(&right_text) + display_width(stop);
        let rx = right.saturating_sub(rw as u16);
        put(buf, rx, y, &right_text, st(th.gray));
        put(
            buf,
            rx + display_width(&right_text) as u16,
            y,
            stop,
            st(th.gray),
        );
        // label and its phase timer
        let phase_t = format!(
            " {}",
            fmt_duration(now.saturating_sub(app.turn.phase_started))
        );
        let room = (rx.saturating_sub(x0 + 3)) as usize;
        let room_label = room.saturating_sub(display_width(&phase_t));
        let spans = clip_phase(&p.spans, room_label);
        let mut x = x0 + 2;
        for (t, s) in &spans {
            x = put(buf, x, y, t, *s);
        }
        put(buf, x, y, &phase_t, st(th.gray));
    }
    if let (Some(y), Some(b)) = (lay.banner_y, app.banner.clone()) {
        let remaining = b.until.saturating_sub(app.tick());
        let opacity = if b.fade && remaining < 9 {
            remaining as f32 / 9.0
        } else {
            1.0
        };
        let base = th.accent_user;
        let color = blend(th.bg_base, base, opacity);
        let mut x = if b.mode { lay.hpad } else { lay.hpad + 1 };
        for (i, (t, bolded)) in b.runs.iter().enumerate() {
            let style = if *bolded {
                bold(color)
            } else if b.mode || i > 0 {
                st(color)
            } else {
                st(blend(th.bg_base, th.gray, opacity))
            };
            x = put(buf, x, y, t, style);
        }
    }
    if let Some((y0, h)) = lay.dock {
        draw_dock(buf, app, y0, h, lay);
    }
}

fn draw_dock(buf: &mut Buffer, app: &App, y0: u16, _h: u16, lay: &Layout) {
    let th = &app.theme;
    let n = app.queue.len();
    let x = lay.hpad + 1;
    let right = lay.w.saturating_sub(lay.hpad);
    let nx = put(buf, x, y0, "▾ ", st(th.gray));
    let nx = put(buf, nx, y0, "Queued", bold(th.gray_bright));
    let nx = put(buf, nx, y0, &format!(" {n} "), st(th.gray));
    for c in nx..right {
        put(buf, c, y0, "─", st(th.gray_dim));
    }
    for (i, q) in app.queue.iter().take(3).enumerate() {
        let y = y0 + 1 + i as u16;
        put(buf, lay.hpad + 3, y, &format!("#{}", i + 1), st(th.gray));
        let room = right.saturating_sub(lay.hpad + 7) as usize;
        let t = tuikit::width::truncate(q.lines().next().unwrap_or(""), room);
        put(buf, lay.hpad + 6, y, &t, st(th.accent_user));
    }
}

/// Todo rows after `h` (hide done) is applied.
pub fn todo_visible(app: &App) -> Vec<&agent_core::Todo> {
    app.tr
        .todos
        .iter()
        .filter(|t| !(app.todo.hide_done && t.status == agent_core::TodoStatus::Completed))
        .collect()
}

/// The todo pane: a frame in `selection_border` under the header with one row per todo, the
/// selected row banded, a thumb when it scrolls and `✗` in the top corner (spec 4.15).
pub fn draw_todo(buf: &mut Buffer, app: &App, lay: &Layout) {
    let Some((y0, rows)) = lay.todo else { return };
    let th = &app.theme;
    let c = st(th.selection_border);
    let (l, r) = (1u16, lay.w.saturating_sub(2));
    let items = todo_visible(app);
    let inner = rows.saturating_sub(2) as usize;
    put(buf, l, y0, "┌", c);
    put(buf, r, y0, "✗", c);
    put(buf, l, y0 + rows - 1, "└", c);
    put(buf, r, y0 + rows - 1, "┘", c);
    let top = app.todo.top.min(items.len().saturating_sub(inner));
    for i in 0..inner {
        let y = y0 + 1 + i as u16;
        put(buf, l, y, "│", c);
        put(buf, r, y, "│", c);
        let Some(t) = items.get(top + i) else {
            continue;
        };
        let selected = app.focus == Focus::Todo && top + i == app.todo.sel;
        let bg = if selected { th.bg_light } else { th.bg_base };
        if selected {
            super::fill(buf, Rect::new(5, y, lay.w.saturating_sub(11), 1), bg);
        }
        let (icon, ic, tc, bolded) = match t.status {
            agent_core::TodoStatus::Completed => ("✓", th.accent_success, th.gray_bright, false),
            agent_core::TodoStatus::InProgress => ("▶", th.warning, th.text_primary, true),
            agent_core::TodoStatus::Pending => ("□", th.gray_bright, th.text_primary, false),
        };
        put(buf, 5, y, icon, Style::new().fg(ic).bg(bg));
        let mut ts = Style::new().fg(tc).bg(bg);
        if bolded {
            ts = ts.add_modifier(Modifier::BOLD);
        }
        let room = (r.saturating_sub(10)) as usize;
        put(buf, 7, y, &tuikit::width::truncate(&t.text, room), ts);
    }
    if items.len() > inner && inner > 0 {
        let sx = lay.w.saturating_sub(5);
        for i in 0..inner - 1 {
            put(
                buf,
                sx,
                y0 + 1 + i as u16,
                "█",
                st(th.scrollbar_fg).bg(th.scrollbar_fg),
            );
        }
        if top + inner < items.len() {
            put(
                buf,
                lay.w.saturating_sub(7),
                y0 + inner as u16,
                "▼",
                st(th.gray),
            );
        }
    }
}

pub fn dock_rows(app: &App) -> u16 {
    if app.queue.is_empty() {
        0
    } else {
        1 + app.queue.len().min(3) as u16
    }
}

pub fn banner_text(app: &App) -> Option<()> {
    app.banner.as_ref().map(|_| ())
}

// ---- shortcuts bar --------------------------------------------------------------------------

pub type Hint = (String, String);

fn h(k: &str, l: &str) -> Hint {
    (k.to_string(), l.to_string())
}

/// The hints for the current state, in order, ending with the help hint.
pub fn hints(app: &App) -> Vec<Hint> {
    let mut v: Vec<Hint> = Vec::new();
    let has_text = !app.ed.is_empty();
    if app.focus == Focus::Todo {
        v.push(h(
            "h",
            if app.todo.hide_done {
                "show done"
            } else {
                "hide done"
            },
        ));
    } else if app.view.nav.viewer.is_some() {
        v.push(h("Esc", "close"));
        v.push(h("Enter", "quote"));
        v.push(h("y", "copy"));
        v.push(h("Shift+y", "copy path"));
    } else if let Some(f) = app.view.nav.find.as_ref() {
        // the find bar: the arrows step through matches, `n`/`N` once the query is accepted
        if app.vim_mode && f.composing {
            v.push(h("Enter", "go"));
        } else if app.vim_mode {
            v.push(h("n/N", "next/prev"));
        } else {
            v.push(h("↓/↑", "next/prev"));
        }
        v.push(h("Esc", "cancel"));
    } else if app.focus == Focus::Scrollback && app.vim_mode {
        let sel = app
            .view
            .selected
            .and_then(|k| app.view.doc.index_of(k))
            .map(|i| &app.view.doc.entries[i]);
        match sel {
            Some(e) if e.foldable && e.collapsed => v.push(h("l", "expand")),
            Some(e) if e.foldable => v.push(h("h", "collapse")),
            _ => {}
        }
        if sel.is_some() {
            v.push(h("y", "copy"));
        }
        v.push(h("Space", "prompt"));
        if sel.is_some() {
            v.push(h("Enter", "open"));
        }
        v.push(h("j/k", "nav"));
        v.push(h("Shift+l/h", "turn"));
        if sel.is_none() {
            v.push(h(
                "Ctrl+e",
                if app.view.think_open {
                    "collapse thinking"
                } else {
                    "expand thinking"
                },
            ));
            v.push(h("g/Shift+g", "top/btm"));
        }
    } else if app.focus == Focus::Scrollback {
        // what the keys do depends on the selected block
        let sel = app
            .view
            .selected
            .and_then(|k| app.view.doc.index_of(k))
            .map(|i| &app.view.doc.entries[i]);
        // a member of an open group acts on its own block: `→` opens it, `←` closes it
        let member = app
            .view
            .member()
            .and_then(|m| sel.and_then(|e| e.members.get(m)));
        match sel {
            _ if member.is_some_and(|m| !m.open) => {
                v.push(h("→", "expand"));
                v.push(h("Enter", "open"));
            }
            _ if member.is_some() => {
                v.push(h("←", "collapse"));
                v.push(h("Enter", "open"));
            }
            Some(e) if e.kind == super::transcript::Kind::Group && e.collapsed => {
                v.push(h("Enter", "expand"));
            }
            Some(e) if e.foldable && e.collapsed => v.push(h("→", "expand")),
            Some(e) if e.foldable => {
                v.push(h("←", "collapse"));
                v.push(h("Enter", "open"));
            }
            _ => {
                v.push(h("Space", "prompt"));
                v.push(h("Enter", "open"));
            }
        }
        v.push(h(
            "Ctrl+e",
            if app.view.think_open {
                "collapse thinking"
            } else {
                "expand thinking"
            },
        ));
    } else if app.inp.hist.is_some() {
        v.push(h("↑/↓", "nav"));
        v.push(h("PgUp/PgDn", "page"));
        v.push(h("Enter", "select"));
        v.push(h("Esc", "cancel"));
    } else {
        let busy = app.busy();
        let on_paste =
            super::composer::input::chip_at(app).is_some_and(|(i, _)| !app.inp.chips[i].image);
        if on_paste {
            v.push(h("Enter", "expand"));
        } else if has_text {
            let label = if busy { "queue" } else { "send" };
            // multiline swaps the pair: Enter breaks the line, Shift+Enter sends
            let key = if app.multiline {
                "Shift+Enter"
            } else {
                "Enter"
            };
            v.push(h(key, label));
        } else if busy && !app.queue.is_empty() {
            v.push(h("Enter", "send now"));
        }
        v.push(h("Shift+Tab", "mode"));
        if busy {
            v.push(h("Ctrl+c", "cancel"));
            if has_text {
                v.push(h("Ctrl+Enter", "send now"));
            }
            if !app.queue.is_empty() {
                v.push(h("Ctrl+;", "queue"));
            }
        }
    }
    v.push(h("Ctrl+.", "shortcuts"));
    v
}

pub fn draw_bar(buf: &mut Buffer, app: &App, lay: &Layout) {
    if app.screen == Screen::Home {
        return;
    }
    if app.modal.as_ref().is_some_and(|m| m.hides_bar()) {
        return;
    }
    let th = &app.theme;
    let y = lay.bar_y;
    let x0 = lay.hpad;
    let clip = Rect::new(x0, y, lay.w.saturating_sub(2 * lay.hpad), 1);
    if let Some(p) = &app.pending {
        // `Ctrl+q:press again to quit`
        let (k, rest) = p.label.split_once(':').unwrap_or((&p.label, ""));
        let x = put_str(buf, x0, y, k, bold(th.text_secondary), clip);
        put_str(buf, x, y, &format!(":{rest}"), st(th.gray), clip);
        return;
    }
    let mut x = x0;
    let hs = hints(app);
    let right = clip.right();
    // each piece is painted only if it fits; the first that does not ends the bar
    let mut piece = |x: &mut u16, text: &str, style: Style| -> bool {
        if *x as usize + display_width(text) > right as usize {
            return false;
        }
        *x = put_str(buf, *x, y, text, style, clip);
        true
    };
    for (i, (k, l)) in hs.iter().enumerate() {
        if i > 0 && !piece(&mut x, "  │  ", dim(th.gray)) {
            return;
        }
        // a pair such as `↑/↓` keeps its slash muted
        match k
            .split_once('/')
            .filter(|(a, b)| !a.is_empty() && !b.is_empty())
        {
            Some((a, b)) => {
                if !piece(&mut x, a, bold(th.text_secondary))
                    || !piece(&mut x, "/", st(th.gray))
                    || !piece(&mut x, b, bold(th.text_secondary))
                {
                    return;
                }
            }
            None => {
                if !piece(&mut x, k, bold(th.text_secondary)) {
                    return;
                }
            }
        }
        if !piece(&mut x, ":", st(th.gray)) || !piece(&mut x, l, st(th.gray)) {
            return;
        }
    }
}

pub fn draw_toast(buf: &mut Buffer, app: &App, lay: &Layout) {
    let Some(t) = &app.toast else { return };
    let th = &app.theme;
    let y = (lay.view.bottom()).saturating_sub(1);
    let avail = lay.w.saturating_sub(lay.hpad * 2 + 4) as usize;
    let text = format!(" {} ", tuikit::width::truncate(&t.text, avail));
    put_right(
        buf,
        lay.w.saturating_sub(lay.hpad + 1),
        y,
        &text,
        bold(th.text_secondary).bg(th.bg_base),
    );
}
