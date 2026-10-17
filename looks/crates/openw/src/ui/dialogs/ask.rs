// OWNER: dialogs
//! The permission and question prompts that replace the prompt box while the backend waits for
//! an answer (`routes/session/permission.tsx`, `question.tsx`).
//!
//! `wizard acp` never sends either: wizard's `genie` mode approves tool calls without asking and
//! it declines the `interview` tool over ACP. [`Event::Permission`] exists for backends that do
//! (the scripted mock backend's `perm` scenario, and `openc`); the question prompt has no event
//! at all in `agent-core`, so it is built from [`QuestionInfo`] by whoever has questions to ask
//! and answers through [`AskOutcome::Answer`].

use agent_core::{DecideScope, FileDiff, PermissionRequest, Request, ToolKind};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use tuikit::diff::{self, DiffOptions, DiffView};
use tuikit::editor::{Editor, EditorStyle};
use tuikit::paint::{fill, put_line, put_str};
use tuikit::width::display_width;
use tuikit::Theme;

/// What a key did to a prompt.
#[derive(Debug, PartialEq)]
pub enum AskOutcome {
    Stay,
    /// Answer the backend and drop the prompt.
    Send(Request),
    /// A question was answered: one list of picked labels per question.
    Answer {
        id: String,
        answers: Vec<Vec<String>>,
    },
    /// A question was dismissed.
    Reject {
        id: String,
    },
}

pub enum Ask {
    Permission(Box<PermissionPrompt>),
    Question(Box<QuestionPrompt>),
}

impl Ask {
    pub fn height(&self, screen: (u16, u16), theme: &Theme) -> u16 {
        match self {
            Ask::Permission(p) => p.height(screen, theme),
            Ask::Question(q) => q.height(),
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> AskOutcome {
        match self {
            Ask::Permission(p) => p.handle_key(key),
            Ask::Question(q) => q.handle_key(key),
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        match self {
            Ask::Permission(p) => p.handle_paste(text),
            Ask::Question(q) => q.handle_paste(text),
        }
    }

    /// Paint into `area`, the bottom block (`x` = 2, to the right edge). A fullscreen permission
    /// paints over the page instead and ignores `area`.
    pub fn draw(
        &mut self,
        buf: &mut Buffer,
        area: Rect,
        screen: Rect,
        theme: &Theme,
    ) -> Option<(u16, u16)> {
        match self {
            Ask::Permission(p) => p.draw(buf, area, screen, theme),
            Ask::Question(q) => q.draw(buf, area, theme),
        }
    }
}

const SIGIL_WARN: &str = "△";

/// `base` moved `alpha` of the way to `over`, per channel.
fn tint(base: Color, over: Color, alpha: f32) -> Color {
    match (base, over) {
        (Color::Rgb(a, b, c), Color::Rgb(d, e, f)) => {
            let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * alpha).round() as u8;
            Color::Rgb(m(a, d), m(b, e), m(c, f))
        }
        _ => over,
    }
}

// ---- permission --------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Permission,
    Always,
    Reject,
}

pub struct PermissionPrompt {
    pub req: PermissionRequest,
    stage: Stage,
    sel: usize,
    pub expanded: bool,
    reject: Editor,
    /// Rejecting in a subagent session asks for a reason first.
    child: bool,
}

/// Icon, title and body of a request, as `PermissionPrompt` words them.
struct Info {
    icon: &'static str,
    title: String,
    body: Body,
}

enum Body {
    Text(Vec<(String, bool)>), // (text, muted)
    Diff(Option<FileDiff>),
}

fn str_of(v: &serde_json::Value, keys: &[&str]) -> String {
    for k in keys {
        if let Some(s) = v.get(*k).and_then(|x| x.as_str()) {
            return s.to_string();
        }
    }
    String::new()
}

fn info_of(req: &PermissionRequest) -> Info {
    let tool = req.tool.to_lowercase();
    let input = &req.input;
    let path = || {
        let p = str_of(input, &["path", "file_path", "filePath"]);
        if p.is_empty() {
            req.title.clone()
        } else {
            p
        }
    };
    let text = |s: String, muted: bool| {
        Body::Text(if s.is_empty() {
            vec![]
        } else {
            vec![(s, muted)]
        })
    };
    match req.kind {
        ToolKind::Edit => Info {
            icon: "→",
            title: format!(
                "Edit {}",
                req.diff.as_ref().map_or_else(path, |d| d.path.clone())
            ),
            body: Body::Diff(req.diff.clone()),
        },
        ToolKind::Execute => {
            let cmd = {
                let c = str_of(input, &["command", "cmd"]);
                if c.is_empty() {
                    req.title.clone()
                } else {
                    c
                }
            };
            Info {
                icon: "#",
                title: "Shell command".into(),
                body: text(
                    if cmd.is_empty() {
                        cmd
                    } else {
                        format!("$ {cmd}")
                    },
                    false,
                ),
            }
        }
        ToolKind::Read => {
            let p = path();
            Info {
                icon: "→",
                title: format!("Read {p}"),
                body: text(
                    if p.is_empty() {
                        p
                    } else {
                        format!("Path: {p}")
                    },
                    true,
                ),
            }
        }
        ToolKind::Search => {
            let pat = {
                let p = str_of(input, &["pattern", "query"]);
                if p.is_empty() {
                    req.title.clone()
                } else {
                    p
                }
            };
            let name = if tool.contains("glob") {
                "Glob"
            } else {
                "Grep"
            };
            Info {
                icon: "✱",
                title: format!("{name} \"{pat}\""),
                body: text(
                    if pat.is_empty() {
                        pat
                    } else {
                        format!("Pattern: {pat}")
                    },
                    true,
                ),
            }
        }
        ToolKind::Fetch => {
            let url = {
                let u = str_of(input, &["url"]);
                if u.is_empty() {
                    req.title.clone()
                } else {
                    u
                }
            };
            if tool.contains("search") {
                let q = str_of(input, &["query"]);
                let q = if q.is_empty() { url } else { q };
                Info {
                    icon: "◈",
                    title: format!("Web Search \"{q}\""),
                    body: text(format!("Query: {q}"), true),
                }
            } else {
                Info {
                    icon: "%",
                    title: format!("WebFetch {url}"),
                    body: text(format!("URL: {url}"), true),
                }
            }
        }
        _ => Info {
            icon: "⚙",
            title: format!("Call tool {}", req.tool),
            body: Body::Text(vec![(format!("Tool: {}", req.tool), true)]),
        },
    }
}

const MAX_H: u16 = 15;
const STRIP_H: u16 = 3;

impl PermissionPrompt {
    pub fn new(req: PermissionRequest) -> Self {
        PermissionPrompt {
            req,
            stage: Stage::Permission,
            sel: 0,
            expanded: false,
            reject: Editor::new(),
            child: false,
        }
    }

