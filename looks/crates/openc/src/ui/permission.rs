// OWNER: tools
//! The docked permission panel. It takes the composer's place so the diff or the command stays
//! in view while you decide. Keys are letters shown in the row, so the panel must never
//! take letters meant for the next message: for 300 ms after it appears nothing reaches it, and
//! if you were typing when it appeared (or are still typing) it stays locked until you have
//! paused for [`TYPING_QUIET`]. The lock shows on the question row.
//!
//! One layout per kind of call: a command, a diff, a URL, a path, a plan, an MCP tool. A
//! question from the model (`AskUserQuestion`) is its own panel in [`ask`].

pub mod ask;

use std::cell::Cell;
use std::time::{Duration, Instant};

use agent_core::{DecideScope, PermissionRequest, ToolKind};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use serde_json::Value;
use tuikit::editor::{Editor, EditorStyle};
use tuikit::paint::{fill, put_str};
use tuikit::width::spans_width;

use super::row::{cut, sp, spaces, wrap_plain, Cx, Row};
use super::tools::{diff_rows, DiffData, DiffOpts};
use ask::{AskOut, AskPanel};

pub const GRACE: Duration = Duration::from_millis(300);
/// A printable key this recently before the panel appears means you were mid-message.
pub const TYPING_WINDOW: Duration = Duration::from_millis(1500);
/// How long a locked panel waits after your last key before its letters work.
pub const TYPING_QUIET: Duration = Duration::from_millis(1500);
const BODY_ROWS: usize = 10;
const PLAN_ROWS: usize = 14;

#[derive(Debug, PartialEq, Eq)]
pub enum PermOut {
    None,
    Decide {
        allow: bool,
        scope: DecideScope,
        note: String,
    },
    /// Answers to an `AskUserQuestion`.
    Answer {
        answers: Vec<(String, String)>,
    },
    /// `d`: show the pending edit in the full diff viewer.
    OpenDiff,
    /// `v`: read the whole thing (a plan, a long command) in the pager.
    View(String),
    /// Enter after the grace window: nothing is decided, so say which keys do.
    Nudge,
    /// The panel is locked and ignored the key. The app files a printable one in the draft.
    Locked,
}

pub struct PermPanel {
    pub req: PermissionRequest,
    pub shown: Instant,
    note: Option<Editor>,
    diff: Option<DiffData>,
    ask: Option<AskPanel>,
    cwd: String,
    /// The last body was cut, so `v` has more to show.
    cut: Cell<bool>,
    /// Requests waiting behind this one; set by the app before each draw.
    pub queued: usize,
    /// Keys do nothing before this. Set by the grace window and pushed out by typing.
    lock_until: Instant,
    /// The lock is because of typing, not just the grace window, so the row says so.
    typing: bool,
    /// The tail of the draft typed meanwhile, set by the app before each draw.
    pub draft: String,
}

/// `(question, short always label, tell label, keep d or v, keep e, keep a)`.
type Tier<'a> = (Option<&'a str>, bool, &'a str, bool, bool, bool);

fn s_of<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty())
}

impl PermPanel {
    pub fn new(req: PermissionRequest, now: Instant) -> PermPanel {
        let full = req.tool == "Write";
        let diff = req.diff.as_ref().map(|d| {
            DiffData::build_with(
                d.old.as_deref(),
                &d.new,
                &d.path,
                false,
                DiffOpts { context: 2, full },
            )
        });
        let ask = (req.tool == "AskUserQuestion")
            .then(|| ask::parse(&req.input))
            .flatten()
            .map(AskPanel::new);
        PermPanel {
            req,
            shown: now,
            note: None,
            diff,
            ask,
            cwd: String::new(),
            cut: Cell::new(false),
            queued: 0,
            lock_until: now + GRACE,
            typing: false,
            draft: String::new(),
        }
    }

