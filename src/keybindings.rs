//! `keybindings.json`, the way Claude Code reads it.
//!
//! The file lives at `~/.claude/keybindings.json` (and `.claude/keybindings.json` in the
//! project, which is read after it and wins; `~/.wizard/keybindings.json`, which
//! `/keybindings init` writes, is read before both):
//!
//! ```json
//! { "bindings": [ { "context": "Chat", "bindings": { "ctrl+k": "chat:stash", "ctrl+s": null } } ] }
//! ```
//!
//! A key maps to an action name, or to `null` to unbind a default. Keys are modifiers joined
//! with `+` (`ctrl`, `alt`/`opt`/`option`/`meta`, `shift`, `cmd`/`super`/`win`) and a key name;
//! a space between keystrokes makes a chord that must finish within three seconds. Unknown
//! actions and contexts are skipped with a warning, and the default binding stays.
//!
//! This file has no dependencies beyond `crossterm` and `serde_json`. The wizard TUI uses it
//! directly and the `claude` look in `looks/` compiles the same file (`#[path]`), so the two
//! read the file alike. Each TUI turns a resolved action into what it does; what it cannot do
//! it ignores, and the key then does nothing.

// Shared with the looks workspace, which is edition 2021 and has no `let` chains to collapse into.
#![allow(clippy::collapsible_if)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// How long the next step of a chord may take.
pub const CHORD_TIMEOUT: Duration = Duration::from_secs(3);

/// One keystroke: modifiers and a lowercase key name (`k`, `enter`, `escape`, `up`, `f5`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Stroke {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub sup: bool,
    pub key: String,
}

impl Stroke {
    /// How the key reads in a hint: `ctrl+x`, `shift+tab`.
    pub fn label(&self) -> String {
        let mut out = String::new();
        for (on, name) in [
            (self.ctrl, "ctrl"),
            (self.alt, "alt"),
            (self.shift, "shift"),
            (self.sup, "cmd"),
        ] {
            if on {
                out.push_str(name);
                out.push('+');
            }
        }
        out.push_str(&self.key);
        out
    }
}

impl From<KeyEvent> for Stroke {
    fn from(k: KeyEvent) -> Stroke {
        let m = k.modifiers;
        let mut shift = m.contains(KeyModifiers::SHIFT);
        let key = match k.code {
            KeyCode::Enter => "enter".to_string(),
            KeyCode::Esc => "escape".to_string(),
            KeyCode::Tab => "tab".to_string(),
            KeyCode::BackTab => {
                shift = true;
                "tab".to_string()
            }
            KeyCode::Backspace => "backspace".to_string(),
            KeyCode::Delete => "delete".to_string(),
            KeyCode::Up => "up".to_string(),
            KeyCode::Down => "down".to_string(),
            KeyCode::Left => "left".to_string(),
            KeyCode::Right => "right".to_string(),
            KeyCode::Home => "home".to_string(),
            KeyCode::End => "end".to_string(),
            KeyCode::PageUp => "pageup".to_string(),
            KeyCode::PageDown => "pagedown".to_string(),
            KeyCode::F(n) => format!("f{n}"),
            KeyCode::Char(' ') => "space".to_string(),
            KeyCode::Char(c) => {
                // a terminal that reports Shift+K as 'K' with no flag
                if c.is_uppercase() {
                    shift = true;
                }
                c.to_lowercase().collect()
            }
            _ => String::new(),
        };
        Stroke {
            ctrl: m.contains(KeyModifiers::CONTROL),
            alt: m.contains(KeyModifiers::ALT),
            shift,
            sup: m.contains(KeyModifiers::SUPER),
            key,
        }
    }
}