    /// Ask for a reason before rejecting, as in a subagent session.
    pub fn in_child_session(mut self, child: bool) -> Self {
        self.child = child;
        self
    }

    fn buttons(&self) -> Vec<&'static str> {
        match self.stage {
            Stage::Permission if self.req.rule.is_empty() => vec!["Allow once", "Reject"],
            Stage::Permission => vec!["Allow once", "Allow always", "Reject"],
            Stage::Always => vec!["Confirm", "Cancel"],
            Stage::Reject => vec![],
        }
    }

    fn patterns(&self) -> Vec<String> {
        self.req
            .rule
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect()
    }

    /// Rows the block wants: padding, header, gap, body, padding and the button strip.
    pub fn height(&self, screen: (u16, u16), _theme: &Theme) -> u16 {
        if self.expanded {
            return 0;
        }
        let h = match self.stage {
            Stage::Permission => {
                let info = info_of(&self.req);
                let header = 2;
                let body = match &info.body {
                    Body::Text(t) => t.len() as u16,
                    Body::Diff(Some(_)) => return MAX_H.min(screen.1.saturating_sub(3)),
                    Body::Diff(None) => 1,
                };
                1 + header + u16::from(body > 0) + body + 1 + STRIP_H
            }
            Stage::Always => {
                let p = self.patterns();
                let body = if p.len() <= 1 && self.patterns().first().is_none_or(|x| x == "*") {
                    1
                } else {
                    1 + 1 + p.len() as u16
                };
                1 + 1 + 1 + body + 1 + STRIP_H
            }
            Stage::Reject => 1 + 1 + 1 + 1 + 1 + STRIP_H,
        };
        h.min(MAX_H).min(screen.1.saturating_sub(3))
    }

