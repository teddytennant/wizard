// OWNER: dialogs (stack, trait, shared frames, command palette)
//! Modal dialogs. A dialog is a small state machine: it gets keys, mouse and backend events,
//! draws itself over the dimmed screen, and answers with an [`Outcome`] (navigate plus effects).
//! The app owns the stack and performs the effects, so a dialog never touches app state.

pub mod agents;
pub mod ask;
pub mod clock;
pub mod connect;
pub mod debug;
pub mod export;
pub mod help;
pub mod list;
pub mod mcp;
pub mod models;
pub mod panel;
pub mod plugins;
pub mod prefs;
pub mod rename;
pub mod sessions;
pub mod skills;
pub mod stash;
pub mod status;
pub mod themes;
pub mod timeline;
pub mod wizard_dir;

use agent_core::{Event, Request};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use tuikit::dialog::SelectDialog;
use tuikit::select::{SelectEvent, SelectItem};
use tuikit::theme::Variant;
use tuikit::Theme;

use crate::app::AgentKind;
use crate::commands::{self, CmdCtx};
use crate::keys::{Action, Keymap};

/// Things a dialog asks the app to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Run(Action),
    Request(Request),
    Toast(Variant, String),
    /// Apply a theme without saving it (live preview).
    PreviewTheme(String),
    /// Keep a theme.
    SetTheme(String),
    SetAgent(AgentKind),
    Rename(String),
    /// Rename a session other than the open one (the sessions list).
    RenameSession {
        id: String,
        title: String,
    },
    InsertPrompt(String),
    Export {
        path: String,
        options: export::Options,
    },
    /// A model was picked: remember it as recent.
    RecordModel(String),
    ToggleFavorite(String),
    TogglePin(String),
    /// Remove the session's file from wizard's session directory.
    DeleteSession(String),
    /// Drop the stash entry at this index (oldest first).
    StashRemove(usize),
    /// Pop the stash entry at this index into the prompt.
    StashPop(usize),
    Copy(String),
    /// Put the text of a past user message back in the prompt (what revert and fork do).
    RestorePrompt(String),
    /// Scroll the transcript so this message is near the top.
    JumpToMessage(usize),
}

/// A past user message, for the timeline, fork and message-action dialogs.
#[derive(Clone, Debug, PartialEq)]
pub struct UserMsg {
    /// Index into the transcript's messages.
    pub index: usize,
    /// 1-based position among user messages; what `/rewind <turn>` takes.
    pub turn: usize,
    pub text: String,
    /// Unix seconds when it was sent.
    pub at: i64,
}

/// A prompt the user stashed.
#[derive(Clone, Debug, PartialEq)]
pub struct StashItem {
    pub text: String,
    pub at: i64,
}

/// Everything a dialog may need from the app, copied when it opens. A dialog never borrows the
/// app, so a backend event arriving while it is open cannot alias it.
#[derive(Clone, Debug, Default)]
pub struct Ctx {
    pub version: String,
    pub cwd: String,
    pub config: agent_core::Config,
    pub prefs: prefs::Prefs,
    pub sessions: Vec<agent_core::SessionInfo>,
    pub session_id: String,
    pub session_title: String,
    pub in_session: bool,
    pub now: i64,
    /// Seconds east of UTC for every clock string.
    pub utc_offset: i64,
    pub stash: Vec<StashItem>,
    pub theme: String,
    pub agent: Option<AgentKind>,
    pub palette_key: String,
    /// `~/.wizard`; `None` in tests, which then see no MCP servers or skills.
    pub wizard_dir: Option<std::path::PathBuf>,
    pub users: Vec<UserMsg>,
    /// Export defaults mirror the current view flags.
    pub export: export::Options,
}

pub enum Nav {
    Stay,
    Close,
    Replace(Box<dyn Dialog>),
    Push(Box<dyn Dialog>),
}

pub struct Outcome {
    pub nav: Nav,
    pub effects: Vec<Effect>,
}

impl Outcome {
    pub fn stay() -> Self {
        Outcome {
            nav: Nav::Stay,
            effects: Vec::new(),
        }
    }
    pub fn close() -> Self {
        Outcome {
            nav: Nav::Close,
            effects: Vec::new(),
        }
    }
    pub fn close_with(effects: Vec<Effect>) -> Self {
        Outcome {
            nav: Nav::Close,
            effects,
        }
    }
    pub fn with(mut self, e: Effect) -> Self {
        self.effects.push(e);
        self
    }
}