/// Parse one keystroke such as `ctrl+shift+k`.
pub fn parse_stroke(text: &str) -> Result<Stroke, String> {
    let mut st = Stroke {
        ctrl: false,
        alt: false,
        shift: false,
        sup: false,
        key: String::new(),
    };
    let lower = text.trim().to_lowercase();
    if lower.is_empty() {
        return Err("empty keystroke".into());
    }
    // a bare "+" is the plus key; "ctrl++" is ctrl and plus
    let parts: Vec<&str> = if lower == "+" {
        vec!["+"]
    } else if let Some(head) = lower.strip_suffix("++") {
        let mut v: Vec<&str> = head.split('+').collect();
        v.push("+");
        v
    } else {
        lower.split('+').collect()
    };
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            return Err(format!("`{text}` has an empty part"));
        }
        let last = i + 1 == parts.len();
        match *part {
            "ctrl" | "control" if !last => st.ctrl = true,
            "alt" | "opt" | "option" | "meta" if !last => st.alt = true,
            "shift" if !last => st.shift = true,
            "cmd" | "command" | "super" | "win" if !last => st.sup = true,
            name if last => {
                st.key = match name {
                    "esc" => "escape".into(),
                    "return" => "enter".into(),
                    "pgup" => "pageup".into(),
                    "pgdn" => "pagedown".into(),
                    "del" => "delete".into(),
                    other => other.into(),
                };
            }
            other => return Err(format!("unknown modifier `{other}` in `{text}`")),
        }
    }
    Ok(st)
}

/// Parse a chord: keystrokes separated by spaces.
pub fn parse_chord(text: &str) -> Result<Vec<Stroke>, String> {
    let steps: Vec<&str> = text.split_whitespace().collect();
    if steps.is_empty() {
        return Err("empty keystroke".into());
    }
    steps.into_iter().map(parse_stroke).collect()
}

/// The contexts a binding block may name.
pub const CONTEXTS: &[&str] = &[
    "Global",
    "Chat",
    "Autocomplete",
    "Settings",
    "Confirmation",
    "Tabs",
    "Help",
    "Transcript",
    "HistorySearch",
    "Task",
    "ThemePicker",
    "Attachments",
    "Footer",
    "MessageSelector",
    "DiffDialog",
    "DiffPanel",
    "ModelPicker",
    "EffortSlider",
    "Select",
    "Plugin",
    "AbovePrompt",
    "AbovePromptInput",
    "AbovePromptSelect",
    "Pane",
    "PaneField",
    "Agents",
    "Scroll",
    "Selection",
    "Voice",
];

