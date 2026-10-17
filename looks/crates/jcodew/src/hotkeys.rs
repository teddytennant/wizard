//! The note jcode shows when a chord is pressed that the user has not got used to yet
//! (`⌨ Ctrl+G → toggle the scroll bookmark`), and when a chord does nothing.

use std::collections::HashMap;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::App;

/// After this many uses an action is familiar and its note stops.
const FAMILIAR_USES: u32 = 4;
const NOTE_TIME: Duration = Duration::from_secs(5);
const UNKNOWN_GAP: Duration = Duration::from_millis(1200);
const UNKNOWN_MAX: u32 = 3;

struct Hotkey {
    code: KeyCode,
    mods: KeyModifiers,
    action: &'static str,
    description: &'static str,
    /// `Some(true)`: only with an empty input; `Some(false)`: only with text in it.
    empty: Option<bool>,
    quiet: bool,
}

const C: KeyModifiers = KeyModifiers::CONTROL;
const A: KeyModifiers = KeyModifiers::ALT;

const fn hk(
    code: KeyCode,
    mods: KeyModifiers,
    action: &'static str,
    description: &'static str,
) -> Hotkey {
    Hotkey {
        code,
        mods,
        action,
        description,
        empty: None,
        quiet: false,
    }
}

const REGISTRY: &[Hotkey] = &[
    Hotkey {
        empty: Some(true),
        ..hk(
            KeyCode::Char('k'),
            C,
            "prompt_jump_up",
            "jump to the previous prompt",
        )
    },
    Hotkey {
        empty: Some(false),
        ..hk(
            KeyCode::Char('k'),
            C,
            "kill_to_end",
            "delete to the end of the input",
        )
    },
    hk(
        KeyCode::Char('j'),
        C,
        "prompt_jump_down",
        "jump to the next prompt",
    ),
    hk(
        KeyCode::Char('g'),
        C,
        "scroll_bookmark",
        "toggle the scroll bookmark",
    ),
    hk(
        KeyCode::Char('u'),
        A,
        "scroll_page_up",
        "scroll the chat up a page",
    ),
    hk(
        KeyCode::Char('d'),
        A,
        "scroll_page_down",
        "scroll the chat down a page",
    ),
    hk(
        KeyCode::Char('l'),
        C,
        "clear_screen",
        "clear the screen; history stays in scrollback",
    ),
    hk(
        KeyCode::Char('a'),
        C,
        "input_home",
        "jump to the start of the input",
    ),
    hk(
        KeyCode::Char('e'),
        C,
        "input_end",
        "jump to the end of the input",
    ),
    hk(KeyCode::Char('b'), C, "word_back", "move back one word"),
    hk(
        KeyCode::Char('f'),
        C,
        "word_forward",
        "move forward one word",
    ),
    hk(
        KeyCode::Char('u'),
        C,
        "kill_to_start",
        "delete to the start of the input",
    ),
    hk(
        KeyCode::Char('w'),
        C,
        "delete_word_back",
        "delete the previous word",
    ),
    hk(KeyCode::Char('z'), C, "input_undo", "undo input edit"),
    hk(
        KeyCode::Char('s'),
        C,
        "input_stash",
        "stash or restore the input draft",
    ),
    Hotkey {
        quiet: true,
        ..hk(
            KeyCode::Char('c'),
            C,
            "interrupt",
            "interrupt (or quit when idle)",
        )
    },
    Hotkey {
        quiet: true,
        ..hk(
            KeyCode::Char('d'),
            C,
            "interrupt",
            "interrupt (or quit when idle)",
        )
    },
    hk(
        KeyCode::Up,
        C,
        "prompt_recall",
        "recall queued prompt / prompt history",
    ),
    Hotkey {
        quiet: true,
        ..hk(
            KeyCode::Down,
            C,
            "prompt_recall",
            "walk prompt history forward",
        )
    },
    hk(KeyCode::Char('b'), A, "word_back", "move back one word"),
    hk(
        KeyCode::Char('f'),
        A,
        "word_forward",
        "move forward one word",
    ),
    hk(
        KeyCode::Backspace,
        A,
        "delete_word_back",
        "delete the previous word",
    ),
    hk(
        KeyCode::Right,
        A,
        "effort_increase",
        "raise reasoning effort",
    ),
    hk(
        KeyCode::Left,
        A,
        "effort_decrease",
        "lower reasoning effort",
    ),
    hk(
        KeyCode::Char('r'),
        A,
        "open_resume",
        "open the session picker",
    ),
    hk(
        KeyCode::Char('x'),
        A,
        "todo_card_toggle",
        "show/dismiss the inline todo card",
    ),
];