pub trait Dialog {
    fn title(&self) -> &str;
    fn handle_key(&mut self, key: KeyEvent) -> Outcome;
    fn handle_paste(&mut self, _text: &str) -> Outcome {
        Outcome::stay()
    }
    fn handle_mouse(&mut self, _ev: MouseEvent) -> Outcome {
        Outcome::stay()
    }
    /// Backend events that may change what the dialog lists (sessions, config).
    fn on_event(&mut self, _ev: &Event) {}
    /// Paint over `screen` (already containing the page underneath). Returns the cursor cell.
    fn draw(&mut self, buf: &mut Buffer, screen: Rect, theme: &Theme) -> Option<(u16, u16)>;
}

#[derive(Default)]
pub struct DialogStack {
    stack: Vec<Box<dyn Dialog>>,
}

impl DialogStack {
    pub fn is_open(&self) -> bool {
        !self.stack.is_empty()
    }
    pub fn len(&self) -> usize {
        self.stack.len()
    }
    pub fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }
    pub fn push(&mut self, d: Box<dyn Dialog>) {
        self.stack.push(d);
    }
    pub fn pop(&mut self) -> Option<Box<dyn Dialog>> {
        self.stack.pop()
    }
    pub fn clear(&mut self) {
        self.stack.clear();
    }
    pub fn top_title(&self) -> Option<&str> {
        self.stack.last().map(|d| d.title())
    }
    pub fn top_mut(&mut self) -> Option<&mut Box<dyn Dialog>> {
        self.stack.last_mut()
    }
    pub fn on_event(&mut self, ev: &Event) {
        for d in &mut self.stack {
            d.on_event(ev);
        }
    }
    /// Apply the navigation part of an outcome; the caller runs the effects.
    pub fn apply(&mut self, nav: Nav) {
        match nav {
            Nav::Stay => {}
            Nav::Close => {
                self.stack.pop();
            }
            Nav::Replace(d) => {
                self.stack.pop();
                self.stack.push(d);
            }
            Nav::Push(d) => self.stack.push(d),
        }
    }
    /// Draw the top dialog. Only one is visible at a time, like opencode.
    pub fn draw(&mut self, buf: &mut Buffer, screen: Rect, theme: &Theme) -> Option<(u16, u16)> {
        self.stack
            .last_mut()
            .and_then(|d| d.draw(buf, screen, theme))
    }
}

// ---- select helper ---------------------------------------------------------------------

/// A select dialog that keeps the selection on the middle row, like opencode's.
pub fn centered<T>(mut d: SelectDialog<T>) -> SelectDialog<T> {
    d.state.set_centered(true);
    d
}

pub enum Pick {
    None,
    Cancel,
    /// Index into the items the dialog was built with.
    Submit(usize),
}

pub fn select_key<T>(dlg: &mut SelectDialog<T>, key: KeyEvent) -> Pick {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Pick::Cancel;
    }
    // bare home and end pick the first and last row (`dialog.select.home` and `.end`)
    if key.modifiers.is_empty() && matches!(key.code, KeyCode::Home | KeyCode::End) {
        if key.code == KeyCode::Home {
            dlg.state.select_first();
        } else {
            dlg.state.select_last();
        }
        return Pick::None;
    }
    match dlg.handle_key(key) {
        SelectEvent::Cancel => Pick::Cancel,
        SelectEvent::Submit(i) => Pick::Submit(i),
        _ => Pick::None,
    }
}

pub fn select_mouse<T>(dlg: &mut SelectDialog<T>, ev: MouseEvent) -> Pick {
    match dlg.handle_mouse(ev) {
        SelectEvent::Cancel => Pick::Cancel,
        SelectEvent::Submit(i) => Pick::Submit(i),
        _ => Pick::None,
    }
}

// ---- command palette -------------------------------------------------------------------

pub struct Palette {
    dlg: SelectDialog<Action>,
    /// All entries including the repeated `Suggested` group, and without it.
    full: Vec<SelectItem<Action>>,
    plain: Vec<SelectItem<Action>>,
    showing_full: bool,
    actions: Vec<Action>,
}

