// OWNER: bottom-pane
//! Key matching and labels, ported from Codex's `key_hint.rs` (spec C.0.3).
//!
//! A binding matches an event after both are normalised: uppercase letters count as `shift` plus
//! the lowercase letter and raw C0 control characters count as `ctrl` plus the letter, so
//! terminals that report keys in either form behave the same.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::style::Style;
use ratatui::text::Span;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct KeyBinding {
    key: KeyCode,
    modifiers: KeyModifiers,
}

pub const fn plain(key: KeyCode) -> KeyBinding {
    KeyBinding::new(key, KeyModifiers::NONE)
}
pub const fn alt(key: KeyCode) -> KeyBinding {
    KeyBinding::new(key, KeyModifiers::ALT)
}
pub const fn shift(key: KeyCode) -> KeyBinding {
    KeyBinding::new(key, KeyModifiers::SHIFT)
}
pub const fn ctrl(key: KeyCode) -> KeyBinding {
    KeyBinding::new(key, KeyModifiers::CONTROL)
}
pub const fn ctrl_alt(key: KeyCode) -> KeyBinding {
    KeyBinding::new(key, KeyModifiers::CONTROL.union(KeyModifiers::ALT))
}

fn c0_control_char_to_ctrl_char(ch: char) -> Option<char> {
    let code = u32::from(ch);
    match code {
        0x00 => Some(' '),
        0x01..=0x1a => char::from_u32(code - 0x01 + u32::from('a')),
        0x1c..=0x1f => char::from_u32(code - 0x1c + u32::from('4')),
        _ => None,
    }
}

pub fn normalize_key_parts(key: KeyCode, mut modifiers: KeyModifiers) -> (KeyCode, KeyModifiers) {
    let KeyCode::Char(ch) = key else {
        return (key, modifiers);
    };
    if modifiers.is_empty() {
        if let Some(c) = c0_control_char_to_ctrl_char(ch) {
            return (KeyCode::Char(c), KeyModifiers::CONTROL);
        }
    }
    if ch.is_ascii_uppercase() {
        modifiers.insert(KeyModifiers::SHIFT);
        return (KeyCode::Char(ch.to_ascii_lowercase()), modifiers);
    }
    (key, modifiers)
}

impl KeyBinding {
    pub const fn new(key: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { key, modifiers }
    }

    pub fn is_press(&self, event: KeyEvent) -> bool {
        normalize_key_parts(self.key, self.modifiers)
            == normalize_key_parts(event.code, event.modifiers)
            && matches!(event.kind, KeyEventKind::Press | KeyEventKind::Repeat)
    }

    /// `ctrl + c`, `shift + tab`, `alt + ,`, `enter`, `←`.
    pub fn label(&self) -> String {
        let mut out = String::new();
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            out.push_str("ctrl + ");
        }
        if self.modifiers.contains(KeyModifiers::SHIFT) {
            out.push_str("shift + ");
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            out.push_str("alt + ");
        }
        out.push_str(&match self.key {
            KeyCode::Enter => "enter".to_string(),
            KeyCode::Char(' ') => "space".to_string(),
            KeyCode::Up => "↑".to_string(),
            KeyCode::Down => "↓".to_string(),
            KeyCode::Left => "←".to_string(),
            KeyCode::Right => "→".to_string(),
            KeyCode::PageUp => "pgup".to_string(),
            KeyCode::PageDown => "pgdn".to_string(),
            KeyCode::Esc => "esc".to_string(),
            KeyCode::Tab | KeyCode::BackTab => "tab".to_string(),
            KeyCode::Backspace => "backspace".to_string(),
            KeyCode::Delete => "delete".to_string(),
            KeyCode::Home => "home".to_string(),
            KeyCode::End => "end".to_string(),
            KeyCode::Char(c) => c.to_ascii_lowercase().to_string(),
            KeyCode::F(n) => format!("f{n}"),
            other => format!("{other:?}").to_ascii_lowercase(),
        });
        out
    }

    /// The label as a DIM span, the way hint rows draw it.
    pub fn span(&self) -> Span<'static> {
        Span::styled(self.label(), Style::default().dim())
    }
}

pub trait KeyBindingListExt {
    fn is_pressed(&self, event: KeyEvent) -> bool;
}

impl KeyBindingListExt for [KeyBinding] {
    fn is_pressed(&self, event: KeyEvent) -> bool {
        self.iter().any(|b| b.is_press(event))
    }
}

/// Plain printable text, not a Ctrl or Alt chord.
pub fn is_plain_text_key(event: KeyEvent) -> bool {
    matches!(
        event,
        KeyEvent { code: KeyCode::Char(ch), modifiers, .. }
            if !ch.is_ascii_control()
                && !modifiers.contains(KeyModifiers::CONTROL)
                && !modifiers.contains(KeyModifiers::ALT)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    #[test]
    fn labels_follow_codex() {
        assert_eq!(ctrl(KeyCode::Char('c')).label(), "ctrl + c");
        assert_eq!(shift(KeyCode::Tab).label(), "shift + tab");
        assert_eq!(alt(KeyCode::Char(',')).label(), "alt + ,");
        assert_eq!(shift(KeyCode::Left).label(), "shift + ←");
        assert_eq!(plain(KeyCode::Enter).label(), "enter");
        assert_eq!(
            KeyBinding::new(
                KeyCode::Char('x'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )
            .label(),
            "ctrl + shift + x"
        );
    }

    #[test]
    fn c0_bytes_and_uppercase_normalise() {
        assert!(ctrl(KeyCode::Char('j')).is_press(ev(KeyCode::Char('\n'), KeyModifiers::NONE)));
        assert!(shift(KeyCode::Char('a')).is_press(ev(KeyCode::Char('A'), KeyModifiers::NONE)));
        assert!(!ctrl(KeyCode::Char('j')).is_press(ev(KeyCode::Char('j'), KeyModifiers::NONE)));
    }
}