/// Claude Code's default bindings: context, action, keys. These are what `null` can remove and
/// what a rebound action's hint replaces.
pub const DEFAULTS: &[(&str, &str, &[&str])] = &[
    ("Global", "app:interrupt", &["ctrl+c"]),
    ("Global", "app:exit", &["ctrl+d"]),
    ("Global", "app:redraw", &[]),
    ("Global", "app:toggleTodos", &["ctrl+t"]),
    ("Global", "app:toggleTranscript", &["ctrl+o"]),
    ("Global", "history:search", &["ctrl+r"]),
    ("Chat", "history:previous", &["up"]),
    ("Chat", "history:next", &["down"]),
    ("Chat", "chat:cancel", &["escape"]),
    ("Chat", "chat:clearInput", &["ctrl+l"]),
    ("Chat", "chat:clearScreen", &["cmd+k"]),
    ("Chat", "chat:killAgents", &["ctrl+x ctrl+k"]),
    ("Chat", "chat:cycleMode", &["shift+tab"]),
    ("Chat", "chat:modelPicker", &["alt+p"]),
    ("Chat", "chat:fastMode", &["alt+o"]),
    ("Chat", "chat:thinkingToggle", &["alt+t"]),
    ("Chat", "chat:submit", &["enter"]),
    ("Chat", "chat:queueSubmit", &["ctrl+x enter"]),
    ("Chat", "chat:sendNow", &["ctrl+enter", "ctrl+x ctrl+s"]),
    ("Chat", "chat:newline", &["ctrl+j"]),
    ("Chat", "chat:undo", &["ctrl+_", "ctrl+shift+-"]),
    ("Chat", "chat:externalEditor", &["ctrl+g", "ctrl+x ctrl+e"]),
    ("Chat", "chat:stash", &["ctrl+s"]),
    ("Chat", "chat:imagePaste", &["ctrl+v"]),
    ("Autocomplete", "autocomplete:accept", &["tab"]),
    ("Autocomplete", "autocomplete:dismiss", &["escape"]),
    ("Autocomplete", "autocomplete:previous", &["up"]),
    ("Autocomplete", "autocomplete:next", &["down"]),
    ("Confirmation", "confirm:yes", &["enter"]),
    ("Confirmation", "confirm:no", &["escape"]),
    ("Confirmation", "confirm:previous", &["up"]),
    ("Confirmation", "confirm:next", &["down"]),
    ("Confirmation", "confirm:nextField", &["tab"]),
    ("Confirmation", "confirm:toggle", &["space"]),
    ("Confirmation", "confirm:cycleMode", &["shift+tab"]),
    ("Transcript", "transcript:toggleShowAll", &["ctrl+e"]),
    ("Transcript", "transcript:exit", &["q", "ctrl+c", "escape"]),
    ("HistorySearch", "historySearch:next", &["ctrl+r"]),
    ("HistorySearch", "historySearch:accept", &["escape", "tab"]),
    ("HistorySearch", "historySearch:cancel", &["ctrl+c"]),
    ("HistorySearch", "historySearch:execute", &["enter"]),
    ("HistorySearch", "historySearch:cycleScope", &["ctrl+s"]),
    ("Task", "task:background", &["ctrl+b", "ctrl+x ctrl+b"]),
    ("Help", "help:dismiss", &["escape"]),
    ("Tabs", "tabs:next", &["tab", "right"]),
    ("Tabs", "tabs:previous", &["shift+tab", "left"]),
    ("Select", "select:next", &["down", "j", "ctrl+n"]),
    ("Select", "select:previous", &["up", "k", "ctrl+p"]),
    ("Select", "select:accept", &["enter"]),
    ("Select", "select:cancel", &["escape"]),
    ("Scroll", "scroll:pageUp", &["pageup"]),
    ("Scroll", "scroll:pageDown", &["pagedown"]),
    ("Scroll", "scroll:top", &["ctrl+home"]),
    ("Scroll", "scroll:bottom", &["ctrl+end"]),
];

/// Not rebindable: they are the terminal's own bytes.
const RESERVED: &[&str] = &["ctrl+c", "ctrl+d", "ctrl+m", "ctrl+[", "ctrl+i", "ctrl+h"];

/// Where a binding came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum From2 {
    Default,
    User,
}

#[derive(Debug, Clone)]
struct Binding {
    context: String,
    chord: Vec<Stroke>,
    action: Option<String>,
    from: From2,
}

/// What a key means once the file has been consulted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// A binding from the file names this action.
    Action(String),
    /// The file unbinds a default on this key: nothing happens.
    Unbound,
    /// The first step of a chord from the file: wait for the next.
    Pending,
    /// A chord from the file was started and the key does not continue it.
    Cancelled,
    /// No rule from the file: the TUI's own handling stands.
    Default,
}

/// The default and user bindings, with the chord in progress.
#[derive(Debug, Default)]
pub struct Keymap {
    bindings: Vec<Binding>,
    pending: Vec<Stroke>,
    pending_at: Option<Instant>,
    files: Vec<(PathBuf, Option<SystemTime>)>,
    pub warnings: Vec<String>,
}

fn is_action(name: &str) -> bool {
    name.starts_with("command:") && name.len() > "command:".len()
        || DEFAULTS.iter().any(|(_, a, _)| *a == name)
        || EXTRA_ACTIONS.contains(&name)
}

