//! Multiplexer prefixes, and whether truecolor actually survives one.
//!
//! tmux swallows its prefix. A second press sends that key to the pane
//! (`send-prefix`), so a Wizard binding on the same key is the prefix pressed
//! twice, and the hint has to say so. Screen works the same way. Zellij has
//! no prefix unless `[ui] mux_prefix` names one.
//!
//! Inside tmux or screen, `COLORTERM=truecolor` is often inherited from a
//! shell whose multiplexer was not told about RGB. Truecolor is trusted only
//! when the multiplexer advertises it; otherwise the palette drops to 256.

use std::cell::RefCell;
use std::sync::Mutex;

use crate::config::{MuxMode, UiConfig};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MuxState {
    pub kind: MuxKind,
    pub prefixes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MuxKind {
    #[default]
    None,
    Tmux,
    Screen,
    Zellij,
}

thread_local! {
    static PINNED: RefCell<Option<MuxState>> = const { RefCell::new(None) };
}

static GLOBAL: Mutex<MuxState> = Mutex::new(MuxState {
    kind: MuxKind::None,
    prefixes: Vec::new(),
});

/// Install what this process should treat as the multiplexer prefix.
/// Called once when the TUI comes up. Tests pin a state instead.
pub fn install(config: &UiConfig) {
    let state = if config.mux == MuxMode::Off {
        MuxState::default()
    } else if let Some(prefix) = config
        .mux_prefix
        .as_deref()
        .map(str::trim)
        .filter(|prefix| !prefix.is_empty())
    {
        MuxState {
            kind: detect_kind(),
            prefixes: vec![normalize_label(prefix)],
        }
    } else {
        detect_live()
    };
    *GLOBAL.lock().unwrap_or_else(|err| err.into_inner()) = state;
}

/// Pin the prefix table for this thread. Dropping the guard clears it.
pub fn pin(state: MuxState) -> Pin {
    PINNED.with(|slot| *slot.borrow_mut() = Some(state));
    Pin
}

pub struct Pin;

impl Drop for Pin {
    fn drop(&mut self) {
        PINNED.with(|slot| *slot.borrow_mut() = None);
    }
}

fn current() -> MuxState {
    if let Some(state) = PINNED.with(|slot| slot.borrow().clone()) {
        return state;
    }
    GLOBAL.lock().unwrap_or_else(|err| err.into_inner()).clone()
}

/// The background-output hint, with the prefix doubled when it collides.
pub fn background_hint() -> String {
    format!("started in background · {} output", rewrite("ctrl+b"))
}

/// Double any prefix that appears as a chord in `text`. Idempotent.
pub fn rewrite(text: &str) -> String {
    let state = current();
    let mut out = text.to_string();
    for prefix in &state.prefixes {
        for form in chord_forms(prefix) {
            out = double_once(&out, &form);
        }
    }
    out
}

fn double_once(text: &str, form: &str) -> String {
    if form.is_empty() || !text.contains(form) {
        return text.to_string();
    }
    let doubled = format!("{form} {form}");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(form) {
        let after = at + form.len();
        let before_ok = rest[..at].chars().next_back().is_none_or(is_chord_boundary);
        let already = rest[after..].starts_with(&format!(" {form}"));
        let end = if already {
            after + 1 + form.len()
        } else {
            after
        };
        let after_ok = rest[end..].chars().next().is_none_or(is_chord_boundary);
        out.push_str(&rest[..at]);
        if before_ok && after_ok {
            out.push_str(&doubled);
            rest = &rest[end..];
        } else {
            out.push_str(form);
            rest = &rest[after..];
        }
    }
    out.push_str(rest);
    out
}

/// A chord ends at whitespace or at the punctuation a hint uses between keys.
/// `/` does not, so `Ctrl-W/U/K` stays one token.
fn is_chord_boundary(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, ',' | '.' | ';' | ':' | ')' | '(' | '·')
}

