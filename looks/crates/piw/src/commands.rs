// OWNER: commands (slash command table and what each one does on wizard)
//! The slash commands: wizard's own, the ones every look answers itself
//! (`agent_core::commands`), and three of Pi's that only touch the look. Anything else goes to
//! wizard as typed.

use agent_core::commands::{self as shared, Command, Shared};
use agent_core::SlashCommand;

use crate::app::{App, Deferred};
use crate::ui::selectors::Selector;

pub fn extras() -> Vec<SlashCommand> {
    shared::extras(&[
        ("settings", "", "Open settings menu"),
        (
            "scoped-models",
            "",
            "Enable/disable models for Ctrl+P cycling",
        ),
        ("copy", "", "Copy last agent message to clipboard"),
    ])
}

/// The menu: wizard's commands, the shared ones it did not list, then piw's extras.
pub fn items(backend: &[SlashCommand]) -> Vec<SlashCommand> {
    shared::menu(backend, &extras())
}

/// Run `text` when piw answers it: a shared command, an extra, or `/effort` with no argument,
/// which opens the thinking selector. Returns `false` for what goes to wizard.
pub fn run(app: &mut App, text: &str) -> bool {
    let Some((name, arg)) = shared::split(text) else {
        return false;
    };
    match shared::resolve(name, &extras()) {
        Command::Shared(Shared::Clear) => app.new_session(),
        Command::Shared(Shared::Exit) => app.quit = true,
        Command::Shared(Shared::Help) => app.show_help(),
        Command::Shared(Shared::Model) => app.cmd_model(arg),
        Command::Shared(Shared::Resume) => app.cmd_resume(arg),
        Command::Extra("settings") => app.open_settings(),
        Command::Extra("scoped-models") => app.open_scoped_models(),
        Command::Extra("copy") => match app.last_assistant_text() {
            Some(t) => {
                app.pending.push(Deferred::Copy(t));
                app.status("Copied last agent message to clipboard".into());
            }
            None => app.error("No agent messages to copy yet.".into()),
        },
        Command::Wizard if name == "effort" && arg.is_empty() && !app.config.efforts.is_empty() => {
            app.open_thinking()
        }
        Command::Extra(_) | Command::Wizard => return false,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_is_wizards_then_shared_then_extras() {
        let it = items(&agent_core::mock::commands());
        let names: Vec<&str> = it.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "model",
                "mode",
                "effort",
                "plan",
                "compact",
                "diff",
                "todos",
                "status",
                "cost",
                "agents",
                "help",
                "clear",
                "exit",
                "quit",
                "resume",
                "settings",
                "scoped-models",
                "copy",
            ]
        );
    }
}
