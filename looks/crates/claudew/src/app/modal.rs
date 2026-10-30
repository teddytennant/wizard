//! Vim mode and `keybindings.json` for the prompt, on the engines shared with wizard's own TUI
//! (`src/vim.rs`, `src/keybindings.rs`).

use std::path::Path;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{App, Block, Dialog, Menu};
use crate::keybindings::{Keymap, Resolved, Stroke};
use crate::vim::{Out, VimKey, VimMode, VimState};

/// `editorMode` from Claude's settings files, the user's first, then the project's and the
/// project's local one, which win.
pub fn editor_mode_is_vim(home: &Path, project: &Path) -> bool {
    let mut vim = false;
    for file in [
        home.join(".claude/settings.json"),
        project.join(".claude/settings.json"),
        project.join(".claude/settings.local.json"),
    ] {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if let Some(mode) = doc.get("editorMode").and_then(|m| m.as_str()) {
            vim = mode == "vim";
        }
    }
    vim
}

/// Write `editorMode` into the user's settings, the way `/config` does.
fn save_editor_mode(home: &Path, vim: bool) {
    let path = home.join(".claude/settings.json");
    let mut doc = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(map) = doc.as_object_mut() {
        map.insert(
            "editorMode".into(),
            if vim { "vim" } else { "normal" }.into(),
        );
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    if let Ok(text) = serde_json::to_string_pretty(&doc) {
        std::fs::write(path, text + "\n").ok();
    }
}

fn home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(Into::into)
}

impl App {
    pub fn init_modal(&mut self) {
        let home = home().unwrap_or_default();
        self.vim = VimState::default();
        self.vim
            .set_enabled(editor_mode_is_vim(&home, &self.opts.cwd));
        self.keymap = Keymap::load(&home, &self.opts.cwd);
    }

    /// `/vim`: flip the editor mode and keep it in the user's settings.
    pub(super) fn toggle_vim(&mut self, text: String) {
        let on = !self.vim.enabled;
        self.vim.set_enabled(on);
        if let Some(h) = home() {
            save_editor_mode(&h, on);
        }
        self.blocks.push(Block::User { text, bash: false });
        let said = if on {
            "Editor mode set to vim. Use Escape key to toggle between INSERT and NORMAL modes."
        } else {
            "Editor mode set to normal. Using standard (readline) keyboard bindings."
        };
        self.output(said, String::new());
    }

    /// The editor's text and cursor as the vim engine reads them: a character index.
    fn vim_view(&self) -> (String, usize) {
        let text = self.editor.text().to_string();
        let cur = text[..self.editor.cursor().min(text.len())].chars().count();
        (text, cur)
    }

    fn vim_apply(&mut self, before: &str, text: &str, cur: usize) {
        if text != before {
            self.editor.set_text(text);
        }
        let byte = text.char_indices().nth(cur).map_or(text.len(), |(i, _)| i);
        self.editor.set_cursor(byte);
    }

