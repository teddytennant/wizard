// OWNER: keys (keybinding table, display text, `keybindings.json` remaps)
//! Pi's default application bindings (`docs/piw-spec.md` 12) as a table of chords per [`Action`].
//!
//! The table only maps keys to actions; whether an action applies right now (exit only on an
//! empty editor, interrupt only while busy) is decided by the app, which falls through to the
//! editor when it declines. Editor movement and editing keys live in `tuikit::editor`.
//! `<agent-dir>/keybindings.json` remapping is not read yet.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    Interrupt,
    Clear,
    Exit,
    Suspend,
    ExternalEditor,
    PasteImage,
    ThinkingCycle,
    ThinkingToggle,
    ModelSelect,
    ModelCycleForward,
    ModelCycleBackward,
    ToolsExpand,
    MessageCopy,
    FollowUp,
    Dequeue,
    PageUp,
    PageDown,
    Top,
    Bottom,
    PreviousPrompt,
    NextPrompt,
    NewLine,
    Submit,
    Tab,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chord {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

/// Parse `ctrl+shift+f`, `alt+enter`, `pageUp`, `escape`, ...
pub fn parse_chord(s: &str) -> Option<Chord> {
    let mut mods = KeyModifiers::NONE;
    let mut parts: Vec<&str> = s.split('+').collect();
    let key = parts.pop()?;
    // `ctrl+-` ends in a literal `+`-free key; `ctrl++` is not a Pi binding
    for m in parts {
        mods |= match m.to_ascii_lowercase().as_str() {
            "ctrl" => KeyModifiers::CONTROL,
            "alt" => KeyModifiers::ALT,
            "shift" => KeyModifiers::SHIFT,
            _ => return None,
        };
    }
    let code = match key.to_ascii_lowercase().as_str() {
        "escape" | "esc" => KeyCode::Esc,
        "enter" | "return" => KeyCode::Enter,
        "tab" if mods.contains(KeyModifiers::SHIFT) => {
            mods.remove(KeyModifiers::SHIFT);
            KeyCode::BackTab
        }
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        "delete" => KeyCode::Delete,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "space" => KeyCode::Char(' '),
        k if k.chars().count() == 1 => KeyCode::Char(k.chars().next()?),
        _ => return None,
    };
    Some(Chord { code, mods })
}

fn norm(code: KeyCode, mods: KeyModifiers) -> (KeyCode, KeyModifiers) {
    match code {
        // BackTab already says shift
        KeyCode::BackTab => (code, mods - KeyModifiers::SHIFT),
        // case carries the shift of a letter; with ctrl the shift is explicit and the letter is lower
        KeyCode::Char(c) if mods.contains(KeyModifiers::CONTROL) => {
            (KeyCode::Char(c.to_ascii_lowercase()), mods)
        }
        KeyCode::Char(c) => (KeyCode::Char(c), mods - KeyModifiers::SHIFT),
        _ => (code, mods),
    }
}

impl Chord {
    pub fn matches(&self, key: &KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        let (c0, m0) = norm(self.code, self.mods);
        let (c1, m1) = norm(key.code, key.modifiers);
        c0 == c1 && m0 == m1
    }

    /// `ctrl+o` for the header hints, `Ctrl+O` for selector hints.
    pub fn text(&self, capitalise: bool) -> String {
        let mut parts: Vec<String> = Vec::new();
        let m = self.mods;
        let shift_tab = self.code == KeyCode::BackTab;
        for (flag, name) in [
            (KeyModifiers::CONTROL, "ctrl"),
            (KeyModifiers::ALT, "alt"),
            (KeyModifiers::SHIFT, "shift"),
        ] {
            if m.contains(flag) || (flag == KeyModifiers::SHIFT && shift_tab) {
                parts.push(name.into());
            }
        }
        parts.push(match self.code {
            KeyCode::Esc => "escape".into(),
            KeyCode::Enter => "enter".into(),
            KeyCode::Tab | KeyCode::BackTab => "tab".into(),
            KeyCode::Backspace => "backspace".into(),
            KeyCode::Delete => "delete".into(),
            KeyCode::Up => "up".into(),
            KeyCode::Down => "down".into(),
            KeyCode::Left => "left".into(),
            KeyCode::Right => "right".into(),
            KeyCode::Home => "home".into(),
            KeyCode::End => "end".into(),
            KeyCode::PageUp => "pageUp".into(),
            KeyCode::PageDown => "pageDown".into(),
            KeyCode::Char(' ') => "space".into(),
            KeyCode::Char(c) => c.to_string(),
            _ => "?".into(),
        });
        let s = parts.join("+");
        if capitalise {
            s.split('+')
                .map(|p| {
                    let mut c = p.chars();
                    c.next()
                        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>()
                .join("+")
        } else {
            s
        }
    }
}

pub struct Keymap {
    bindings: Vec<(Action, Vec<Chord>)>,
}

fn chords(list: &[&str]) -> Vec<Chord> {
    list.iter().filter_map(|s| parse_chord(s)).collect()
}

impl Default for Keymap {
    fn default() -> Self {
        Self::new()
    }
}

impl Keymap {
    pub fn new() -> Self {
        use Action::*;
        let b = |a, l: &[&str]| (a, chords(l));
        Keymap {
            bindings: vec![
                b(Interrupt, &["escape"]),
                b(Clear, &["ctrl+c"]),
                b(Exit, &["ctrl+d"]),
                b(Suspend, &["ctrl+z"]),
                b(ExternalEditor, &["ctrl+g"]),
                b(PasteImage, &["ctrl+v"]),
                b(ThinkingCycle, &["shift+tab"]),
                b(ThinkingToggle, &["ctrl+t"]),
                b(ModelSelect, &["ctrl+l"]),
                b(ModelCycleForward, &["ctrl+p"]),
                b(ModelCycleBackward, &["ctrl+shift+p"]),
                b(ToolsExpand, &["ctrl+o"]),
                b(MessageCopy, &["ctrl+x"]),
                b(FollowUp, &["alt+enter"]),
                b(Dequeue, &["alt+up"]),
                b(PageUp, &["pageUp"]),
                b(PageDown, &["pageDown"]),
                b(Top, &["ctrl+home"]),
                b(Bottom, &["ctrl+end"]),
                b(PreviousPrompt, &["ctrl+shift+up", "ctrl+up"]),
                b(NextPrompt, &["ctrl+shift+down", "ctrl+down"]),
                b(NewLine, &["shift+enter", "ctrl+j"]),
                b(Submit, &["enter"]),
                b(Tab, &["tab"]),
            ],
        }
    }

    pub fn resolve(&self, key: &KeyEvent) -> Option<Action> {
        // ctrl+j reaches us as a bare LF in some terminals
        if key.code == KeyCode::Char('\n') {
            return Some(Action::NewLine);
        }
        self.bindings
            .iter()
            .find(|(_, cs)| cs.iter().any(|c| c.matches(key)))
            .map(|(a, _)| *a)
    }

    pub fn chords(&self, a: Action) -> &[Chord] {
        self.bindings
            .iter()
            .find(|(x, _)| *x == a)
            .map_or(&[], |(_, c)| c.as_slice())
    }

    /// First binding as header text (`ctrl+o`).
    pub fn key(&self, a: Action) -> String {
        self.chords(a)
            .first()
            .map(|c| c.text(false))
            .unwrap_or_default()
    }

    /// First binding as selector-hint text (`Alt+Up`).
    pub fn display(&self, a: Action) -> String {
        self.chords(a)
            .first()
            .map(|c| c.text(true))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn defaults_resolve() {
        let m = Keymap::new();
        assert_eq!(
            m.resolve(&k(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            Some(Action::ToolsExpand)
        );
        assert_eq!(
            m.resolve(&k(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Some(Action::ThinkingCycle)
        );
        assert_eq!(
            m.resolve(&k(KeyCode::Enter, KeyModifiers::ALT)),
            Some(Action::FollowUp)
        );
        assert_eq!(
            m.resolve(&k(KeyCode::Enter, KeyModifiers::SHIFT)),
            Some(Action::NewLine)
        );
        assert_eq!(
            m.resolve(&k(KeyCode::Up, KeyModifiers::CONTROL | KeyModifiers::SHIFT)),
            Some(Action::PreviousPrompt)
        );
        assert_eq!(m.resolve(&k(KeyCode::Char('x'), KeyModifiers::NONE)), None);
    }

    #[test]
    fn display_text() {
        let m = Keymap::new();
        assert_eq!(m.key(Action::ToolsExpand), "ctrl+o");
        assert_eq!(m.display(Action::Dequeue), "Alt+Up");
        assert_eq!(m.key(Action::Interrupt), "escape");
        assert_eq!(m.key(Action::ThinkingCycle), "shift+tab");
    }
}