    pub fn handle_paste(&mut self, text: &str) {
        if self.stage == Stage::Reject {
            self.reject.paste(text);
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> AskOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let exit = key.code == KeyCode::Esc
            || (ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d')));
        let id = self.req.id.clone();
        if self.stage == Stage::Reject {
            return match key.code {
                _ if exit => {
                    self.stage = Stage::Permission;
                    AskOutcome::Stay
                }
                KeyCode::Enter => AskOutcome::Send(Request::Decide {
                    id,
                    allow: false,
                    scope: DecideScope::Once,
                    note: self.reject.text().trim().to_string(),
                }),
                _ => {
                    self.reject.apply_key(key);
                    AskOutcome::Stay
                }
            };
        }
        let n = self.buttons().len();
        match key.code {
            KeyCode::Left | KeyCode::Char('h') if !ctrl => {
                self.sel = (self.sel + n - 1) % n;
                AskOutcome::Stay
            }
            KeyCode::Right | KeyCode::Char('l') if !ctrl => {
                self.sel = (self.sel + 1) % n;
                AskOutcome::Stay
            }
            KeyCode::Char('f') if ctrl && self.stage == Stage::Permission => {
                self.expanded = !self.expanded;
                AskOutcome::Stay
            }
            _ if exit => match self.stage {
                // The escape key is the last option: reject, or cancel.
                Stage::Permission => self.choose(n - 1),
                _ => self.choose(n - 1),
            },
            KeyCode::Enter => self.choose(self.sel),
            _ => AskOutcome::Stay,
        }
    }

    fn choose(&mut self, i: usize) -> AskOutcome {
        let id = self.req.id.clone();
        let label = self.buttons().get(i).copied().unwrap_or("");
        match (self.stage, label) {
            (Stage::Permission, "Allow once") => AskOutcome::Send(Request::Decide {
                id,
                allow: true,
                scope: DecideScope::Once,
                note: String::new(),
            }),
            (Stage::Permission, "Allow always") => {
                self.stage = Stage::Always;
                self.sel = 0;
                AskOutcome::Stay
            }
            (Stage::Permission, _) => {
                if self.child {
                    self.stage = Stage::Reject;
                    AskOutcome::Stay
                } else {
                    AskOutcome::Send(Request::Decide {
                        id,
                        allow: false,
                        scope: DecideScope::Once,
                        note: String::new(),
                    })
                }
            }
            (Stage::Always, "Confirm") => AskOutcome::Send(Request::Decide {
                id,
                allow: true,
                scope: DecideScope::Always,
                note: String::new(),
            }),
            (Stage::Always, _) => {
                self.stage = Stage::Permission;
                self.sel = 0;
                AskOutcome::Stay
            }
            _ => AskOutcome::Stay,
        }
    }

    pub fn draw(
        &mut self,
        buf: &mut Buffer,
        area: Rect,
        screen: Rect,
        theme: &Theme,
    ) -> Option<(u16, u16)> {
        let a = if self.expanded {
            Rect::new(
                2,
                1,
                screen.width.saturating_sub(2),
                screen.height.saturating_sub(2),
            )
        } else {
            area
        };
        if a.is_empty() || a.height <= STRIP_H {
            return None;
        }
        let bar = if self.stage == Stage::Reject {
            theme.error
        } else {
            theme.warning
        };
        let panel = theme.background_panel;
        let strip_y = a.bottom() - STRIP_H;
        fill(
            buf,
            Rect::new(a.x, a.y, a.width, a.height - STRIP_H),
            Style::new().bg(panel),
        );
        fill(
            buf,
            Rect::new(a.x, strip_y, a.width, STRIP_H),
            Style::new().bg(theme.background_element),
        );
        for y in a.y..a.bottom() {
            put_str(buf, a.x, y, "┃", Style::new().fg(bar).bg(panel), a);
        }
        let x = a.x + 3; // border, left padding, header padding
        let text = |fg: Color| Style::new().fg(fg).bg(panel);
        let mut y = a.y + 1;
        let content = Rect::new(a.x + 1, a.y, a.width.saturating_sub(4), strip_y - a.y);
        let mut cursor = None;
        match self.stage {
            Stage::Permission => {
                let info = info_of(&self.req);
                put_str(buf, x, y, SIGIL_WARN, text(theme.warning), content);
                put_str(
                    buf,
                    x + 2,
                    y,
                    "Permission required",
                    text(theme.text),
                    content,
                );
                y += 1;
                put_str(buf, x + 2, y, info.icon, text(theme.text_muted), content);
                put_str(buf, x + 4, y, &info.title, text(theme.text), content);
                y += 2;
                match &info.body {
                    Body::Text(lines) => {
                        for (t, muted) in lines {
                            let fg = if *muted { theme.text_muted } else { theme.text };
                            put_str(buf, x, y, t, text(fg), content);
                            y += 1;
                        }
                    }
                    Body::Diff(None) => {
                        put_str(
                            buf,
                            x,
                            y,
                            "No diff provided",
                            text(theme.text_muted),
                            content,
                        );
                    }
                    Body::Diff(Some(d)) => {
                        let w = a.width.saturating_sub(3);
                        let opts = DiffOptions {
                            view: DiffView::auto(screen.width),
                            context: 4,
                            word_highlight: false,
                            lang: d.path.rsplit_once('.').map(|(_, e)| e.to_string()),
                            ..Default::default()
                        };
                        let lines: Vec<Line<'static>> = diff::from_texts(
                            d.old.as_deref().unwrap_or(""),
                            &d.new,
                            w,
                            theme,
                            &opts,
                        );
                        let room = strip_y.saturating_sub(y + 1) as usize;
                        let area = Rect::new(a.x + 2, y, w, room as u16);
                        for (i, l) in lines.iter().take(room).enumerate() {
                            if let Some(bg) = l.style.bg {
                                fill(
                                    buf,
                                    Rect::new(area.x, y + i as u16, area.width, 1),
                                    Style::new().bg(bg),
                                );
                            }
                            put_line(buf, area.x, y + i as u16, l, area);
                        }
                    }
                }
            }
            Stage::Always => {
                put_str(buf, x, y, SIGIL_WARN, text(theme.warning), content);
                put_str(buf, x + 2, y, "Always allow", text(theme.text), content);
                y += 2;
                let p = self.patterns();
                if p.is_empty() || (p.len() == 1 && p[0] == "*") {
                    let t = format!(
                        "This will allow {} for the rest of the session",
                        self.req.tool
                    );
                    put_str(buf, x, y, &t, text(theme.text_muted), content);
                } else {
                    put_str(
                        buf,
                        x,
                        y,
                        "This will allow the following patterns for the rest of the session",
                        text(theme.text_muted),
                        content,
                    );
                    y += 2;
                    for pat in &p {
                        put_str(buf, x, y, &format!("- {pat}"), text(theme.text), content);
                        y += 1;
                    }
                }
            }
            Stage::Reject => {
                put_str(buf, x, y, SIGIL_WARN, text(theme.error), content);
                put_str(
                    buf,
                    x + 2,
                    y,
                    "Reject permission",
                    text(theme.text),
                    content,
                );
                y += 2;
                put_str(
                    buf,
                    x,
                    y,
                    "Tell wizard what to do differently",
                    text(theme.text_muted),
                    content,
                );
                // the reason is typed in the strip, on `backgroundElement`
                let el = theme.background_element;
                let area = Rect::new(a.x + 3, strip_y + 1, a.width.saturating_sub(3 + 24), 1);
                let style = EditorStyle {
                    text: Style::new().fg(theme.text).bg(el),
                    placeholder: None,
                };
                cursor = self.reject.render(area, buf, &style).cursor;
            }
        }
        // button strip
        let el = theme.background_element;
        let by = strip_y + 1;
        let strip = Rect::new(a.x + 1, by, a.width.saturating_sub(1), 1);
        let mut bx = a.x + 3;
        if self.stage != Stage::Reject {
            for (i, b) in self.buttons().into_iter().enumerate() {
                let on = i == self.sel;
                let w = display_width(b) as u16 + 2;
                let (fg, bg) = if on {
                    (
                        theme.selected_foreground(Some(theme.warning)),
                        theme.warning,
                    )
                } else {
                    (theme.text_muted, theme.background_menu)
                };
                fill(buf, Rect::new(bx, by, w, 1), Style::new().bg(bg));
                put_str(buf, bx + 1, by, b, Style::new().fg(fg).bg(bg), strip);
                bx += w + 1;
            }
        }
        // hints on the right, ending 3 columns from the edge
        let hints: Vec<(&str, &str)> = match self.stage {
            Stage::Permission => {
                let mut v = Vec::new();
                v.push((
                    "ctrl+f",
                    if self.expanded {
                        "minimize"
                    } else {
                        "fullscreen"
                    },
                ));
                v.push(("⇆", "select"));
                v.push(("enter", "confirm"));
                v
            }
            Stage::Always => vec![("⇆", "select"), ("enter", "confirm")],
            Stage::Reject => vec![("enter", "confirm"), ("esc", "cancel")],
        };
        let total: usize = hints
            .iter()
            .map(|(k, v)| display_width(k) + 1 + display_width(v))
            .sum::<usize>()
            + 2 * hints.len().saturating_sub(1);
        let mut hx = (a.right() as usize).saturating_sub(3 + total) as u16;
        for (k, v) in hints {
            hx = put_str(buf, hx, by, k, Style::new().fg(theme.text).bg(el), strip);
            hx = put_str(
                buf,
                hx,
                by,
                &format!(" {v}"),
                Style::new().fg(theme.text_muted).bg(el),
                strip,
            );
            hx += 2;
        }
        cursor
    }
}

// ---- question ----------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct QuestionInfo {
    pub header: String,
    pub question: String,
    pub options: Vec<QuestionOption>,
    pub multiple: bool,
    /// Offer `Type your own answer`; on unless the backend says otherwise.
    pub custom: bool,
}

