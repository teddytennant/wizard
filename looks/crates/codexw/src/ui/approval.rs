// OWNER: approvals
//! The approval views, built from `agent_core::Event::Permission` (spec C.8).
//!
//! Wizard 3.7.1 never emits a permission request over ACP, so these views only appear with the
//! mock backend or a future wizard. A request is shown as the Codex view that fits it:
//!
//! | request                          | view                                                      |
//! |----------------------------------|-----------------------------------------------------------|
//! | `Bash` / execute                 | exec approval (`Would you like to run ...`)               |
//! | edit with a diff                 | patch approval (`Would you like to make ...`)             |
//! | `mcp__server__tool`              | the MCP form (`Field 1/1`)                                |
//! | `AskUserQuestion`                | `request_user_input` (`Question 1/1`), answered by `Answer` |
//! | `ExitPlanMode`                   | `Implement this plan?`                                    |
//! | anything else (fetch, read, ...) | `Would you like to grant these permissions?`              |
//!
//! Only the options the backend can honour are offered. `Always` stands for the request's `rule`,
//! so the "don't ask again" rows are hidden when there is none, and the MCP form has no
//! `Always allow` row because agent-core has no scope longer than the session.

use std::collections::VecDeque;
use std::io::Write;
use std::time::Duration;

use agent_core::{PermissionRequest, Request, ToolKind};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use unicode_segmentation::UnicodeSegmentation;

use crate::app::App;
use crate::style::palette;
use crate::ui::diff_preview;
use crate::ui::{AppAction, BottomView, HistoryCell, ViewResult};
use crate::wrap::{WrapOpts, word_wrap_line};

/// How long the composer must have been idle before an approval replaces it, so a half-typed
/// `yes please` cannot answer a prompt that arrived mid-word (spec C.8).
pub const TYPING_IDLE: Duration = Duration::from_secs(1);

// ---- decisions and their history cells -------------------------------------------------------

/// What a view hands back to the app: the reply for the backend and the cells to print.
#[derive(Clone, Debug, PartialEq)]
pub struct Decision {
    pub id: String,
    pub allow: bool,
    pub always: bool,
    pub note: String,
    /// Codex's `Abort` interrupts the turn after the request is refused.
    pub cancel_turn: bool,
    /// `Request::Answer` pairs instead of a plain decision (request_user_input).
    pub answers: Option<Vec<(String, String)>>,
    pub cell: Option<DecisionCell>,
}

impl Decision {
    fn plain(id: &str, allow: bool) -> Self {
        Self {
            id: id.to_string(),
            allow,
            always: false,
            note: String::new(),
            cancel_turn: false,
            answers: None,
            cell: None,
        }
    }
}

/// A line printed into the history after a decision (spec C.8.4, B.10).
#[derive(Clone, Debug, PartialEq)]
pub enum DecisionCell {
    /// `✔ You approved codex to run {snippet} this time`
    Approved(String),
    /// `✔ You approved codex to always run commands that start with {prefix}`
    ApprovedPrefix(String),
    /// `✔ You approved codex to run {snippet} every time this session`
    ApprovedSession(String),
    /// `✗ You did not approve codex to run {snippet}`
    Denied(String),
    /// `✗ You canceled the request to run {snippet}`
    Aborted(String),
    /// Unstyled text, used for permission grants.
    Plain(String),
    /// `• Questions 1/1 answered` and the answers.
    Questions(Vec<QuestionResult>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct QuestionResult {
    pub question: String,
    pub options: Vec<String>,
    pub note: Option<String>,
}

/// The first line of a command, ` ...` appended when there were more, cut to 80 graphemes.
fn exec_snippet(cmd: &str) -> String {
    let s = match cmd.split_once('\n') {
        Some((first, _)) => format!("{first} ..."),
        None => cmd.to_string(),
    };
    if s.graphemes(true).count() > 80 {
        let cut: String = s.graphemes(true).take(77).collect();
        format!("{cut}...")
    } else {
        s
    }
}

impl HistoryCell for DecisionCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let green = Style::default().fg(Color::Green);
        let red = Style::default().fg(Color::Red);
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let dim = Style::default().add_modifier(Modifier::DIM);
        let wrap = |first: Vec<Span<'static>>| {
            let line = Line::from(first);
            word_wrap_line(
                &line,
                &WrapOpts::new(width as usize).subsequent_indent(Line::from("  ")),
            )
        };
        match self {
            Self::Approved(s) => wrap(vec![
                Span::styled("✔ ", green),
                Span::from("You "),
                Span::styled("approved", bold),
                Span::from(" codex to run "),
                Span::styled(exec_snippet(s), dim),
                Span::styled(" this time", bold),
            ]),
            Self::ApprovedPrefix(s) => wrap(vec![
                Span::styled("✔ ", green),
                Span::from("You "),
                Span::styled("approved", bold),
                Span::from(" codex to always run commands that start with "),
                Span::styled(exec_snippet(s), dim),
            ]),
            Self::ApprovedSession(s) => wrap(vec![
                Span::styled("✔ ", green),
                Span::from("You "),
                Span::styled("approved", bold),
                Span::from(" codex to run "),
                Span::styled(exec_snippet(s), dim),
                Span::styled(" every time this session", bold),
            ]),
            Self::Denied(s) => wrap(vec![
                Span::styled("✗ ", red),
                Span::from("You "),
                Span::styled("did not approve", bold),
                Span::from(" codex to run "),
                Span::styled(exec_snippet(s), dim),
            ]),
            Self::Aborted(s) => wrap(vec![
                Span::styled("✗ ", red),
                Span::from("You "),
                Span::styled("canceled", bold),
                Span::from(" the request to run "),
                Span::styled(exec_snippet(s), dim),
            ]),
            Self::Plain(t) => {
                word_wrap_line(&Line::from(t.clone()), &WrapOpts::new(width as usize))
            }
            Self::Questions(qs) => question_lines(qs, width),
        }
    }
}

fn prefixed(
    text: &str,
    width: u16,
    first: Span<'static>,
    rest: Span<'static>,
    style: Style,
) -> Vec<Line<'static>> {
    let line = Line::from(Span::styled(text.to_string(), style));
    crate::wrap::adaptive_wrap_line(
        &line,
        &WrapOpts::new(width.max(1) as usize)
            .initial_indent(Line::from(first))
            .subsequent_indent(Line::from(rest)),
    )
}

/// `• Questions 1/1 answered` with each question and its answers (spec B.10).
fn question_lines(qs: &[QuestionResult], width: u16) -> Vec<Line<'static>> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let cyan = Style::default().fg(Color::Cyan);
    let answered = qs
        .iter()
        .filter(|q| !q.options.is_empty() || q.note.is_some())
        .count();
    let mut out = vec![Line::from(vec![
        Span::styled("•", dim),
        Span::from(" "),
        Span::styled("Questions", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(format!(" {answered}/{} answered", qs.len()), dim),
    ])];
    for q in qs {
        let mut rows = prefixed(
            &q.question,
            width,
            Span::from("  • "),
            Span::from("    "),
            Style::default(),
        );
        if q.options.is_empty() && q.note.is_none() {
            if let Some(last) = rows.last_mut() {
                last.spans.push(Span::styled(" (unanswered)", dim));
            }
        }
        out.extend(rows);
        for o in &q.options {
            out.extend(prefixed(
                o,
                width,
                Span::styled("    answer: ", dim),
                Span::styled("            ", dim),
                cyan,
            ));
        }
        if let Some(n) = &q.note {
            let (a, b) = if q.options.is_empty() {
                ("    answer: ", "            ")
            } else {
                ("    note: ", "          ")
            };
            out.extend(prefixed(
                n,
                width,
                Span::styled(a, dim),
                Span::styled(b, dim),
                cyan,
            ));
        }
    }
    out
}

// ---- layout helpers ---------------------------------------------------------------------------

