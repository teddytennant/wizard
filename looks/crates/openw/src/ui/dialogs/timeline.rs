// OWNER: dialogs
//! Dialogs about messages: `Timeline`, `Fork session`, `Message Actions` and `Subagent Actions`.

use agent_core::Request;
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use tuikit::dialog::{SelectDialog, LARGE};
use tuikit::select::SelectItem;
use tuikit::theme::Variant;
use tuikit::Theme;

use super::clock;
use super::list::{ListDialog, ListEvent};
use super::{Ctx, Dialog, Effect, Nav, Outcome, UserMsg};

fn one_line(t: &str) -> String {
    t.replace('\n', " ")
}

// ---- timeline ----------------------------------------------------------------------------

pub struct TimelineDialog {
    list: ListDialog<usize>,
    users: Vec<UserMsg>,
    ctx: Ctx,
}

fn rows(users: &[UserMsg], off: i64) -> Vec<SelectItem<usize>> {
    users
        .iter()
        .rev()
        .map(|u| SelectItem::new(u.index, one_line(&u.text)).hint(clock::time(u.at, off)))
        .collect()
}

pub fn open(ctx: &Ctx) -> Box<dyn Dialog> {
    Box::new(TimelineDialog {
        list: ListDialog::new(
            SelectDialog::new("Timeline", rows(&ctx.users, ctx.utc_offset)).width(LARGE),
        ),
        users: ctx.users.clone(),
        ctx: ctx.clone(),
    })
}

impl TimelineDialog {
    fn pick(&mut self, ev: ListEvent) -> Outcome {
        match ev {
            ListEvent::Cancel => Outcome::close(),
            ListEvent::Moved | ListEvent::Filtered => match self.list.selected_index() {
                Some(i) => Outcome::stay().with(Effect::JumpToMessage(self.list.items()[i].value)),
                None => Outcome::stay(),
            },
            ListEvent::Select(i) => {
                let idx = self.list.items()[i].value;
                match self.users.iter().find(|u| u.index == idx) {
                    Some(u) => Outcome {
                        nav: Nav::Replace(message_actions(&self.ctx, u.clone())),
                        effects: Vec::new(),
                    },
                    None => Outcome::stay(),
                }
            }
            _ => Outcome::stay(),
        }
    }
}

impl Dialog for TimelineDialog {
    fn title(&self) -> &str {
        "Timeline"
    }
    fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        let ev = self.list.handle_key(key);
        self.pick(ev)
    }
    fn handle_paste(&mut self, text: &str) -> Outcome {
        let ev = self.list.handle_paste(text);
        self.pick(ev)
    }
    fn handle_mouse(&mut self, ev: MouseEvent) -> Outcome {
        let ev = self.list.handle_mouse(ev);
        self.pick(ev)
    }
    fn draw(&mut self, buf: &mut Buffer, screen: Rect, theme: &Theme) -> Option<(u16, u16)> {
        self.list.draw(buf, screen, theme)
    }
}

// ---- fork --------------------------------------------------------------------------------

pub struct ForkDialog {
    list: ListDialog<Option<usize>>,
}

/// `Full session`, then every user message newest first. wizard has no fork, so picking one
/// says so instead of pretending.
pub fn fork(ctx: &Ctx) -> Box<dyn Dialog> {
    let mut items = vec![SelectItem::new(None, "Full session")];
    items.extend(ctx.users.iter().rev().map(|u| {
        SelectItem::new(Some(u.index), one_line(&u.text)).hint(clock::time(u.at, ctx.utc_offset))
    }));
    Box::new(ForkDialog {
        list: ListDialog::new(SelectDialog::new("Fork session", items).width(LARGE)),
    })
}

impl ForkDialog {
    fn pick(&mut self, ev: ListEvent) -> Outcome {
        match ev {
            ListEvent::Cancel => Outcome::close(),
            ListEvent::Moved | ListEvent::Filtered => match self.list.selected_index() {
                Some(i) => match self.list.items()[i].value {
                    Some(m) => Outcome::stay().with(Effect::JumpToMessage(m)),
                    None => Outcome::stay(),
                },
                None => Outcome::stay(),
            },
            ListEvent::Select(_) => Outcome::close_with(vec![Effect::Toast(
                Variant::Warning,
                "wizard cannot fork a session".into(),
            )]),
            _ => Outcome::stay(),
        }
    }
}

