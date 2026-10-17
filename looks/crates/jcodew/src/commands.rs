//! Slash commands: the few this look answers itself, and wizard's own, which go to the backend
//! as typed. The suggestion list ranks them the way jcode ranks its registry.

use agent_core::Request;

use crate::app::{App, Overlay, VERSION};

/// Commands handled in the look, with the text jcode shows for the same command.
pub const LOCAL: &[(&str, &str)] = &[
    ("help", "Show help and keyboard shortcuts"),
    ("?", "Show help and keyboard shortcuts"),
    ("commands", "Alias for /help"),
    ("model", "List or switch models"),
    ("models", "Alias for /model"),
    ("cancel", "Cancel the current prompt or operation"),
    ("clear", "Clear conversation history"),
    ("cls", "Clear the view only, keeping context"),
    ("version", "Show current version"),
    ("diff", "Cycle or set diff display mode (off/inline/full)"),
    ("resume", "Open session picker"),
    ("sessions", "Alias for /resume"),
    ("session", "Alias for /resume"),
    ("quit", "Exit wizard"),
];

#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    /// With the leading slash.
    pub command: String,
    pub description: String,
    /// Character positions of `command` the typed text matched.
    pub matched: Vec<usize>,
}

/// Every command on offer: the local ones first, then what wizard advertises.
pub fn all(app: &App) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = LOCAL
        .iter()
        .map(|(n, d)| (format!("/{n}"), (*d).to_string()))
        .collect();
    for c in &app.commands {
        let name = format!("/{}", c.name);
        if !out.iter().any(|(n, _)| *n == name) {
            out.push((name, c.description.clone()));
        }
    }
    out
}

const MATCH: i32 = 16;
const CONSECUTIVE: i32 = 8;
const FIRST: i32 = 12;
const GAP: i32 = -3;

/// An anchored subsequence match: the first typed character must be the command's first.
fn fuzzy(needle: &str, hay: &str) -> Option<(i32, Vec<usize>)> {
    let n: Vec<char> = needle.chars().collect();
    let h: Vec<char> = hay.chars().collect();
    if n.is_empty() || h.is_empty() || !n[0].eq_ignore_ascii_case(&h[0]) {
        return None;
    }
    let mut score = FIRST;
    let mut pos = Vec::new();
    let mut hi = 0usize;
    let mut last: Option<usize> = None;
    for c in &n {
        let mut found = None;
        while hi < h.len() {
            if h[hi].eq_ignore_ascii_case(c) {
                found = Some(hi);
                hi += 1;
                break;
            }
            hi += 1;
        }
        let at = found?;
        score += MATCH;
        match last {
            Some(l) if at == l + 1 => score += CONSECUTIVE,
            Some(l) => score += GAP * (at - l - 1) as i32,
            None => {}
        }
        last = Some(at);
        pos.push(at);
    }
    let floor = n.len() as i32 * MATCH * 15 / 20;
    (score >= floor).then_some((score, pos))
}

/// The looks wizard's `/ui` switches to.
const LOOKS: [&str; 7] = [
    "wizard", "codex", "grok", "opencode", "pi", "jcode", "claude",
];

/// Suggestions for a command being typed (`input` starts with `/` and has no space yet), or for
/// the look after `/ui `.
pub fn suggestions(app: &App, input: &str) -> Vec<Suggestion> {
    if let Some(arg) = input.strip_prefix("/ui ") {
        return LOOKS
            .iter()
            .filter(|l| l.starts_with(arg.trim_start()))
            .map(|l| Suggestion {
                command: format!("/ui {l}"),
                description: String::new(),
                matched: (4..4 + arg.trim_start().chars().count()).collect(),
            })
            .collect();
    }
    if !input.starts_with('/') || input.contains(char::is_whitespace) {
        return Vec::new();
    }
    let needle = &input[1..];
    let mut prefix: Vec<Suggestion> = Vec::new();
    let mut loose: Vec<(i32, Suggestion)> = Vec::new();
    for (cmd, desc) in all(app) {
        let body = &cmd[1..];
        if body.starts_with(needle) {
            prefix.push(Suggestion {
                matched: (0..needle.chars().count()).map(|i| i + 1).collect(),
                command: cmd,
                description: desc,
            });
        } else if let Some((score, pos)) = fuzzy(needle, body) {
            loose.push((
                score,
                Suggestion {
                    matched: pos.into_iter().map(|i| i + 1).collect(),
                    command: cmd,
                    description: desc,
                },
            ));
        }
    }
    let by_len = |a: &Suggestion, b: &Suggestion| {
        a.command
            .len()
            .cmp(&b.command.len())
            .then_with(|| a.command.cmp(&b.command))
    };
    prefix.sort_by(by_len);
    loose.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| by_len(&a.1, &b.1)));
    prefix.extend(loose.into_iter().map(|(_, s)| s));
    prefix
}

