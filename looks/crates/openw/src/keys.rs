//! Keybind table with opencode's defaults, parsed from strings like `ctrl+x l`.
//!
//! A binding is one chord (`ctrl+p`) or a leader sequence (`ctrl+x l`). The table only maps keys
//! to [`Action`]s; whether an action applies right now (exit only on an empty prompt, interrupt
//! only while busy) is decided by the app, which falls through to the editor when it declines.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Everything a key, a palette row or a slash command can ask for.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Leader,
    Exit,
    Palette,
    Suspend,
    TipsToggle,
    Editor,
    Themes,
    SidebarToggle,
    Status,
    Export,
    NewSession,
    Sessions,
    Timeline,
    Rename,
    Interrupt,
    Compact,
    PageUp,
    PageDown,
    HalfPageUp,
    HalfPageDown,
    LineUp,
    LineDown,
    First,
    Last,
    CopyLast,
    CopyTranscript,
    Undo,
    Redo,
    ToggleConceal,
    ToggleThinking,
    ToggleTimestamps,
    ToggleToolDetails,
    ToggleScrollbar,
    ToggleGenericOutput,
    Models,
    ModelCycle,
    ModelCycleReverse,
    Agents,
    AgentCycle,
    AgentCycleReverse,
    VariantCycle,
    /// `Switch model variant`: the dialog behind the effort levels.
    Variants,
    Help,
    Debug,
    Skills,
    Mcps,
    Connect,
    Diff,
    StashPush,
    StashPop,
    StashList,
    ToggleMode,
    /// Child sessions (subagent runs): enter the first one, back to the parent, previous, next.
    ChildFirst,
    ChildParent,
    ChildPrev,
    ChildNext,
    Fork,
    Share,
    Plugins,
    InstallPlugin,
    LockThemeMode,
    OpenDocs,
    /// Entries opencode has for its own debugging; here they say they are not available.
    DebugPanel,
    Console,
    HeapSnapshot,
    ToggleTerminalTitle,
    ToggleAnimations,
    /// A setting wizard has no equivalent of; the palette lists it as opencode does.
    Unsupported(&'static str),
    MoveSession,
    /// Open the n-th pinned session (`ctrl+x 1` to `ctrl+x 9`).
    SwitchPinned(u8),
    /// Send `/name` to the backend as a prompt.
    Backend(String),
    /// Put `/name ` in the prompt and let the user finish it.
    InsertSlash(String),
    /// A slash command that is a prompt template (`/init`, `/review`), as in opencode.
    Template(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chord {
    pub code: KeyCode,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Chord {
    pub fn parse(s: &str) -> Option<Chord> {
        let mut c = Chord {
            code: KeyCode::Null,
            ctrl: false,
            alt: false,
            shift: false,
        };
        let mut parts: Vec<&str> = s.split('+').collect();
        // `ctrl++` style is not used by any default, so a trailing empty part is an error.
        let key = parts.pop()?;
        for m in parts {
            match m {
                "ctrl" => c.ctrl = true,
                "alt" | "meta" => c.alt = true,
                "shift" => c.shift = true,
                _ => return None,
            }
        }
        c.code = match key {
            "escape" | "esc" => KeyCode::Esc,
            "return" | "enter" => KeyCode::Enter,
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
            k if k.len() > 1
                && k.starts_with('f')
                && k[1..].chars().all(|c| c.is_ascii_digit()) =>
            {
                KeyCode::F(k[1..].parse().ok()?)
            }
            k if k.chars().count() == 1 => KeyCode::Char(k.chars().next()?),
            _ => return None,
        };
        if c.code == KeyCode::Tab && c.shift {
            c.code = KeyCode::BackTab;
            c.shift = false;
        }
        Some(c)
    }

    pub fn matches(&self, ev: &KeyEvent) -> bool {
        let m = ev.modifiers;
        let ctrl = m.contains(KeyModifiers::CONTROL);
        let alt = m.contains(KeyModifiers::ALT);
        let shift = m.contains(KeyModifiers::SHIFT);
        let code_eq = match (self.code, ev.code) {
            (KeyCode::Char(a), KeyCode::Char(b)) => a.eq_ignore_ascii_case(&b),
            (a, b) => a == b,
        };
        if !code_eq || ctrl != self.ctrl || alt != self.alt {
            return false;
        }
        match self.code {
            // ctrl+shift+z is the prompt's redo, not ctrl+z's suspend. Terminals that cannot tell
            // them apart never set shift, so this only matters where they can.
            KeyCode::Char(c) if ctrl && shift && !self.shift && c.is_alphabetic() => false,
            // Shift is already part of the character, and BackTab always carries it.
            KeyCode::Char(_) | KeyCode::BackTab => true,
            _ => shift == self.shift,
        }
    }

    pub fn display(&self) -> String {
        let mut s = String::new();
        if self.ctrl {
            s.push_str("ctrl+");
        }
        if self.alt {
            s.push_str("alt+");
        }
        if self.shift || self.code == KeyCode::BackTab {
            s.push_str("shift+");
        }
        match self.code {
            KeyCode::Esc => s.push_str("esc"),
            KeyCode::Enter => s.push_str("enter"),
            KeyCode::Tab | KeyCode::BackTab => s.push_str("tab"),
            KeyCode::Backspace => s.push_str("backspace"),
            KeyCode::Delete => s.push_str("del"),
            KeyCode::Up => s.push_str("up"),
            KeyCode::Down => s.push_str("down"),
            KeyCode::Left => s.push_str("left"),
            KeyCode::Right => s.push_str("right"),
            KeyCode::Home => s.push_str("home"),
            KeyCode::End => s.push_str("end"),
            KeyCode::PageUp => s.push_str("pgup"),
            KeyCode::PageDown => s.push_str("pgdn"),
            KeyCode::F(n) => s.push_str(&format!("f{n}")),
            KeyCode::Char(' ') => s.push_str("space"),
            KeyCode::Char(c) => s.push(c),
            _ => s.push('?'),
        }
        s
    }
}

/// One way to trigger an action: a chord, or the leader chord followed by one.
#[derive(Clone, Debug, PartialEq)]
pub struct Seq(pub Vec<Chord>);

impl Seq {
    pub fn parse(s: &str) -> Option<Seq> {
        let v: Option<Vec<Chord>> = s.split_whitespace().map(Chord::parse).collect();
        v.filter(|v| !v.is_empty() && v.len() <= 2).map(Seq)
    }

    pub fn display(&self) -> String {
        self.0
            .iter()
            .map(Chord::display)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Resolve {
    Action(Action),
    /// The leader key was pressed; the next key completes the sequence.
    Pending,
    None,
}

/// opencode's defaults, `<leader>` written out. Order is the order the palette lists keys in.
type Default_ = (fn() -> Action, &'static [&'static str]);
const DEFAULTS: &[Default_] = &[
    (|| Action::Exit, &["ctrl+c", "ctrl+d", "ctrl+x q"]),
    (|| Action::Palette, &["ctrl+p"]),
    (|| Action::Suspend, &["ctrl+z"]),
    (|| Action::TipsToggle, &["ctrl+x h"]),
    (|| Action::Editor, &["ctrl+x e"]),
    (|| Action::Themes, &["ctrl+x t"]),
    (|| Action::SidebarToggle, &["ctrl+x b"]),
    (|| Action::Status, &["ctrl+x s"]),
    (|| Action::Export, &["ctrl+x x"]),
    (|| Action::NewSession, &["ctrl+x n"]),
    (|| Action::Sessions, &["ctrl+x l"]),
    (|| Action::Timeline, &["ctrl+x g"]),
    (|| Action::Rename, &["ctrl+r"]),
    (|| Action::Interrupt, &["escape"]),
    (|| Action::Compact, &["ctrl+x c"]),
    (|| Action::PageUp, &["pageup", "ctrl+alt+b"]),
    (|| Action::PageDown, &["pagedown", "ctrl+alt+f"]),
    (|| Action::LineUp, &["ctrl+alt+y"]),
    (|| Action::LineDown, &["ctrl+alt+e"]),
    (|| Action::HalfPageUp, &["ctrl+alt+u"]),
    (|| Action::HalfPageDown, &["ctrl+alt+d"]),
    (|| Action::First, &["ctrl+g", "home"]),
    (|| Action::Last, &["ctrl+alt+g", "end"]),
    (|| Action::CopyLast, &["ctrl+x y"]),
    (|| Action::Undo, &["ctrl+x u"]),
    (|| Action::Redo, &["ctrl+x r"]),
    (|| Action::ToggleConceal, &["ctrl+x h"]),
    (|| Action::Models, &["ctrl+x m"]),
    (|| Action::ModelCycle, &["f2"]),
    (|| Action::ModelCycleReverse, &["shift+f2"]),
    (|| Action::Agents, &["ctrl+x a"]),
    (|| Action::AgentCycle, &["tab"]),
    (|| Action::AgentCycleReverse, &["shift+tab"]),
    (|| Action::VariantCycle, &["ctrl+t"]),
    (|| Action::ChildFirst, &["ctrl+x down"]),
    (|| Action::ChildParent, &["up"]),
    (|| Action::ChildPrev, &["left"]),
    (|| Action::ChildNext, &["right"]),
    (|| Action::SwitchPinned(1), &["ctrl+x 1"]),
    (|| Action::SwitchPinned(2), &["ctrl+x 2"]),
    (|| Action::SwitchPinned(3), &["ctrl+x 3"]),
    (|| Action::SwitchPinned(4), &["ctrl+x 4"]),
    (|| Action::SwitchPinned(5), &["ctrl+x 5"]),
    (|| Action::SwitchPinned(6), &["ctrl+x 6"]),
    (|| Action::SwitchPinned(7), &["ctrl+x 7"]),
    (|| Action::SwitchPinned(8), &["ctrl+x 8"]),
    (|| Action::SwitchPinned(9), &["ctrl+x 9"]),
];

pub struct Keymap {
    pub leader: Chord,
    bindings: Vec<(Action, Vec<Seq>)>,
}

impl Default for Keymap {
    fn default() -> Self {
        Self::new()
    }
}

impl Keymap {
    pub fn new() -> Self {
        let bindings = DEFAULTS
            .iter()
            .map(|(a, keys)| (a(), keys.iter().filter_map(|k| Seq::parse(k)).collect()))
            .collect();
        Keymap {
            leader: Chord::parse("ctrl+x").expect("leader parses"),
            bindings,
        }
    }

    /// Resolve one key. `pending` is true right after the leader.
    ///
    /// `ctrl+x h` is bound to both tips and conceal; the caller picks by screen, so this returns
    /// the first match in table order (conceal wins in a session, tips on home are handled by the
    /// app before it asks).
    pub fn resolve(&self, pending: bool, ev: &KeyEvent) -> Resolve {
        if pending {
            for (a, seqs) in &self.bindings {
                for s in seqs {
                    if s.0.len() == 2 && s.0[1].matches(ev) {
                        return Resolve::Action(a.clone());
                    }
                }
            }
            return Resolve::None;
        }
        if self.leader.matches(ev) {
            return Resolve::Pending;
        }
        for (a, seqs) in &self.bindings {
            for s in seqs {
                if s.0.len() == 1 && s.0[0].matches(ev) {
                    return Resolve::Action(a.clone());
                }
            }
        }
        Resolve::None
    }

    /// Like [`resolve`](Self::resolve) after the leader, but for the `h` key: tips on home,
    /// conceal in a session.
    pub fn leader_h(&self, home: bool) -> Action {
        if home {
            Action::TipsToggle
        } else {
            Action::ToggleConceal
        }
    }

    pub fn seqs(&self, action: &Action) -> Vec<&Seq> {
        self.bindings
            .iter()
            .filter(|(a, _)| a == action)
            .flat_map(|(_, s)| s.iter())
            .collect()
    }

    /// Palette text: `ctrl+x l`, or `ctrl+c, ctrl+d, ctrl+x q` for several.
    pub fn display(&self, action: &Action) -> String {
        self.seqs(action)
            .iter()
            .map(|s| s.display())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// First binding only, for hints like `tab agents`.
    pub fn first(&self, action: &Action) -> String {
        self.seqs(action)
            .first()
            .map(|s| s.display())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    #[test]
    fn leader_sequences_resolve() {
        let km = Keymap::new();
        let ctrl_x = key(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert_eq!(km.resolve(false, &ctrl_x), Resolve::Pending);
        assert_eq!(
            km.resolve(true, &key(KeyCode::Char('l'), KeyModifiers::NONE)),
            Resolve::Action(Action::Sessions)
        );
        assert_eq!(
            km.resolve(true, &key(KeyCode::Char('q'), KeyModifiers::NONE)),
            Resolve::Action(Action::Exit)
        );
        assert_eq!(
            km.resolve(true, &key(KeyCode::Char('z'), KeyModifiers::NONE)),
            Resolve::None
        );
    }

    #[test]
    fn plain_chords_and_display() {
        let km = Keymap::new();
        assert_eq!(
            km.resolve(false, &key(KeyCode::Char('p'), KeyModifiers::CONTROL)),
            Resolve::Action(Action::Palette)
        );
        assert_eq!(
            km.resolve(false, &key(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Resolve::Action(Action::AgentCycleReverse)
        );
        assert_eq!(km.display(&Action::Exit), "ctrl+c, ctrl+d, ctrl+x q");
        assert_eq!(km.display(&Action::Sessions), "ctrl+x l");
        assert_eq!(km.display(&Action::PageUp), "pgup, ctrl+alt+b");
        assert_eq!(km.first(&Action::AgentCycle), "tab");
    }

    #[test]
    fn plain_letters_do_not_match_bound_chords() {
        let km = Keymap::new();
        assert_eq!(
            km.resolve(false, &key(KeyCode::Char('p'), KeyModifiers::NONE)),
            Resolve::None
        );
    }
}
