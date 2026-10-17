// OWNER: sidebar
//! Right-hand panel: 42 columns on `backgroundPanel`, shown automatically when the terminal is
//! wider than 120 columns and as a dimmed overlay when toggled open on a narrow one.
//!
//! Layout follows opencode's `routes/session/sidebar.tsx` and the `feature-plugins/sidebar/*`
//! sections: a scrollbox (title, Context, Todo, Modified Files) above a footer (Getting started
//! box when no provider is connected, directory, product line). wizard reports no MCP servers
//! over ACP, so that section is left out like opencode does when there are none. wizard has no
//! language servers either, which opencode draws as the fixed `LSPs are disabled` text.

use agent_core::transcript::Part;
use agent_core::{TodoStatus, ToolStatus};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use tuikit::paint::{fill, pad, put_line};
use tuikit::scroll::{ScrollState, Scrollbar};
use tuikit::width::{display_width, truncate_left, wrap};

use super::footer;
use crate::app::App;

pub const WIDTH: u16 = 42;

/// Rows one wheel notch moves the sidebar scrollbox.
const WHEEL_ROWS: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Todo,
    Files,
}

/// Sidebar state that outlives a frame: scroll position, collapsed sections, and where the
/// last draw put things so the mouse handler can find them.
#[derive(Debug, Default)]
pub struct SideState {
    offset: usize,
    todo_collapsed: bool,
    files_collapsed: bool,
    /// Whole panel as last drawn; empty when the sidebar was not drawn.
    panel: Rect,
    /// Screen rows holding a `▼`/`▶` toggle.
    toggles: Vec<(u16, Section)>,
    /// Content rows and viewport rows of the scrollbox as last drawn.
    extent: (usize, usize),
}

impl SideState {
    fn collapsed(&self, s: Section) -> bool {
        match s {
            Section::Todo => self.todo_collapsed,
            Section::Files => self.files_collapsed,
        }
    }

    fn toggle(&mut self, s: Section) {
        match s {
            Section::Todo => self.todo_collapsed = !self.todo_collapsed,
            Section::Files => self.files_collapsed = !self.files_collapsed,
        }
    }
}

/// Wheel and click handling for the sidebar. Returns true when the event landed on the panel.
pub fn on_mouse(app: &mut App, ev: crossterm::event::MouseEvent) -> bool {
    use crossterm::event::{MouseButton, MouseEventKind};
    if !app.sidebar_state().shown {
        return false;
    }
    let p = app.sb.panel;
    if ev.column < p.x || ev.column >= p.right() || ev.row < p.y || ev.row >= p.bottom() {
        return false;
    }
    let max = app.sb.extent.0.saturating_sub(app.sb.extent.1);
    match ev.kind {
        MouseEventKind::ScrollUp => app.sb.offset = app.sb.offset.saturating_sub(WHEEL_ROWS),
        MouseEventKind::ScrollDown => app.sb.offset = (app.sb.offset + WHEEL_ROWS).min(max),
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some((_, s)) = app.sb.toggles.iter().find(|(y, _)| *y == ev.row).copied() {
                app.sb.toggle(s);
            }
        }
        _ => {}
    }
    true
}

// ---- data ------------------------------------------------------------------------------

/// opencode's `Locale.number` is `toLocaleString`: thousands separators.
pub fn group_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `Intl.NumberFormat("en-US", {style: "currency", currency: "USD"})`.
pub fn money(v: f64) -> String {
    let cents = (v.max(0.0) * 100.0).round() as u64;
    format!("${}.{:02}", group_thousands(cents / 100), cents % 100)
}

/// One row of the Modified Files section.
#[derive(Debug, PartialEq, Eq)]
pub struct Changed {
    pub file: String,
    pub additions: usize,
    pub deletions: usize,
}

/// Files the transcript's finished edit and write calls touched, with line counts summed per
/// file in the order they first appeared. wizard's diffs are old and new snippets, so counts
/// are per call, not a whole-session net diff like opencode's `session.diff`.
pub fn modified_files(app: &App) -> Vec<Changed> {
    let mut out: Vec<Changed> = Vec::new();
    for m in &app.transcript.messages {
        for p in &m.parts {
            let Part::Tool(c) = p else { continue };
            if c.status != ToolStatus::Completed {
                continue;
            }
            let Some(d) = &c.diff else { continue };
            let (a, r) = tuikit::diff::count(d.old.as_deref().unwrap_or(""), &d.new);
            let file = relative(&d.path, &app.cwd);
            match out.iter_mut().find(|x| x.file == file) {
                Some(x) => {
                    x.additions += a;
                    x.deletions += r;
                }
                None => out.push(Changed {
                    file,
                    additions: a,
                    deletions: r,
                }),
            }
        }
    }
    out
}