fn accepts_args(app: &App, command: &str) -> bool {
    let name = command.trim_start_matches('/');
    matches!(name, "model" | "models" | "diff")
        || app
            .commands
            .iter()
            .any(|c| c.name == name && !c.input_hint.is_empty())
}

/// Tab: take the selected suggestion; repeated Tab walks the list of the text it started from.
pub fn autocomplete(app: &mut App) {
    let text = app.ed.text().to_string();
    let (base, idx) = match app.tab_base.clone() {
        Some((b, i)) => (b, i + 1),
        None => (text, app.sugg_sel),
    };
    let list = suggestions(app, &base);
    if list.is_empty() {
        return;
    }
    let idx = idx % list.len();
    let mut cmd = list[idx].command.clone();
    if list.len() == 1 && accepts_args(app, &cmd) {
        cmd.push(' ');
    }
    app.ed.set_text(&cmd);
    app.tab_base = (list.len() > 1).then_some((base, idx));
    app.sugg_sel = 0;
    app.dirty = true;
}

/// Models the `/model` picker lists for a filter: every term must appear in the name or the
/// provider.
pub fn model_matches(app: &App, filter: &str) -> Vec<usize> {
    let terms: Vec<String> = filter.split_whitespace().map(str::to_lowercase).collect();
    app.config
        .models
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            let hay = format!("{} {} {}", m.name, m.id, m.provider).to_lowercase();
            terms.iter().all(|t| hay.contains(t))
        })
        .map(|(i, _)| i)
        .collect()
}

/// The filter text when the composer holds `/model ...`.
pub fn model_filter(input: &str) -> Option<&str> {
    input
        .strip_prefix("/models ")
        .or_else(|| input.strip_prefix("/model "))
}

pub fn pick_model(app: &mut App, index: usize) {
    let Some(m) = app.config.models.get(index).cloned() else {
        return;
    };
    app.chosen_model = Some(m.id.clone());
    app.send(Request::SetModel(m.id.clone()));
    let via = if m.provider.is_empty() {
        String::new()
    } else {
        format!(" via {}", m.provider)
    };
    let name = if m.name.is_empty() { m.id } else { m.name };
    app.set_notice(format!("Model → {name}{via}"));
    app.system(format!("Model → {name}{via}"));
}

/// Run `name` here if it is one of ours. Anything else is wizard's to answer.
pub fn run_local(app: &mut App, name: &str, args: &str) -> bool {
    match name {
        "help" | "?" | "commands" if args.is_empty() => {
            app.overlay = Some(Overlay::Help { scroll: 0 });
        }
        "model" | "models" => {
            if args.is_empty() {
                app.ed.set_text("/model ");
                app.picker_sel = 0;
            } else {
                let want = args.to_lowercase();
                match app
                    .config
                    .models
                    .iter()
                    .position(|m| m.id.to_lowercase() == want || m.name.to_lowercase() == want)
                {
                    Some(i) => pick_model(app, i),
                    None => {
                        app.chosen_model = Some(args.to_string());
                        app.send(Request::SetModel(args.to_string()));
                    }
                }
            }
        }
        "cancel" | "stop" => {
            if app.busy() {
                app.interrupt();
            } else {
                app.system("Nothing to cancel: no prompt or operation is in progress.");
            }
        }
        "clear" => app.new_session(),
        "cls" | "clear-view" => {
            app.cleared_at = Some(app.tr.messages.len());
            app.scroll = None;
            app.set_notice("View cleared (context kept)");
        }
        "version" => app.system(format!(
            "wizard look: jcode {VERSION}\nbackend: {}",
            if app.config.backend.is_empty() {
                "wizard"
            } else {
                &app.config.backend
            }
        )),
        "diff" => {
            app.diff_mode = match args {
                "off" => crate::ui::tools::DiffMode::Off,
                "inline" => crate::ui::tools::DiffMode::Inline,
                "full" | "full-inline" => crate::ui::tools::DiffMode::FullInline,
                _ => app.diff_mode.next(),
            };
            app.set_notice(format!("Diffs: {}", app.diff_mode.label()));
        }
        "resume" | "sessions" | "session" => {
            if args.is_empty() {
                app.open_sessions();
            } else {
                app.send(Request::LoadSession(args.to_string()));
            }
        }
        "quit" | "exit" => app.quit = true,
        _ => return false,
    }
    app.dirty = true;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_is_anchored_and_scores_runs() {
        assert!(fuzzy("g", "config").is_none());
        let (run, _) = fuzzy("mod", "model").unwrap();
        let (gap, _) = fuzzy("mdl", "model").unwrap();
        assert!(run > gap);
    }
}