fn item_for(c: &commands::Command, group: &str) -> SelectItem<Action> {
    let mut it = SelectItem::new(c.action.clone(), c.title.clone()).group(group);
    if !c.keybind.is_empty() {
        it = it.hint(c.keybind.clone());
    }
    if let Some(d) = &c.palette_desc {
        it = it.description(d.clone());
    }
    it
}

impl Palette {
    pub fn new(ctx: &CmdCtx, keymap: &Keymap) -> Self {
        let mut cmds = commands::build(ctx, keymap);
        cmds.retain(|c| {
            c.palette && (ctx.in_session || !c.session_only) && (!ctx.in_session || !c.home_only)
        });
        let mut full = Vec::new();
        let mut plain = Vec::new();
        for cat in commands::category_order(ctx.in_session, ctx.has_sessions, ctx.stash_nonempty) {
            if *cat == commands::Category::Suggested {
                for c in cmds.iter().filter(|c| c.suggested) {
                    full.push(item_for(c, "Suggested"));
                }
                continue;
            }
            for c in cmds.iter().filter(|c| c.category == *cat) {
                let it = item_for(c, cat.label());
                full.push(it.clone());
                plain.push(it);
            }
        }
        let actions = full.iter().map(|i| i.value.clone()).collect();
        let dlg = centered(SelectDialog::new("Commands", full.clone()));
        Palette {
            dlg,
            full,
            plain,
            showing_full: true,
            actions,
        }
    }

    fn sync_items(&mut self) {
        let want_full = self.dlg.state.query().trim().is_empty();
        if want_full != self.showing_full {
            self.showing_full = want_full;
            let items = if want_full {
                self.full.clone()
            } else {
                self.plain.clone()
            };
            self.dlg.state.set_items(items);
        }
    }
}

impl Dialog for Palette {
    fn title(&self) -> &str {
        "Commands"
    }

    fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        let pick = select_key(&mut self.dlg, key);
        self.sync_items();
        match pick {
            Pick::Cancel => Outcome::close(),
            Pick::Submit(i) => {
                let items = self.dlg.state.items();
                let a = items
                    .get(i)
                    .map(|it| it.value.clone())
                    .or_else(|| self.actions.get(i).cloned());
                match a {
                    Some(a) => Outcome::close_with(vec![Effect::Run(a)]),
                    None => Outcome::stay(),
                }
            }
            Pick::None => Outcome::stay(),
        }
    }

    fn handle_paste(&mut self, text: &str) -> Outcome {
        self.dlg.state.paste_query(text);
        self.sync_items();
        Outcome::stay()
    }

    fn handle_mouse(&mut self, ev: MouseEvent) -> Outcome {
        match select_mouse(&mut self.dlg, ev) {
            Pick::Submit(i) => match self.dlg.state.items().get(i) {
                Some(it) => Outcome::close_with(vec![Effect::Run(it.value.clone())]),
                None => Outcome::stay(),
            },
            _ => Outcome::stay(),
        }
    }

    fn draw(&mut self, buf: &mut Buffer, screen: Rect, theme: &Theme) -> Option<(u16, u16)> {
        self.dlg.render(buf, screen, theme).cursor
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn palette_has_suggested_first_and_drops_it_when_filtering() {
        let km = Keymap::new();
        let ctx = CmdCtx::default();
        let mut p = Palette::new(&ctx, &km);
        let first = p.dlg.state.items()[0].group.clone();
        assert_eq!(first.as_deref(), Some("Suggested"));
        for c in "theme".chars() {
            p.handle_key(key(KeyCode::Char(c)));
        }
        assert!(p
            .dlg
            .state
            .items()
            .iter()
            .all(|i| i.group.as_deref() != Some("Suggested")));
        let out = p.handle_key(key(KeyCode::Enter));
        assert!(matches!(out.nav, Nav::Close));
        assert_eq!(out.effects, vec![Effect::Run(Action::Themes)]);
    }

    #[test]
    fn esc_closes_and_alert_closes_on_enter() {
        let km = Keymap::new();
        let mut p = Palette::new(&CmdCtx::default(), &km);
        assert!(matches!(p.handle_key(key(KeyCode::Esc)).nav, Nav::Close));
        let mut a = panel::Alert::new("Help", "x");
        assert!(matches!(a.handle_key(key(KeyCode::Enter)).nav, Nav::Close));
    }
}