fn relative(path: &str, cwd: &str) -> String {
    let base = cwd.trim_end_matches('/');
    match path.strip_prefix(base) {
        Some(rest) if rest.starts_with('/') && !base.is_empty() => rest[1..].to_string(),
        _ => path.to_string(),
    }
}

// ---- scrollbox content -----------------------------------------------------------------

struct Content {
    lines: Vec<Line<'static>>,
    toggles: Vec<(usize, Section)>,
}

fn span(s: impl Into<String>, fg: Color, bold: bool) -> Span<'static> {
    let mut st = Style::new().fg(fg);
    if bold {
        st = st.add_modifier(Modifier::BOLD);
    }
    Span::styled(s.into(), st)
}

fn heading(label: &str, toggle: Option<bool>, t: &tuikit::Theme) -> Line<'static> {
    let mut v = Vec::new();
    if let Some(open) = toggle {
        v.push(span(if open { "▼ " } else { "▶ " }, t.text, false));
    }
    v.push(span(label, t.text, true));
    Line::from(v)
}

/// `w` is the width of the content box, `title_w` the title's (it has a right padding of its
/// own).
fn content(app: &App, w: usize, title_w: usize) -> Content {
    let t = &app.theme;
    let mut sections: Vec<Vec<Line<'static>>> = Vec::new();
    let mut toggles = Vec::new();
    // `(index of the block, section)`; turned into row numbers once the blocks are joined.
    let mut local: Vec<(usize, Section)> = Vec::new();

    // Title
    let title = app.session_title();
    sections.push(
        wrap(&title, title_w.max(1))
            .into_iter()
            .map(|l| Line::from(span(l, t.text, true)))
            .collect(),
    );

    // Context
    let u = &app.transcript.usage;
    let tokens = footer::context_tokens(u);
    let percent = if u.context_window > 0 && tokens > 0 {
        ((tokens as f64 / u.context_window as f64) * 100.0).round() as u64
    } else {
        0
    };
    let muted = |s: String| Line::from(span(s, t.text_muted, false));
    sections.push(vec![
        heading("Context", None, t),
        muted(format!("{} tokens", group_thousands(tokens))),
        muted(format!("{percent}% used")),
        muted(format!("{} spent", money(u.cost_usd.unwrap_or(0.0)))),
    ]);

    // LSP: wizard runs no language servers, so this is always the empty-and-disabled state.
    sections.push(vec![
        heading("LSP", None, t),
        muted("LSPs are disabled".to_string()),
    ]);

    // Todo: only while something is not finished
    let todos = &app.transcript.todos;
    if todos.iter().any(|x| x.status != TodoStatus::Completed) {
        let toggle = (todos.len() > 2).then_some(!app.sb.collapsed(Section::Todo));
        let mut block = vec![heading("Todo", toggle, t)];
        if toggle.is_some() {
            local.push((sections.len(), Section::Todo));
        }
        if toggle != Some(false) {
            for item in todos {
                let (mark, fg) = match item.status {
                    TodoStatus::Completed => ("[✓] ", t.text_muted),
                    TodoStatus::InProgress => ("[•] ", t.warning),
                    TodoStatus::Pending => ("[ ] ", t.text_muted),
                };
                for (i, l) in wrap(&item.text, w.saturating_sub(4).max(1))
                    .into_iter()
                    .enumerate()
                {
                    let lead = if i == 0 { mark } else { "    " };
                    block.push(Line::from(vec![span(lead, fg, false), span(l, fg, false)]));
                }
            }
        }
        sections.push(block);
    }

    // Modified Files
    let files = modified_files(app);
    if !files.is_empty() {
        let toggle = (files.len() > 2).then_some(!app.sb.collapsed(Section::Files));
        let mut block = vec![heading("Modified Files", toggle, t)];
        if toggle.is_some() {
            local.push((sections.len(), Section::Files));
        }
        if toggle != Some(false) {
            for f in &files {
                block.push(file_row(f, w, t));
            }
        }
        sections.push(block);
    }

    // Join with one blank row between sections and turn block numbers into row numbers.
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut starts = Vec::new();
    for (i, s) in sections.into_iter().enumerate() {
        if i > 0 {
            lines.push(Line::default());
        }
        starts.push(lines.len());
        lines.extend(s);
    }
    for (block, sec) in local {
        toggles.push((starts[block], sec));
    }
    Content { lines, toggles }
}