/// Actions that exist but have no default key.
const EXTRA_ACTIONS: &[&str] = &[
    "app:toggleBrief",
    "app:toggleTeammatePreview",
    "app:toggleTerminal",
    "app:globalSearch",
    "app:quickOpen",
    "chat:messageActions",
    "chat:undoCommand",
    "chat:redo",
    "scroll:lineUp",
    "scroll:lineDown",
    "scroll:halfPageUp",
    "scroll:halfPageDown",
    "scroll:fullPageUp",
    "scroll:fullPageDown",
    "selection:copy",
    "selection:clear",
    "footer:up",
    "footer:down",
    "footer:next",
    "footer:previous",
    "footer:openSelected",
    "footer:clearSelection",
    "modelPicker:decreaseEffort",
    "modelPicker:increaseEffort",
    "modelPicker:thisSessionOnly",
    "effortSlider:decreaseEffort",
    "effortSlider:increaseEffort",
    "settings:search",
    "settings:retry",
    "settings:close",
    "plugin:toggle",
    "plugin:install",
    "plugin:favorite",
    "voice:pushToTalk",
];

impl Keymap {
    /// The defaults only.
    pub fn new() -> Keymap {
        let mut bindings = Vec::new();
        for (ctx, action, keys) in DEFAULTS {
            for k in *keys {
                if let Ok(chord) = parse_chord(k) {
                    bindings.push(Binding {
                        context: (*ctx).into(),
                        chord,
                        action: Some((*action).into()),
                        from: From2::Default,
                    });
                }
            }
        }
        Keymap {
            bindings,
            ..Keymap::default()
        }
    }

    /// The user's file, then the project's, if they exist. Edits are picked up by
    /// [`Keymap::reload`].
    pub fn load(home: &Path, project: &Path) -> Keymap {
        let mut km = Keymap::new();
        // the file `/keybindings init` writes comes first, so Claude's own files win over it
        km.files = vec![
            (home.join(".wizard/keybindings.json"), None),
            (home.join(".claude/keybindings.json"), None),
            (project.join(".claude/keybindings.json"), None),
        ];
        km.read_files();
        km
    }

    fn read_files(&mut self) {
        self.bindings.retain(|b| b.from == From2::Default);
        self.warnings.clear();
        let mut files = std::mem::take(&mut self.files);
        for (path, stamp) in files.iter_mut() {
            *stamp = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            if stamp.is_none() {
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(text) => self.add_json(&text, &path.display().to_string()),
                Err(e) => self.warnings.push(format!("{}: {e}", path.display())),
            }
        }
        self.files = files;
    }