/// What a view draws: rows on the tinted band, then rows below it on the plain background.
#[derive(Default)]
struct Frame {
    band: Vec<Line<'static>>,
    hint: Vec<Line<'static>>,
    /// Row and column of the text cursor inside the band, when notes are being typed.
    cursor: Option<(usize, u16)>,
}

impl Frame {
    fn height(&self) -> u16 {
        (self.band.len() + self.hint.len()) as u16
    }

    fn paint(&self, area: Rect, buf: &mut Buffer) {
        let tint = palette().user_message_style();
        for (i, l) in self.band.iter().chain(self.hint.iter()).enumerate() {
            if i as u16 >= area.height {
                break;
            }
            let r = Rect::new(area.x, area.y + i as u16, area.width, 1);
            let style = if i < self.band.len() {
                tint
            } else {
                Style::default()
            };
            Paragraph::new(l.clone()).style(style).render(r, buf);
        }
    }
}

/// Two columns of inset on both sides; continuation rows keep the left inset and no more.
fn indent(text: Line<'static>, width: u16) -> Vec<Line<'static>> {
    let w = (width as usize).saturating_sub(2).max(1);
    let mut spans = vec![Span::from("  ")];
    spans.extend(text.spans);
    word_wrap_line(
        &Line::from(spans),
        &WrapOpts::new(w).subsequent_indent(Line::from("  ")),
    )
}

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

fn accent() -> Style {
    palette().accent()
}

/// One selectable row.
#[derive(Clone, Debug)]
struct Choice {
    label: String,
    /// Key shown as ` (y)` after the label and accepted as a shortcut.
    shortcut: Option<&'static str>,
    keys: Vec<KeyCode>,
    desc: Option<&'static str>,
    decision: Decision,
}

/// Rows of a numbered list. `left` is the margin before the gutter: 0 for the approval lists,
/// whose `›` sits in the surface's inset, 2 for the forms that render inside it.
fn choice_rows(
    choices: &[(Vec<Span<'static>>, Option<String>)],
    selected: usize,
    width: u16,
    left: usize,
) -> Vec<Line<'static>> {
    let num_w = choices.len().to_string().len() + 2;
    let name_w = choices
        .iter()
        .map(|(spans, _)| {
            spans
                .iter()
                .map(|s| crate::ui::text_width(&s.content))
                .sum::<usize>()
        })
        .max()
        .unwrap_or(0);
    let any_desc = choices.iter().any(|c| c.1.is_some());
    let desc_col = left + 2 + num_w + name_w + 2;
    let right = (width as usize).saturating_sub(2).max(1);
    let mut out = Vec::new();
    for (i, (name, desc)) in choices.iter().enumerate() {
        let sel = i == selected;
        let gutter = format!("{}{}", " ".repeat(left), if sel { "› " } else { "  " });
        let num = format!("{}. ", i + 1);
        let mut spans: Vec<Span<'static>> = vec![Span::from(gutter), Span::from(num)];
        spans.extend(name.iter().cloned());
        let hang = left + 2 + num_w;
        let mut rows: Vec<Line<'static>>;
        if let (true, Some(d)) = (any_desc, desc) {
            // name, padding to the description column, then the wrapped description
            let used: usize = spans
                .iter()
                .map(|s| crate::ui::text_width(&s.content))
                .sum();
            let first_desc = Span::styled(d.clone(), dim());
            let mut line = Line::from(spans);
            line.spans
                .push(Span::from(" ".repeat(desc_col.saturating_sub(used))));
            line.spans.push(first_desc);
            rows = word_wrap_line(
                &line,
                &WrapOpts::new(right).subsequent_indent(Line::from(" ".repeat(desc_col))),
            );
        } else {
            let line = Line::from(spans);
            rows = word_wrap_line(
                &line,
                &WrapOpts::new(right).subsequent_indent(Line::from(" ".repeat(hang))),
            );
        }
        if sel {
            for r in &mut rows {
                for s in &mut r.spans {
                    s.style = accent();
                }
            }
        }
        out.extend(rows);
    }
    out
}

fn move_sel(sel: usize, n: usize, key: &KeyEvent) -> Option<usize> {
    let none = key.modifiers == KeyModifiers::NONE;
    let ctrl = key.modifiers == KeyModifiers::CONTROL;
    let up = (none && matches!(key.code, KeyCode::Up | KeyCode::Char('k')))
        || (ctrl && matches!(key.code, KeyCode::Char('p' | 'k')));
    let down = (none && matches!(key.code, KeyCode::Down | KeyCode::Char('j')))
        || (ctrl && matches!(key.code, KeyCode::Char('n' | 'j')));
    if n == 0 {
        return None;
    }
    if up {
        Some((sel + n - 1) % n)
    } else if down {
        Some((sel + 1) % n)
    } else if none && key.code == KeyCode::Home {
        Some(0)
    } else if none && key.code == KeyCode::End {
        Some(n - 1)
    } else {
        None
    }
}

// ---- bash highlighting ------------------------------------------------------------------------

/// Catppuccin colours as Codex's bash highlighter produces them: command words blue, a flag's
/// dash and its letters two different tones, quoted text green, operators teal.
fn highlight_bash(src: &str) -> Vec<Span<'static>> {
    let light = palette().light_bg();
    let c = |dark: (u8, u8, u8), lt: (u8, u8, u8)| {
        let (r, g, b) = if light { lt } else { dark };
        Style::default().fg(Color::Rgb(r, g, b))
    };
    let text = c((205, 214, 244), (76, 79, 105));
    let cmd = c((137, 180, 250), (30, 102, 245));
    let dash = c((147, 153, 178), (124, 127, 147));
    let flag = c((235, 160, 172), (230, 69, 83));
    let string = c((166, 227, 161), (64, 160, 43));
    let op = c((148, 226, 213), (23, 146, 153));
    let mut spans: Vec<Span<'static>> = Vec::new();
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut at_command = true;
    let push = |spans: &mut Vec<Span<'static>>, s: String, st: Style| {
        if s.is_empty() {
            return;
        }
        if let Some(last) = spans.last_mut() {
            if last.style == st {
                last.content.to_mut().push_str(&s);
                return;
            }
        }
        spans.push(Span::styled(s, st));
    };
    while i < chars.len() {
        let ch = chars[i];
        if ch.is_whitespace() {
            push(&mut spans, ch.to_string(), text);
            i += 1;
            continue;
        }
        if matches!(ch, '&' | '|' | ';') {
            let mut j = i;
            while j < chars.len() && matches!(chars[j], '&' | '|' | ';') {
                j += 1;
            }
            push(&mut spans, chars[i..j].iter().collect(), op);
            at_command = true;
            i = j;
            continue;
        }
        // a word, with quotes kept whole
        let start = i;
        let mut j = i;
        let mut quote: Option<char> = None;
        while j < chars.len() {
            let c = chars[j];
            match quote {
                Some(q) => {
                    if c == '\\' && q == '"' {
                        j += 1;
                    } else if c == q {
                        quote = None;
                    }
                }
                None => {
                    if c == '\'' || c == '"' {
                        quote = Some(c);
                    } else if c.is_whitespace() || matches!(c, '&' | '|' | ';') {
                        break;
                    }
                }
            }
            j += 1;
        }
        let j = j.min(chars.len());
        let word: String = chars[start..j].iter().collect();
        if word.starts_with('\'') || word.starts_with('"') {
            push(&mut spans, word, string);
        } else if at_command {
            push(&mut spans, word, cmd);
        } else if word.starts_with('-') && word.len() > 1 {
            let dashes = word.chars().take_while(|c| *c == '-').count();
            push(&mut spans, word[..dashes].to_string(), dash);
            push(&mut spans, word[dashes..].to_string(), flag);
        } else {
            push(&mut spans, word, text);
        }
        at_command = false;
        i = j;
    }
    spans
}

/// `$ ` and the command, highlighted; one `Line` per source line.
fn command_lines(cmd: &str) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = cmd
        .split('\n')
        .map(|l| Line::from(highlight_bash(l)))
        .collect();
    if let Some(first) = out.first_mut() {
        first.spans.insert(0, Span::from("$ "));
    }
    out
}

