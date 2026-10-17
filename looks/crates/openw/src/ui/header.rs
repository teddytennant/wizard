// OWNER: sidebar (session header)
//! opencode has no header row in a session. What it has in its place is the subagent bar: when
//! you are inside a child session the prompt is swapped for a three row bar with the agent
//! name, `(i of n)` and the `Parent` / `Prev` / `Next` shortcuts (`routes/session/subagent-footer.tsx`).
//!
//! wizard cannot produce child sessions over ACP, so a "child session" here is a `task` tool
//! call of the parent session: its nested calls (`parent_id` pointing at it) become the child's
//! transcript. Only the mock backend's `sub` scenario produces those today.

use std::time::Instant;

use agent_core::transcript::{Message, Part, Role};
use agent_core::ToolCall;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use tuikit::paint::{fill, put_str};
use tuikit::width::display_width;
use tuikit::Theme;

use super::session;
use super::tools::{classify, titlecase, Tool};
use crate::app::App;
use crate::keys::Action;

/// Rows of the bar: padding, content, padding.
pub const BAR_HEIGHT: u16 = 3;

/// One child session: a `task` call and what it did.
#[derive(Clone, Debug)]
pub struct Child {
    pub label: String,
    pub prompt: String,
    pub calls: Vec<ToolCall>,
    pub output: Option<String>,
}

fn str_in<'a>(c: &'a ToolCall, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| c.input.get(*k).and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())
}

/// Every `task` call of the session, oldest first.
pub fn children(app: &App) -> Vec<Child> {
    let all: Vec<&ToolCall> = app
        .transcript
        .messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            Part::Tool(c) => Some(c),
            _ => None,
        })
        .collect();
    all.iter()
        .filter(|c| c.parent_id.is_none() && classify(c) == Tool::Task)
        .map(|c| Child {
            label: str_in(c, &["subagent_type", "agent"])
                .map(titlecase)
                .unwrap_or_else(|| "Subagent".into()),
            prompt: str_in(c, &["prompt", "description"])
                .unwrap_or(&c.title)
                .to_string(),
            calls: all
                .iter()
                .filter(|k| k.parent_id.as_deref() == Some(c.id.as_str()))
                .map(|k| ToolCall {
                    parent_id: None,
                    ..(*k).clone()
                })
                .collect(),
            output: c.output.clone().filter(|o| !o.is_empty()),
        })
        .collect()
}

/// What the bar shows.
#[derive(Debug, PartialEq, Eq)]
pub struct BarInfo {
    pub label: String,
    pub index: usize,
    pub total: usize,
}

pub fn bar_info(app: &App) -> Option<BarInfo> {
    let list = children(app);
    let i = app.child?;
    let c = list.get(i)?;
    Some(BarInfo {
        label: c.label.clone(),
        index: i + 1,
        total: list.len(),
    })
}

// ---- navigation ------------------------------------------------------------------------

/// `ctrl+x down`: go to the first child session, if there is one.
pub fn enter_first(app: &mut App) {
    if app.child.is_none() && !children(app).is_empty() {
        app.child = Some(0);
        app.scroll.to_bottom();
    }
}

/// `up`: back to the parent session.
pub fn leave(app: &mut App) {
    if app.child.take().is_some() {
        app.scroll.to_bottom();
    }
}

/// `left` / `right`: previous and next child, wrapping.
pub fn step(app: &mut App, dir: isize) {
    let n = children(app).len() as isize;
    if let (Some(i), true) = (app.child, n > 0) {
        app.child = Some((i as isize + dir).rem_euclid(n) as usize);
        app.scroll.to_bottom();
    }
}

// ---- drawing ---------------------------------------------------------------------------

/// The child session as messages: the prompt it was given, then what it ran and said.
fn child_messages(c: &Child) -> Vec<Message> {
    let now = Instant::now();
    let msg = |role, parts| Message {
        role,
        parts,
        started: now,
        took: None,
        stop: None,
        model: String::new(),
    };
    let mut parts: Vec<Part> = c.calls.iter().cloned().map(Part::Tool).collect();
    if let Some(o) = &c.output {
        parts.push(Part::Text(o.clone()));
    }
    vec![
        msg(Role::User, vec![Part::Text(c.prompt.clone())]),
        msg(Role::Assistant, parts),
    ]
}

/// The session screen; inside a child session the transcript is the child's and the bar covers
/// the prompt.
pub fn draw_session(buf: &mut Buffer, app: &mut App) -> Option<(u16, u16)> {
    let Some(child) = app.child.and_then(|i| children(app).into_iter().nth(i)) else {
        app.child = None;
        return session::draw(buf, app);
    };
    let info = bar_info(app)?;
    let mut msgs = child_messages(&child);
    std::mem::swap(&mut app.transcript.messages, &mut msgs);
    let busy = std::mem::replace(&mut app.transcript.busy, false);
    let g = session::geometry(app);
    session::draw(buf, app);
    app.transcript.busy = busy;
    std::mem::swap(&mut app.transcript.messages, &mut msgs);

    // The prompt box and hint row belong to the parent; wipe them and put the bar at the
    // bottom with the one padding row below it.
    let h = app.size.1;
    let area = Rect::new(g.col_x, g.prompt.y, g.col_w, h.saturating_sub(g.prompt.y));
    fill(buf, area, Style::new().bg(app.theme.background));
    let y = h.saturating_sub(1 + BAR_HEIGHT);
    draw_bar(buf, Rect::new(g.col_x, y, g.col_w, BAR_HEIGHT), &info, app);
    None
}

fn draw_bar(buf: &mut Buffer, r: Rect, info: &BarInfo, app: &App) {
    let t: &Theme = &app.theme;
    let panel = Style::new().bg(t.background_panel).fg(t.text);
    fill(buf, r, panel);
    for y in r.y..r.bottom() {
        put_str(
            buf,
            r.x,
            y,
            "┃",
            Style::new().fg(t.border).bg(t.background_panel),
            r,
        );
    }
    let y = r.y + 1;
    let row = Rect::new(r.x, y, r.width, 1);
    let bold = panel.add_modifier(Modifier::BOLD);
    let muted = Style::new().fg(t.text_muted).bg(t.background_panel);
    let mut x = r.x + 3;
    x = put_str(buf, x, y, &info.label, bold, row);
    put_str(
        buf,
        x + 1,
        y,
        &format!("({} of {})", info.index, info.total),
        muted,
        row,
    );

    let keys = |a: &Action| app.keymap.first(a);
    let items = [
        ("Parent", keys(&Action::ChildParent)),
        ("Prev", keys(&Action::ChildPrev)),
        ("Next", keys(&Action::ChildNext)),
    ];
    let width: usize = items
        .iter()
        .map(|(l, k)| display_width(l) + 1 + display_width(k))
        .sum::<usize>()
        + 2 * (items.len() - 1);
    // padding right 1
    let mut x = (r.right() as usize)
        .saturating_sub(1 + width)
        .max(r.x as usize + 3) as u16;
    for (i, (label, key)) in items.iter().enumerate() {
        if i > 0 {
            x += 2;
        }
        x = put_str(buf, x, y, label, panel, row);
        x = put_str(buf, x, y, &format!(" {key}"), muted, row);
    }
}