/// `ctrl+b`, `ctrl-b`, and `Ctrl-B`, the three ways the UI spells a chord.
fn chord_forms(label: &str) -> Vec<String> {
    let label = normalize_label(label);
    let mut forms = vec![label.clone()];
    if let Some(rest) = label.strip_prefix("ctrl+") {
        forms.push(format!("ctrl-{rest}"));
        forms.push(format!("Ctrl-{}", rest.to_ascii_uppercase()));
        forms.push(format!("Ctrl+{}", rest.to_ascii_uppercase()));
    }
    forms
}

fn normalize_label(raw: &str) -> String {
    let raw = raw.trim().to_ascii_lowercase().replace(' ', "");
    if let Some(ch) = raw
        .strip_prefix("ctrl+")
        .or_else(|| raw.strip_prefix("ctrl-"))
    {
        return format!("ctrl+{}", ch.trim_start_matches('+'));
    }
    if let Some(ch) = raw.strip_prefix("c-") {
        return format!("ctrl+{ch}");
    }
    raw
}

/// Parse one tmux key (`C-b`, `C-a`, `M-x`). Empty and `None` are no prefix.
pub fn parse_tmux_key(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("none") {
        return None;
    }
    let mut ctrl = false;
    let mut alt = false;
    let mut rest = raw;
    loop {
        if let Some(next) = rest.strip_prefix("C-") {
            ctrl = true;
            rest = next;
            continue;
        }
        if let Some(next) = rest.strip_prefix("M-") {
            alt = true;
            rest = next;
            continue;
        }
        break;
    }
    let mut chars = rest.chars();
    let ch = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    let ch = ch.to_ascii_lowercase();
    match (ctrl, alt) {
        (true, false) => Some(format!("ctrl+{ch}")),
        (false, true) => Some(format!("alt+{ch}")),
        (true, true) => Some(format!("ctrl+alt+{ch}")),
        (false, false) => Some(ch.to_string()),
    }
}

/// Screen's `escape` answer is `^Aa`: the command character, then the
/// literal that follows it.
pub fn parse_screen_escape(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let rest = raw.strip_prefix('^')?;
    let ch = rest.chars().next()?;
    Some(format!("ctrl+{}", ch.to_ascii_lowercase()))
}

pub fn prefixes_from(prefix: &str, prefix2: &str) -> Vec<String> {
    [prefix, prefix2]
        .into_iter()
        .filter_map(parse_tmux_key)
        .collect()
}

fn detect_kind() -> MuxKind {
    let set = |key: &str| std::env::var_os(key).is_some_and(|value| !value.is_empty());
    if set("TMUX") {
        MuxKind::Tmux
    } else if set("ZELLIJ") {
        MuxKind::Zellij
    } else if set("STY") {
        MuxKind::Screen
    } else {
        MuxKind::None
    }
}

fn detect_live() -> MuxState {
    match detect_kind() {
        MuxKind::Tmux => MuxState {
            kind: MuxKind::Tmux,
            prefixes: prefixes_from(&tmux_show("prefix"), &tmux_show("prefix2")),
        },
        MuxKind::Screen => MuxState {
            kind: MuxKind::Screen,
            prefixes: screen_escape().into_iter().collect(),
        },
        MuxKind::Zellij => MuxState {
            kind: MuxKind::Zellij,
            prefixes: Vec::new(),
        },
        MuxKind::None => MuxState::default(),
    }
}

fn tmux_show(option: &str) -> String {
    std::process::Command::new("tmux")
        .args(["show", "-gv", option])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default()
}

fn screen_escape() -> Option<String> {
    let output = std::process::Command::new("screen")
        .args(["-Q", "escape"])
        .output()
        .ok()?;
    if !output.status.success() {
        // Screen's default command character, when the session will not say.
        return Some("ctrl+a".to_string());
    }
    parse_screen_escape(&String::from_utf8_lossy(&output.stdout)).or(Some("ctrl+a".to_string()))
}

