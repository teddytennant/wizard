// OWNER: tools
//! The glue between the app and the tool-side screens: the diff viewer, the rewind panel, the
//! permission panel's answers and the subagent focus key. Kept out of `app.rs` so that file
//! only calls in.

use std::time::Instant;

use agent_core::{Request, RewindPreview};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};

use crate::app::{App, External, Focus};
use crate::ui::permission::{PermOut, PermPanel};
use crate::ui::row::Cx;
use crate::ui::tools::diffview::{self, DiffView, DvOut};
use crate::ui::tools::rewind::{RewindPanel, RwOut};
use crate::ui::transcript::Kind as BKind;

impl App {
    fn flash_msg(&mut self, msg: &str, now: Instant) {
        self.set_flash(msg, false, now);
    }

    /// Keys for whatever tool-side screen is up. True when the key was theirs.
    pub fn tools_key(&mut self, key: KeyEvent, now: Instant) -> bool {
        if let Some(v) = self.diffview.as_mut() {
            if v.on_key(key) == DvOut::Close {
                self.diffview = None;
            }
            return true;
        }
        if let Some(r) = self.rewind.as_mut() {
            // ctrl+c still backs out.
            let out = if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
                RwOut::Cancel
            } else {
                r.on_key(key)
            };
            match out {
                RwOut::None => {}
                RwOut::Cancel => self.rewind = None,
                RwOut::Do {
                    conversation,
                    files,
                    edit,
                } => {
                    let Some(r) = self.rewind.take() else {
                        return true;
                    };
                    if conversation || files {
                        let _ = self.tx.send(Request::Rewind {
                            turn: r.turn,
                            conversation,
                            files,
                        });
                    }
                    if edit {
                        self.composer.set_text(&r.text);
                        self.focus = Focus::Composer;
                        self.view.cursor = None;
                    }
                    self.flash_msg(
                        match (conversation, files, edit) {
                            (true, true, _) => "rewinding conversation and files",
                            (true, false, _) => "rewinding the conversation",
                            (false, true, _) => "putting the files back",
                            _ => "message back in the composer, nothing undone",
                        },
                        now,
                    );
                }
            }
            return true;
        }
        false
    }

    pub fn tools_mouse(&mut self, m: MouseEvent) -> bool {
        match self.diffview.as_mut() {
            Some(v) => {
                v.on_mouse(m);
                true
            }
            None => false,
        }
    }

    /// What the permission panel decided.
    pub fn perm_out(&mut self, out: PermOut, now: Instant) {
        match out {
            PermOut::None | PermOut::Locked => {}
            PermOut::Decide { allow, scope, note } => {
                if let Some(p) = self.perm.take() {
                    self.settle(&p.req.tool, &p.req.input, allow, now);
                    let _ = self.tx.send(Request::Decide {
                        id: p.req.id,
                        allow,
                        scope,
                        note,
                    });
                }
                self.next_perm(now);
            }
            PermOut::Answer { answers } => {
                if let Some(p) = self.perm.take() {
                    self.settle(&p.req.tool, &p.req.input, true, now);
                    let _ = self.tx.send(Request::Answer {
                        id: p.req.id,
                        answers,
                    });
                }
                self.next_perm(now);
            }
            PermOut::OpenDiff => {
                if let Some(p) = &self.perm {
                    if let Some(d) = p.diff() {
                        let mut v = DiffView::pending(d, &p.title());
                        v.cwd = self.cwd_str();
                        self.diffview = Some(v);
                    }
                }
            }
            PermOut::View(text) => self.external = Some(External::Pager(text)),
            PermOut::Nudge => self.flash_msg("press y or n", now),
        }
    }

    /// Find the running card a permission request is about: the newest running call of that
    /// tool with the same input.
    fn card_for(
        &mut self,
        tool: &str,
        input: &serde_json::Value,
    ) -> Option<(&mut crate::ui::tools::ToolEntry, &mut u64)> {
        let blocks = self.tr.blocks.iter_mut().rev().take(30);
        for b in blocks {
            let BKind::Tools(es) = &mut b.kind else {
                continue;
            };
            let hit = es.iter_mut().rev().find(|e| {
                e.running()
                    && e.call.name == tool
                    && (e.call.input == *input || e.call.input.is_null())
            });
            if let Some(e) = hit {
                return Some((e, &mut b.ver));
            }
        }
        None
    }

    /// While a request waits for an answer its card says so instead of counting seconds.
    pub fn mark_awaiting(&mut self, tool: &str, input: &serde_json::Value) {
        if let Some((e, ver)) = self.card_for(tool, input) {
            e.awaiting = true;
            *ver += 1;
            self.tr.rev += 1;
        }
    }

    /// The request was answered. A call's clock starts when it may run, not when it was
    /// announced: the wait for an answer is yours, not the tool's.
    fn settle(&mut self, tool: &str, input: &serde_json::Value, allowed: bool, now: Instant) {
        if let Some((e, ver)) = self.card_for(tool, input) {
            e.awaiting = false;
            if allowed {
                e.started = Some(now);
            }
            *ver += 1;
            self.tr.rev += 1;
        }
    }

    /// The next request that piled up behind the one just answered.
    fn next_perm(&mut self, now: Instant) {
        if let Some(r) = self.perm_queue.pop_front() {
            self.perm = Some(
                PermPanel::new(r, now)
                    .with_cwd(&self.cwd_str())
                    .lock_for_typing(self.last_typed, now),
            );
        }
    }

    /// `d` in nav mode and `/diff`: the viewer over everything changed so far, opened on the
    /// edit under the cursor (or the first edit of the cursor's turn).
    pub fn open_diff(&mut self, cursor: Option<usize>) {
        let files = diffview::collect(
            self.tr
                .blocks
                .iter()
                .filter_map(|b| match &b.kind {
                    BKind::Tools(e) => Some(e.iter()),
                    _ => None,
                })
                .flatten(),
        );
        let target = cursor.and_then(|c| self.diff_target(c));
        let mut v = DiffView::new("Changes this session", files);
        v.cwd = self.cwd_str();
        if let Some(id) = target {
            if let Some((f, h)) = v.locate(&id) {
                v.select(f, h);
            }
        }
        self.diffview = Some(v);
    }

    /// The call whose edit `d` should land on: the cursor block's own, else the first one in
    /// the same turn.
    fn diff_target(&self, cursor: usize) -> Option<String> {
        let has_diff = |b: &crate::ui::transcript::Block| -> Option<String> {
            match &b.kind {
                BKind::Tools(es) => es
                    .iter()
                    .flat_map(|e| std::iter::once(e).chain(e.children.iter()))
                    .find(|e| e.done() && e.call.diff.is_some())
                    .map(|e| e.call.id.clone()),
                _ => None,
            }
        };
        let b = self.tr.blocks.get(cursor)?;
        if let Some(id) = has_diff(b) {
            return Some(id);
        }
        self.tr.blocks[cursor..]
            .iter()
            .skip(1)
            .take_while(|b| !b.is_user())
            .find_map(has_diff)
    }

    /// `r` in nav mode: offer to rewind to the user message at (or above) the cursor.
    pub fn start_rewind(&mut self, now: Instant) {
        let Some(c) = self.view.cursor else { return };
        let Some(ub) = (0..=c.min(self.tr.blocks.len().saturating_sub(1)))
            .rev()
            .find(|&i| self.tr.blocks.get(i).is_some_and(|b| b.is_user()))
        else {
            return;
        };
        let BKind::User { text, .. } = &self.tr.blocks[ub].kind else {
            return;
        };
        let text = text.clone();
        if self.busy() {
            // Nothing can be undone under a running turn; the message is still yours to edit.
            self.composer.set_text(&text);
            self.focus = Focus::Composer;
            self.view.cursor = None;
            self.flash_msg(
                "turn running: message copied to the composer, nothing undone",
                now,
            );
            return;
        }
        let turn = self.tr.blocks[..ub].iter().filter(|b| b.is_user()).count();
        self.rewind = Some(RewindPanel::new(turn, text, now).with_cwd(&self.cwd_str()));
        let _ = self.tx.send(Request::RewindPreview { turn });
    }

    pub fn on_rewind_preview(&mut self, p: RewindPreview) {
        if let Some(r) = self.rewind.as_mut() {
            if r.turn == p.turn {
                r.set_preview(p);
            }
        }
    }

    /// Nav `a`: jump to the next subagent card and open it with every call listed; the one
    /// focused before folds again.
    pub fn focus_agent(&mut self, now: Instant) {
        let is_agent = |b: &crate::ui::transcript::Block| matches!(&b.kind, BKind::Tools(e) if e.iter().any(|x| x.verb() == "Task"));
        let n = self.tr.blocks.len();
        if n == 0 {
            return;
        }
        let from = self.view.cursor.map_or(0, |c| c + 1);
        let Some(target) = (0..n)
            .map(|i| (from + i) % n)
            .find(|&i| is_agent(&self.tr.blocks[i]))
        else {
            self.flash_msg("no subagents in this session", now);
            return;
        };
        for (i, b) in self.tr.blocks.iter_mut().enumerate() {
            if let BKind::Tools(es) = &mut b.kind {
                let mut changed = false;
                for e in es.iter_mut().filter(|e| e.verb() == "Task") {
                    let want = i == target;
                    if e.focus != want {
                        e.focus = want;
                        changed = true;
                    }
                }
                if i == target {
                    b.open = Some(true);
                    changed = true;
                } else if changed {
                    b.open = None;
                }
                if changed {
                    b.ver += 1;
                }
            }
        }
        self.tr.rev += 1;
        self.view.cursor = Some(target);
        self.focus = Focus::Nav;
        let vh = self.geo.transcript.height as usize;
        let cx = Cx {
            p: &self.p,
            theme: &self.theme,
            g: &self.g,
            width: self.geo.c as usize,
            detail: self.detail,
            now,
            spin: 0,
        };
        self.tr.reveal(&mut self.view, target, vh.max(1), &cx);
    }
}