    /// Modal editing in the prompt. `true` when the engine took the key; otherwise the key goes
    /// on to ordinary handling.
    pub(super) fn vim_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let vk = match key.code {
            KeyCode::Esc => VimKey::Esc,
            KeyCode::Char('[') if ctrl => VimKey::Esc,
            _ if ctrl || alt => return false,
            KeyCode::Char(c) => VimKey::Char(c),
            KeyCode::Backspace => VimKey::Backspace,
            KeyCode::Delete => VimKey::Delete,
            KeyCode::Left => VimKey::Left,
            KeyCode::Right => VimKey::Right,
            KeyCode::Up => VimKey::Up,
            KeyCode::Down => VimKey::Down,
            KeyCode::Home => VimKey::Home,
            KeyCode::End => VimKey::End,
            _ => VimKey::Other,
        };
        let insert = self.vim.mode == VimMode::Insert;
        // a menu or a shell prompt that has the key keeps it in INSERT
        if insert && !matches!(vk, VimKey::Esc | VimKey::Char(_) | VimKey::Backspace) {
            return false;
        }
        let pending = self.vim.is_pending();
        let visual = self.vim.is_visual();
        // `?` on an empty prompt and `!` open their modes from INSERT only; in NORMAL `?` is help
        if !insert && vk == VimKey::Char('/') && !pending {
            self.dialog = Some(Dialog::Search {
                query: String::new(),
                sel: 0,
            });
            return true;
        }
        if !insert && vk == VimKey::Char('?') && !pending && self.editor.is_empty() {
            return false;
        }
        // Esc in INSERT only changes the mode; it is not the key that cancels
        let (mut text, mut cur) = self.vim_view();
        let before = text.clone();
        let out = self.vim.key(&mut text, &mut cur, vk);
        match out {
            Out::Handled => {
                self.vim_apply(&before, &text, cur);
                if vk == VimKey::Esc && !insert && !pending && !visual {
                    // nothing to leave: Esc does what it does without vim
                    self.escape(None);
                }
            }
            Out::HistoryUp => {
                if !self.queued.is_empty() {
                    let back = std::mem::take(&mut self.queued).join("\n");
                    self.editor.set_text(&back);
                } else {
                    self.history(-1);
                }
            }
            Out::HistoryDown => self.history(1),
            Out::Pass => {
                if insert {
                    return false;
                }
                // Enter, Tab and the page keys keep their meaning in NORMAL; a letter with no
                // command is not typed
                return !matches!(vk, VimKey::Other);
            }
        }
        self.refresh_menu();
        true
    }

    /// The contexts a key is looked up in, from what is on screen.
    fn keymap_contexts(&self) -> Vec<&'static str> {
        let mut c = vec!["Global"];
        match &self.dialog {
            Some(Dialog::Permission { .. }) => c.push("Confirmation"),
            Some(Dialog::Search { .. }) => c.push("HistorySearch"),
            Some(_) => c.push("Select"),
            None if self.detail => c.push("Transcript"),
            None => {
                c.push("Chat");
                if self.menu_open() {
                    c.push("Autocomplete");
                }
            }
        }
        c
    }

    /// `keybindings.json`: `true` when a rule from the file took the key.
    pub(super) fn keymap_key(&mut self, key: KeyEvent) -> bool {
        self.keymap.reload();
        if !self.keymap.has_user_rules() {
            return false;
        }
        let contexts = self.keymap_contexts();
        let stroke = Stroke::from(key);
        match self
            .keymap
            .resolve(&contexts, stroke.clone(), Instant::now())
        {
            Resolved::Action(a) => {
                self.run_action(&a);
                true
            }
            Resolved::Unbound => self.keymap.default_bound(&contexts, &stroke),
            Resolved::Pending | Resolved::Cancelled => true,
            Resolved::Default => false,
        }
    }

    /// What an action from the file does here. Actions this look has no use for do nothing.
    fn run_action(&mut self, action: &str) {
        if let Some(name) = action.strip_prefix("command:") {
            self.submit(format!("/{name}"));
            return;
        }
        match action {
            "app:exit" => self.quit = true,
            "app:toggleTodos" => self.show_todos = !self.show_todos,
            "app:toggleTranscript" => {
                self.detail = !self.detail;
                self.scroll = 0;
            }
            "app:redraw" | "chat:clearInput" | "chat:clearScreen" => self.repaint = true,
            "history:search" => {
                self.dialog = Some(Dialog::Search {
                    query: String::new(),
                    sel: 0,
                })
            }
            "history:previous"
            | "autocomplete:previous"
            | "select:previous"
            | "confirm:previous"
                if self.menu_open() =>
            {
                self.menu_move(false)
            }
            "history:next" | "autocomplete:next" | "select:next" | "confirm:next"
                if self.menu_open() =>
            {
                self.menu_move(true)
            }
            "history:previous" => self.history(-1),
            "history:next" => self.history(1),
            "autocomplete:accept" => {
                self.menu_accept(true);
            }
            "autocomplete:dismiss" => self.menu = Menu::None,
            "chat:cancel" => self.escape(None),
            "chat:cycleMode" => self.cycle_mode(),
            "chat:modelPicker" => self.open_model(),
            "chat:submit" => self.enter(),
            "chat:newline" => self.insert_newline(),
            "chat:undo" => {
                self.editor.undo();
            }
            "chat:externalEditor" => self.edit_request = true,
            "chat:stash" => self.toggle_stash(),
            "chat:sendNow" | "chat:queueSubmit"
                if self.busy.is_some() && !self.queued.is_empty() =>
            {
                self.send(super::Request::Cancel);
            }
            "transcript:exit" => self.detail = false,
            "scroll:pageUp" => self.scroll_by(self.page() as isize),
            "scroll:pageDown" => self.scroll_by(-(self.page() as isize)),
            "scroll:bottom" => self.scroll = 0,
            "confirm:yes" | "confirm:no" => self.answer_permission(action == "confirm:yes"),
            _ => {}
        }
        self.refresh_menu();
    }

    /// Answer the permission prompt on screen with its first option or its last.
    fn answer_permission(&mut self, yes: bool) {
        let Some(Dialog::Permission { req, .. }) = self.dialog.clone() else {
            return;
        };
        let options = super::permission_options(&req, self.auto_offered());
        let pick = if yes { 0 } else { options.len() - 1 };
        self.decide(req, options[pick].1);
    }
}