/// `Ctrl+K`, `Alt+Left`, `Ctrl+Shift+P`.
pub fn label(code: KeyCode, mods: KeyModifiers) -> String {
    let mut parts: Vec<String> = Vec::new();
    if mods.contains(KeyModifiers::CONTROL) {
        parts.push("Ctrl".into());
    }
    if mods.contains(KeyModifiers::ALT) {
        parts.push("Alt".into());
    }
    if mods.contains(KeyModifiers::SHIFT) {
        parts.push("Shift".into());
    }
    parts.push(match code {
        KeyCode::Char(c) => c.to_ascii_uppercase().to_string(),
        KeyCode::F(n) => format!("F{n}"),
        other => format!("{other:?}"),
    });
    parts.join("+")
}

fn lookup(code: KeyCode, mods: KeyModifiers, empty: bool) -> Option<&'static Hotkey> {
    REGISTRY
        .iter()
        .find(|h| h.code == code && h.mods == mods && h.empty.is_none_or(|e| e == empty))
}

pub fn load(dir: Option<&std::path::Path>) -> HashMap<String, u32> {
    dir.and_then(|d| std::fs::read_to_string(d.join("hotkeys.json")).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save(app: &App) {
    if let Some(dir) = app.opts.state_dir.as_deref() {
        let _ = std::fs::create_dir_all(dir);
        if let Ok(t) = serde_json::to_string(&app.hotkey_uses) {
            let _ = std::fs::write(dir.join("hotkeys.json"), t);
        }
    }
}

fn note(app: &mut App, text: String) {
    app.hotkey_note = Some((text, app.now() + NOTE_TIME));
    app.dirty = true;
}

/// Called before a key is handled. `listing` is true while the command list is open, when
/// Ctrl+J and Ctrl+K move its selection instead.
pub fn observe(app: &mut App, key: &KeyEvent, listing: bool) {
    let mods = key.modifiers;
    if mods.is_empty() || mods == KeyModifiers::SHIFT || listing {
        return;
    }
    if !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
        return;
    }
    match lookup(key.code, mods, app.ed.is_empty()) {
        Some(h) => {
            let uses = app.hotkey_uses.entry(h.action.to_string()).or_insert(0);
            let unfamiliar = *uses < FAMILIAR_USES;
            *uses += 1;
            if unfamiliar {
                save(app);
                if !h.quiet {
                    note(
                        app,
                        format!("⌨ {} → {}", label(h.code, h.mods), h.description),
                    );
                }
            }
        }
        None if matches!(key.code, KeyCode::Char(_) | KeyCode::F(_)) => {
            let now = app.now();
            if app.last_unknown_note.is_some_and(|t| now < t + UNKNOWN_GAP) {
                return;
            }
            let chord = label(key.code, mods);
            let seen = app.unknown_notes.entry(chord.clone()).or_insert(0);
            if *seen >= UNKNOWN_MAX {
                return;
            }
            *seen += 1;
            app.last_unknown_note = Some(now);
            let nearest = REGISTRY.iter().find(|h| {
                h.code == key.code
                    && !h.quiet
                    && h.mods != mods
                    && h.empty.is_none_or(|e| e == app.ed.is_empty())
            });
            let text = match nearest {
                Some(h) => format!(
                    "⌨ {chord} isn't bound · nearest: {} → {}",
                    label(h.code, h.mods),
                    h.description
                ),
                None => format!("⌨ {chord} isn't bound · /help lists shortcuts"),
            };
            note(app, text);
        }
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(label(KeyCode::Char('k'), C), "Ctrl+K");
        assert_eq!(label(KeyCode::Left, A), "Alt+Left");
    }
}