    /// Add the bindings in a file's text. Bad entries become warnings.
    pub fn add_json(&mut self, text: &str, name: &str) {
        let doc: serde_json::Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                self.warnings.push(format!("{name}: {e}"));
                return;
            }
        };
        let Some(blocks) = doc.get("bindings").and_then(|b| b.as_array()) else {
            self.warnings.push(format!("{name}: no `bindings` array"));
            return;
        };
        for block in blocks {
            let ctx = block.get("context").and_then(|c| c.as_str()).unwrap_or("");
            if !CONTEXTS.contains(&ctx) {
                self.warnings
                    .push(format!("{name}: unknown context `{ctx}`"));
                continue;
            }
            let Some(map) = block.get("bindings").and_then(|b| b.as_object()) else {
                continue;
            };
            for (key, value) in map {
                let action = match value {
                    serde_json::Value::Null => None,
                    serde_json::Value::String(a) if is_action(a) => Some(a.clone()),
                    serde_json::Value::String(a) => {
                        self.warnings
                            .push(format!("{name}: unknown action `{a}` for `{key}`"));
                        continue;
                    }
                    _ => {
                        self.warnings
                            .push(format!("{name}: `{key}` needs a string or null"));
                        continue;
                    }
                };
                let chord = match parse_chord(key) {
                    Ok(c) => c,
                    Err(e) => {
                        self.warnings.push(format!("{name}: {e}"));
                        continue;
                    }
                };
                if chord.len() == 1 && RESERVED.contains(&chord[0].label().as_str()) {
                    self.warnings
                        .push(format!("{name}: `{key}` cannot be rebound"));
                    continue;
                }
                self.bindings.push(Binding {
                    context: ctx.into(),
                    chord,
                    action,
                    from: From2::User,
                });
            }
        }
    }

    /// Re-read the files when one has changed. Cheap enough to call on every key.
    pub fn reload(&mut self) -> bool {
        let stale = self
            .files
            .iter()
            .any(|(p, was)| std::fs::metadata(p).and_then(|m| m.modified()).ok() != *was);
        if stale {
            self.read_files();
        }
        stale
    }

    /// True when the file adds a rule of its own, so the TUI can skip the lookup otherwise.
    pub fn has_user_rules(&self) -> bool {
        self.bindings.iter().any(|b| b.from == From2::User)
    }

    /// Resolve `stroke` in `contexts` (most specific first is not needed: the last rule wins).
    pub fn resolve(&mut self, contexts: &[&str], stroke: Stroke, now: Instant) -> Resolved {
        if let Some(at) = self.pending_at {
            if now.saturating_duration_since(at) > CHORD_TIMEOUT {
                self.pending.clear();
                self.pending_at = None;
            }
        }
        let was_pending = !self.pending.is_empty();
        let mut chord = std::mem::take(&mut self.pending);
        self.pending_at = None;
        chord.push(stroke);
        let active = |b: &&Binding| contexts.contains(&b.context.as_str());
        // the last rule that matches wins, so the file beats the defaults
        let exact = self
            .bindings
            .iter()
            .filter(active)
            .rfind(|b| b.chord == chord);
        let longer = self
            .bindings
            .iter()
            .filter(active)
            .filter(|b| b.from == From2::User && b.action.is_some())
            .any(|b| b.chord.len() > chord.len() && b.chord.starts_with(&chord));
        match exact {
            Some(b) if b.from == From2::User => match &b.action {
                Some(a) => Resolved::Action(a.clone()),
                None => Resolved::Unbound,
            },
            // the file started a chord on this key, or finished a default one
            _ if longer => {
                self.pending = chord;
                self.pending_at = Some(now);
                Resolved::Pending
            }
            Some(_) if !was_pending => Resolved::Default,
            _ if was_pending => Resolved::Cancelled,
            _ => Resolved::Default,
        }
    }

    /// The key the user's file gives `action`, if it does. A hint shows this over the TUI's
    /// own default, which is not always Claude's.
    pub fn user_shortcut(&self, contexts: &[&str], action: &str) -> Option<String> {
        self.bindings
            .iter()
            .rev()
            .filter(|b| b.from == From2::User && contexts.contains(&b.context.as_str()))
            .find(|b| b.action.as_deref() == Some(action))
            .map(|b| {
                b.chord
                    .iter()
                    .map(Stroke::label)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
    }

    /// Whether a default binding in `contexts` is on this key, so unbinding it should silence
    /// the key rather than leave the TUI's own handling in place.
    pub fn default_bound(&self, contexts: &[&str], stroke: &Stroke) -> bool {
        self.bindings.iter().any(|b| {
            b.from == From2::Default
                && contexts.contains(&b.context.as_str())
                && b.chord.first() == Some(stroke)
        })
    }

    /// The first key bound to `action`, as a hint reads it. `None` when it is unbound.
    pub fn shortcut(&self, contexts: &[&str], action: &str) -> Option<String> {
        self.bindings
            .iter()
            .rev()
            .filter(|b| contexts.contains(&b.context.as_str()))
            .find(|b| b.action.as_deref() == Some(action))
            .map(|b| {
                b.chord
                    .iter()
                    .map(Stroke::label)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
    }

    /// Every action name this reader accepts, for a TUI that wants to list them.
    pub fn actions() -> Vec<&'static str> {
        let mut v: Vec<&'static str> = DEFAULTS.iter().map(|(_, a, _)| *a).collect();
        v.extend_from_slice(EXTRA_ACTIONS);
        v.sort_unstable();
        v.dedup();
        v
    }

    /// For tests and a settings view: which action each default key runs.
    pub fn defaults_by_action() -> HashMap<&'static str, Vec<&'static str>> {
        let mut m: HashMap<&'static str, Vec<&'static str>> = HashMap::new();
        for (_, a, keys) in DEFAULTS {
            m.entry(a).or_default().extend(keys.iter().copied());
        }
        m
    }
}

#[cfg(test)]
#[path = "keybindings_tests.rs"]
mod tests;