impl Dialog for ForkDialog {
    fn title(&self) -> &str {
        "Fork session"
    }
    fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        let ev = self.list.handle_key(key);
        self.pick(ev)
    }
    fn handle_paste(&mut self, text: &str) -> Outcome {
        let ev = self.list.handle_paste(text);
        self.pick(ev)
    }
    fn handle_mouse(&mut self, ev: MouseEvent) -> Outcome {
        let ev = self.list.handle_mouse(ev);
        self.pick(ev)
    }
    fn draw(&mut self, buf: &mut Buffer, screen: Rect, theme: &Theme) -> Option<(u16, u16)> {
        self.list.draw(buf, screen, theme)
    }
}

// ---- message actions ---------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MsgAct {
    Revert,
    Copy,
    Fork,
}

pub struct MessageDialog {
    list: ListDialog<MsgAct>,
    msg: UserMsg,
}

pub fn message_actions(_ctx: &Ctx, msg: UserMsg) -> Box<dyn Dialog> {
    let items = vec![
        SelectItem::new(MsgAct::Revert, "Revert").description("undo messages and file changes"),
        SelectItem::new(MsgAct::Copy, "Copy").description("message text to clipboard"),
        SelectItem::new(MsgAct::Fork, "Fork").description("create a new session"),
    ];
    Box::new(MessageDialog {
        list: ListDialog::new(SelectDialog::new("Message Actions", items)),
        msg,
    })
}

impl MessageDialog {
    fn pick(&mut self, ev: ListEvent) -> Outcome {
        match ev {
            ListEvent::Cancel => Outcome::close(),
            ListEvent::Select(i) => match self.list.items()[i].value {
                // `/rewind N` restores the files and history to before turn N.
                MsgAct::Revert => Outcome::close_with(vec![
                    Effect::Request(Request::Prompt(format!("/rewind {}", self.msg.turn))),
                    Effect::RestorePrompt(self.msg.text.clone()),
                ]),
                MsgAct::Copy => Outcome::close_with(vec![
                    Effect::Copy(self.msg.text.clone()),
                    Effect::Toast(Variant::Success, "Message copied to clipboard!".into()),
                ]),
                MsgAct::Fork => Outcome::close_with(vec![Effect::Toast(
                    Variant::Warning,
                    "wizard cannot fork a session".into(),
                )]),
            },
            _ => Outcome::stay(),
        }
    }
}

impl Dialog for MessageDialog {
    fn title(&self) -> &str {
        "Message Actions"
    }
    fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        let ev = self.list.handle_key(key);
        self.pick(ev)
    }
    fn handle_paste(&mut self, text: &str) -> Outcome {
        let ev = self.list.handle_paste(text);
        self.pick(ev)
    }
    fn handle_mouse(&mut self, ev: MouseEvent) -> Outcome {
        let ev = self.list.handle_mouse(ev);
        self.pick(ev)
    }
    fn draw(&mut self, buf: &mut Buffer, screen: Rect, theme: &Theme) -> Option<(u16, u16)> {
        self.list.draw(buf, screen, theme)
    }
}

// ---- subagent ----------------------------------------------------------------------------

pub struct SubagentDialog {
    list: ListDialog<()>,
}

/// One row, `Open`. wizard's subagents run inside the parent session, so there is no session to
/// navigate to; picking the row says so.
pub fn subagent() -> Box<dyn Dialog> {
    let items = vec![SelectItem::new((), "Open").description("the subagent's session")];
    Box::new(SubagentDialog {
        list: ListDialog::new(SelectDialog::new("Subagent Actions", items)),
    })
}

impl Dialog for SubagentDialog {
    fn title(&self) -> &str {
        "Subagent Actions"
    }
    fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        match self.list.handle_key(key) {
            ListEvent::Cancel => Outcome::close(),
            ListEvent::Select(_) => Outcome::close_with(vec![Effect::Toast(
                Variant::Info,
                "wizard runs subagents inside this session".into(),
            )]),
            _ => Outcome::stay(),
        }
    }
    fn handle_paste(&mut self, text: &str) -> Outcome {
        self.list.handle_paste(text);
        Outcome::stay()
    }
    fn draw(&mut self, buf: &mut Buffer, screen: Rect, theme: &Theme) -> Option<(u16, u16)> {
        self.list.draw(buf, screen, theme)
    }
}