pub struct QuestionPrompt {
    pub id: String,
    qs: Vec<QuestionInfo>,
    tab: usize,
    answers: Vec<Vec<String>>,
    custom: Vec<String>,
    selected: usize,
    editing: bool,
    editor: Editor,
}

impl QuestionPrompt {
    pub fn new(id: impl Into<String>, qs: Vec<QuestionInfo>) -> Self {
        let n = qs.len();
        QuestionPrompt {
            id: id.into(),
            qs,
            tab: 0,
            answers: vec![Vec::new(); n],
            custom: vec![String::new(); n],
            selected: 0,
            editing: false,
            editor: Editor::new(),
        }
    }

    fn single(&self) -> bool {
        self.qs.len() == 1 && !self.qs[0].multiple
    }

    fn tabs(&self) -> usize {
        if self.single() {
            1
        } else {
            self.qs.len() + 1
        }
    }

    fn confirm(&self) -> bool {
        !self.single() && self.tab == self.qs.len()
    }

    fn q(&self) -> &QuestionInfo {
        &self.qs[self.tab.min(self.qs.len() - 1)]
    }

    fn total(&self) -> usize {
        self.q().options.len() + usize::from(self.q().custom)
    }

    fn other(&self) -> bool {
        self.q().custom && self.selected == self.q().options.len()
    }

