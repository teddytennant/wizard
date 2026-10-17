// OWNER: dialogs
//! `DialogSelect` as a reusable piece: a filterable list with footer actions bound to keys,
//! tab focus over those actions, and move/filter notifications for dialogs that preview.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use tuikit::dialog::{FooterHint, SelectDialog};
use tuikit::select::{SelectEvent, SelectItem};
use tuikit::Theme;

use crate::keys::Chord;

/// A footer action: a key that works on the selected row, shown as `title key`.
#[derive(Clone, Debug)]
pub struct Act {
    pub id: &'static str,
    pub title: &'static str,
    pub key: &'static str,
    pub right: bool,
    pub hidden: bool,
}

impl Act {
    pub fn new(id: &'static str, title: &'static str, key: &'static str) -> Self {
        Act {
            id,
            title,
            key,
            right: false,
            hidden: false,
        }
    }
    pub fn hidden(mut self, h: bool) -> Self {
        self.hidden = h;
        self
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ListEvent {
    None,
    /// Esc or ctrl+c.
    Cancel,
    /// Enter or click on the item with this index into the items the list was built with.
    Select(usize),
    /// A footer action fired on the selected item (index into the items).
    Action(&'static str, usize),
    /// The selection moved, or the filter changed which row is selected.
    Moved,
    /// The query changed.
    Filtered,
}

pub struct ListDialog<T> {
    pub dlg: SelectDialog<T>,
    acts: Vec<Act>,
    chords: Vec<Option<Chord>>,
    /// While set the list ignores everything but cancel (a failed load, say).
    pub locked: bool,
}

impl<T> ListDialog<T> {
    pub fn new(dlg: SelectDialog<T>) -> Self {
        ListDialog {
            dlg,
            acts: Vec::new(),
            chords: Vec::new(),
            locked: false,
        }
    }

    /// Centre the selection on the middle row, as opencode's scrollbox does.
    pub fn centered(mut self) -> Self {
        self.dlg.state.set_centered(true);
        self
    }

    pub fn actions(mut self, acts: Vec<Act>) -> Self {
        self.set_actions(acts);
        self
    }

    pub fn set_actions(&mut self, acts: Vec<Act>) {
        self.chords = acts.iter().map(|a| Chord::parse(a.key)).collect();
        self.dlg.footer = acts
            .iter()
            .filter(|a| !a.hidden)
            .map(|a| {
                let mut h = FooterHint::new(a.title, a.key).action();
                h.right = a.right;
                h
            })
            .collect();
        self.acts = acts;
    }

    /// Plain footer hints that are not actions (`switch ctrl+x 1-9`), after the actions.
    pub fn hint(&mut self, title: &str, key: &str) {
        self.dlg.footer.push(FooterHint::new(title, key).bold());
    }

    pub fn selected_index(&self) -> Option<usize> {
        self.dlg.state.selected_index()
    }

    pub fn query(&self) -> &str {
        self.dlg.state.query()
    }

    fn fire(&self, id: &'static str) -> ListEvent {
        match self.dlg.state.selected_index() {
            Some(i) => ListEvent::Action(id, i),
            None => ListEvent::None,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ListEvent {
        if key.kind == KeyEventKind::Release {
            return ListEvent::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('c')) {
            return ListEvent::Cancel;
        }
        if self.locked {
            return ListEvent::None;
        }
        for (a, c) in self.acts.iter().zip(&self.chords) {
            if c.is_some_and(|c| c.matches(&key)) && !a.hidden {
                self.dlg.focus = None;
                return self.fire(a.id);
            }
        }
        match key.code {
            KeyCode::Tab if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.dlg.move_focus(1);
                return ListEvent::None;
            }
            KeyCode::BackTab => {
                self.dlg.move_focus(-1);
                return ListEvent::None;
            }
            // opencode binds bare home and end to the first and last row, filter or not
            KeyCode::Home | KeyCode::End if key.modifiers.is_empty() => {
                let before = self.dlg.state.selected_index();
                if key.code == KeyCode::Home {
                    self.dlg.state.select_first();
                } else {
                    self.dlg.state.select_last();
                }
                self.dlg.focus = None;
                return if before == self.dlg.state.selected_index() {
                    ListEvent::None
                } else {
                    ListEvent::Moved
                };
            }
            KeyCode::Enter => {
                if let Some(h) = self.dlg.focused_action() {
                    let key = h.key.clone();
                    self.dlg.focus = None;
                    if let Some(a) = self.acts.iter().find(|a| a.key == key && !a.hidden) {
                        return self.fire(a.id);
                    }
                    return ListEvent::None;
                }
            }
            _ => {}
        }
        let before = (
            self.dlg.state.selected_index(),
            self.dlg.state.query().len(),
        );
        let q_before = self.dlg.state.query().to_string();
        let ev = self.dlg.handle_key(key);
        let moved = !matches!(ev, SelectEvent::Submit(_) | SelectEvent::Cancel);
        if moved {
            // Any list movement or typing hands focus back from the footer.
            self.dlg.focus = None;
        }
        match ev {
            SelectEvent::Cancel => ListEvent::Cancel,
            SelectEvent::Submit(i) => ListEvent::Select(i),
            SelectEvent::Changed => {
                if self.dlg.state.query() != q_before {
                    if self.dlg.state.query().trim().is_empty() {
                        // Clearing the search goes back to the current item, as opencode does.
                        self.dlg.state.select_current();
                    }
                    ListEvent::Filtered
                } else if before.0 != self.dlg.state.selected_index() {
                    ListEvent::Moved
                } else {
                    ListEvent::None
                }
            }
            SelectEvent::Ignored => ListEvent::None,
        }
    }

    pub fn handle_paste(&mut self, text: &str) -> ListEvent {
        if self.locked {
            return ListEvent::None;
        }
        self.dlg.state.paste_query(text);
        ListEvent::Filtered
    }

    pub fn handle_mouse(&mut self, ev: MouseEvent) -> ListEvent {
        if self.locked {
            return ListEvent::None;
        }
        let before = self.dlg.state.selected_index();
        match self.dlg.handle_mouse(ev) {
            SelectEvent::Submit(i) => ListEvent::Select(i),
            SelectEvent::Changed if before != self.dlg.state.selected_index() => ListEvent::Moved,
            _ => ListEvent::None,
        }
    }

    pub fn items(&self) -> &[SelectItem<T>] {
        self.dlg.state.items()
    }

    pub fn draw(&mut self, buf: &mut Buffer, screen: Rect, theme: &Theme) -> Option<(u16, u16)> {
        self.dlg.render(buf, screen, theme).cursor
    }
}
