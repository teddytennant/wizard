// OWNER: selectors (settings, model, thinking, scoped models, resume, trust, login dialogs)
//! Selectors replace the editor inside the dock; the spacer above and the footer below stay.
//! Frame: a full-width `border` rule above and below. Spec 11. Each selector is a port of the
//! matching component in Pi's `dist/modes/interactive/components` and of pi-tui's `SelectList`,
//! `SettingsList` and `Input`.

mod blocks;
mod input;
mod list;
mod model;
mod resume;
mod scoped;
mod settings;
mod thinking;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::Line;
use tuikit::width::display_width;
use unicode_segmentation::UnicodeSegmentation;

use agent_core::{ModelOption, SessionInfo};

use super::{rule, Cx, Lines};
use crate::theme::Tok;

pub use blocks::{changelog, hotkeys, session_info, SessionStats};
pub use input::Input;
pub use model::ModelSelector;
pub use resume::ResumeSelector;
pub use scoped::ScopedSelector;
pub use settings::SettingsSelector;
pub use thinking::ThinkingSelector;

use crate::settings::Settings;

/// What a selector asks the app to do.
#[derive(Clone, Debug, PartialEq)]
pub enum SelOut {
    None,
    /// Close the selector, nothing else.
    Close,
    /// Close and switch to this model id (`Request::SetModel`).
    SetModel(String),
    /// Close and set this thinking level (`Request::SetEffort`).
    SetThinking(String),
    /// Close and load this session (`Request::LoadSession`).
    Resume(String),
    /// A `Warning:` line for the transcript; the selector stays open.
    Warning(String),
    /// A settings value changed: apply it live and write `settings.json`.
    Settings(Box<Settings>),
    /// Show this theme now (preview, or restoring the one before the preview). Not persisted.
    Theme(String),
    /// The scoped model list changed this session (ids in cycle order, empty means all).
    Scoped(Vec<String>),
    /// `ctrl+s` in the scoped model list: write it to `settings.json`.
    ScopedSave(Vec<String>),
}

pub enum Selector {
    Model(ModelSelector),
    Thinking(ThinkingSelector),
    Scoped(ScopedSelector),
    Resume(ResumeSelector),
    Settings(Box<SettingsSelector>),
}

impl Selector {
    /// `scoped` are the ids of the scoped models (empty: no scope, the selector says so).
    pub fn model(models: Vec<ModelOption>, current: &str, scoped: &[String]) -> Selector {
        Selector::Model(ModelSelector::new(models, current, scoped))
    }

    pub fn thinking(levels: &[String], current: &str) -> Selector {
        Selector::Thinking(ThinkingSelector::new(levels, current))
    }

    /// The thinking selector with the backend's starting level marked `· default`.
    pub fn thinking_with_default(
        levels: &[String],
        current: &str,
        default: Option<&str>,
    ) -> Selector {
        Selector::Thinking(ThinkingSelector::with_default(levels, current, default))
    }

    /// Mark the model the backend starts with as `· default`.
    pub fn with_default_model(mut self, id: Option<&str>) -> Selector {
        if let Selector::Model(m) = &mut self {
            m.set_default(id);
        }
        self
    }

    pub fn scoped(models: &[ModelOption], enabled: &[String]) -> Selector {
        Selector::Scoped(ScopedSelector::new(models, enabled))
    }

    pub fn resume(
        sessions: &[SessionInfo],
        cwd: &str,
        current_id: &str,
        home: &str,
        now: i64,
    ) -> Selector {
        Selector::Resume(ResumeSelector::new(sessions, cwd, current_id, home, now))
    }

    pub fn settings(s: &Settings, theme: &str) -> Selector {
        Selector::Settings(Box::new(SettingsSelector::new(s, theme)))
    }

    pub fn render(&self, cx: &Cx, screen_h: u16) -> Lines {
        match self {
            Selector::Model(m) => m.render(cx, screen_h),
            Selector::Thinking(m) => m.render(cx),
            Selector::Scoped(m) => m.render(cx),
            Selector::Resume(m) => m.render(cx),
            Selector::Settings(m) => m.render(cx),
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> SelOut {
        match self {
            Selector::Model(m) => m.on_key(key),
            Selector::Thinking(m) => m.on_key(key),
            Selector::Scoped(m) => m.on_key(key),
            Selector::Resume(m) => m.on_key(key),
            Selector::Settings(m) => m.on_key(key),
        }
    }

    /// Backend events a selector may care about (the session list for `/resume`).
    pub fn on_event(&mut self, ev: &agent_core::Event) {
        if let (Selector::Resume(r), agent_core::Event::Sessions(s)) = (self, ev) {
            r.set_sessions(s);
        }
    }

    pub fn on_paste(&mut self, text: &str) {
        match self {
            Selector::Model(m) => m.paste(text),
            Selector::Thinking(m) => m.paste(text),
            Selector::Scoped(m) => m.paste(text),
            Selector::Resume(m) => m.paste(text),
            Selector::Settings(m) => m.paste(text),
        }
    }
}

pub(crate) fn is_cancel(key: &KeyEvent) -> bool {
    key.code == KeyCode::Esc
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}

pub(crate) fn is_ctrl(key: &KeyEvent, c: char) -> bool {
    key.code == KeyCode::Char(c)
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT)
}

/// `truncateToWidth(s, max, "")`: cut at a cell boundary, no ellipsis.
pub fn cut(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for g in s.graphemes(true) {
        let gw = tuikit::width::grapheme_width(g);
        if w + gw > max {
            break;
        }
        out.push_str(g);
        w += gw;
    }
    out
}

/// The tail of `s` that fits `max` cells.
pub fn cut_left(s: &str, max: usize) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut w = 0;
    for g in s.graphemes(true).rev() {
        let gw = tuikit::width::grapheme_width(g);
        if w + gw > max {
            break;
        }
        kept.push(g);
        w += gw;
    }
    kept.reverse();
    kept.concat()
}

/// pi-tui `fuzzyFilter`: every whitespace-separated token must match; best score first.
pub fn fuzzy_filter<T: Clone>(items: &[T], query: &str, text: impl Fn(&T) -> String) -> Vec<T> {
    let tokens: Vec<&str> = query.split_whitespace().collect();
    if tokens.is_empty() {
        return items.to_vec();
    }
    let mut scored: Vec<(i32, usize, T)> = Vec::new();
    for (i, it) in items.iter().enumerate() {
        let t = text(it);
        let mut total = 0;
        let mut ok = true;
        for tok in &tokens {
            match tuikit::fuzzy::score(tok, &t) {
                Some(m) => total += m.score,
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            scored.push((total, i, it.clone()));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, _, it)| it).collect()
}

/// Rule, content, rule.
pub fn frame(cx: &Cx, border: Tok, body: Lines) -> Lines {
    let r = rule(cx.width, cx.th().fg(border));
    let mut out = vec![r.clone()];
    out.extend(body);
    out.push(r);
    out
}

pub fn blank_line() -> Line<'static> {
    Line::from("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cut_has_no_ellipsis() {
        assert_eq!(cut("abcdef", 3), "abc");
        assert_eq!(cut_left("abcdef", 3), "def");
    }

    #[test]
    fn fuzzy_filter_needs_every_token() {
        let v = vec![
            "fake-model [fake]".to_string(),
            "big-model [other]".to_string(),
        ];
        assert_eq!(fuzzy_filter(&v, "big oth", |s| s.clone()).len(), 1);
        assert_eq!(fuzzy_filter(&v, "zzz", |s| s.clone()).len(), 0);
        assert_eq!(fuzzy_filter(&v, "", |s| s.clone()).len(), 2);
    }
}