fn command_of(req: &PermissionRequest) -> String {
    req.input
        .get("command")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| req.title.clone())
}

// ---- the list approval (exec, patch, permissions, plan) --------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Exec,
    Patch,
    Permissions,
    Plan,
}

#[derive(Debug)]
pub struct ApprovalView {
    kind: Kind,
    title: String,
    header: Vec<Line<'static>>,
    choices: Vec<Choice>,
    selected: usize,
    /// What Esc and Ctrl+C decide.
    cancel: Decision,
    fullscreen: Option<(String, Vec<Line<'static>>)>,
}

impl ApprovalView {
    fn frame(&self, width: u16) -> Frame {
        let mut f = Frame::default();
        f.band.push(Line::default());
        f.band.extend(indent(
            Line::from(Span::styled(
                self.title.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            width,
        ));
        f.band.push(Line::default());
        for l in &self.header {
            f.band.extend(indent(l.clone(), width));
        }
        f.band.push(Line::default());
        let rows: Vec<(Vec<Span<'static>>, Option<String>)> = self
            .choices
            .iter()
            .map(|c| {
                let mut name = vec![Span::from(c.label.clone())];
                if let Some(k) = c.shortcut {
                    name.push(Span::from(" ("));
                    name.push(Span::styled(k, dim()));
                    name.push(Span::from(")"));
                }
                (name, c.desc.map(str::to_string))
            })
            .collect();
        f.band.extend(choice_rows(&rows, self.selected, width, 0));
        f.band.push(Line::default());
        let hint = if self.kind == Kind::Plan {
            "Press enter to confirm or esc to go back"
        } else {
            "Press enter to confirm or esc to cancel"
        };
        f.hint.push(Line::from(vec![
            Span::from("  "),
            Span::styled(hint, dim()),
        ]));
        f
    }

    fn accept(&mut self, idx: usize) -> ViewResult {
        match self.choices.get(idx) {
            Some(c) => ViewResult::CloseWith(AppAction::Decision(c.decision.clone())),
            None => ViewResult::Pending,
        }
    }
}

impl BottomView for ApprovalView {
    fn desired_height(&self, width: u16) -> u16 {
        self.frame(width).height()
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.frame(area.width).paint(area, buf);
    }

    fn needs_action(&self) -> bool {
        true
    }

    fn handle_key(&mut self, key: KeyEvent) -> ViewResult {
        let none = key.modifiers == KeyModifiers::NONE;
        let ctrl = key.modifiers == KeyModifiers::CONTROL;
        if ctrl && key.code == KeyCode::Char('a') {
            return match &self.fullscreen {
                Some((t, l)) => ViewResult::Action(AppAction::ShowStatic {
                    title: t.clone(),
                    lines: l.clone(),
                }),
                None => ViewResult::Pending,
            };
        }
        if (none && key.code == KeyCode::Esc) || (ctrl && key.code == KeyCode::Char('c')) {
            return ViewResult::CloseWith(AppAction::Decision(self.cancel.clone()));
        }
        // Only keys of the offered decisions act; `d` on a prompt without a decline row is dead.
        if none {
            if let KeyCode::Char(c) = key.code {
                if let Some(i) = self
                    .choices
                    .iter()
                    .position(|ch| ch.keys.contains(&KeyCode::Char(c)))
                {
                    return self.accept(i);
                }
                if let Some(d) = c.to_digit(10) {
                    let i = d as usize;
                    if i >= 1 && i <= self.choices.len() {
                        return self.accept(i - 1);
                    }
                }
            }
        }
        if let Some(s) = move_sel(self.selected, self.choices.len(), &key) {
            self.selected = s;
            return ViewResult::Pending;
        }
        if none && key.code == KeyCode::Enter {
            return self.accept(self.selected);
        }
        ViewResult::Pending
    }
}

/// The rule as a command prefix: `cargo test *` offers `cargo test`.
fn rule_prefix(rule: &str) -> String {
    rule.trim().trim_end_matches('*').trim_end().to_string()
}

fn environment_rows() -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::from("Environment: "),
            Span::styled("local", Style::default().add_modifier(Modifier::BOLD)),
        ]),
        Line::default(),
    ]
}

fn reason_rows(reason: &str) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::from("Reason: "),
            Span::styled(
                reason.to_string(),
                Style::default().add_modifier(Modifier::ITALIC),
            ),
        ]),
        Line::default(),
    ]
}