    /// The user was typing when this appeared (`last` is their latest printable key).
    pub fn lock_for_typing(mut self, last: Option<Instant>, now: Instant) -> PermPanel {
        if let Some(t) = last {
            if now.saturating_duration_since(t) < TYPING_WINDOW {
                self.lock_until = self.lock_until.max(t + TYPING_QUIET);
                self.typing = true;
            }
        }
        self
    }

    pub fn locked(&self, now: Instant) -> bool {
        now < self.lock_until
    }

    /// Locked because of typing, the case the row explains.
    pub fn typing_locked(&self, now: Instant) -> bool {
        self.typing && self.locked(now)
    }

    /// A key arrived at a locked panel and is going to the draft: wait for a pause.
    pub fn extend_lock(&mut self, now: Instant) {
        self.lock_until = self.lock_until.max(now + TYPING_QUIET);
        self.typing = true;
    }

    /// When the lock ends, for the redraw timer.
    pub fn lock_end(&self, now: Instant) -> Option<Instant> {
        (self.lock_until > now).then_some(self.lock_until)
    }

    /// The project directory, so a path outside it can be called out.
    pub fn with_cwd(mut self, cwd: &str) -> PermPanel {
        self.cwd = cwd.to_string();
        self
    }

    fn is_edit(&self) -> bool {
        self.diff.is_some()
    }

    fn is_plan(&self) -> bool {
        self.req.tool == "ExitPlanMode"
    }

    fn input(&self, k: &str) -> Option<&str> {
        s_of(&self.req.input, k)
    }

    fn command(&self) -> Option<String> {
        self.input("command").map(str::to_string)
    }

    /// The `server:tool` of an MCP call.
    fn mcp(&self) -> Option<String> {
        self.req
            .tool
            .strip_prefix("mcp__")
            .map(|r| r.replacen("__", ":", 1))
    }

    /// What the title rule says: `Edit src/render.rs`, `Bash`, `Fetch docs.rs`.
    pub fn title(&self) -> String {
        if let Some(a) = &self.ask {
            return a.title("-");
        }
        if self.is_plan() {
            return "Plan".into();
        }
        let target = if !self.req.title.is_empty() {
            self.req.title.clone()
        } else {
            self.diff
                .as_ref()
                .map(|d| d.path.clone())
                .unwrap_or_default()
        };
        let verb: String = match self.req.kind {
            ToolKind::Execute => "Bash".into(),
            ToolKind::Edit => match &self.diff {
                Some(d) if d.new_file => "Write".into(),
                _ => "Edit".into(),
            },
            ToolKind::Fetch => "Fetch".into(),
            ToolKind::Read => "Read".into(),
            ToolKind::Search => "Search".into(),
            _ if self.mcp().is_some() => "Tool".into(),
            _ if matches!(self.req.tool.as_str(), "Agent" | "Task") => "Task".into(),
            _ => self.req.tool.clone(),
        };
        let target = match self.mcp() {
            Some(m) if target.is_empty() || target == m => m,
            _ => target,
        };
        if self.req.kind == ToolKind::Execute || target.is_empty() {
            verb
        } else {
            format!("{verb} {target}")
        }
    }