    fn custom_picked(&self) -> bool {
        let v = &self.custom[self.tab];
        !v.is_empty() && self.answers[self.tab].contains(v)
    }

    fn done(&self) -> AskOutcome {
        AskOutcome::Answer {
            id: self.id.clone(),
            answers: self.answers.clone(),
        }
    }

    fn pick(&mut self, answer: String, custom: bool) -> AskOutcome {
        self.answers[self.tab] = vec![answer.clone()];
        if custom {
            self.custom[self.tab] = answer;
        }
        if self.single() {
            return self.done();
        }
        self.tab += 1;
        self.selected = 0;
        AskOutcome::Stay
    }

    fn toggle(&mut self, answer: &str) {
        let a = &mut self.answers[self.tab];
        match a.iter().position(|x| x == answer) {
            Some(i) => {
                a.remove(i);
            }
            None => a.push(answer.to_string()),
        }
    }

    fn select_option(&mut self) -> AskOutcome {
        if self.other() {
            if !self.q().multiple || !self.custom_picked() {
                self.editing = true;
                self.editor.set_text(&self.custom[self.tab].clone());
                self.editor.move_doc_end();
                return AskOutcome::Stay;
            }
            let v = self.custom[self.tab].clone();
            self.toggle(&v);
            return AskOutcome::Stay;
        }
        let Some(opt) = self.q().options.get(self.selected).cloned() else {
            return AskOutcome::Stay;
        };
        if self.q().multiple {
            self.toggle(&opt.label);
            AskOutcome::Stay
        } else {
            self.pick(opt.label, false)
        }
    }

    fn select_tab(&mut self, i: usize) {
        self.tab = i;
        self.selected = 0;
    }