/// Whether a truecolor `COLORTERM` should be believed.
///
/// Outside a multiplexer, yes. Zellij passes 24-bit color. tmux and screen
/// do only when they advertise RGB; a bare `COLORTERM` inside them is the
/// outer shell's, and the pane is often 256.
pub fn trust_truecolor() -> bool {
    match detect_kind() {
        MuxKind::None | MuxKind::Zellij => true,
        MuxKind::Tmux => tmux_advertises_rgb(),
        MuxKind::Screen => term_claims_direct(),
    }
}

fn term_claims_direct() -> bool {
    std::env::var("TERM").is_ok_and(|term| {
        let term = term.to_ascii_lowercase();
        term.contains("truecolor") || term.contains("direct")
    })
}

fn tmux_advertises_rgb() -> bool {
    if term_claims_direct() {
        return true;
    }
    let features = tmux_show("terminal-features");
    let overrides = tmux_show("terminal-overrides");
    let terminal = tmux_show("default-terminal");
    [features, overrides, terminal]
        .iter()
        .any(|value| value.contains("RGB") || value.contains("Tc"))
}

/// Drop truecolor to 256 (or 16) when the multiplexer will not carry it.
pub fn degrade(depth: crate::theme::ColorDepth, term: Option<&str>) -> crate::theme::ColorDepth {
    let rgb = trust_truecolor();
    degrade_when(depth, term, rgb)
}

