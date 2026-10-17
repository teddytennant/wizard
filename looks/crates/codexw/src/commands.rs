// OWNER: commands (slash command table and dispatch; add arms, keep the table in popup order)
//! Codex's slash commands mapped onto what `wizard acp` can back (spec C.6 and Part E).
//!
//! The table is in Codex's popup order (spec C.6.1). Every command has a [`Support`] verdict and
//! the messages for the not-supported ones are the ones Part E gives, in Codex's voice. Wizard
//! 3.7.1 sends no permission requests over ACP, has no sandbox, no fork, no review mode.

use std::io::Write;

use crate::app::App;
use crate::ui::history_cells::{ErrorCell, InfoCell};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Support {
    /// Wizard backs it directly.
    Backed,
    /// codexw implements it locally.
    Client,
    /// Works with a caveat; the text is shown as an info cell after the action.
    Partial(&'static str),
    /// Wizard cannot; the text is shown as an error cell.
    No(&'static str),
}

#[derive(Clone, Copy, Debug)]
pub struct CommandSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub support: Support,
    /// Allowed while a turn runs (spec C.6.2).
    pub during_task: bool,
}

const fn c(
    name: &'static str,
    description: &'static str,
    support: Support,
    during_task: bool,
) -> CommandSpec {
    CommandSpec {
        name,
        description,
        support,
        during_task,
    }
}

use Support::{Backed, Client, No, Partial};

/// Popup order, descriptions verbatim. `/fast` is listed as Codex does, and says why it does nothing.
pub const COMMANDS: &[CommandSpec] = &[
    c(
        "model",
        "choose what model and reasoning effort to use",
        Backed,
        true,
    ),
    c(
        "fast",
        "1.5x speed, increased usage",
        No(
            "'/fast' is not supported by wizard. It has no service tiers; the model list in /model is the speed choice.",
        ),
        true,
    ),
    c(
        "ide",
        "include current selection, open files, and other context from your IDE",
        No("'/ide' is not supported by wizard."),
        true,
    ),
    c(
        "permissions",
        "choose what Wizard is allowed to do",
        No(
            "'/permissions' is not supported by wizard. It runs tools without asking in genie and sovereign mode and has no sandbox. Use the Mode setting: chat mode has no file or shell tools.",
        ),
        true,
    ),
    c("keymap", "remap TUI shortcuts", Client, false),
    c("vim", "toggle Vim mode for the composer", Client, false),
    c(
        "experimental",
        "toggle experimental features",
        No("'/experimental' is not supported by wizard."),
        false,
    ),
    c(
        "approve",
        "approve one retry of a recent auto-review denial",
        No("'/approve' is not supported by wizard."),
        true,
    ),
    c(
        "memories",
        "configure memory use and generation",
        Backed,
        false,
    ),
    c(
        "skills",
        "use skills to improve how Wizard performs specific tasks",
        Client,
        true,
    ),
    c(
        "import",
        "import setup, this project, and recent chats from Claude Code",
        No(
            "'/import' is not supported by wizard over ACP. Run `wizard resume --claude` in a terminal to continue a Claude Code conversation.",
        ),
        false,
    ),
    c("hooks", "view and manage lifecycle hooks", Client, true),
    c(
        "review",
        "review my current changes and find issues",
        Partial(
            "'/review' runs as a normal prompt in wizard. It has no review mode or structured findings.",
        ),
        false,
    ),
    c("rename", "rename the current thread", Client, true),
    c(
        "new",
        "start a new chat during a conversation",
        Backed,
        false,
    ),
    c(
        "archive",
        "archive this session and exit",
        No(
            "'/archive' is not supported by wizard. Sessions stay in ~/.wizard/sessions until you remove them.",
        ),
        false,
    ),
    c(
        "delete",
        "permanently delete this session and exit",
        No("'/delete' is not supported by wizard. Remove ~/.wizard/sessions/<id>.jsonl by hand."),
        false,
    ),
    c(
        "resume",
        "resume a saved chat",
        Partial(
            "Resume list shows the first prompt, working directory and time; wizard does not keep branch or thread names.",
        ),
        true,
    ),
    c(
        "fork",
        "fork the current chat",
        No(
            "'/fork' is not supported by wizard. It can only start a background side task, which is not a copy of this chat. Use /new to start over, or /side to ask a one-off question.",
        ),
        false,
    ),
    c(
        "init",
        "create an AGENTS.md file with instructions for Wizard",
        Client,
        false,
    ),
    c(
        "compact",
        "summarize conversation to prevent hitting the context limit",
        Backed,
        false,
    ),
    c(
        "plan",
        "switch to Plan mode",
        Partial(
            "'/plan' toggles plan mode in wizard, but plans are approved automatically over ACP, so there is no plan review step.",
        ),
        false,
    ),
    c(
        "goal",
        "set or view the goal for a long-running task",
        Backed,
        true,
    ),
    c("agent", "switch the active agent thread", Backed, true),
    c(
        "side",
        "start a side conversation in an ephemeral fork",
        Backed,
        true,
    ),
    c("copy", "copy last response as markdown", Client, true),
    c(
        "raw",
        "toggle raw scrollback mode for copy-friendly terminal selection",
        Client,
        true,
    ),
    c(
        "diff",
        "show git diff (including untracked files)",
        Client,
        true,
    ),
    c("mention", "mention a file", Client, true),
    c(
        "status",
        "show current session configuration and token usage",
        Partial("Lines with no source read `unknown`."),
        true,
    ),
    c(
        "title",
        "configure which items appear in the terminal title",
        Client,
        true,
    ),
    c(
        "statusline",
        "configure which items appear in the status line",
        Client,
        true,
    ),
    c("theme", "choose a syntax highlighting theme", Client, false),
    c(
        "pets",
        "choose or hide the terminal pet",
        No("'/pets' is not supported by wizard."),
        false,
    ),
    c(
        "mcp",
        "list configured MCP tools; use /mcp verbose for details",
        Client,
        true,
    ),
    c(
        "plugins",
        "browse plugins",
        No("'/plugins' is not supported by wizard."),
        true,
    ),
    c(
        "logout",
        "log out of Wizard",
        No(
            "'/logout' is not supported by wizard. Sign in with `wizard --login xai` or `wizard --login chatgpt`. To sign out of xAI, delete ~/.wizard/xai_oauth.json.",
        ),
        false,
    ),
    c("exit", "exit Wizard", Client, true),
    c(
        "feedback",
        "send logs to maintainers",
        No(
            "'/feedback' is not supported by wizard. Run `wizard doctor --bundle` to write a redacted report under ~/.wizard/bundles/; read it before you attach it anywhere.",
        ),
        true,
    ),
    c("ps", "list background terminals", Backed, true),
    c(
        "stop",
        "stop all background terminals",
        No(
            "'/stop' is not supported by wizard. /ps lists background tasks; ask the agent to stop one.",
        ),
        true,
    ),
    c(
        "clear",
        "clear the terminal and start a new chat",
        Backed,
        false,
    ),
    c(
        "personality",
        "choose a communication style for Wizard",
        No(
            "'/personality' is not supported by wizard. Its modes (genie, sovereign, chat) change how autonomous it is, not its tone.",
        ),
        true,
    ),
    c("subagents", "switch the active agent thread", Backed, true),
    c(
        "ui",
        "switch look: /ui wizard|codex|grok|opencode|pi|jcode|claude",
        Backed,
        false,
    ),
];

/// Commands that exist but are not listed in the popup unless typed.
pub const HIDDEN: &[(&str, &str)] = &[
    ("rewind", "go back to before an earlier message"),
    ("quit", "exit Wizard"),
    ("btw", "start a side conversation in an ephemeral fork"),
    ("clean", "stop all background terminals"),
    ("usage", "view account usage or use a usage limit reset"),
    (
        "debug-config",
        "show config layers and requirement sources for debugging",
    ),
];

/// Typed-only commands that are real: they resolve like the listed ones but stay out of the popup.
pub const HIDDEN_SPECS: &[CommandSpec] = &[
    c(
        "rewind",
        "go back to before an earlier message",
        Backed,
        false,
    ),
    c(
        "usage",
        "view account usage or use a usage limit reset",
        Backed,
        true,
    ),
    c(
        "debug-config",
        "show config layers and requirement sources for debugging",
        Client,
        true,
    ),
];

pub fn lookup(name: &str) -> Option<&'static CommandSpec> {
    let name = match name {
        "quit" => "exit",
        "btw" => "side",
        "clean" => "stop",
        "pet" => "pets",
        n => n,
    };
    COMMANDS.iter().chain(HIDDEN_SPECS).find(|c| c.name == name)
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
    /// Run a typed slash command. Unknown names get Codex's own message.
    pub fn run_slash(&mut self, text: &str) -> Outcome {
        let Some((name, rest)) = parse(text) else {
            return Outcome::Done;
        };
        let Some(spec) = lookup(name) else {
            if self.commands.iter().any(|c| c.name == name) {
                self.wizard_command(text.trim().to_string());
                return Outcome::Done;
            }
            self.push_cell(Box::new(InfoCell {
                text: format!(
                    "Unrecognized command '/{name}'. Type \"/\" for a list of supported commands."
                ),
                hint: None,
            }));
            return Outcome::Done;
        };
        if self.running && !spec.during_task {
            self.push_cell(Box::new(ErrorCell {
                text: format!("'/{}' is disabled while a task is in progress.", spec.name),
            }));
            return Outcome::Done;
        }
        match spec.name {
            "exit" => return Outcome::Quit,
            "new" => self.new_session(),
            "clear" => self.clear_and_new(),
            "model" => self.open_model_picker_with(rest),
            "resume" => self.open_resume(rest),
            "rename" => self.rename_command(rest),
            "status" => self.show_status(),
            "diff" => self.open_diff(),
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
            "mention" => self.pane.composer.insert_at_mention(),
            "skills" => self.skills_command(),
            "copy" => self.copy_last_message(),
            "mcp" => self.mcp_command(rest),
            "hooks" => self.hooks_command(),
            "debug-config" => self.debug_config_command(),
            "usage" => self.wizard_command("/usage".into()),
            "ui" => self.wizard_command(if rest.is_empty() {
                "/ui".into()
            } else {
                format!("/ui {rest}")
            }),
            "memories" => {
                let cmd = if rest.is_empty() {
                    "/memory".to_string()
                } else {
                    format!("/memory {rest}")
                };
                self.wizard_command(cmd);
            }
            "goal" => match rest.to_ascii_lowercase().as_str() {
                "clear" | "edit" | "pause" | "resume" => self.say_error(format!(
                    "'/goal {}' is not supported by wizard. The goal lives in .wizard/mission.toml; edit or delete that file.",
                    rest.to_ascii_lowercase()
                )),
                _ if rest.is_empty() => self.wizard_command("/goal".into()),
                _ => self.wizard_command(format!("/goal {rest}")),
            },
            "side" => {
                if rest.is_empty() {
                    self.say_error("'/side' needs a question in wizard: /side <question>. It is answered without entering the conversation.");
                } else {
                    self.wizard_command(format!("/btw {rest}"));
                }
            }
            "ps" => {
                use ratatui::style::Stylize;
                self.push_output(
                    Some("/ps"),
                    vec![ratatui::text::Line::from("Background terminals".bold())],
                );
                self.wizard_command("/bashes".into());
            }
            "agent" | "subagents" => self.wizard_command("/agents".into()),
            "theme" => self.theme_command(),
            "keymap" => match rest.to_ascii_lowercase().as_str() {
                "" => {
                    self.pane
                        .push_view(Box::new(crate::ui::pickers::keymap::KeymapView::new()));
                    self.request_draw();
                }
                "debug" => {
                    self.pane
                        .push_view(Box::new(crate::ui::pickers::keymap::InspectorView::default()));
                    self.request_draw();
                }
                _ => self.say_error("Usage: /keymap [debug]"),
            },
            "statusline" => self.statusline_command(),
            "title" => self.title_command(),
            "rewind" => self.rewind_command(rest),
            "raw" => match rest.to_ascii_lowercase().as_str() {
                "" => self.set_raw_output(!self.raw_output, true),
                "on" => self.set_raw_output(true, true),
                "off" => self.set_raw_output(false, true),
                _ => self.say_error("Usage: /raw [on|off]"),
            },
            "compact" => self.wizard_command("/compact".into()),
            "init" | "review" | "plan" => {
                if spec.name == "plan" {
                    self.pane.footer.plan_mode = !self.pane.footer.plan_mode;
                }
                let prompt = match spec.name {
                    "init" => crate::commands::INIT_PROMPT.to_string(),
                    "review" if rest.is_empty() => {
                        "Review my current changes and find issues.".to_string()
                    }
                    _ => text.trim().to_string(),
                };
                if let Support::Partial(msg) = spec.support {
                    self.push_cell(Box::new(InfoCell {
                        text: msg.into(),
                        hint: None,
                    }));
                }
                self.submit_prompt(prompt);
            }
            _ => match spec.support {
                No(msg) => self.push_cell(Box::new(ErrorCell { text: msg.into() })),
                Partial(msg) => self.push_cell(Box::new(InfoCell {
                    text: msg.into(),
                    hint: None,
                })),
                _ => self.push_cell(Box::new(InfoCell {
                    text: format!("'/{}' is not implemented in codexw yet.", spec.name),
                    hint: None,
                })),
            },
        }
        Outcome::Done
    }

    fn new_session(&mut self) {
        self.new_session_cmd();
    }

    /// `/status`: the status card (spec B.13.4).
    pub fn show_status(&mut self) {
        self.push_status_card();
    }
}

