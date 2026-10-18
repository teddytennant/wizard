//! The slash commands every look offers, and who runs each one.
//!
//! A look's `/` menu is the same everywhere: what `wizard acp` advertises (its built-in
//! commands, the user's own commands and the skills), the few in [`SHARED`] that every look
//! answers itself because wizard cannot run them over ACP, because they open a picker or because
//! they only touch the look, and a
//! short list of a look's own visual extras (`/theme`). Anything else typed as `/name` goes to
//! wizard as typed, which runs it or says it does not know it.

use crate::SlashCommand;

/// Answered by the look, the same way in every look: name, argument hint, description. The
/// descriptions are wizard's own.
pub const SHARED: &[(&str, &str, &str)] = &[
    ("clear", "", "clear the conversation"),
    ("exit", "", "exit wizard"),
    (
        "export",
        "[file]",
        "write the conversation to a markdown file",
    ),
    ("help", "", "show available commands and keys"),
    ("model", "[tag]", "pick or switch the model"),
    ("quit", "", "exit wizard"),
    ("resume", "[id]", "reopen and continue a past session"),
    ("settings", "", "open this look's settings"),
];

/// What `/settings` says in a look that has no settings view.
pub const NO_SETTINGS: &str =
    "This look has no settings view. Wizard's own settings are in ~/.wizard/config.toml.";

/// A command in [`SHARED`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shared {
    /// A new session in place of this one, with an empty transcript.
    Clear,
    /// Leave the look (`/exit` and `/quit`).
    Exit,
    /// The conversation to a markdown file; see [`crate::export`].
    Export,
    /// The commands in the menu and the look's keys.
    Help,
    /// No argument: the model picker. With one: switch to that model.
    Model,
    /// No argument: the session picker. With one: open that session.
    Resume,
    /// The look's settings view, or [`NO_SETTINGS`].
    Settings,
}

/// Who runs a typed `/name`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command<'a> {
    Shared(Shared),
    /// One of the look's own extras, by name.
    Extra(&'a str),
    /// Sent to wizard as typed.
    Wizard,
}

/// `/name rest` into `("name", "rest")`, `None` when `text` is not a slash command.
pub fn split(text: &str) -> Option<(&str, &str)> {
    let rest = text.trim_start().strip_prefix('/')?;
    let (name, args) = rest
        .split_once(char::is_whitespace)
        .map_or((rest, ""), |(n, a)| (n, a.trim()));
    (!name.is_empty()).then_some((name, args))
}

/// Who runs `name`: [`SHARED`] first, then the look's `extras`, then wizard.
pub fn resolve<'a>(name: &'a str, extras: &[SlashCommand]) -> Command<'a> {
    let shared = match name {
        "clear" => Some(Shared::Clear),
        "exit" | "quit" => Some(Shared::Exit),
        "export" => Some(Shared::Export),
        "help" => Some(Shared::Help),
        "model" => Some(Shared::Model),
        "resume" => Some(Shared::Resume),
        "settings" => Some(Shared::Settings),
        _ => None,
    };
    match shared {
        Some(s) => Command::Shared(s),
        None if extras.iter().any(|e| e.name == name) => Command::Extra(name),
        None => Command::Wizard,
    }
}

/// A look's extras from `(name, hint, description)` rows.
pub fn extras(rows: &[(&str, &str, &str)]) -> Vec<SlashCommand> {
    rows.iter()
        .map(|(name, hint, description)| SlashCommand {
            name: (*name).into(),
            description: (*description).into(),
            input_hint: (*hint).into(),
        })
        .collect()
}

/// The `/` menu: what wizard advertised in its order, then the shared commands it did not
/// list, then the look's extras. A name is listed once; the first wins.
pub fn menu(advertised: &[SlashCommand], extras: &[SlashCommand]) -> Vec<SlashCommand> {
    let mut out: Vec<SlashCommand> = Vec::new();
    let shared = self::extras(SHARED);
    for c in advertised.iter().chain(&shared).chain(extras) {
        if !out.iter().any(|o| o.name == c.name) {
            out.push(c.clone());
        }
    }
    out
}

/// `/help` as text, one command a line, for a look that has no help view of its own.
pub fn help_text(menu: &[SlashCommand]) -> String {
    let width = menu
        .iter()
        .map(|c| usage(c).chars().count())
        .max()
        .unwrap_or(0);
    menu.iter()
        .map(|c| format!("{:width$}  {}", usage(c), c.description))
        .collect::<Vec<_>>()
        .join("\n")
}

fn usage(c: &SlashCommand) -> String {
    match c.input_hint.is_empty() {
        true => format!("/{}", c.name),
        false => format!("/{} {}", c.name, c.input_hint),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(name: &str, description: &str) -> SlashCommand {
        SlashCommand {
            name: name.into(),
            description: description.into(),
            input_hint: String::new(),
        }
    }

    #[test]
    fn split_reads_the_name_and_the_rest() {
        assert_eq!(split("/model  big "), Some(("model", "big")));
        assert_eq!(split("  /clear"), Some(("clear", "")));
        assert_eq!(split("hello"), None);
        assert_eq!(split("/"), None);
    }

    #[test]
    fn shared_then_extras_then_wizard() {
        let extras = extras(&[("theme", "[name]", "switch theme")]);
        assert_eq!(resolve("quit", &extras), Command::Shared(Shared::Exit));
        assert_eq!(resolve("model", &extras), Command::Shared(Shared::Model));
        assert_eq!(resolve("theme", &extras), Command::Extra("theme"));
        assert_eq!(resolve("compact", &extras), Command::Wizard);
        assert_eq!(resolve("nonsense", &extras), Command::Wizard);
    }

    #[test]
    fn menu_lists_each_name_once_with_wizards_wording_first() {
        let advertised = vec![cmd("model", "wizard's model"), cmd("compact", "compact")];
        let extras = extras(&[("theme", "", "switch theme"), ("compact", "", "a copy")]);
        let m = menu(&advertised, &extras);
        let names: Vec<&str> = m.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "model", "compact", "clear", "exit", "export", "help", "quit", "resume",
                "settings", "theme"
            ]
        );
        assert_eq!(m[0].description, "wizard's model");
        assert_eq!(m[1].description, "compact");
    }

    #[test]
    fn help_text_lines_up_the_descriptions() {
        let mut model = cmd("model", "pick or switch the model");
        model.input_hint = "[tag]".into();
        let text = help_text(&[model, cmd("clear", "clear the conversation")]);
        assert_eq!(
            text,
            "/model [tag]  pick or switch the model\n/clear        clear the conversation"
        );
    }
}