    fn question(&self) -> &'static str {
        match self.req.kind {
            ToolKind::Execute => "Run this command?",
            ToolKind::Edit => match &self.diff {
                Some(d) if d.new_file => "Create this file?",
                _ => "Allow this edit?",
            },
            ToolKind::Fetch => "Fetch this page?",
            ToolKind::Read => "Read this?",
            ToolKind::Search => "Search?",
            _ if self.is_plan() => "Start on this plan?",
            _ if matches!(self.req.tool.as_str(), "Agent" | "Task") => "Run this subagent?",
            _ if self.mcp().is_some() => "Call this tool?",
            _ => "Allow this tool call?",
        }
    }

    /// Text the pager can show in full, for the `v` key.
    fn full_text(&self) -> Option<String> {
        if self.is_plan() {
            return self.input("plan").map(str::to_string);
        }
        self.command()
            .or_else(|| self.input("prompt").map(str::to_string))
    }

    fn kv(&self, k: &str, v: &str, w: usize, cx: &Cx, rows: &mut Vec<Row>, max_rows: usize) {
        let p = cx.p;
        let kw = 7usize;
        let lines = wrap_plain(v, w.saturating_sub(kw + 4).max(8));
        let n = lines.len();
        for (i, l) in lines.into_iter().take(max_rows).enumerate() {
            let key = if i == 0 {
                format!("{k:<kw$} ")
            } else {
                " ".repeat(kw + 1)
            };
            let last = i + 1 == max_rows && n > max_rows;
            rows.push(
                Row::new(vec![
                    spaces(2),
                    sp(key, p.s_faint()),
                    sp(
                        if last {
                            format!("{l}{}", cx.g.ellipsis)
                        } else {
                            l
                        },
                        p.s_text(),
                    ),
                ])
                .bg(p.surface),
            );
        }
    }

    pub fn body(&self, cx: &Cx, max: usize) -> Vec<Row> {
        let p = cx.p;
        let w = cx.width;
        let mut rows: Vec<Row> = Vec::new();
        if let Some(d) = &self.diff {
            rows = diff_rows(d, cx, max + 1);
        } else if self.is_plan() {
            // Leaving plan mode: the plan is the thing being approved, read as prose.
            if let Some(plan) = self.input("plan") {
                let mut md = crate::ui::md::render(plan, cx);
                for r in &mut md {
                    r.bg = Some(p.surface);
                }
                rows = md;
            }
        } else if let Some(cmd) = self.command() {
            for l in cmd
                .lines()
                .flat_map(|l| wrap_plain(l, w.saturating_sub(6).max(8)))
            {
                let first = rows.is_empty();
                rows.push(
                    Row::new(vec![
                        spaces(2),
                        sp(if first { "$ " } else { "  " }, p.s_faint()),
                        sp(l, p.s_text()),
                    ])
                    .bg(p.surface),
                );
            }
            if let Some(desc) = self.input("description") {
                rows.push(
                    Row::new(vec![
                        spaces(4),
                        sp(cut(desc, w.saturating_sub(6)), p.s_dim()),
                    ])
                    .bg(p.surface),
                );
            }
            if self
                .req
                .input
                .get("run_in_background")
                .and_then(Value::as_bool)
                == Some(true)
            {
                rows.push(
                    Row::new(vec![spaces(4), sp("runs in the background", p.s_faint())])
                        .bg(p.surface),
                );
            }
        } else if self.req.kind == ToolKind::Fetch || self.req.tool == "WebFetch" {
            if let Some(u) = self.input("url") {
                self.kv("url", u, w, cx, &mut rows, 3);
            }
            if let Some(q) = self.input("prompt") {
                self.kv("ask", q, w, cx, &mut rows, 2);
            }
        } else if matches!(self.req.tool.as_str(), "Agent" | "Task") {
            if let Some(t) = self.input("subagent_type") {
                self.kv("type", t, w, cx, &mut rows, 1);
            }
            if let Some(t) = self.input("description") {
                self.kv("task", t, w, cx, &mut rows, 2);
            }
            if let Some(t) = self.input("prompt") {
                self.kv("prompt", t, w, cx, &mut rows, 4);
            }
        } else if let Some(path) = self
            .input("file_path")
            .or_else(|| self.input("path"))
            .or_else(|| self.input("notebook_path"))
        {
            self.kv("path", path, w, cx, &mut rows, 2);
            if !self.cwd.is_empty() && path.starts_with('/') && !path.starts_with(&self.cwd) {
                rows.push(
                    Row::new(vec![
                        spaces(10),
                        sp(
                            format!("outside {}", cut(&self.cwd, w.saturating_sub(20))),
                            p.s_warn(),
                        ),
                    ])
                    .bg(p.surface),
                );
            }
            for (k, label) in [("pattern", "match"), ("glob", "glob"), ("query", "query")] {
                if let Some(v) = self.input(k) {
                    self.kv(label, v, w, cx, &mut rows, 2);
                }
            }
        } else if let Some(obj) = self.req.input.as_object() {
            let key_w = obj
                .keys()
                .map(|k| k.chars().count())
                .max()
                .unwrap_or(0)
                .min(14);
            for (k, v) in obj.iter().take(8) {
                let val = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let one: String = val.split_whitespace().collect::<Vec<_>>().join(" ");
                rows.push(
                    Row::new(vec![
                        spaces(2),
                        sp(format!("{k:<key_w$}  "), p.s_faint()),
                        sp(cut(&one, w.saturating_sub(key_w + 6)), p.s_text()),
                    ])
                    .bg(p.surface),
                );
            }
        }
        self.cut.set(rows.len() > max);
        if rows.len() > max {
            // The diff was built only as far as it is drawn; its own row count says how much
            // was left out.
            let more = match &self.diff {
                Some(d) => d.rows.len().saturating_sub(max).max(1),
                None => rows.len() - max,
            };
            rows.truncate(max);
            let hint = if self.is_edit() {
                "d full diff"
            } else if self.full_text().is_some() {
                "v view all"
            } else {
                ""
            };
            rows.push(
                Row::new(vec![
                    spaces(4),
                    sp(
                        format!("{} {more} more rows  {hint}", cx.g.dashed),
                        p.s_faint(),
                    ),
                ])
                .bg(p.surface),
            );
        }
        rows
    }

    fn cap(&self, avail: usize) -> usize {
        let base = if self.is_plan() { PLAN_ROWS } else { BODY_ROWS };
        base.min(avail.saturating_sub(7).max(2))
    }

    /// Total panel height for `avail` rows of screen above the status line.
    pub fn height(&self, cx: &Cx, avail: usize) -> u16 {
        if let Some(a) = &self.ask {
            return a.height(cx, avail);
        }
        let body = self.body(cx, self.cap(avail)).len();
        (body + 2) as u16
    }

    /// One hint, `y yes`. `y` leads in `accent`: it is the one-time allow, the least permanent
    /// of the keys that let the call through, so the eye has somewhere to start.
    fn hint(k: &str, label: &str, p: &crate::palette::Palette) -> Vec<Span<'static>> {
        let key = if k == "y" {
            p.s_accent().add_modifier(Modifier::BOLD)
        } else {
            p.s_text().add_modifier(Modifier::BOLD)
        };
        vec![sp(k.to_string(), key), sp(format!(" {label}"), p.s_dim())]
    }

    /// The label of the `a` key: what it would allow for the rest of the session.
    fn always_label(&self, short: bool) -> Option<String> {
        let r = &self.req.rule;
        if r.is_empty() {
            return None;
        }
        Some(if short {
            "always".to_string()
        } else if r.ends_with("this session") {
            format!("yes, {r}")
        } else {
            format!("yes, for {r}")
        })
    }

    /// The question and its keys. A row that does not fit loses whole things, never half of
    /// one (finding 4 saw `e w`): first shorter labels, then the question itself, then `d` and
    /// `v`, then `e`, then `a`. `y` and `n` always stay.
    fn question_spans(&self, cx: &Cx) -> Vec<Span<'static>> {
        let p = cx.p;
        let deny = if self.is_plan() {
            "no, keep planning"
        } else {
            "no"
        };
        let more = if self.is_edit() {
            Some(("d", "full diff"))
        } else if self.cut.get() && self.full_text().is_some() {
            Some(("v", "view all"))
        } else {
            None
        };
        // `(question, always label, tell label, keep the more key, keep tell, keep always)`
        let tell_long = if self.is_plan() {
            "tell claude what to change"
        } else {
            "tell claude why"
        };
        let tiers: [Tier; 8] = [
            (Some(self.question()), false, tell_long, true, true, true),
            (Some("Allow?"), false, "tell why", true, true, true),
            (Some("Allow?"), true, "why", true, true, true),
            (None, true, "why", true, true, true),
            (None, true, "why", false, true, true),
            (None, true, "why", false, false, true),
            (None, true, "why", false, false, false),
            (None, true, "why", false, false, false),
        ];
        let build = |q: Option<&str>,
                     short: bool,
                     tell: &str,
                     keep_more: bool,
                     keep_tell: bool,
                     keep_always: bool| {
            let mut hints: Vec<Vec<Span<'static>>> = vec![Self::hint("y", "yes", p)];
            if keep_always {
                if let Some(r) = self.always_label(short) {
                    hints.push(Self::hint("a", &r, p));
                }
            }
            hints.push(Self::hint("n", deny, p));
            if keep_tell {
                hints.push(Self::hint("e", tell, p));
            }
            if let (true, Some((k, l))) = (keep_more, more) {
                hints.push(Self::hint(k, l, p));
            }
            let mut out = vec![spaces(2)];
            if let Some(q) = q {
                out.push(sp(format!("{q}   "), p.s_text()));
            }
            for (i, h) in hints.into_iter().enumerate() {
                if i > 0 {
                    out.push(spaces(3));
                }
                out.extend(h);
            }
            out
        };
        let w = cx.width;
        let mut last = Vec::new();
        for (q, short, tell, m, t, a) in tiers {
            let row = build(q, short, tell, m, t, a);
            if spans_width(&row) <= w {
                return row;
            }
            last = row;
        }
        last
    }

    /// Paint the panel into `rect` (its height comes from [`height`](Self::height)).
    pub fn draw(&mut self, buf: &mut Buffer, rect: Rect, cx: &Cx) -> Option<(u16, u16)> {
        let p = cx.p;
        if rect.height < 3 {
            return None;
        }
        if let Some(a) = self.ask.as_mut() {
            let c = a.draw(buf, rect, cx);
            if self.typing_locked(cx.now) {
                self.draw_lock(buf, rect, cx);
            }
            return c;
        }
        // Title rule.
        let title = cut(
            &self.title(),
            (rect.width as usize).saturating_sub(6).max(8),
        );
        let lead = format!("{} ", cx.g.rule);
        let mut spans = vec![
            sp(lead, p.s_warn()),
            sp(title, p.s_warn().add_modifier(Modifier::BOLD)),
            sp(" ", p.s_warn()),
        ];
        let used = spans_width(&spans);
        let waiting = if self.queued > 0 {
            format!(" {} more waiting ", self.queued)
        } else {
            String::new()
        };
        let room = (rect.width as usize).saturating_sub(used + waiting.chars().count());
        spans.push(sp(cx.g.rule.repeat(room), p.s_warn()));
        spans.push(sp(waiting, p.s_faint()));
        let mut x = rect.x;
        for s in &spans {
            x = put_str(buf, x, rect.y, &s.content, s.style, rect);
        }
        // Body.
        let body_h = rect.height.saturating_sub(2) as usize;
        let mut rows = self.body(cx, body_h);
        if self.cut.get() {
            // The row that says what was left out has to fit too.
            rows = self.body(cx, body_h.saturating_sub(1));
        }
        for (i, r) in rows.iter().take(body_h).enumerate() {
            let y = rect.y + 1 + i as u16;
            let bg = r.bg.unwrap_or(p.bg);
            fill(
                buf,
                Rect::new(rect.x, y, rect.width, 1),
                Style::new().bg(bg),
            );
            let mut cx_ = rect.x;
            for s in &r.spans {
                cx_ = put_str(
                    buf,
                    cx_,
                    y,
                    &s.content,
                    Style::new().bg(bg).patch(s.style),
                    rect,
                );
            }
        }
        // Question row, or the note editor.
        let y = rect.bottom() - 1;
        if let Some(ed) = self.note.as_mut() {
            let label = if self.req.tool == "ExitPlanMode" {
                "what to change"
            } else {
                "tell claude why"
            };
            // A bar marks it as a field. The label is the placeholder, so it goes away when you
            // type and what you typed is not butted against a grey caption (finding 14).
            // On the composer's column and in its colour: it is the same voice, typing.
            put_str(buf, rect.x, y, cx.g.bar, p.s_user(), rect);
            let area = Rect::new(rect.x + 2, y, rect.width.saturating_sub(3), 1);
            let hint = format!("{}  enter sends  esc goes back", label.trim_end());
            let st = EditorStyle {
                text: Style::new().fg(p.text),
                placeholder: Some((cut(&hint, area.width as usize), p.s_faint())),
            };
            return ed.render(area, buf, &st).cursor;
        }
        if self.typing_locked(cx.now) {
            self.draw_lock(buf, rect, cx);
            return None;
        }
        let mut x = rect.x;
        for s in self.question_spans(cx) {
            x = put_str(buf, x, y, &s.content, s.style, rect);
        }
        None
    }

    /// The question row while you are typing: what is happening, and what you have typed so far
    /// (it is going to the composer, which this panel covers).
    fn draw_lock(&self, buf: &mut Buffer, rect: Rect, cx: &Cx) {
        let p = cx.p;
        let y = rect.bottom() - 1;
        fill(
            buf,
            Rect::new(rect.x, y, rect.width, 1),
            Style::new().bg(p.bg),
        );
        let lead = "  locked while you type; answer keys wake after a pause";
        let mut x = put_str(buf, rect.x, y, lead, p.s_warn(), rect);
        let left = (rect.right().saturating_sub(x)) as usize;
        if left > 12 && !self.draft.is_empty() {
            let tail: String = {
                let flat = self.draft.replace('\n', " ");
                let n = flat.chars().count();
                let keep = left.saturating_sub(5);
                flat.chars().skip(n.saturating_sub(keep)).collect()
            };
            x = put_str(buf, x, y, "  > ", p.s_faint(), rect);
            put_str(buf, x, y, &tail, p.s_text(), rect);
        }
    }

    pub fn on_key(&mut self, key: KeyEvent, now: Instant) -> PermOut {
        if self.locked(now) {
            return PermOut::Locked;
        }
        if let Some(a) = self.ask.as_mut() {
            return match a.on_key(key) {
                AskOut::None => PermOut::None,
                AskOut::Answer(answers) => PermOut::Answer { answers },
                AskOut::Skip => PermOut::Decide {
                    allow: false,
                    scope: DecideScope::Once,
                    note: "The user chose not to answer the questions.".into(),
                },
            };
        }
        if let Some(ed) = self.note.as_mut() {
            match key.code {
                KeyCode::Esc => self.note = None,
                KeyCode::Enter => {
                    let note = ed.text().trim().to_string();
                    return PermOut::Decide {
                        allow: false,
                        scope: DecideScope::Once,
                        note,
                    };
                }
                _ => {
                    ed.apply_key(key);
                }
            }
            return PermOut::None;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return PermOut::None;
        }
        match key.code {
            KeyCode::Char('y' | 'Y') => PermOut::Decide {
                allow: true,
                scope: DecideScope::Once,
                note: String::new(),
            },
            KeyCode::Char('a' | 'A') if !self.req.rule.is_empty() => PermOut::Decide {
                allow: true,
                scope: DecideScope::Always,
                note: String::new(),
            },
            KeyCode::Char('n' | 'N') | KeyCode::Esc => PermOut::Decide {
                allow: false,
                scope: DecideScope::Once,
                note: String::new(),
            },
            KeyCode::Char('e' | 'E') => {
                self.note = Some(Editor::new());
                PermOut::None
            }
            // Enter is not a choice here (a typed-ahead one must never approve), but a key that
            // does nothing and says nothing looked like a hang (finding 14).
            KeyCode::Enter => PermOut::Nudge,
            KeyCode::Char('d' | 'D') if self.is_edit() => PermOut::OpenDiff,
            KeyCode::Char('v' | 'V') => match self.full_text() {
                Some(t) => PermOut::View(t),
                None => PermOut::None,
            },
            _ => PermOut::None,
        }
    }

    pub fn paste(&mut self, s: &str) {
        if let Some(a) = self.ask.as_mut() {
            a.paste(s);
        } else if let Some(ed) = self.note.as_mut() {
            ed.paste(s);
        }
    }

    /// The diff of a pending edit, for the viewer.
    pub fn diff(&self) -> Option<&DiffData> {
        self.diff.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::{Depth, Kind, Palette, UNICODE};
    use agent_core::FileDiff;
    use tuikit::testing::TestTerminal;
    use tuikit::width::display_width;

    fn edit_req() -> PermissionRequest {
        PermissionRequest {
            id: "p1".into(),
            tool: "Edit".into(),
            kind: ToolKind::Edit,
            title: "src/render.rs".into(),
            input: serde_json::json!({}),
            diff: Some(FileDiff {
                path: "/nonexistent/src/render.rs".into(),
                old: Some("a\nb\n".into()),
                new: "a\nc\n".into(),
            }),
            rule: "src/**".into(),
        }
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn input_in_the_first_300ms_is_ignored_and_enter_never_approves() {
        let t0 = Instant::now();
        let mut p = PermPanel::new(edit_req(), t0);
        assert_eq!(
            p.on_key(key('y'), t0 + Duration::from_millis(50)),
            PermOut::Locked
        );
        let later = t0 + Duration::from_millis(400);
        assert_eq!(
            p.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), later),
            PermOut::Nudge
        );
        assert!(matches!(
            p.on_key(key('y'), later),
            PermOut::Decide {
                allow: true,
                scope: DecideScope::Once,
                ..
            }
        ));
        assert!(matches!(
            p.on_key(key('a'), later),
            PermOut::Decide {
                allow: true,
                scope: DecideScope::Always,
                ..
            }
        ));
        assert!(matches!(
            p.on_key(key('n'), later),
            PermOut::Decide { allow: false, .. }
        ));
    }

    #[test]
    fn note_mode_denies_with_the_text() {
        let t0 = Instant::now();
        let later = t0 + Duration::from_secs(1);
        let mut p = PermPanel::new(edit_req(), t0);
        assert_eq!(p.on_key(key('e'), later), PermOut::None);
        for c in "use b".chars() {
            p.on_key(key(c), later);
        }
        let out = p.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), later);
        assert_eq!(
            out,
            PermOut::Decide {
                allow: false,
                scope: DecideScope::Once,
                note: "use b".into()
            }
        );
    }

    #[test]
    fn panel_draws_rule_diff_and_keys() {
        let pal = Palette::new(Kind::Hearth, Depth::True);
        let th = pal.theme();
        let cx = Cx {
            p: &pal,
            theme: &th,
            g: &UNICODE,
            width: 90,
            detail: false,
            now: Instant::now(),
            spin: 0,
        };
        let mut panel = PermPanel::new(edit_req(), Instant::now());
        let h = panel.height(&cx, 30);
        let mut t = TestTerminal::new(90, h);
        t.draw(|b, a| {
            panel.draw(b, a, &cx);
        });
        let s = t.plain();
        assert!(
            s.lines()
                .next()
                .unwrap()
                .starts_with("─ Edit src/render.rs ─"),
            "{s}"
        );
        assert!(
            s.contains("Allow this edit?")
                && s.contains("y yes")
                && s.contains("a yes, for src/**")
                && s.contains("d full diff"),
            "{s}"
        );
        assert!(
            s.contains("- b") || s.contains("-  b") || s.contains("b"),
            "{s}"
        );
    }

    fn cx_of<'a>(p: &'a Palette, th: &'a tuikit::theme::Theme, width: usize) -> Cx<'a> {
        Cx {
            p,
            theme: th,
            g: &UNICODE,
            width,
            detail: false,
            now: Instant::now(),
            spin: 0,
        }
    }

    #[test]
    fn the_key_row_drops_whole_hints_and_never_cuts_one() {
        // Finding 4: at 44 columns `e tell why` was cut to `e w` and `d full diff` was gone.
        let pal = Palette::new(Kind::Hearth, Depth::True);
        let th = pal.theme();
        let panel = PermPanel::new(edit_req(), Instant::now());
        let mut seen = Vec::new();
        for width in [30, 36, 40, 44, 52, 60, 76, 90, 120] {
            let cx = cx_of(&pal, &th, width);
            let row: String = panel
                .question_spans(&cx)
                .iter()
                .map(|s| s.content.as_ref())
                .collect();
            assert!(display_width(&row) <= width, "{width}: {row:?}");
            // Whole words only: every key letter is followed by its full label.
            for (key, labels) in [
                ("y", &["yes"][..]),
                ("a", &["always", "yes, for src/**"]),
                ("n", &["no"]),
                ("e", &["why", "tell why", "tell claude why"]),
                ("d", &["full diff"]),
            ] {
                for part in row.split("   ").map(str::trim) {
                    if let Some(rest) = part.strip_prefix(&format!("{key} ")) {
                        assert!(
                            labels.contains(&rest),
                            "{width}: cut hint `{part}` in {row:?}"
                        );
                    }
                }
            }
            assert!(
                row.contains("y yes") && row.contains("n no"),
                "{width}: {row:?}"
            );
            seen.push((width, row));
        }
        // 44 columns keeps `e` whole, and drops `d` before `e`.
        let (_, r44) = seen.iter().find(|(w, _)| *w == 44).unwrap();
        assert!(
            r44.contains("e why") || r44.contains("e tell why"),
            "{r44:?}"
        );
        // At 120 nothing is dropped.
        let (_, r120) = seen.iter().find(|(w, _)| *w == 120).unwrap();
        assert!(
            r120.contains("Allow this edit?") && r120.contains("d full diff"),
            "{r120:?}"
        );
    }

    #[test]
    fn enter_after_the_grace_window_says_which_keys_decide() {
        // Finding 14: Enter did nothing and said nothing. It still never decides.
        let t0 = Instant::now();
        let mut p = PermPanel::new(edit_req(), t0);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            p.on_key(enter, t0 + Duration::from_millis(100)),
            PermOut::Locked
        );
        assert_eq!(
            p.on_key(enter, t0 + Duration::from_millis(400)),
            PermOut::Nudge
        );
    }

    #[test]
    fn the_note_row_is_a_field_with_a_bar_and_a_placeholder_not_a_caption() {
        let pal = Palette::new(Kind::Hearth, Depth::True);
        let th = pal.theme();
        let cx = cx_of(&pal, &th, 80);
        let t0 = Instant::now();
        let mut panel = PermPanel::new(edit_req(), t0);
        let later = t0 + Duration::from_secs(1);
        panel.on_key(key('e'), later);
        let h = panel.height(&cx, 30);
        let draw = |panel: &mut PermPanel| {
            let mut t = TestTerminal::new(80, h);
            t.draw(|b, a| {
                panel.draw(b, a, &cx);
            });
            t.plain().lines().last().unwrap().to_string()
        };
        let empty = draw(&mut panel);
        assert!(
            empty.contains("▎ tell claude why") && empty.contains("enter sends"),
            "{empty:?}"
        );
        for c in "run it later".chars() {
            panel.on_key(key(c), later);
        }
        let typed = draw(&mut panel);
        assert!(typed.contains("▎ run it later"), "{typed:?}");
        assert!(
            !typed.contains("tell claude why"),
            "the label is gone once you type: {typed:?}"
        );
    }
}