/// Codex's `/init` prompt, condensed. Wizard reads AGENTS.md at the repo root.
pub const INIT_PROMPT: &str = "Generate a file named AGENTS.md that serves as a contributor guide for this repository.\nYour goal is to produce a clear, concise, and well-structured document with descriptive headings and actionable explanations for each section.\nFollow the outline below, but adapt as needed: add sections if relevant, and omit those that do not apply to this project.\n\nDocument Requirements\n\n- Title the document \"Repository Guidelines\".\n- Use Markdown headings (#, ##, etc.) for structure.\n- Keep the document concise. 200-400 words is optimal.\n- Keep explanations short, direct, and specific to this repository.\n- Provide examples where helpful (commands, directory paths, naming patterns).\n- Maintain a professional, instructional tone.\n\nRecommended Sections\n\nProject Structure & Module Organization\n\n- Outline the project structure, including where the source code, tests, and assets are located.\n\nBuild, Test, and Development Commands\n\n- List key commands for building, testing, and running locally.\n\nCoding Style & Naming Conventions\n\n- Specify indentation rules, language-specific style preferences, and naming patterns.\n\nTesting Guidelines\n\n- Identify testing frameworks and coverage requirements.\n\nCommit & Pull Request Guidelines\n\n- Summarize commit message conventions and PR requirements.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_splits_name_and_args() {
        assert_eq!(parse("/model gpt"), Some(("model", "gpt")));
        assert_eq!(parse("  /new"), Some(("new", "")));
        assert_eq!(parse("/usr/bin/ls"), None);
        assert_eq!(parse("hello"), None);
    }

    #[test]
    fn aliases_resolve() {
        assert_eq!(lookup("quit").unwrap().name, "exit");
        assert_eq!(lookup("btw").unwrap().name, "side");
        assert!(lookup("nonsense").is_none());
    }

    #[test]
    fn table_is_in_popup_order() {
        let names: Vec<&str> = COMMANDS.iter().map(|c| c.name).collect();
        let pos = |n: &str| names.iter().position(|x| *x == n).unwrap();
        assert!(pos("model") < pos("ide") && pos("ide") < pos("permissions"));
        assert!(pos("status") < pos("title") && pos("title") < pos("statusline"));
        assert_eq!(names[0], "model");
    }
}