fn count_text(f: &Changed) -> String {
    [
        (f.additions > 0).then(|| format!("+{}", f.additions)),
        (f.deletions > 0).then(|| format!("-{}", f.deletions)),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ")
}

fn file_row(f: &Changed, w: usize, t: &tuikit::Theme) -> Line<'static> {
    let cw = display_width(&count_text(f));
    let name = truncate_left(&f.file, 36usize.saturating_sub(cw).max(2));
    let used = display_width(&name) + cw;
    let gap = w.saturating_sub(used).max(1);
    let mut v = vec![span(name, t.text_muted, false), Span::raw(" ".repeat(gap))];
    if f.additions > 0 {
        v.push(span(format!("+{}", f.additions), t.diff_added, false));
    }
    if f.additions > 0 && f.deletions > 0 {
        v.push(Span::raw(" "));
    }
    if f.deletions > 0 {
        v.push(span(format!("-{}", f.deletions), t.diff_removed, false));
    }
    Line::from(v)
}

// ---- footer ----------------------------------------------------------------------------

/// Lines of the sidebar footer at width `w`, bottom block of the panel. Rows that carry a
/// background of their own (the Getting started box) set it on the line.
fn footer_lines(app: &App, w: usize, copy: &Copy) -> Vec<Line<'static>> {
    let t = &app.theme;
    let mut out: Vec<Line<'static>> = Vec::new();

    if getting_started(app) {
        out.extend(gs_box(&app.theme, w, copy));
        out.push(Line::default());
    }

    // `<parent>/` muted, last segment (with `:branch`) in text
    let label = footer::dir_label(app);
    let (parent, name) = match label.rfind('/') {
        Some(i) => (&label[..i], &label[i + 1..]),
        None => ("", label.as_str()),
    };
    let full = format!("{parent}/{name}");
    let split = parent.chars().count() + 1;
    let mut seen = 0;
    for row in footer::wrap_chars(&full, w) {
        let n = row.chars().count();
        let (a, b): (String, String) = if seen + n <= split {
            (row.clone(), String::new())
        } else if seen >= split {
            (String::new(), row.clone())
        } else {
            let k = split - seen;
            (row.chars().take(k).collect(), row.chars().skip(k).collect())
        };
        let mut v = Vec::new();
        if !a.is_empty() {
            v.push(span(a, t.text_muted, false));
        }
        if !b.is_empty() {
            v.push(span(b, t.text, false));
        }
        out.push(Line::from(v));
        seen += n;
    }
    out.push(Line::default());

    // `• OpenCode 1.18.34` with the product word split in two weights; openw keeps the shape.
    out.push(Line::from(vec![
        span("•", t.success, false),
        Span::raw(" "),
        span("wiz", t.text_muted, true),
        span("ard", t.text, true),
        Span::raw(" "),
        span(app.config.version(), t.text_muted, false),
    ]));
    out
}

/// opencode shows the box until a provider that costs money is connected; here that is no
/// model at all, once the backend has said hello.
fn getting_started(app: &App) -> bool {
    app.backend_ready && app.config.models.is_empty()
}

/// What the Getting started box says. opencode's own wording is kept for the layout test.
struct Copy {
    paras: [&'static str; 2],
    dismiss: bool,
}

const COPY: Copy = Copy {
    paras: [
        "wizard has no provider configured, so there is no model to talk to yet.",
        "Connect one to use Claude, GPT, Gemini etc",
    ],
    dismiss: false,
};

fn gs_box(t: &tuikit::Theme, w: usize, copy: &Copy) -> Vec<Line<'static>> {
    let bg = t.background_element;
    let inner = w.saturating_sub(4);
    let col = inner.saturating_sub(2);
    let pad_row = || Line::from(" ".repeat(w)).style(Style::new().bg(bg));
    let mk = |lead: Span<'static>, body: Vec<Span<'static>>, pad_to: usize| {
        let mut v = vec![Span::raw("  "), lead, Span::raw(" ")];
        let used: usize = 2
            + 1
            + 1
            + body
                .iter()
                .map(|s| display_width(&s.content))
                .sum::<usize>();
        v.extend(body);
        v.push(Span::raw(" ".repeat(pad_to.saturating_sub(used))));
        Line::from(v).style(Style::new().bg(bg))
    };
    let mut rows = vec![pad_row()];
    let mut head = vec![span("Getting started", t.text, true)];
    if copy.dismiss {
        let gap = col.saturating_sub(display_width("Getting started") + 1);
        head.push(Span::raw(" ".repeat(gap)));
        head.push(span("✕", t.text_muted, false));
    }
    rows.push(mk(span("⬖", t.text, false), head, w));
    rows.push(pad_row());
    for (i, p) in copy.paras.iter().enumerate() {
        if i > 0 {
            rows.push(pad_row());
        }
        for l in wrap(p, col.saturating_sub(1).max(1)) {
            rows.push(mk(Span::raw(" "), vec![span(l, t.text_muted, false)], w));
        }
    }
    rows.push(pad_row());
    let left = "Connect provider";
    let right = "/connect";
    let gap = col.saturating_sub(display_width(left) + display_width(right));
    rows.push(mk(
        Span::raw(" "),
        vec![
            span(left, t.text, false),
            Span::raw(" ".repeat(gap)),
            span(right, t.text_muted, false),
        ],
        w,
    ));
    rows.push(pad_row());
    rows
}