fn reason_of(req: &PermissionRequest) -> Option<String> {
    ["reason", "justification"]
        .iter()
        .find_map(|k| req.input.get(*k).and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn exec_view(req: &PermissionRequest) -> ApprovalView {
    let cmd = command_of(req);
    let mut header = environment_rows();
    if let Some(r) = reason_of(req) {
        header.extend(reason_rows(&r));
    }
    header.extend(command_lines(&cmd));
    let mut choices = vec![Choice {
        label: "Yes, proceed".into(),
        shortcut: Some("y"),
        keys: vec![KeyCode::Char('y')],
        desc: None,
        decision: Decision {
            cell: Some(DecisionCell::Approved(cmd.clone())),
            ..Decision::plain(&req.id, true)
        },
    }];
    let prefix = rule_prefix(&req.rule);
    if !prefix.is_empty() && !prefix.contains('\n') {
        choices.push(Choice {
            label: format!("Yes, and don't ask again for commands that start with `{prefix}`"),
            shortcut: Some("p"),
            keys: vec![KeyCode::Char('p')],
            desc: None,
            decision: Decision {
                always: true,
                cell: Some(DecisionCell::ApprovedPrefix(prefix)),
                ..Decision::plain(&req.id, true)
            },
        });
    }
    let abort = Decision {
        cancel_turn: true,
        cell: Some(DecisionCell::Aborted(cmd.clone())),
        ..Decision::plain(&req.id, false)
    };
    choices.push(Choice {
        label: "No, and tell Wizard what to do differently".into(),
        shortcut: Some("esc"),
        keys: vec![KeyCode::Char('n')],
        desc: None,
        decision: abort.clone(),
    });
    ApprovalView {
        kind: Kind::Exec,
        title: "Would you like to run the following command?".into(),
        header,
        choices,
        selected: 0,
        cancel: abort,
        fullscreen: Some((
            "E X E C".into(),
            command_lines(&cmd)
                .into_iter()
                .map(|mut l| {
                    l.spans.remove(0);
                    l
                })
                .collect(),
        )),
    }
}

fn patch_view(req: &PermissionRequest) -> ApprovalView {
    let mut header = Vec::new();
    if let Some(r) = reason_of(req) {
        header.extend(reason_rows(&r));
        header.pop();
    }
    let mut choices = vec![Choice {
        label: "Yes, proceed".into(),
        shortcut: Some("y"),
        keys: vec![KeyCode::Char('y')],
        desc: None,
        decision: Decision::plain(&req.id, true),
    }];
    if !req.rule.is_empty() {
        choices.push(Choice {
            label: "Yes, and don't ask again for these files".into(),
            shortcut: Some("a"),
            keys: vec![KeyCode::Char('a')],
            desc: None,
            decision: Decision {
                always: true,
                ..Decision::plain(&req.id, true)
            },
        });
    }
    let cancel = Decision {
        cancel_turn: true,
        ..Decision::plain(&req.id, false)
    };
    choices.push(Choice {
        label: "No, and tell Wizard what to do differently".into(),
        shortcut: Some("esc"),
        keys: vec![KeyCode::Char('n')],
        desc: None,
        decision: cancel.clone(),
    });
    let full = req.diff.as_ref().map(|d| {
        (
            "P A T C H".to_string(),
            diff_preview::file_diff_lines(d, 120),
        )
    });
    ApprovalView {
        kind: Kind::Patch,
        title: "Would you like to make the following edits?".into(),
        header,
        choices,
        selected: 0,
        cancel,
        fullscreen: full,
    }
}

fn permissions_view(req: &PermissionRequest) -> ApprovalView {
    let what = if req.title.is_empty() {
        req.tool.clone()
    } else {
        format!("{} {}", req.tool, req.title)
    };
    let mut header = environment_rows();
    header.extend(reason_rows(&what));
    if !req.rule.is_empty() {
        header.push(Line::from(vec![
            Span::from("Permission rule: "),
            Span::styled(req.rule.clone(), Style::default().fg(Color::Cyan)),
        ]));
    } else {
        header.pop();
    }
    let decline = Decision {
        cell: Some(DecisionCell::Plain(
            "You did not grant additional permissions".into(),
        )),
        ..Decision::plain(&req.id, false)
    };
    let mut choices = vec![Choice {
        label: "Yes, grant these permissions for this turn".into(),
        shortcut: Some("y"),
        keys: vec![KeyCode::Char('y')],
        desc: None,
        decision: Decision {
            cell: Some(DecisionCell::Plain(
                "You granted additional permissions".into(),
            )),
            ..Decision::plain(&req.id, true)
        },
    }];
    if !req.rule.is_empty() {
        choices.push(Choice {
            label: "Yes, grant these permissions for this session".into(),
            shortcut: Some("a"),
            keys: vec![KeyCode::Char('a')],
            desc: None,
            decision: Decision {
                always: true,
                cell: Some(DecisionCell::Plain(
                    "You granted additional permissions for this session".into(),
                )),
                ..Decision::plain(&req.id, true)
            },
        });
    }
    choices.push(Choice {
        label: "No, continue without permissions".into(),
        shortcut: Some("d"),
        keys: vec![KeyCode::Char('d')],
        desc: None,
        decision: decline.clone(),
    });
    ApprovalView {
        kind: Kind::Permissions,
        title: "Would you like to grant these permissions?".into(),
        header,
        choices,
        selected: 0,
        cancel: decline,
        fullscreen: None,
    }
}

fn plan_view(req: &PermissionRequest) -> ApprovalView {
    let plan = req.input.get("plan").and_then(|v| v.as_str()).unwrap_or("");
    let no = Decision::plain(&req.id, false);
    let choices = vec![
        Choice {
            label: "Yes, implement this plan".into(),
            shortcut: None,
            keys: vec![KeyCode::Char('y')],
            desc: Some("Switch to Default and start coding."),
            decision: Decision::plain(&req.id, true),
        },
        Choice {
            label: "No, stay in Plan mode".into(),
            shortcut: None,
            keys: vec![KeyCode::Char('n')],
            desc: Some("Continue planning with the model."),
            decision: no.clone(),
        },
    ];
    ApprovalView {
        kind: Kind::Plan,
        title: "Implement this plan?".into(),
        header: Vec::new(),
        choices,
        selected: 0,
        cancel: no,
        fullscreen: Some((
            "P L A N".into(),
            plan.lines().map(|l| Line::from(l.to_string())).collect(),
        )),
    }
}

// ---- MCP form ---------------------------------------------------------------------------------

#[derive(Debug)]
pub struct McpFormView {
    id: String,
    prompt: String,
    params: Vec<String>,
    choices: Vec<Choice>,
    selected: usize,
}

/// `mcp__brave__browser_click` as `("brave", "browser_click")`.
fn mcp_names(tool: &str) -> (String, String) {
    let rest = tool.strip_prefix("mcp__").unwrap_or(tool);
    match rest.split_once("__") {
        Some((s, t)) => (s.to_string(), t.to_string()),
        None => (rest.to_string(), rest.to_string()),
    }
}

/// Up to three `name: value` lines, values cut at 60 graphemes (spec C.8.6).
fn param_summary(input: &serde_json::Value) -> Vec<String> {
    let Some(map) = input.as_object() else {
        return Vec::new();
    };
    map.iter()
        .take(3)
        .map(|(k, v)| {
            let val = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let val = if val.graphemes(true).count() > 60 {
                let cut: String = val.graphemes(true).take(57).collect();
                format!("{cut}...")
            } else {
                val
            };
            format!("{k}: {val}")
        })
        .collect()
}

fn mcp_view(req: &PermissionRequest) -> McpFormView {
    let (server, tool) = mcp_names(&req.tool);
    let mut choices = vec![Choice {
        label: "Allow".into(),
        shortcut: None,
        keys: vec![],
        desc: Some("Run the tool and continue."),
        decision: Decision::plain(&req.id, true),
    }];
    if !req.rule.is_empty() {
        choices.push(Choice {
            label: "Allow for this session".into(),
            shortcut: None,
            keys: vec![],
            desc: Some("Run the tool and remember this choice for this session."),
            decision: Decision {
                always: true,
                ..Decision::plain(&req.id, true)
            },
        });
    }
    choices.push(Choice {
        label: "Cancel".into(),
        shortcut: None,
        keys: vec![],
        desc: Some("Cancel this tool call"),
        decision: Decision::plain(&req.id, false),
    });
    McpFormView {
        id: req.id.clone(),
        prompt: format!("Allow the {server} MCP server to run tool \"{tool}\"?"),
        params: param_summary(&req.input),
        choices,
        selected: 0,
    }
}

fn form_tips(tips: &[(String, bool)], width: u16) -> Vec<Line<'static>> {
    // tips joined by ` | `, wrapped by tip
    let w = (width as usize).saturating_sub(4).max(1);
    let mut rows: Vec<Vec<(String, bool)>> = vec![Vec::new()];
    let mut used = 0usize;
    for (t, hi) in tips {
        let tw = crate::ui::text_width(t);
        let add = if rows.last().unwrap().is_empty() {
            tw
        } else {
            tw + 3
        };
        if used + add > w && !rows.last().unwrap().is_empty() {
            rows.push(Vec::new());
            used = 0;
        }
        used += if rows.last().unwrap().is_empty() {
            tw
        } else {
            tw + 3
        };
        rows.last_mut().unwrap().push((t.clone(), *hi));
    }
    rows.into_iter()
        .map(|r| {
            let mut spans = vec![Span::from("  ")];
            for (i, (t, hi)) in r.into_iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled(" | ", dim()));
                }
                spans.push(if hi {
                    Span::styled(t, accent())
                } else {
                    Span::styled(t, dim())
                });
            }
            Line::from(spans)
        })
        .collect()
}

impl McpFormView {
    fn frame(&self, width: u16) -> Frame {
        let mut f = Frame::default();
        f.band.push(Line::default());
        f.band.push(Line::from(vec![
            Span::from("  "),
            Span::styled("Field 1/1", dim()),
        ]));
        f.band
            .extend(indent(Line::from(self.prompt.clone()), width));
        if !self.params.is_empty() {
            f.band.push(Line::default());
            for p in &self.params {
                f.band.extend(indent(Line::from(p.clone()), width));
            }
            f.band.push(Line::default());
        }
        let rows: Vec<_> = self
            .choices
            .iter()
            .map(|c| {
                (
                    vec![Span::from(c.label.clone())],
                    c.desc.map(str::to_string),
                )
            })
            .collect();
        f.band.extend(choice_rows(&rows, self.selected, width, 2));
        f.band.extend(form_tips(
            &[
                ("enter to submit".into(), true),
                ("esc to cancel".into(), false),
            ],
            width,
        ));
        f.band.push(Line::default());
        pad_to_min(&mut f.band);
        f
    }
}

/// Forms are never shorter than eight rows (`MIN_OVERLAY_HEIGHT`); the slack goes above the
/// bottom padding row.
fn pad_to_min(band: &mut Vec<Line<'static>>) {
    while band.len() < 8 {
        let at = band.len() - 1;
        band.insert(at, Line::default());
    }
}

