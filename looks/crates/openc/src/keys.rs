// OWNER: input
//! The keymap as data. The help overlay is generated from [`BINDINGS`], so it cannot drift
//! from what the keys actually do. Modal navigation lives in the `Nav` scope; the composer
//! keeps readline keys that the editor handles itself.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    Global,
    Composer,
    /// Readline editing in the composer. The editor handles these itself; they are listed so
    /// the help is complete and a test can check the editor really does handle each one.
    Edit,
    /// While a slash or file popup is open above the composer.
    Popup,
    Nav,
    /// Every list dialog: palette, sessions, history, themes.
    Dialog,
    Resume,
    Settings,
}

impl Scope {
    pub const ALL: [Scope; 8] = [
        Scope::Global,
        Scope::Composer,
        Scope::Edit,
        Scope::Popup,
        Scope::Nav,
        Scope::Dialog,
        Scope::Resume,
        Scope::Settings,
    ];

    /// Heading in the help overlay.
    pub fn title(self) -> &'static str {
        match self {
            Scope::Global => "Everywhere",
            Scope::Composer => "Composer",
            Scope::Edit => "Editing",
            Scope::Popup => "Completion popup (/ and @)",
            Scope::Nav => "Nav mode (esc to enter)",
            Scope::Dialog => "Lists (palette, history, themes)",
            Scope::Resume => "Resume dialog",
            Scope::Settings => "Turn settings (ctrl+t)",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    // global
    CtrlC,
    CtrlD,
    Esc,
    Palette,
    Settings,
    CycleMode,
    Detail,
    Rail,
    Repaint,
    Suspend,
    Help,
    PageUp,
    PageDown,
    // composer
    Send,
    Newline,
    Steer,
    HistorySearch,
    ExternalEditor,
    PasteImage,
    Complete,
    /// Up on an empty composer: take the newest queued message back for editing.
    PullQueue,
    /// Home and end on an empty composer scroll the transcript.
    ScrollTop,
    ScrollBottom,
    /// Handled by the editor, the popup or a dialog, not by `App::act`.
    Local,
    // nav
    NavDown,
    NavUp,
    HalfDown,
    HalfUp,
    NavTop,
    NavBottom,
    PrevTurn,
    NextTurn,
    PrevTool,
    NextTool,
    Toggle,
    ToggleTurn,
    CopyBlock,
    CopyTurn,
    Pager,
    DiffView,
    Rewind,
    FocusAgent,
    Search,
    SearchNext,
    SearchPrev,
    ToComposer,
}

pub struct Binding {
    pub scope: Scope,
    /// Alternatives separated by a space, e.g. `"j down"`.
    pub keys: &'static str,
    pub action: Action,
    pub help: &'static str,
}

const fn b(scope: Scope, keys: &'static str, action: Action, help: &'static str) -> Binding {
    Binding {
        scope,
        keys,
        action,
        help,
    }
}

use Action as A;
use Scope::{
    Composer as C, Dialog as D, Edit as E, Global as G, Nav as N, Popup as P, Resume as R,
    Settings as S,
};

pub const BINDINGS: &[Binding] = &[
    b(
        G,
        "ctrl+c",
        A::CtrlC,
        "interrupt; clear the draft; twice to quit",
    ),
    b(G, "ctrl+d", A::CtrlD, "quit when the composer is empty"),
    b(
        G,
        "esc",
        A::Esc,
        "close; busy: arm interrupt; idle: nav mode",
    ),
    b(G, "ctrl+p", A::Palette, "command palette"),
    b(G, "ctrl+t", A::Settings, "model, effort and mode"),
    b(
        G,
        "shift+tab",
        A::CycleMode,
        "cycle permission mode: ask, edits, plan",
    ),
    b(G, "ctrl+o", A::Detail, "expand every tool body and thought"),
    b(G, "ctrl+\\", A::Rail, "toggle the right rail"),
    b(G, "ctrl+l", A::Repaint, "repaint the screen"),
    b(G, "ctrl+z", A::Suspend, "suspend to the shell"),
    b(G, "f1", A::Help, "this help"),
    b(G, "pgup", A::PageUp, "scroll up a page"),
    b(G, "pgdn", A::PageDown, "scroll down a page"),
    b(C, "enter", A::Send, "send; while busy, queue"),
    b(
        C,
        "ctrl+j shift+enter alt+enter",
        A::Newline,
        "newline (or \\ then enter)",
    ),
    b(
        C,
        "ctrl+s",
        A::Steer,
        "steer: send now, after the running tool",
    ),
    b(C, "ctrl+r", A::HistorySearch, "search history"),
    b(C, "ctrl+g", A::ExternalEditor, "edit in $VISUAL or $EDITOR"),
    b(C, "ctrl+v", A::PasteImage, "paste a clipboard image"),
    b(C, "tab", A::Complete, "accept the popup selection"),
    b(
        C,
        "up",
        A::PullQueue,
        "empty composer: take back the last queued message",
    ),
    b(
        C,
        "home",
        A::ScrollTop,
        "empty composer: top of the transcript",
    ),
    b(
        C,
        "end",
        A::ScrollBottom,
        "empty composer: bottom of the transcript",
    ),
    b(
        E,
        "up down",
        A::Local,
        "move in the text; at an edge walk history",
    ),
    b(E, "ctrl+a ctrl+e", A::Local, "start and end of line"),
    b(E, "alt+b alt+f", A::Local, "word left and right"),
    b(E, "ctrl+left ctrl+right", A::Local, "word left and right"),
    b(
        E,
        "ctrl+w alt+backspace",
        A::Local,
        "delete the word before the caret",
    ),
    b(E, "alt+d", A::Local, "delete the word after the caret"),
    b(
        E,
        "ctrl+u ctrl+k",
        A::Local,
        "delete to line start, to line end",
    ),
    b(
        E,
        "ctrl+y",
        A::Local,
        "yank what ctrl+u, ctrl+k or ctrl+w deleted",
    ),
    b(E, "ctrl+_", A::Local, "undo, a paste included"),
    b(
        E,
        "backspace",
        A::Local,
        "delete a character; a paste chip goes whole",
    ),
    b(P, "up down ctrl+p ctrl+n", A::Local, "move the selection"),
    b(
        P,
        "tab enter",
        A::Local,
        "accept; enter on a complete command sends",
    ),
    b(P, "esc", A::Local, "close until the token changes"),
    b(N, "j down", A::NavDown, "next block"),
    b(N, "k up", A::NavUp, "previous block"),
    b(N, "ctrl+d", A::HalfDown, "half page down"),
    b(N, "ctrl+u", A::HalfUp, "half page up"),
    b(N, "g home", A::NavTop, "top"),
    b(N, "G end", A::NavBottom, "bottom, back to the composer"),
    b(N, "[", A::PrevTurn, "previous user turn"),
    b(N, "]", A::NextTurn, "next user turn"),
    b(N, "{", A::PrevTool, "previous tool block"),
    b(N, "}", A::NextTool, "next tool block"),
    b(N, "o enter", A::Toggle, "fold or unfold"),
    b(N, "O", A::ToggleTurn, "fold or unfold the whole turn"),
    b(N, "y", A::CopyBlock, "copy the block"),
    b(N, "Y", A::CopyTurn, "copy the turn as markdown"),
    b(N, "e", A::Pager, "open in $PAGER"),
    b(N, "d", A::DiffView, "diff of the block or turn"),
    b(
        N,
        "r",
        A::Rewind,
        "rewind to this message, or edit and resend it",
    ),
    b(N, "a", A::FocusAgent, "next subagent, opened in full"),
    b(N, "/", A::Search, "search"),
    b(N, "n", A::SearchNext, "next match"),
    b(N, "N", A::SearchPrev, "previous match"),
    b(N, "i esc", A::ToComposer, "back to the composer"),
    b(N, "?", A::Help, "this help"),
    b(D, "up down ctrl+p ctrl+n", A::Local, "move"),
    b(D, "pgup pgdn", A::Local, "move half a page"),
    b(D, "enter", A::Local, "choose"),
    b(
        D,
        "tab",
        A::Local,
        "history: put the entry in the composer to edit",
    ),
    b(D, "esc", A::Local, "close"),
    b(R, "space", A::Local, "preview the session"),
    b(R, "ctrl+r", A::Local, "rename"),
    b(R, "ctrl+d", A::Local, "delete, then y to confirm"),
    b(R, "ctrl+a", A::Local, "all projects or this directory"),
    b(R, "ctrl+b", A::Local, "this branch only"),
    b(S, "left right", A::Local, "change the focused row"),
    b(S, "up down tab", A::Local, "move between rows"),
    b(S, "enter esc", A::Local, "close"),
];

fn code_of(name: &str) -> Option<KeyCode> {
    Some(match name {
        "enter" => KeyCode::Enter,
        "esc" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "pgup" => KeyCode::PageUp,
        "pgdn" => KeyCode::PageDown,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "backspace" => KeyCode::Backspace,
        "space" => KeyCode::Char(' '),
        "f1" => KeyCode::F(1),
        s if s.chars().count() == 1 => KeyCode::Char(s.chars().next()?),
        _ => return None,
    })
}

/// Does `key` match one spec such as `ctrl+p`, `shift+tab`, `G`?
pub fn matches(spec: &str, key: &KeyEvent) -> bool {
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut name = spec;
    // `ctrl+\` and `ctrl++` end in a separator, so peel modifiers by prefix.
    loop {
        if let Some(r) = name.strip_prefix("ctrl+") {
            ctrl = true;
            name = r;
        } else if let Some(r) = name.strip_prefix("alt+") {
            alt = true;
            name = r;
        } else if let Some(r) = name.strip_prefix("shift+") {
            shift = true;
            name = r;
        } else {
            break;
        }
    }
    let m = key.modifiers;
    if ctrl != m.contains(KeyModifiers::CONTROL) || alt != m.contains(KeyModifiers::ALT) {
        return false;
    }
    if name == "tab" && shift {
        return key.code == KeyCode::BackTab
            || (key.code == KeyCode::Tab && m.contains(KeyModifiers::SHIFT));
    }
    // ctrl+\ arrives as ctrl+4 in raw mode (0x1c).
    if ctrl && name == "\\" {
        return matches!(key.code, KeyCode::Char('\\' | '4'));
    }
    let Some(want) = code_of(name) else {
        return false;
    };
    match (want, key.code) {
        (KeyCode::Char(a), KeyCode::Char(b)) => {
            if ctrl {
                a.eq_ignore_ascii_case(&b)
            } else {
                // Shift is already in the character; do not demand it.
                a == b
            }
        }
        (w, k) => {
            let shift_ok = !shift || m.contains(KeyModifiers::SHIFT);
            w == k && shift_ok
        }
    }
}

pub fn lookup(scope: Scope, key: &KeyEvent) -> Option<Action> {
    BINDINGS
        .iter()
        .filter(|bd| bd.scope == scope)
        .find(|bd| bd.keys.split(' ').any(|k| matches(k, key)))
        .map(|bd| bd.action)
}

/// The key event a terminal would deliver for `spec`, for tests that press every binding.
pub fn event_for(spec: &str) -> Option<KeyEvent> {
    let mut mods = KeyModifiers::NONE;
    let mut name = spec;
    loop {
        if let Some(r) = name.strip_prefix("ctrl+") {
            mods |= KeyModifiers::CONTROL;
            name = r;
        } else if let Some(r) = name.strip_prefix("alt+") {
            mods |= KeyModifiers::ALT;
            name = r;
        } else if let Some(r) = name.strip_prefix("shift+") {
            mods |= KeyModifiers::SHIFT;
            name = r;
        } else {
            break;
        }
    }
    if name == "tab" && mods.contains(KeyModifiers::SHIFT) {
        return Some(KeyEvent::new(KeyCode::BackTab, mods));
    }
    let code = code_of(name)?;
    if let KeyCode::Char(c) = code {
        if c.is_ascii_uppercase() {
            mods |= KeyModifiers::SHIFT;
        }
    }
    Some(KeyEvent::new(code, mods))
}

/// The same key as `tmux send-keys` spells it (`C-p`, `BTab`, `M-Enter`).
pub fn tmux_name(spec: &str) -> Option<String> {
    let mut pre = String::new();
    let mut name = spec;
    loop {
        if let Some(r) = name.strip_prefix("ctrl+") {
            pre.push_str("C-");
            name = r;
        } else if let Some(r) = name.strip_prefix("alt+") {
            pre.push_str("M-");
            name = r;
        } else if let Some(r) = name.strip_prefix("shift+") {
            pre.push_str("S-");
            name = r;
        } else {
            break;
        }
    }
    let base = match name {
        "enter" => "Enter",
        "esc" => "Escape",
        "tab" if pre == "S-" => return Some("BTab".into()),
        "tab" => "Tab",
        "up" => "Up",
        "down" => "Down",
        "left" => "Left",
        "right" => "Right",
        "pgup" => "PgUp",
        "pgdn" => "PgDn",
        "home" => "Home",
        "end" => "End",
        "backspace" => "BSpace",
        "space" => "Space",
        "f1" => "F1",
        s if s.chars().count() == 1 => s,
        _ => return None,
    };
    Some(format!("{pre}{base}"))
}

/// First key of the binding for `action`, for hints.
pub fn key_for(action: Action) -> Option<&'static str> {
    BINDINGS
        .iter()
        .find(|bd| bd.action == action)
        .and_then(|bd| bd.keys.split(' ').next())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    #[test]
    fn chords_match() {
        assert!(matches(
            "ctrl+p",
            &k(KeyCode::Char('p'), KeyModifiers::CONTROL)
        ));
        assert!(!matches(
            "ctrl+p",
            &k(KeyCode::Char('p'), KeyModifiers::NONE)
        ));
        assert!(matches(
            "shift+tab",
            &k(KeyCode::BackTab, KeyModifiers::SHIFT)
        ));
        assert!(matches(
            "ctrl+\\",
            &k(KeyCode::Char('4'), KeyModifiers::CONTROL)
        ));
        assert!(matches("G", &k(KeyCode::Char('G'), KeyModifiers::SHIFT)));
        assert!(matches("G", &k(KeyCode::Char('G'), KeyModifiers::NONE)));
        assert!(!matches("g", &k(KeyCode::Char('G'), KeyModifiers::SHIFT)));
        assert!(matches("pgup", &k(KeyCode::PageUp, KeyModifiers::NONE)));
        assert!(matches(
            "shift+enter",
            &k(KeyCode::Enter, KeyModifiers::SHIFT)
        ));
    }