// ---- draw ------------------------------------------------------------------------------

pub fn draw(buf: &mut Buffer, app: &mut App, area: Rect, overlay: bool) {
    draw_with(buf, app, area, overlay, &COPY);
}

fn draw_with(buf: &mut Buffer, app: &mut App, area: Rect, overlay: bool, copy: &Copy) {
    if overlay {
        // alpha 70/255 black over everything left of the panel
        let mut dim = app.theme.clone();
        dim.overlay_alpha = 70.0 / 255.0;
        let left = Rect::new(0, 0, area.x, area.height);
        tuikit::dialog::dim(buf, left, &dim);
    }
    let t = app.theme.clone();
    let panel = Style::new().bg(t.background_panel).fg(t.text);
    fill(buf, area, panel);
    app.sb.panel = area;
    app.sb.toggles.clear();

    let inner = pad(area, 2, 1, 2, 1);
    if inner.is_empty() {
        return;
    }
    let iw = inner.width as usize;

    // footer: 1 row of padding on top, then its rows, bottom aligned
    let foot = footer_lines(app, iw, copy);
    let foot_h = (foot.len() as u16 + 1).min(inner.height);
    let foot_top = inner.bottom() - foot_h;
    let skip = foot.len().saturating_sub(foot_h.saturating_sub(1) as usize);
    for (i, l) in foot.iter().skip(skip).enumerate() {
        let y = foot_top + 1 + i as u16;
        if let Some(bg) = l.style.bg {
            fill(
                buf,
                Rect::new(inner.x, y, inner.width, 1),
                Style::new().bg(bg),
            );
        }
        put_line(buf, inner.x, y, l, Rect::new(inner.x, y, inner.width, 1));
    }

    // scrollbox: a one-column scrollbar appears only when the content overflows, and takes
    // that column from the text
    let body = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        foot_top.saturating_sub(inner.y),
    );
    let mut c = content(
        app,
        iw.saturating_sub(1).max(1),
        iw.saturating_sub(2).max(1),
    );
    let overflow = c.lines.len() > body.height as usize;
    if overflow {
        c = content(
            app,
            iw.saturating_sub(2).max(1),
            iw.saturating_sub(3).max(1),
        );
    }
    let vp = body.height as usize;
    app.sb.extent = (c.lines.len(), vp);
    app.sb.offset = app.sb.offset.min(c.lines.len().saturating_sub(vp));
    let off = app.sb.offset;
    let text_area = Rect::new(
        body.x,
        body.y,
        body.width.saturating_sub(u16::from(overflow)),
        body.height,
    );
    for (i, l) in c.lines.iter().enumerate().skip(off).take(vp) {
        let y = body.y + (i - off) as u16;
        put_line(
            buf,
            body.x,
            y,
            l,
            Rect::new(text_area.x, y, text_area.width, 1),
        );
    }
    for (row, sec) in &c.toggles {
        if *row >= off && *row < off + vp {
            app.sb.toggles.push((body.y + (*row - off) as u16, *sec));
        }
    }
    if overflow {
        let mut s = ScrollState::new();
        s.set_extent(c.lines.len(), vp);
        s.scroll_by(-(c.lines.len() as isize));
        s.scroll_by(off as isize);
        let track = Rect::new(body.right() - 1, body.y, 1, body.height);
        ratatui::widgets::Widget::render(
            Scrollbar {
                state: &s,
                track: t.background,
                thumb: t.border_active,
            },
            track,
            buf,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, AppOpts, Route};
    use agent_core::{Event, Todo, TodoStatus, Usage};
    use tuikit::testing::TestTerminal;

    const OPENCODE: Copy = Copy {
        paras: [
            "OpenCode includes free models so you can start immediately.",
            "Connect from 75+ providers to use other models, including Claude, GPT, Gemini etc",
        ],
        dismiss: true,
    };

    fn app(w: u16, h: u16) -> App {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (mtx, _mrx) = tokio::sync::mpsc::unbounded_channel();
        let mut a = App::new(
            AppOpts {
                cwd: "/tmp/claude-1000/-home-nixos/46dc526d-4fc6-4f47-b09e-820c7c4f2577/scratchpad/spec/proj".into(),
                mock: true,
                ..Default::default()
            },
            tx,
            mtx,
        );
        a.size = (w, h);
        a.route = Route::Session;
        a.branch = Some("main".into());
        a.backend_ready = true;
        a
    }

    fn rows(t: &TestTerminal, x: u16) -> Vec<String> {
        t.plain()
            .lines()
            .map(|l| {
                l.chars()
                    .skip(x as usize)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// The state of `reference/extra/session-150x42-sidebar`.
    fn reference_state(a: &mut App) {
        a.titles.insert(
            a.transcript.session_id.clone(),
            "Rename util.py greet and create notes.md".into(),
        );
        a.transcript.usage = Usage {
            context_tokens: 12_256,
            context_window: 200_000,
            ..Default::default()
        };
        a.transcript.apply(&Event::Todos(vec![
            Todo {
                text: "Rename greet to hello in util.py".into(),
                status: TodoStatus::Completed,
            },
            Todo {
                text: "Write notes.md with heading and 2x2 table".into(),
                status: TodoStatus::Completed,
            },
            Todo {
                text: "List project files with ls".into(),
                status: TodoStatus::InProgress,
            },
        ]));
    }

    fn render(a: &mut App, w: u16, h: u16, copy: &Copy) -> TestTerminal {
        let mut t = TestTerminal::new(w, h);
        let area = Rect::new(w - WIDTH, 0, WIDTH, h);
        t.draw(|buf, _| draw_with(buf, a, area, false, copy));
        t
    }

    /// Every text row of the real opencode sidebar except the last line, where the product name
    /// is openw's own.
    fn check_against(capture: &str, w: u16, h: u16) {
        let reference = std::fs::read_to_string(format!(
            "{}/../../reference/extra/{capture}.txt",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("reference capture");
        let x0 = (w - WIDTH) as usize;
        let want: Vec<String> = reference
            .lines()
            .map(|l| {
                l.chars()
                    .skip(x0)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        let mut a = app(w, h);
        reference_state(&mut a);
        a.config.models.clear();
        let t = render(&mut a, w, h, &OPENCODE);
        let got = rows(&t, x0 as u16);
        for y in 0..=13 {
            assert_eq!(got[y], want[y], "row {y}");
        }
        // the footer is anchored at the bottom of the panel
        let foot = h as usize - 20;
        for y in foot..h as usize - 2 {
            assert_eq!(got[y], want[y], "footer row {y}");
        }
        assert_eq!(got[h as usize - 2], "  • openw 0.1.0");
        assert_eq!(want[h as usize - 2], "  • OpenCode 1.18.34");
        if let Some(dir) = std::env::var_os("OPENW_DUMP_DIR") {
            std::fs::write(
                std::path::Path::new(&dir).join(format!("{capture}.ansi")),
                t.ansi(),
            )
            .unwrap();
        }
    }

    #[test]
    #[cfg_attr(
        not(reference_captures),
        ignore = "needs the reference/ captures, not in this repository; see build.rs"
    )]
    fn matches_the_opencode_capture_at_150x42() {
        check_against("session-150x42-sidebar", 150, 42);
    }

    #[test]
    #[cfg_attr(
        not(reference_captures),
        ignore = "needs the reference/ captures, not in this repository; see build.rs"
    )]
    fn matches_the_opencode_overlay_capture_at_120x50() {
        check_against("session-120-sidebar-overlay", 120, 50);
    }

    #[test]
    #[cfg_attr(
        not(reference_captures),
        ignore = "needs the reference/ captures, not in this repository; see build.rs"
    )]
    fn matches_the_opencode_capture_at_170x50() {
        check_against("session-170x50-top", 170, 50);
    }

    #[test]
    fn thousands_and_money_follow_intl() {
        assert_eq!(group_thousands(12_256), "12,256");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1_234_567), "1,234,567");
        assert_eq!(money(0.0), "$0.00");
        assert_eq!(money(0.0412), "$0.04");
        assert_eq!(money(1234.567), "$1,234.57");
    }

    #[test]
    fn paths_are_shown_relative_to_the_project() {
        assert_eq!(
            relative("/home/me/proj/src/a.rs", "/home/me/proj"),
            "src/a.rs"
        );
        assert_eq!(relative("src/a.rs", "/home/me/proj"), "src/a.rs");
        assert_eq!(relative("/etc/hosts", "/home/me/proj"), "/etc/hosts");
    }
}