impl BottomView for McpFormView {
    fn desired_height(&self, width: u16) -> u16 {
        self.frame(width).height()
    }
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.frame(area.width).paint(area, buf);
    }
    fn needs_action(&self) -> bool {
        true
    }
    fn handle_key(&mut self, key: KeyEvent) -> ViewResult {
        let none = key.modifiers == KeyModifiers::NONE;
        let ctrl = key.modifiers == KeyModifiers::CONTROL;
        let cancel =
            |id: &str| ViewResult::CloseWith(AppAction::Decision(Decision::plain(id, false)));
        // Esc is always Cancel for this form
        if (none && key.code == KeyCode::Esc) || (ctrl && key.code == KeyCode::Char('c')) {
            return cancel(&self.id);
        }
        if none {
            if let KeyCode::Char(c) = key.code {
                if let Some(d) = c.to_digit(10) {
                    let i = d as usize;
                    if i >= 1 && i <= self.choices.len() {
                        return ViewResult::CloseWith(AppAction::Decision(
                            self.choices[i - 1].decision.clone(),
                        ));
                    }
                }
            }
            if matches!(key.code, KeyCode::Enter | KeyCode::Char(' ')) {
                return ViewResult::CloseWith(AppAction::Decision(
                    self.choices[self.selected].decision.clone(),
                ));
            }
        }
        if let Some(s) = move_sel(self.selected, self.choices.len(), &key) {
            self.selected = s;
        }
        ViewResult::Pending
    }
}

// ---- request_user_input -----------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Question {
    text: String,
    multi: bool,
    options: Vec<(String, String)>,
}

#[derive(Clone, Debug, Default)]
struct Answer {
    selected: usize,
    checked: Vec<bool>,
    done: bool,
    note: String,
    notes_open: bool,
}

#[derive(Debug)]
pub struct UserInputView {
    id: String,
    questions: Vec<Question>,
    answers: Vec<Answer>,
    current: usize,
}

const NONE_OF_THE_ABOVE: &str = "None of the above";

