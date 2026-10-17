// OWNER: bottom-pane
//! The bottom pane: status row, queued input preview, composer band, footer, and the modal view
//! that replaces them (spec C.1.1).

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};

use super::composer::{Composer, ComposerEvent, FooterCtx};
use super::key_hint::{alt, shift};
use super::status_indicator::{StatusIndicator, pending_preview_lines};
use super::{BottomView, ViewResult};
use crate::style::process_elapsed;

#[derive(Debug)]
pub struct BottomPane {
    pub composer: Composer,
    pub status: Option<StatusIndicator>,
    /// The status row is hidden while answer text is being committed (spec B.16.1).
    pub status_hidden: bool,
    pub view: Option<Box<dyn BottomView>>,
    /// Steers sent mid-turn, shown as `Messages to be submitted after next tool call`.
    pub steers: Vec<String>,
    /// Tab-queued drafts, sent when the turn ends.
    pub queued: Vec<String>,
    pub footer: FooterCtx,
}

impl BottomPane {
    pub fn new(placeholder: &'static str) -> Self {
        Self {
            composer: Composer::new(placeholder),
            status: None,
            status_hidden: false,
            view: None,
            steers: Vec::new(),
            queued: Vec::new(),
            footer: FooterCtx::default(),
        }
    }

    pub fn set_task_running(&mut self, running: bool) {
        self.footer.task_running = running;
        match (running, self.status.is_some()) {
            (true, false) => self.status = Some(StatusIndicator::new()),
            (false, true) => self.status = None,
            _ => {}
        }
    }

    pub fn visible_status(&self) -> Option<&StatusIndicator> {
        self.status.as_ref().filter(|_| !self.status_hidden)
    }

    /// Height without the one-row inset the chat widget adds above the pane.
    pub fn desired_height(&self, width: u16) -> u16 {
        if let Some(v) = &self.view {
            return v.desired_height(width);
        }
        let mut h = 0;
        let status = self.visible_status();
        if let Some(s) = status {
            h += s.desired_height();
        }
        let preview = pending_preview_lines(&self.steers, &self.queued, width).len() as u16;
        if status.is_some() || preview > 0 {
            h += 1;
        }
        h += preview;
        h + self.composer.desired_height(width, &self.footer)
    }

    pub fn render(&mut self, area: Rect, buf: &mut Buffer) -> Option<Position> {
        if let Some(v) = &self.view {
            v.render(area, buf);
            return v.cursor(area);
        }
        let mut y = area.y;
        let has_status = self.visible_status().is_some();
        if let Some(s) = self.visible_status() {
            for l in s.lines(area.width, process_elapsed()) {
                tuikit::paint::put_line(buf, area.x, y, &l, area);
                y += 1;
            }
        }
        let preview = pending_preview_lines(&self.steers, &self.queued, area.width);
        if has_status || !preview.is_empty() {
            y += 1;
        }
        for l in &preview {
            tuikit::paint::put_line(buf, area.x, y, l, area);
            y += 1;
        }
        let rest = Rect::new(area.x, y, area.width, area.bottom().saturating_sub(y));
        self.composer.render(rest, buf, &self.footer)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PaneEvent {
        if let Some(v) = &mut self.view {
            return match v.handle_key(key) {
                ViewResult::Pending => PaneEvent::None,
                ViewResult::Close => {
                    self.view = None;
                    if let Some(s) = &mut self.status {
                        s.resume();
                    }
                    PaneEvent::None
                }
                ViewResult::CloseWith(a) => {
                    self.view = None;
                    if let Some(s) = &mut self.status {
                        s.resume();
                    }
                    PaneEvent::Action(a)
                }
                ViewResult::Action(a) => PaneEvent::Action(a),
            };
        }
        // Alt+Up and Shift+Left take the newest queued message back into the composer, but
        // never over a draft: the queue keeps it.
        if !self.queued.is_empty()
            && self.composer.is_empty()
            && (alt(KeyCode::Up).is_press(key) || shift(KeyCode::Left).is_press(key))
        {
            if let Some(last) = self.queued.pop() {
                self.composer.set_text(&last);
            }
            return PaneEvent::None;
        }
        match self.composer.handle_key(key, self.footer.task_running) {
            ComposerEvent::None | ComposerEvent::Cleared => PaneEvent::None,
            ComposerEvent::Submit(t) => PaneEvent::Submit(t),
            ComposerEvent::Queue(t) => PaneEvent::Queue(t),
            ComposerEvent::Command(t) => PaneEvent::Command(t),
            ComposerEvent::Shell(c) => PaneEvent::Shell(c),
            ComposerEvent::Info(t) => PaneEvent::Info(t),
            ComposerEvent::Error(t) => PaneEvent::Error(t),
        }
    }

    pub fn push_view(&mut self, v: Box<dyn BottomView>) {
        if let Some(s) = &mut self.status {
            s.pause();
        }
        self.view = Some(v);
    }
}

#[derive(Debug, PartialEq)]
pub enum PaneEvent {
    None,
    Submit(String),
    Queue(String),
    /// `/name args` from the popup or typed bare; the draft is already cleared.
    Command(String),
    /// `!cmd`: run it in the shell now.
    Shell(String),
    /// Print an info cell (the draft stays in the composer).
    Info(String),
    /// Print an error cell (the draft stays in the composer).
    Error(String),
    Action(super::AppAction),
}
