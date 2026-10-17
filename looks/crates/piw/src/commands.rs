// OWNER: commands (slash command table and what each one does on wizard)
//! Pi's built-in slash commands in the order of its menu (`docs/piw-spec.md` 10.1), mapped onto
//! wizard where wizard can back them (14.3). The rest print an honest `Warning:` line and do
//! nothing else. Anything that is not a built-in goes to the backend as a plain prompt, which is
//! how wizard's own commands (`/mode`, `/plan`, `/diff` ...) run.

use agent_core::{Request, SlashCommand};

use crate::app::{App, Deferred};
use crate::ui::selectors::Selector;

#[derive(Clone, Debug, PartialEq)]
pub struct SlashItem {
    /// Without the slash.
    pub name: String,
    pub hint: String,
    pub description: String,
    pub builtin: bool,
}

const BUILTINS: &[(&str, &str, &str)] = &[
    ("settings", "", "Open settings menu"),
    (
        "model",
        "<provider/model>",
        "Select model (opens selector UI)",
    ),
    ("tree", "", "Navigate session tree (switch branches)"),
    ("thinking", "<level>", "Set thinking level"),
    (
        "scoped-models",
        "",
        "Enable/disable models for Ctrl+P cycling",
    ),
    (
        "export",
        "",
        "Export session (HTML default, or specify path: .html/.jsonl)",
    ),
    (
        "import",
        "",
        "Import and resume a session from a JSONL file",
    ),
    ("share", "", "Share session as a secret GitHub gist"),
    (
        "bug",
        "<description>",
        "Report a bug to the Wizard developers",
    ),
    ("copy", "", "Copy last agent message to clipboard"),
    ("name", "", "Set session display name"),
    ("session", "", "Show session info and stats"),
    ("changelog", "", "Show changelog entries"),
    ("hotkeys", "", "Show all keyboard shortcuts"),
    ("fork", "", "Create a new fork from a previous user message"),
    (
        "clone",
        "",
        "Duplicate the current session at the current position",
    ),
    (
        "trust",
        "",
        "Save project trust decision for future sessions",
    ),
    ("login", "<provider>", "Configure provider authentication"),
    ("logout", "", "Remove provider authentication"),
    ("new", "", "Start a new session"),
    ("compact", "", "Manually compact the session context"),
    ("resume", "", "Resume a different session"),
    (
        "reload",
        "",
        "Reload keybindings, extensions, skills, prompts, themes, and context files",
    ),
    ("quit", "", "Quit wizard"),
];

/// The menu: Pi's built-ins, then wizard's own commands that do not share a name with one.
pub fn items(backend: &[SlashCommand]) -> Vec<SlashItem> {
    let mut v: Vec<SlashItem> = BUILTINS
        .iter()
        .map(|(n, h, d)| SlashItem {
            name: (*n).into(),
            hint: (*h).into(),
            description: (*d).into(),
            builtin: true,
        })
        .collect();
    for c in backend {
        if v.iter().any(|i| i.name == c.name) {
            continue;
        }
        v.push(SlashItem {
            name: c.name.clone(),
            hint: c.input_hint.clone(),
            description: c.description.clone(),
            builtin: false,
        });
    }
    v
}

/// `/name rest` into `("name", "rest")`.
pub fn split(text: &str) -> Option<(&str, &str)> {
    let t = text.strip_prefix('/')?;
    let (name, rest) = match t.find(char::is_whitespace) {
        Some(i) => (&t[..i], t[i..].trim()),
        None => (t, ""),
    };
    (!name.is_empty()).then_some((name, rest))
}

fn unsupported(app: &mut App, what: &str, why: &str) {
    app.warn(format!("{what} {why}"));
}

/// Run a built-in. Returns `false` when `name` is not one, so the caller sends the text to the
/// backend.
pub fn run(app: &mut App, name: &str, arg: &str) -> bool {
    match name {
        "settings" => app.open_settings(),
        "model" => app.cmd_model(arg),
        "thinking" => app.cmd_thinking(arg),
        "scoped-models" => app.open_scoped_models(),
        "tree" | "fork" | "clone" => unsupported(
            app,
            "wizard sessions do not branch;",
            &format!("/{name} is not supported."),
        ),
        "export" | "import" | "share" | "bug" => {
            unsupported(app, &format!("/{name}"), "is not supported yet.")
        }
        "copy" => match app.last_assistant_text() {
            Some(t) => {
                app.pending.push(Deferred::Copy(t));
                app.status("Copied last agent message to clipboard".into());
            }
            None => app.error("No agent messages to copy yet.".into()),
        },
        "name" => app.cmd_name(arg),
        "session" => app.show_session_info(),
        "changelog" => app.show_changelog(),
        "hotkeys" => app.show_hotkeys(),
        "trust" => unsupported(app, "wizard has no project trust prompt.", ""),
        "login" | "logout" => unsupported(
            app,
            "wizard signs in to its providers from ~/.wizard;",
            "run wizard in a shell to log in.",
        ),
        "new" => app.new_session(),
        "compact" => app.compact(arg),
        "resume" => app.open_resume(),
        "reload" => app.reload(),
        "quit" => app.quit = true,
        _ => return false,
    }
    true
}

/// A model picked by `/model x`: id, `provider/id` or name, exact.
pub fn find_model<'a>(app: &'a App, x: &str) -> Option<&'a agent_core::ModelOption> {
    let x = x.trim();
    app.config.models.iter().find(|m| {
        m.id == x
            || m.name == x
            || format!("{}/{}", m.provider, m.name) == x
            || m.id.rsplit('/').next() == Some(x)
                && app
                    .config
                    .models
                    .iter()
                    .filter(|o| o.id.rsplit('/').next() == Some(x))
                    .count()
                    == 1
    })
}

pub fn open_model_selector(app: &mut App, prefill: &str) {
    let mut sel = Selector::model(
        app.config.models.clone(),
        &app.config.model,
        &app.settings.enabled_models,
    )
    .with_default_model(app.default_model.as_deref());
    if !prefill.is_empty() {
        sel.on_paste(prefill);
    }
    app.open_selector(sel);
}

pub fn send(app: &mut App, r: Request) {
    app.send(r);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_order_and_dedupe() {
        let it = items(&[
            SlashCommand {
                name: "plan".into(),
                description: "toggle plan".into(),
                input_hint: String::new(),
            },
            SlashCommand {
                name: "fork".into(),
                description: "wizard side quest".into(),
                input_hint: String::new(),
            },
        ]);
        assert_eq!(it[0].name, "settings");
        assert_eq!(it[23].name, "quit");
        assert_eq!(it.len(), 25);
        assert_eq!(it[24].name, "plan");
        assert!(!it[24].builtin);
    }

    #[test]
    fn split_command_and_argument() {
        assert_eq!(split("/model  big "), Some(("model", "big")));
        assert_eq!(split("/new"), Some(("new", "")));
        assert_eq!(split("hello"), None);
        assert_eq!(split("/"), None);
    }
}