    #[test]
    fn scopes_do_not_leak() {
        let j = k(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(lookup(Scope::Nav, &j), Some(Action::NavDown));
        assert_eq!(lookup(Scope::Composer, &j), None);
        let cj = k(KeyCode::Char('j'), KeyModifiers::CONTROL);
        assert_eq!(lookup(Scope::Composer, &cj), Some(Action::Newline));
        assert_eq!(lookup(Scope::Nav, &cj), None);
        assert_eq!(
            lookup(Scope::Global, &k(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            Some(Action::Detail)
        );
    }

    #[test]
    fn every_binding_parses_and_round_trips_through_an_event() {
        for bd in BINDINGS {
            for spec in bd.keys.split(' ') {
                let ev = event_for(spec).unwrap_or_else(|| panic!("no event for {spec}"));
                // Keys the editor, popup or a dialog reads directly are not looked up here,
                // but their specs must still describe a real key.
                assert!(
                    tmux_name(spec).is_some(),
                    "{spec} has no tmux spelling, so the scripted run cannot press it"
                );
                if !matches!(
                    bd.scope,
                    Scope::Edit | Scope::Popup | Scope::Dialog | Scope::Resume | Scope::Settings
                ) {
                    assert!(matches(spec, &ev), "{spec} does not match its own event");
                }
            }
        }
    }

    #[test]
    fn no_key_is_bound_twice_in_one_scope() {
        for sc in Scope::ALL {
            let mut seen: Vec<&str> = Vec::new();
            for bd in BINDINGS.iter().filter(|b| b.scope == sc) {
                for spec in bd.keys.split(' ') {
                    // `enter` and `esc` legitimately appear twice in Settings and Popup rows
                    // that name different actions; a repeat of the same spec is the bug.
                    assert!(!seen.contains(&spec), "{spec} is bound twice in {sc:?}");
                    seen.push(spec);
                }
            }
        }
    }
}