    pub fn handle_paste(&mut self, text: &str) {
        if self.editing {
            self.editor.paste(text);
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> AskOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let exit = key.code == KeyCode::Esc
            || (ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d')));
        if self.editing && !self.confirm() {
            return self.edit_key(key);
        }
        let n = self.tabs();
        match key.code {
            _ if exit => AskOutcome::Reject {
                id: self.id.clone(),
            },
            KeyCode::Left | KeyCode::Char('h') if !ctrl => {
                self.select_tab((self.tab + n - 1) % n);
                AskOutcome::Stay
            }
            KeyCode::Right | KeyCode::Char('l') if !ctrl => {
                self.select_tab((self.tab + 1) % n);
                AskOutcome::Stay
            }
            KeyCode::Tab => {
                self.select_tab((self.tab + 1) % n);
                AskOutcome::Stay
            }
            KeyCode::BackTab => {
                self.select_tab((self.tab + n - 1) % n);
                AskOutcome::Stay
            }
            KeyCode::Enter if self.confirm() => self.done(),
            _ if self.confirm() => AskOutcome::Stay,
            KeyCode::Char(c @ '1'..='9') if !ctrl => {
                let i = c as usize - '1' as usize;
                if i < self.total().min(9) {
                    self.selected = i;
                    return self.select_option();
                }
                AskOutcome::Stay
            }
            KeyCode::Up | KeyCode::Char('k') if !ctrl => {
                let t = self.total().max(1);
                self.selected = (self.selected + t - 1) % t;
                AskOutcome::Stay
            }
            KeyCode::Down | KeyCode::Char('j') if !ctrl => {
                let t = self.total().max(1);
                self.selected = (self.selected + 1) % t;
                AskOutcome::Stay
            }
            KeyCode::Enter => self.select_option(),
            _ => AskOutcome::Stay,
        }
    }

    fn edit_key(&mut self, key: KeyEvent) -> AskOutcome {
        match key.code {
            KeyCode::Esc => {
                self.editing = false;
                AskOutcome::Stay
            }
            KeyCode::Enter => {
                let text = self.editor.text().trim().to_string();
                let prev = self.custom[self.tab].clone();
                self.editing = false;
                if text.is_empty() {
                    if !prev.is_empty() {
                        self.custom[self.tab].clear();
                        self.answers[self.tab].retain(|x| *x != prev);
                    }
                    return AskOutcome::Stay;
                }
                if self.q().multiple {
                    self.custom[self.tab] = text.clone();
                    let a = &mut self.answers[self.tab];
                    a.retain(|x| *x != prev);
                    if !a.contains(&text) {
                        a.push(text);
                    }
                    return AskOutcome::Stay;
                }
                self.pick(text, true)
            }
            _ => {
                self.editor.apply_key(key);
                AskOutcome::Stay
            }
        }
    }

    pub fn height(&self) -> u16 {
        let mut rows = 1; // top padding
        if !self.single() {
            rows += 1 + 1; // tab strip and its gap
        }
        if self.confirm() {
            rows += 1 + self.qs.len() as u16; // Review + one row per question
        } else {
            rows += 1 + 1; // question and the gap under it
            for o in &self.q().options {
                rows += 1 + u16::from(!o.description.is_empty());
            }
            if self.q().custom {
                rows += 1;
                if self.editing {
                    rows += self.editor.line_count().clamp(1, 6) as u16;
                } else if !self.custom[self.tab].is_empty() {
                    rows += 1;
                }
            }
        }
        rows + 1 + 1 + 1 // bottom padding, footer, footer padding
    }

    pub fn draw(&mut self, buf: &mut Buffer, area: Rect, theme: &Theme) -> Option<(u16, u16)> {
        if area.is_empty() {
            return None;
        }
        let panel = theme.background_panel;
        fill(buf, area, Style::new().bg(panel));
        for y in area.y..area.bottom() {
            put_str(
                buf,
                area.x,
                y,
                "┃",
                Style::new().fg(theme.accent).bg(panel),
                area,
            );
        }
        let x = area.x + 3;
        let clip = Rect::new(
            area.x + 1,
            area.y,
            area.width.saturating_sub(4),
            area.height,
        );
        let st = |fg: Color| Style::new().fg(fg).bg(panel);
        let mut y = area.y + 1;
        let mut cursor = None;
        if !self.single() {
            let mut tx = x;
            let mut tab = |buf: &mut Buffer, label: &str, active: bool, answered: bool| {
                let w = display_width(label) as u16 + 2;
                let (fg, bg) = if active {
                    (theme.selected_foreground(Some(theme.accent)), theme.accent)
                } else if answered {
                    (theme.text, panel)
                } else {
                    (theme.text_muted, panel)
                };
                fill(buf, Rect::new(tx, y, w, 1), Style::new().bg(bg));
                put_str(buf, tx + 1, y, label, Style::new().fg(fg).bg(bg), clip);
                tx += w + 1;
            };
            for (i, q) in self.qs.iter().enumerate() {
                tab(buf, &q.header, i == self.tab, !self.answers[i].is_empty());
            }
            tab(buf, "Confirm", self.confirm(), false);
            y += 2;
        }
        if self.confirm() {
            put_str(buf, x, y, "Review", st(theme.text), clip);
            y += 1;
            for (i, q) in self.qs.iter().enumerate() {
                let v = self.answers[i].join(", ");
                let hx = put_str(
                    buf,
                    x,
                    y,
                    &format!("{}: ", q.header),
                    st(theme.text_muted),
                    clip,
                );
                if v.is_empty() {
                    put_str(buf, hx, y, "(not answered)", st(theme.error), clip);
                } else {
                    put_str(buf, hx, y, &v, st(theme.text), clip);
                }
                y += 1;
            }
        } else {
            let q = self.q().clone();
            let mut title = q.question.clone();
            if q.multiple {
                title.push_str(" (select all that apply)");
            }
            put_str(buf, x, y, &title, st(theme.text), clip);
            y += 2;
            let el = theme.background_element;
            let num_active = tint(theme.text_muted, theme.secondary, 0.6);
            let row = |buf: &mut Buffer,
                       y: u16,
                       n: usize,
                       label: &str,
                       active: bool,
                       picked: bool,
                       multi: bool| {
                let nums = format!("{n}.");
                let (nfg, lfg) = if active {
                    (num_active, theme.secondary)
                } else if picked {
                    (theme.text_muted, theme.success)
                } else {
                    (theme.text_muted, theme.text)
                };
                let nbg = if active { el } else { panel };
                let nw = display_width(&nums) as u16 + 1;
                fill(buf, Rect::new(x, y, nw, 1), Style::new().bg(nbg));
                put_str(buf, x, y, &nums, Style::new().fg(nfg).bg(nbg), clip);
                let text = if multi {
                    format!("[{}] {label}", if picked { "✓" } else { " " })
                } else {
                    label.to_string()
                };
                let lw = display_width(&text) as u16;
                fill(buf, Rect::new(x + nw, y, lw, 1), Style::new().bg(nbg));
                let ex = put_str(buf, x + nw, y, &text, Style::new().fg(lfg).bg(nbg), clip);
                if !multi && picked {
                    put_str(buf, ex, y, " ✓", st(theme.success), clip);
                }
            };
            for (i, o) in q.options.iter().enumerate() {
                let picked = self.answers[self.tab].contains(&o.label);
                row(
                    buf,
                    y,
                    i + 1,
                    &o.label,
                    i == self.selected,
                    picked,
                    q.multiple,
                );
                y += 1;
                if !o.description.is_empty() {
                    put_str(buf, x + 3, y, &o.description, st(theme.text_muted), clip);
                    y += 1;
                }
            }
            if q.custom {
                let picked = self.custom_picked();
                row(
                    buf,
                    y,
                    q.options.len() + 1,
                    "Type your own answer",
                    self.other(),
                    picked,
                    q.multiple,
                );
                y += 1;
                if self.editing {
                    let h = self.editor.line_count().clamp(1, 6) as u16;
                    let ea = Rect::new(x + 3, y, clip.right().saturating_sub(x + 3), h);
                    let style = EditorStyle {
                        text: st(theme.text),
                        placeholder: Some(("Type your own answer".into(), st(theme.text_muted))),
                    };
                    cursor = self.editor.render(ea, buf, &style).cursor;
                } else if !self.custom[self.tab].is_empty() {
                    put_str(
                        buf,
                        x + 3,
                        y,
                        &self.custom[self.tab],
                        st(theme.text_muted),
                        clip,
                    );
                }
            }
        }
        // footer: after the inner box's bottom padding, so the last-but-one row of the block
        let fy = area.bottom().saturating_sub(2);
        let mut hints: Vec<(&str, &str)> = Vec::new();
        if !self.single() {
            hints.push(("⇆", "tab"));
        }
        if !self.confirm() {
            hints.push(("↑↓", "select"));
        }
        hints.push((
            "enter",
            if self.confirm() || self.single() {
                "submit"
            } else if self.q().multiple {
                "toggle"
            } else {
                "confirm"
            },
        ));
        hints.push(("esc", "dismiss"));
        let mut hx = x;
        for (k, v) in hints {
            hx = put_str(buf, hx, fy, k, st(theme.text), clip);
            hx = put_str(buf, hx, fy, &format!(" {v}"), st(theme.text_muted), clip);
            hx += 2;
        }
        cursor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn bash(rule: &str) -> PermissionRequest {
        PermissionRequest {
            id: "p1".into(),
            tool: "Bash".into(),
            kind: ToolKind::Execute,
            title: "cargo test".into(),
            input: serde_json::json!({"command": "cargo test"}),
            diff: None,
            rule: rule.into(),
        }
    }

    #[test]
    fn enter_allows_once_by_default() {
        let mut p = PermissionPrompt::new(bash("cargo test *"));
        assert_eq!(
            p.handle_key(key(KeyCode::Enter)),
            AskOutcome::Send(Request::Decide {
                id: "p1".into(),
                allow: true,
                scope: DecideScope::Once,
                note: String::new()
            })
        );
    }

    #[test]
    fn always_goes_through_a_confirm_stage() {
        let mut p = PermissionPrompt::new(bash("cargo test *"));
        p.handle_key(key(KeyCode::Right));
        assert_eq!(p.handle_key(key(KeyCode::Enter)), AskOutcome::Stay);
        // Cancel goes back, Confirm sends `Always`
        assert_eq!(p.handle_key(key(KeyCode::Esc)), AskOutcome::Stay);
        p.handle_key(key(KeyCode::Right));
        p.handle_key(key(KeyCode::Enter));
        match p.handle_key(key(KeyCode::Enter)) {
            AskOutcome::Send(Request::Decide { allow, scope, .. }) => {
                assert!(allow);
                assert_eq!(scope, DecideScope::Always);
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn escape_rejects_and_a_missing_rule_hides_always() {
        let mut p = PermissionPrompt::new(bash(""));
        assert_eq!(p.buttons(), vec!["Allow once", "Reject"]);
        match p.handle_key(key(KeyCode::Esc)) {
            AskOutcome::Send(Request::Decide { allow, .. }) => assert!(!allow),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn a_child_session_asks_why_before_rejecting() {
        let mut p = PermissionPrompt::new(bash("x")).in_child_session(true);
        assert_eq!(p.handle_key(key(KeyCode::Esc)), AskOutcome::Stay);
        for c in "no".chars() {
            p.handle_key(key(KeyCode::Char(c)));
        }
        match p.handle_key(key(KeyCode::Enter)) {
            AskOutcome::Send(Request::Decide { allow, note, .. }) => {
                assert!(!allow);
                assert_eq!(note, "no");
            }
            o => panic!("{o:?}"),
        }
    }

    fn color_question() -> QuestionInfo {
        QuestionInfo {
            header: "Color".into(),
            question: "Which color?".into(),
            options: vec![
                QuestionOption {
                    label: "Red".into(),
                    description: "Choose red".into(),
                },
                QuestionOption {
                    label: "Blue".into(),
                    description: "Choose blue".into(),
                },
            ],
            multiple: false,
            custom: true,
        }
    }

    #[test]
    fn a_single_question_answers_on_enter() {
        let mut q = QuestionPrompt::new("q1", vec![color_question()]);
        q.handle_key(key(KeyCode::Down));
        assert_eq!(
            q.handle_key(key(KeyCode::Enter)),
            AskOutcome::Answer {
                id: "q1".into(),
                answers: vec![vec!["Blue".into()]]
            }
        );
    }

    #[test]
    fn number_keys_pick_and_a_custom_answer_is_typed() {
        let mut q = QuestionPrompt::new("q1", vec![color_question()]);
        assert!(matches!(
            q.handle_key(key(KeyCode::Char('1'))),
            AskOutcome::Answer { .. }
        ));
        let mut q = QuestionPrompt::new("q1", vec![color_question()]);
        q.handle_key(key(KeyCode::Char('3')));
        for c in "teal".chars() {
            q.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            q.handle_key(key(KeyCode::Enter)),
            AskOutcome::Answer {
                id: "q1".into(),
                answers: vec![vec!["teal".into()]]
            }
        );
    }

    #[test]
    fn several_questions_end_on_a_confirm_tab() {
        let mut b = color_question();
        b.header = "Size".into();
        let mut q = QuestionPrompt::new("q1", vec![color_question(), b]);
        assert_eq!(q.tabs(), 3);
        q.handle_key(key(KeyCode::Enter)); // Red, moves to the next question
        q.handle_key(key(KeyCode::Enter)); // Red again
        assert!(q.confirm());
        assert_eq!(
            q.handle_key(key(KeyCode::Enter)),
            AskOutcome::Answer {
                id: "q1".into(),
                answers: vec![vec!["Red".into()], vec!["Red".into()]]
            }
        );
    }

    #[test]
    fn esc_dismisses() {
        let mut q = QuestionPrompt::new("q1", vec![color_question()]);
        assert_eq!(
            q.handle_key(key(KeyCode::Esc)),
            AskOutcome::Reject { id: "q1".into() }
        );
    }

    #[test]
    fn multi_select_toggles() {
        let mut c = color_question();
        c.multiple = true;
        let mut q = QuestionPrompt::new("q1", vec![c]);
        q.handle_key(key(KeyCode::Enter));
        q.handle_key(key(KeyCode::Down));
        q.handle_key(key(KeyCode::Enter));
        assert_eq!(q.answers[0], vec!["Red".to_string(), "Blue".to_string()]);
        q.handle_key(key(KeyCode::Enter));
        assert_eq!(q.answers[0], vec!["Red".to_string()]);
    }
}