/// `rgb` is whether the multiplexer advertised 24-bit color. When it did not,
/// truecolor becomes 256 if `term` says so, and 16 otherwise. Mono and 16 are
/// left alone: `NO_COLOR` has already won by the time this runs.
pub fn degrade_when(
    depth: crate::theme::ColorDepth,
    term: Option<&str>,
    rgb: bool,
) -> crate::theme::ColorDepth {
    use crate::theme::ColorDepth;
    if depth != ColorDepth::TrueColor || rgb {
        return depth;
    }
    let term = term.unwrap_or("");
    if term.contains("256") {
        ColorDepth::Ansi256
    } else {
        ColorDepth::Ansi16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmux_keys_and_screen_escapes_parse() {
        assert_eq!(parse_tmux_key("C-b").as_deref(), Some("ctrl+b"));
        assert_eq!(parse_tmux_key("C-a").as_deref(), Some("ctrl+a"));
        assert_eq!(parse_tmux_key("M-x").as_deref(), Some("alt+x"));
        assert_eq!(parse_tmux_key("None"), None);
        assert_eq!(parse_tmux_key(""), None);
        assert_eq!(prefixes_from("C-b", "None"), vec!["ctrl+b".to_string()]);
        assert_eq!(prefixes_from("C-a", "C-b").len(), 2);
        assert_eq!(parse_screen_escape("^Aa").as_deref(), Some("ctrl+a"));
    }

    #[test]
    fn a_colliding_chord_is_doubled_and_a_free_one_is_not() {
        let _pin = pin(MuxState {
            kind: MuxKind::Tmux,
            prefixes: vec!["ctrl+b".to_string()],
        });
        assert_eq!(rewrite("ctrl+b output"), "ctrl+b ctrl+b output");
        assert_eq!(rewrite("Ctrl-B"), "Ctrl-B Ctrl-B");
        assert_eq!(
            rewrite("ctrl+b ctrl+b output"),
            "ctrl+b ctrl+b output",
            "doubling twice would stack the chord"
        );
        assert_eq!(rewrite("ctrl+o view"), "ctrl+o view");
        assert_eq!(
            background_hint(),
            "started in background · ctrl+b ctrl+b output"
        );
    }

    #[test]
    fn a_custom_prefix_leaves_ctrl_b_alone() {
        let _pin = pin(MuxState {
            kind: MuxKind::Tmux,
            prefixes: vec!["ctrl+a".to_string()],
        });
        assert_eq!(rewrite("ctrl+b output"), "ctrl+b output");
        assert_eq!(rewrite("Ctrl-A"), "Ctrl-A Ctrl-A");
    }

    #[test]
    fn mux_off_means_no_prefixes_to_double() {
        let _pin = pin(MuxState::default());
        assert_eq!(background_hint(), "started in background · ctrl+b output");
    }

    #[test]
    fn a_chord_inside_a_slash_group_is_not_a_binding() {
        let _pin = pin(MuxState {
            kind: MuxKind::Tmux,
            prefixes: vec!["ctrl+w".to_string()],
        });
        assert_eq!(rewrite("Ctrl-W/U/K kill word"), "Ctrl-W/U/K kill word");
        assert_eq!(rewrite("Ctrl-W kill word"), "Ctrl-W Ctrl-W kill word");
    }

    #[test]
    fn truecolor_falls_back_when_the_mux_does_not_advertise_rgb() {
        use crate::theme::ColorDepth;
        assert_eq!(
            degrade_when(ColorDepth::TrueColor, Some("tmux-256color"), false),
            ColorDepth::Ansi256
        );
        assert_eq!(
            degrade_when(ColorDepth::TrueColor, Some("screen"), false),
            ColorDepth::Ansi16
        );
        assert_eq!(
            degrade_when(ColorDepth::TrueColor, Some("tmux-256color"), true),
            ColorDepth::TrueColor
        );
        assert_eq!(
            degrade_when(ColorDepth::Mono, Some("tmux-256color"), false),
            ColorDepth::Mono,
            "NO_COLOR already decided, and a mux does not bring color back"
        );
    }

    /// Two real tmux servers, on private sockets so they do not touch the
    /// one this process may already be inside. The prefix each reports is
    /// what the hint doubles.
    #[test]
    fn a_live_tmux_prefix_is_the_chord_that_doubles() {
        if std::process::Command::new("tmux")
            .arg("-V")
            .output()
            .ok()
            .is_none_or(|output| !output.status.success())
        {
            // GitHub's Ubuntu image does not ship tmux. The parse and remap
            // tests above still run; this one needs a real server.
            eprintln!("tmux is not installed; skipping the live prefix check");
            return;
        }
        let default = live_prefix("default", None);
        assert_eq!(default.as_deref(), Some("C-b"), "tmux's own default");
        let _pin = pin(MuxState {
            kind: MuxKind::Tmux,
            prefixes: prefixes_from(default.as_deref().unwrap_or(""), "None"),
        });
        assert_eq!(rewrite("ctrl+b output"), "ctrl+b ctrl+b output");
        drop(_pin);

        let custom = live_prefix("ctrl-a", Some("C-a"));
        assert_eq!(custom.as_deref(), Some("C-a"));
        let _pin = pin(MuxState {
            kind: MuxKind::Tmux,
            prefixes: prefixes_from(custom.as_deref().unwrap_or(""), "None"),
        });
        assert_eq!(rewrite("ctrl+b output"), "ctrl+b output");
        assert_eq!(rewrite("Ctrl-A home"), "Ctrl-A Ctrl-A home");
    }

    fn live_prefix(label: &str, prefix: Option<&str>) -> Option<String> {
        let socket = format!("wizardmux{}{label}", std::process::id());
        let dir = std::env::temp_dir().join(format!("wizard-mux-{socket}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mux temp dir");
        let conf = dir.join("tmux.conf");
        let mut body = "set -g status off\n".to_string();
        if let Some(prefix) = prefix {
            body.push_str(&format!("set -g prefix {prefix}\nunbind C-b\n"));
        }
        std::fs::write(&conf, body).expect("tmux conf");
        let started = std::process::Command::new("tmux")
            .args([
                "-L",
                &socket,
                "-f",
                conf.to_str().expect("utf8 path"),
                "new-session",
                "-d",
                "-s",
                "p",
                "sleep",
                "30",
            ])
            .status()
            .expect("tmux is installed");
        assert!(started.success(), "tmux new-session");
        let output = std::process::Command::new("tmux")
            .args(["-L", &socket, "show", "-gv", "prefix"])
            .output()
            .expect("tmux show");
        let _ = std::process::Command::new("tmux")
            .args(["-L", &socket, "kill-server"])
            .status();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(output.status.success(), "tmux show prefix");
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}
