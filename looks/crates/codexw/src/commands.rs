// OWNER: commands (slash command dispatch)
//! The `/` commands: wizard's own, the shared set every look answers itself
//! (`agent_core::commands`), and codexw's few visual extras below. Anything else typed as
//! `/name` goes to wizard as typed.

use std::io::Write;

use agent_core::SlashCommand;
use agent_core::commands::{Command, Shared};

use crate::app::App;
use crate::ui::history_cells::{ErrorCell, InfoCell};

/// codexw's own commands: name, argument hint, description.
pub const EXTRAS: &[(&str, &str, &str)] = &[
    ("theme", "", "choose a syntax highlighting theme"),
    (
        "raw",
        "[on|off]",
        "toggle raw scrollback mode for copy-friendly terminal selection",
    ),
    (
        "vim",
        "",
        "toggle vim-style modal editing of the input line",
    ),
    (
        "statusline",
        "",
        "configure which items appear in the status line",
    ),
    (
        "title",
        "",
        "configure which items appear in the terminal title",
    ),
    ("keymap", "[debug]", "remap TUI shortcuts"),
    ("copy", "", "copy last response as markdown"),
];

pub fn extras() -> Vec<SlashCommand> {
    agent_core::commands::extras(EXTRAS)
}

/// The `/` menu for what wizard advertised.
pub fn menu(advertised: &[SlashCommand]) -> Vec<SlashCommand> {
    agent_core::commands::menu(advertised, &extras())
}

/// False for what Codex disables while a turn runs: some extras and the rewind picker.
pub fn during_task(name: &str) -> bool {
    !matches!(name, "theme" | "vim" | "keymap" | "rewind")
}

/// Split `/name rest` into `("name", "rest")`.
pub fn parse(text: &str) -> Option<(&str, &str)> {
    let t = text.trim_start().strip_prefix('/')?;
    let end = t.find(char::is_whitespace).unwrap_or(t.len());
    let (name, rest) = t.split_at(end);
    if name.is_empty() || name.contains('/') {
        return None;
    }
    Some((name, rest.trim()))
}

/// What running a slash command asks of the app beyond the cells it pushed.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Done,
    Quit,
}

impl<W: Write> App<W> {
    /// Run a typed slash command.
    pub fn run_slash(&mut self, text: &str) -> Outcome {
        let Some((name, rest)) = parse(text) else {
            return Outcome::Done;
        };
        if self.running && !during_task(name) {
            self.push_cell(Box::new(ErrorCell {
                text: format!("'/{name}' is disabled while a task is in progress."),
            }));
            return Outcome::Done;
        }
        let extras = extras();
        match agent_core::commands::resolve(name, &extras) {
            Command::Shared(Shared::Exit) => return Outcome::Quit,
            Command::Shared(Shared::Clear) => self.clear_and_new(),
            Command::Shared(Shared::Help) => self.show_help(),
            Command::Shared(Shared::Model) => self.open_model_picker_with(rest),
            Command::Shared(Shared::Resume) => self.open_resume(rest),
            Command::Shared(Shared::Export) => {
                let (level, text) =
                    agent_core::export::run(self.export.transcript(), rest, &self.opts.cwd);
                match level {
                    agent_core::NoticeLevel::Error => self.say_error(text),
                    _ => self.push_cell(Box::new(InfoCell { text, hint: None })),
                }
            }
            Command::Shared(Shared::Settings) => self.push_cell(Box::new(InfoCell {
                text: agent_core::commands::NO_SETTINGS.into(),
                hint: Some("/theme, /statusline, /title and /keymap change this look".into()),
            })),
            Command::Extra(name) => self.run_extra(name, rest),
            Command::Wizard if name == "rewind" => self.rewind_command(text, rest),
            Command::Wizard => self.wizard_command(text.trim().to_string()),
        }
        Outcome::Done
    }

    fn run_extra(&mut self, name: &str, rest: &str) {
        match name {
            "theme" => self.theme_command(),
            "raw" => match rest.to_ascii_lowercase().as_str() {
                "" => self.set_raw_output(!self.raw_output, true),
                "on" => self.set_raw_output(true, true),
                "off" => self.set_raw_output(false, true),
                _ => self.say_error("Usage: /raw [on|off]"),
            },
            "vim" => {
                let on = !self.pane.composer.is_vim_enabled();
                self.pane.composer.set_vim_enabled(on);
                self.push_cell(Box::new(InfoCell {
                    text: if on {
                        "Vim mode enabled."
                    } else {
                        "Vim mode disabled."
                    }
                    .into(),
                    hint: None,
                }));
            }
            "statusline" => self.statusline_command(),
            "title" => self.title_command(),
            "keymap" => match rest.to_ascii_lowercase().as_str() {
                "" => {
                    self.pane
                        .push_view(Box::new(crate::ui::pickers::keymap::KeymapView::new()));
                    self.request_draw();
                }
                "debug" => {
                    self.pane.push_view(Box::new(
                        crate::ui::pickers::keymap::InspectorView::default(),
                    ));
                    self.request_draw();
                }
                _ => self.say_error("Usage: /keymap [debug]"),
            },
            "copy" => self.copy_last_message(),
            _ => {}
        }
    }

    /// `/help`: the menu and where the keys are.
    fn show_help(&mut self) {
        let commands = agent_core::commands::help_text(&menu(&self.commands));
        self.push_cell(Box::new(InfoCell {
            text: format!("{commands}\n\nPress ? on an empty composer for keyboard shortcuts."),
            hint: None,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_splits_name_and_args() {
        assert_eq!(parse("/model gpt"), Some(("model", "gpt")));
        assert_eq!(parse("  /clear"), Some(("clear", "")));
        assert_eq!(parse("/usr/bin/ls"), None);
        assert_eq!(parse("hello"), None);
    }

    #[test]
    fn menu_is_wizards_then_shared_then_extras() {
        let advertised = vec![SlashCommand {
            name: "compact".into(),
            description: "compact the conversation".into(),
            input_hint: String::new(),
        }];
        let names: Vec<String> = menu(&advertised).into_iter().map(|c| c.name).collect();
        assert_eq!(
            names,
            [
                "compact",
                "clear",
                "exit",
                "export",
                "help",
                "model",
                "quit",
                "resume",
                "settings",
                "theme",
                "raw",
                "vim",
                "statusline",
                "title",
                "keymap",
                "copy"
            ]
        );
    }
}