fn user_input_view(req: &PermissionRequest) -> Option<UserInputView> {
    let qs = req.input.get("questions")?.as_array()?;
    let questions: Vec<Question> = qs
        .iter()
        .filter_map(|q| {
            let text = q.get("question")?.as_str()?.to_string();
            let options = q
                .get("options")
                .and_then(|o| o.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|o| {
                            Some((
                                o.get("label")?.as_str()?.to_string(),
                                o.get("description")
                                    .and_then(|d| d.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(Question {
                text,
                multi: q
                    .get("multiSelect")
                    .and_then(|m| m.as_bool())
                    .unwrap_or(false),
                options,
            })
        })
        .collect();
    if questions.is_empty() {
        return None;
    }
    let answers = questions
        .iter()
        .map(|q| Answer {
            checked: vec![false; q.options.len()],
            ..Default::default()
        })
        .collect();
    Some(UserInputView {
        id: req.id.clone(),
        questions,
        answers,
        current: 0,
    })
}

impl UserInputView {
    fn row_count(&self, q: usize) -> usize {
        self.questions[q].options.len() + 1
    }

    fn unanswered(&self) -> usize {
        self.answers.iter().filter(|a| !a.done).count()
    }

    fn tips(&self) -> Vec<(String, bool)> {
        let a = &self.answers[self.current];
        let last = self.current + 1 == self.questions.len();
        let mut t = Vec::new();
        if a.notes_open {
            t.push(("tab or esc to clear notes".to_string(), false));
        } else {
            t.push(("tab to add notes".to_string(), true));
        }
        t.push((
            if last && self.questions.len() > 1 {
                "enter to submit all"
            } else {
                "enter to submit answer"
            }
            .to_string(),
            true,
        ));
        if self.questions.len() > 1 {
            t.push(("←/→ to navigate questions".to_string(), false));
        }
        if !a.notes_open {
            t.push(("esc to interrupt".to_string(), false));
        }
        t
    }

    fn frame(&self, width: u16) -> Frame {
        let q = &self.questions[self.current];
        let a = &self.answers[self.current];
        let mut f = Frame::default();
        f.band.push(Line::default());
        let un = self.unanswered();
        let mut progress = format!("Question {}/{}", self.current + 1, self.questions.len());
        if un > 0 {
            progress.push_str(&format!(" ({un} unanswered)"));
        }
        f.band.push(Line::from(vec![
            Span::from("  "),
            Span::styled(progress, dim()),
        ]));
        let qstyle = if a.done {
            Style::default()
        } else {
            Style::default().fg(Color::Cyan)
        };
        f.band.extend(indent(
            Line::from(Span::styled(q.text.clone(), qstyle)),
            width,
        ));
        f.band.push(Line::default());
        let mut rows: Vec<(Vec<Span<'static>>, Option<String>)> = Vec::new();
        for (i, (label, desc)) in q.options.iter().enumerate() {
            let mark = if q.multi {
                if a.checked[i] { "[x] " } else { "[ ] " }
            } else {
                ""
            };
            rows.push((
                vec![Span::from(format!("{mark}{label}"))],
                Some(desc.clone()),
            ));
        }
        rows.push((
            vec![Span::from(NONE_OF_THE_ABOVE)],
            Some("Optionally, add details in notes (tab).".into()),
        ));
        f.band.extend(choice_rows(&rows, a.selected, width, 2));
        f.band.push(Line::default());
        if a.notes_open {
            f.cursor = Some((f.band.len(), 4 + crate::ui::text_width(&a.note) as u16));
            f.band.push(Line::from(vec![
                Span::from("  "),
                Span::styled("› ", Style::default().add_modifier(Modifier::BOLD)),
                Span::from(a.note.clone()),
            ]));
            f.band.push(Line::default());
        }
        f.band.extend(form_tips(&self.tips(), width));
        f.band.push(Line::default());
        pad_to_min(&mut f.band);
        f
    }

    /// Record the highlighted choice of the current question.
    fn record(&mut self) {
        let q = &self.questions[self.current];
        let a = &mut self.answers[self.current];
        if q.multi && a.selected < q.options.len() && !a.checked.iter().any(|c| *c) {
            a.checked[a.selected] = true;
        }
        a.done = true;
    }

    fn result(&self, i: usize) -> QuestionResult {
        let q = &self.questions[i];
        let a = &self.answers[i];
        let note = (!a.note.trim().is_empty()).then(|| a.note.trim().to_string());
        if !a.done {
            return QuestionResult {
                question: q.text.clone(),
                options: vec![],
                note: None,
            };
        }
        let options: Vec<String> = if q.multi {
            q.options
                .iter()
                .zip(&a.checked)
                .filter(|(_, c)| **c)
                .map(|(o, _)| o.0.clone())
                .collect()
        } else {
            vec![
                q.options
                    .get(a.selected)
                    .map_or(NONE_OF_THE_ABOVE.to_string(), |o| o.0.clone()),
            ]
        };
        QuestionResult {
            question: q.text.clone(),
            options,
            note,
        }
    }

    fn submit(&self) -> ViewResult {
        let results: Vec<QuestionResult> =
            (0..self.questions.len()).map(|i| self.result(i)).collect();
        // `Request::Answer` carries one string per question: the picks joined with `, `, with a
        // note after them. "None of the above" with a note is just the note.
        let answers = results
            .iter()
            .filter(|r| !r.options.is_empty())
            .map(|r| {
                let picks = r.options.join(", ");
                let text = match (&r.note, picks.as_str()) {
                    (Some(n), NONE_OF_THE_ABOVE) => n.clone(),
                    (Some(n), p) => format!("{p} ({n})"),
                    (None, p) => p.to_string(),
                };
                (r.question.clone(), text)
            })
            .collect();
        ViewResult::CloseWith(AppAction::Decision(Decision {
            answers: Some(answers),
            cell: Some(DecisionCell::Questions(results)),
            ..Decision::plain(&self.id, true)
        }))
    }

    fn interrupt(&self) -> ViewResult {
        ViewResult::CloseWith(AppAction::Decision(Decision {
            cancel_turn: true,
            ..Decision::plain(&self.id, false)
        }))
    }
}

impl BottomView for UserInputView {
    fn desired_height(&self, width: u16) -> u16 {
        self.frame(width).height()
    }
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.frame(area.width).paint(area, buf);
    }
    fn needs_action(&self) -> bool {
        true
    }
    fn cursor(&self, area: Rect) -> Option<Position> {
        let (row, col) = self.frame(area.width).cursor?;
        Some(Position::new(area.x + col, area.y + row as u16))
    }
    fn handle_paste(&mut self, text: &str) {
        let a = &mut self.answers[self.current];
        if a.notes_open {
            a.note.push_str(&text.replace(['\n', '\r'], " "));
        }
    }
    fn handle_key(&mut self, key: KeyEvent) -> ViewResult {
        let none = key.modifiers == KeyModifiers::NONE || key.modifiers == KeyModifiers::SHIFT;
        let ctrl = key.modifiers == KeyModifiers::CONTROL;
        let cur = self.current;
        let notes_open = self.answers[cur].notes_open;
        if ctrl && key.code == KeyCode::Char('c') {
            if notes_open {
                self.answers[cur].notes_open = false;
                self.answers[cur].note.clear();
                return ViewResult::Pending;
            }
            return self.interrupt();
        }
        match key.code {
            KeyCode::Esc => {
                if notes_open {
                    self.answers[cur].notes_open = false;
                    self.answers[cur].note.clear();
                    return ViewResult::Pending;
                }
                return self.interrupt();
            }
            KeyCode::Tab => {
                let a = &mut self.answers[cur];
                if a.notes_open {
                    a.notes_open = false;
                    a.note.clear();
                } else {
                    a.notes_open = true;
                }
                return ViewResult::Pending;
            }
            KeyCode::Enter => {
                self.record();
                let n = self.questions.len();
                let next = (1..=n)
                    .map(|d| (cur + d) % n)
                    .find(|&i| !self.answers[i].done);
                return match next {
                    Some(i) => {
                        self.current = i;
                        ViewResult::Pending
                    }
                    None => self.submit(),
                };
            }
            _ => {}
        }
        if notes_open {
            match key.code {
                KeyCode::Char(c) if none => self.answers[cur].note.push(c),
                KeyCode::Backspace => {
                    self.answers[cur].note.pop();
                }
                _ => {}
            }
            return ViewResult::Pending;
        }
        let rows = self.row_count(cur);
        if none && matches!(key.code, KeyCode::Left | KeyCode::Char('h')) {
            self.current = (cur + self.questions.len() - 1) % self.questions.len();
            return ViewResult::Pending;
        }
        if none && matches!(key.code, KeyCode::Right | KeyCode::Char('l')) {
            self.current = (cur + 1) % self.questions.len();
            return ViewResult::Pending;
        }
        if none && key.code == KeyCode::Char(' ') {
            let q = &self.questions[cur];
            let a = &mut self.answers[cur];
            if q.multi && a.selected < q.options.len() {
                a.checked[a.selected] = !a.checked[a.selected];
            }
            return ViewResult::Pending;
        }
        if none {
            if let KeyCode::Char(c) = key.code {
                if let Some(d) = c.to_digit(10) {
                    let i = d as usize;
                    if i >= 1 && i <= rows {
                        self.answers[cur].selected = i - 1;
                        return self.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                    }
                }
            }
        }
        if let Some(s) = move_sel(self.answers[cur].selected, rows, &key) {
            self.answers[cur].selected = s;
        }
        ViewResult::Pending
    }
}

// ---- entry points -----------------------------------------------------------------------------

/// Called for every `Event::Permission`. The request waits in a queue until the pane is free and
/// the composer has been idle, then its view replaces the composer.
pub fn on_permission<W: Write>(app: &mut App<W>, req: PermissionRequest) {
    for cell in app.log.permission_preview(&req) {
        app.push_cell(cell);
    }
    app.pending_perms.push_back(req);
    open_next_if_idle(app);
}

pub fn build_view(req: &PermissionRequest) -> Box<dyn BottomView> {
    if req.tool == "AskUserQuestion" {
        if let Some(v) = user_input_view(req) {
            return Box::new(v);
        }
    }
    if req.tool == "ExitPlanMode" {
        return Box::new(plan_view(req));
    }
    if req.tool.starts_with("mcp__") {
        return Box::new(mcp_view(req));
    }
    if req.kind == ToolKind::Execute || req.tool.eq_ignore_ascii_case("bash") {
        return Box::new(exec_view(req));
    }
    if req.kind == ToolKind::Edit || req.diff.is_some() {
        return Box::new(patch_view(req));
    }
    Box::new(permissions_view(req))
}

/// Show the oldest waiting request when nothing else holds the bottom pane.
pub fn open_next_if_idle<W: Write>(app: &mut App<W>) {
    if app.pending_perms.is_empty() || app.pane.view.is_some() {
        return;
    }
    if !app.pane.composer.is_empty() && app.last_input.elapsed() < TYPING_IDLE {
        return;
    }
    let Some(req) = app.pending_perms.pop_front() else {
        return;
    };
    app.pane.push_view(build_view(&req));
    app.request_draw();
}

/// A view closed with a decision: print its cells and answer the backend.
pub fn apply_decision<W: Write>(app: &mut App<W>, d: Decision) {
    if let Some(cell) = d.cell {
        app.push_cell(Box::new(cell));
    }
    let _ = match d.answers {
        Some(answers) => app.tx.send(Request::Answer { id: d.id, answers }),
        None => {
            let scope = if d.always {
                agent_core::DecideScope::Always
            } else {
                agent_core::DecideScope::Once
            };
            app.tx.send(Request::Decide {
                id: d.id,
                allow: d.allow,
                scope,
                note: d.note,
            })
        }
    };
    if d.cancel_turn {
        app.interrupt();
    }
    open_next_if_idle(app);
}

pub type PendingPerms = VecDeque<PermissionRequest>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::{ColorLevel, Palette, set_palette};
    use serde_json::json;

    fn dark() {
        set_palette(Palette::new(
            Some((230, 230, 230)),
            Some((0, 0, 0)),
            ColorLevel::TrueColor,
        ));
    }

    fn render(v: &dyn BottomView, w: u16) -> Vec<String> {
        let h = v.desired_height(w);
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        v.render(area, &mut buf);
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn bash(rule: &str) -> PermissionRequest {
        PermissionRequest {
            id: "p1".into(),
            tool: "Bash".into(),
            kind: ToolKind::Execute,
            title: "echo hello world".into(),
            input: json!({"command": "echo hello world"}),
            diff: None,
            rule: rule.into(),
        }
    }

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    // chatwidget__tests__approval_modal_exec, with the Environment row of the real captures
    #[test]
    fn exec_view_matches_the_capture_layout() {
        dark();
        let v = build_view(&bash("echo hello world"));
        let got = render(v.as_ref(), 120);
        let want = [
            "",
            "  Would you like to run the following command?",
            "",
            "  Environment: local",
            "",
            "  $ echo hello world",
            "",
            "› 1. Yes, proceed (y)",
            "  2. Yes, and don't ask again for commands that start with `echo hello world` (p)",
            "  3. No, and tell Wizard what to do differently (esc)",
            "",
            "  Press enter to confirm or esc to cancel",
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn exec_view_styles() {
        dark();
        let v = build_view(&bash("echo hello world"));
        let area = Rect::new(0, 0, 80, v.desired_height(80));
        let mut buf = Buffer::empty(area);
        v.render(area, &mut buf);
        // title bold on the tint
        assert!(buf[(2, 1)].modifier.contains(Modifier::BOLD));
        assert_eq!(buf[(2, 1)].bg, Color::Rgb(30, 30, 30));
        // `echo` blue, args text colour
        assert_eq!(buf[(4, 5)].fg, Color::Rgb(137, 180, 250));
        assert_eq!(buf[(9, 5)].fg, Color::Rgb(205, 214, 244));
        // selected row accent from the gutter, the key of unselected rows dim
        assert!(buf[(0, 7)].modifier.contains(Modifier::BOLD));
        assert!(buf[(0, 7)].fg != Color::Reset);
        let row3 = (0..area.height)
            .find(|&y| {
                (0..80)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .contains("3. No")
            })
            .unwrap();
        assert!(
            buf[(49, row3)].modifier.contains(Modifier::DIM),
            "the key of an unselected row is dim"
        );
        assert!(
            !buf[(46, row3)].modifier.contains(Modifier::DIM),
            "the parenthesis is not"
        );
        // the hint row is outside the band
        let last = area.height - 1;
        assert_eq!(buf[(0, last)].bg, Color::Reset);
        assert!(buf[(2, last)].modifier.contains(Modifier::DIM));
    }

    #[test]
    fn no_rule_hides_the_prefix_row() {
        dark();
        let v = build_view(&bash(""));
        let got = render(v.as_ref(), 100);
        assert!(!got.iter().any(|r| r.contains("don't ask again")));
        assert!(got.iter().any(|r| r.contains("2. No, and tell Wizard")));
    }

    // 80x24 capture: the long option wraps under its text, five columns in
    #[test]
    fn long_option_wraps_with_hang() {
        dark();
        let mut r = bash("touch approved.txt");
        r.input = json!({"command": "touch approved.txt && echo created"});
        let got = render(build_view(&r).as_ref(), 80);
        assert!(
            got.contains(
                &"  2. Yes, and don't ask again for commands that start with `touch".to_string()
            ),
            "{got:?}"
        );
        assert!(got.contains(&"     approved.txt` (p)".to_string()));
    }

    // chatwidget__tests__approval_modal_patch: two blank rows when there is no reason
    #[test]
    fn patch_view_layout() {
        dark();
        let req = PermissionRequest {
            id: "p2".into(),
            tool: "Edit".into(),
            kind: ToolKind::Edit,
            title: "src/main.rs".into(),
            input: json!({}),
            diff: None,
            rule: "src/**".into(),
        };
        let got = render(build_view(&req).as_ref(), 120);
        assert_eq!(
            got,
            vec![
                "",
                "  Would you like to make the following edits?",
                "",
                "",
                "› 1. Yes, proceed (y)",
                "  2. Yes, and don't ask again for these files (a)",
                "  3. No, and tell Wizard what to do differently (esc)",
                "",
                "  Press enter to confirm or esc to cancel",
            ]
        );
    }

    // approval_overlay_permissions_prompt
    #[test]
    fn permissions_view_layout() {
        dark();
        let req = PermissionRequest {
            id: "p3".into(),
            tool: "Read".into(),
            kind: ToolKind::Read,
            title: "/etc/hosts".into(),
            input: json!({}),
            diff: None,
            rule: "/etc/**".into(),
        };
        let got = render(build_view(&req).as_ref(), 120);
        assert_eq!(
            got,
            vec![
                "",
                "  Would you like to grant these permissions?",
                "",
                "  Environment: local",
                "",
                "  Reason: Read /etc/hosts",
                "",
                "  Permission rule: /etc/**",
                "",
                "› 1. Yes, grant these permissions for this turn (y)",
                "  2. Yes, grant these permissions for this session (a)",
                "  3. No, continue without permissions (d)",
                "",
                "  Press enter to confirm or esc to cancel",
            ]
        );
    }

    #[test]
    fn exec_keys_follow_the_offered_decisions() {
        dark();
        let mut v = build_view(&bash("cargo test *"));
        // `d`, `a`, `c` do nothing on this prompt (xf-03-approval-key-*)
        for c in ['d', 'a', 'c'] {
            assert_eq!(v.handle_key(key(KeyCode::Char(c))), ViewResult::Pending);
        }
        let ViewResult::CloseWith(AppAction::Decision(d)) = v.handle_key(key(KeyCode::Char('y')))
        else {
            panic!("y decides");
        };
        assert!(d.allow && !d.always && !d.cancel_turn);
        assert_eq!(
            d.cell,
            Some(DecisionCell::Approved("echo hello world".into()))
        );

        let mut v = build_view(&bash("cargo test *"));
        let ViewResult::CloseWith(AppAction::Decision(d)) = v.handle_key(key(KeyCode::Char('p')))
        else {
            panic!("p decides");
        };
        assert!(d.allow && d.always);
        assert_eq!(
            d.cell,
            Some(DecisionCell::ApprovedPrefix("cargo test".into()))
        );

        // Down Down Enter and Esc and `n` are the abort
        let mut v = build_view(&bash("cargo test *"));
        v.handle_key(key(KeyCode::Down));
        v.handle_key(key(KeyCode::Down));
        let ViewResult::CloseWith(AppAction::Decision(d)) = v.handle_key(key(KeyCode::Enter))
        else {
            panic!("enter decides");
        };
        assert!(!d.allow && d.cancel_turn);
        assert!(matches!(d.cell, Some(DecisionCell::Aborted(_))));
        for k in [KeyCode::Esc, KeyCode::Char('n')] {
            let mut v = build_view(&bash("cargo test *"));
            assert!(matches!(
                v.handle_key(key(k)),
                ViewResult::CloseWith(AppAction::Decision(Decision {
                    allow: false,
                    cancel_turn: true,
                    ..
                }))
            ));
        }
        // Up from the first row wraps to the last
        let mut v = build_view(&bash("cargo test *"));
        v.handle_key(key(KeyCode::Up));
        assert!(
            render(v.as_ref(), 100)
                .iter()
                .any(|r| r.starts_with("› 3."))
        );
    }

    #[test]
    fn needs_action_and_ctrl_a() {
        dark();
        let mut v = build_view(&bash(""));
        assert!(v.needs_action());
        let ViewResult::Action(AppAction::ShowStatic { title, lines }) =
            v.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL))
        else {
            panic!("ctrl+a opens the command");
        };
        assert_eq!(title, "E X E C");
        let t: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(t, "echo hello world");
    }

    #[test]
    fn patch_fullscreen_shows_the_diff() {
        dark();
        let req = PermissionRequest {
            id: "p2".into(),
            tool: "Edit".into(),
            kind: ToolKind::Edit,
            title: "src/main.rs".into(),
            input: json!({}),
            diff: Some(agent_core::FileDiff {
                path: "src/main.rs".into(),
                old: Some("a\n".into()),
                new: "b\n".into(),
            }),
            rule: String::new(),
        };
        let mut v = build_view(&req);
        let ViewResult::Action(AppAction::ShowStatic { title, lines }) =
            v.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL))
        else {
            panic!("ctrl+a opens the patch");
        };
        assert_eq!(title, "P A T C H");
        let t: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(t, "src/main.rs (+1 -1)");
    }

    // approvals-12-mcp-approval
    #[test]
    fn mcp_form_layout() {
        dark();
        let req = PermissionRequest {
            id: "m1".into(),
            tool: "mcp__fake__echo".into(),
            kind: ToolKind::Other,
            title: "fake:echo".into(),
            input: json!({"message": "hello from the model", "times": 2}),
            diff: None,
            rule: "mcp__fake".into(),
        };
        let got = render(build_view(&req).as_ref(), 120);
        assert_eq!(
            got,
            vec![
                "",
                "  Field 1/1",
                "  Allow the fake MCP server to run tool \"echo\"?",
                "",
                "  message: hello from the model",
                "  times: 2",
                "",
                "  › 1. Allow                   Run the tool and continue.",
                "    2. Allow for this session  Run the tool and remember this choice for this session.",
                "    3. Cancel                  Cancel this tool call",
                "  enter to submit | esc to cancel",
                "",
            ]
        );
    }

    #[test]
    fn mcp_form_wraps_descriptions_at_the_column_at_80() {
        dark();
        let req = PermissionRequest {
            id: "m1".into(),
            tool: "mcp__fake__echo".into(),
            kind: ToolKind::Other,
            title: String::new(),
            input: json!({}),
            diff: None,
            rule: "mcp__fake".into(),
        };
        let got = render(build_view(&req).as_ref(), 80);
        assert!(
            got.contains(
                &"    2. Allow for this session  Run the tool and remember this choice for this"
                    .to_string()
            ),
            "{got:?}"
        );
        assert!(
            got.contains(&format!("{}session.", " ".repeat(31))),
            "{got:?}"
        );
        // no parameters: the prompt is followed straight by the options
        assert_eq!(got[2], "  Allow the fake MCP server to run tool \"echo\"?");
        assert!(got[3].starts_with("  › 1. Allow"));
    }

    #[test]
    fn mcp_cancel_and_session() {
        dark();
        let req = PermissionRequest {
            id: "m1".into(),
            tool: "mcp__fake__echo".into(),
            rule: "mcp__fake".into(),
            ..Default::default()
        };
        let mut v = build_view(&req);
        assert!(matches!(
            v.handle_key(key(KeyCode::Esc)),
            ViewResult::CloseWith(AppAction::Decision(Decision {
                allow: false,
                cancel_turn: false,
                ..
            }))
        ));
        let mut v = build_view(&req);
        v.handle_key(key(KeyCode::Down));
        assert!(matches!(
            v.handle_key(key(KeyCode::Enter)),
            ViewResult::CloseWith(AppAction::Decision(Decision {
                allow: true,
                always: true,
                ..
            }))
        ));
    }

    fn ask() -> PermissionRequest {
        PermissionRequest {
            id: "ask-1".into(),
            tool: "AskUserQuestion".into(),
            input: json!({"questions": [{"question": "Which approach should I take for the overflow fix?",
                "header": "Approach", "multiSelect": false,
                "options": [
                    {"label": "wrapping_add (Recommended)", "description": "Wrap on overflow, no panic."},
                    {"label": "checked_add", "description": "Return an Option and let the caller decide."},
                    {"label": "saturating_add", "description": "Clamp at the type bounds."}]}]}),
            ..Default::default()
        }
    }

    // approvals-15-user-input
    #[test]
    fn user_input_layout() {
        dark();
        let got = render(build_view(&ask()).as_ref(), 120);
        assert_eq!(
            got,
            vec![
                "",
                "  Question 1/1 (1 unanswered)",
                "  Which approach should I take for the overflow fix?",
                "",
                "  › 1. wrapping_add (Recommended)  Wrap on overflow, no panic.",
                "    2. checked_add                 Return an Option and let the caller decide.",
                "    3. saturating_add              Clamp at the type bounds.",
                "    4. None of the above           Optionally, add details in notes (tab).",
                "",
                "  tab to add notes | enter to submit answer | esc to interrupt",
                "",
            ]
        );
    }

    #[test]
    fn user_input_answers_and_notes() {
        dark();
        let mut v = build_view(&ask());
        v.handle_key(key(KeyCode::Down));
        v.handle_key(key(KeyCode::Tab));
        for c in "my note".chars() {
            v.handle_key(key(KeyCode::Char(c)));
        }
        let shown = render(v.as_ref(), 120);
        assert!(shown.contains(&"  › my note".to_string()), "{shown:?}");
        assert!(
            shown.contains(&"  tab or esc to clear notes | enter to submit answer".to_string())
        );
        let ViewResult::CloseWith(AppAction::Decision(d)) = v.handle_key(key(KeyCode::Enter))
        else {
            panic!("enter submits");
        };
        assert_eq!(
            d.answers,
            Some(vec![(
                "Which approach should I take for the overflow fix?".to_string(),
                "checked_add (my note)".to_string()
            )])
        );
        let Some(DecisionCell::Questions(q)) = d.cell else {
            panic!()
        };
        assert_eq!(q[0].options, vec!["checked_add"]);
        assert_eq!(q[0].note.as_deref(), Some("my note"));
    }

    #[test]
    fn user_input_esc_interrupts_and_notes_esc_clears() {
        dark();
        let mut v = build_view(&ask());
        v.handle_key(key(KeyCode::Tab));
        assert_eq!(v.handle_key(key(KeyCode::Esc)), ViewResult::Pending);
        assert!(matches!(
            v.handle_key(key(KeyCode::Esc)),
            ViewResult::CloseWith(AppAction::Decision(Decision {
                allow: false,
                cancel_turn: true,
                cell: None,
                ..
            }))
        ));
        // digit 3 picks and submits
        let mut v = build_view(&ask());
        let ViewResult::CloseWith(AppAction::Decision(d)) = v.handle_key(key(KeyCode::Char('3')))
        else {
            panic!()
        };
        assert_eq!(d.answers.unwrap()[0].1, "saturating_add");
        // None of the above
        let mut v = build_view(&ask());
        for _ in 0..3 {
            v.handle_key(key(KeyCode::Down));
        }
        let ViewResult::CloseWith(AppAction::Decision(d)) = v.handle_key(key(KeyCode::Enter))
        else {
            panic!()
        };
        assert_eq!(d.answers.unwrap()[0].1, "None of the above");
    }

    // approvals-17-user-input-answered
    #[test]
    fn questions_cell_layout() {
        dark();
        let cell = DecisionCell::Questions(vec![QuestionResult {
            question: "Which approach should I take for the overflow fix?".into(),
            options: vec!["checked_add".into()],
            note: Some("my note".into()),
        }]);
        let rows: Vec<String> = cell
            .display_lines(120)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(
            rows,
            vec![
                "• Questions 1/1 answered",
                "  • Which approach should I take for the overflow fix?",
                "    answer: checked_add",
                "    note: my note",
            ]
        );
    }

    fn plain(cell: &DecisionCell, w: u16) -> Vec<String> {
        cell.display_lines(w)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    // exec_approval_history_decision_* snapshots
    #[test]
    fn decision_cells_match_the_snapshots() {
        dark();
        assert_eq!(
            plain(&DecisionCell::Approved("echo hello world".into()), 80),
            vec!["✔ You approved codex to run echo hello world this time"]
        );
        assert_eq!(
            plain(&DecisionCell::Aborted("echo line1\necho line2".into()), 80),
            vec!["✗ You canceled the request to run echo line1 ..."]
        );
        let long = format!("echo {}", "a".repeat(200));
        let rows = plain(&DecisionCell::Aborted(long), 80);
        assert!(
            rows[0].starts_with("✗ You canceled the request to run echo"),
            "{rows:?}"
        );
        assert!(rows.last().unwrap().ends_with("..."), "{rows:?}");
        assert_eq!(
            plain(
                &DecisionCell::ApprovedPrefix("touch approved.txt".into()),
                120
            ),
            vec!["✔ You approved codex to always run commands that start with touch approved.txt"]
        );
        assert_eq!(
            plain(
                &DecisionCell::Plain("You granted additional permissions".into()),
                120
            ),
            vec!["You granted additional permissions"]
        );
    }

    #[test]
    fn decision_cell_runs_are_styled_like_the_capture() {
        dark();
        let l = &DecisionCell::Approved("touch a".into()).display_lines(120)[0];
        let st = |i: usize| (l.spans[i].content.to_string(), l.spans[i].style);
        assert_eq!(st(0).0, "✔ ");
        assert_eq!(l.spans[0].style.fg, Some(Color::Green));
        assert!(!l.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert!(l.spans[2].style.add_modifier.contains(Modifier::BOLD));
        assert!(l.spans[4].style.add_modifier.contains(Modifier::DIM));
        assert!(l.spans[5].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn bash_tokens_get_the_codex_colours() {
        dark();
        let spans = highlight_bash("curl -sS https://example.com && echo 'x y'");
        let col = |s: &str| {
            spans
                .iter()
                .find(|sp| sp.content.contains(s))
                .and_then(|sp| sp.style.fg)
        };
        assert_eq!(col("curl"), Some(Color::Rgb(137, 180, 250)));
        assert_eq!(col("sS"), Some(Color::Rgb(235, 160, 172)));
        assert_eq!(col("&&"), Some(Color::Rgb(148, 226, 213)));
        assert_eq!(col("'x y'"), Some(Color::Rgb(166, 227, 161)));
        let echo = spans.iter().find(|s| s.content == "echo").unwrap();
        assert_eq!(echo.style.fg, Some(Color::Rgb(137, 180, 250)));
    }
}
