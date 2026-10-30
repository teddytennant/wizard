//! What an approval prompt offers and says.
//!
//! The policy decides *that* a call needs approval ([`super::Decision::Ask`]).
//! This module decides what the person is shown: which tools Claude Code asks
//! about at all, the "don't ask again" rule on offer, and the text a refusal
//! carries back to the model.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Policy, shell};
use crate::hooks::toolmap::{self, ToolClass};

/// Tools Claude Code asks about outside read-only access: shell commands,
/// file edits, the web, MCP servers, and the few tools that run code or
/// leave a process behind. Everything else (the todo list, questions to the
/// user, subagents, task control) runs without asking in every mode.
pub fn asks_in_claude(tool: &str) -> bool {
    match toolmap::class(tool) {
        ToolClass::Shell | ToolClass::Edit | ToolClass::Web | ToolClass::Mcp => true,
        ToolClass::Read => false,
        ToolClass::Other => matches!(
            toolmap::claude_name(tool).as_str(),
            "Monitor"
                | "run_code"
                | "generate_image"
                | "CronCreate"
                | "ScheduleWakeup"
                | "computer"
        ),
    }
}

/// How long a "don't ask again" answer lasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scope {
    /// Written to the project's `.claude/settings.local.json` as an allow
    /// rule, and added to the rules in force.
    Project,
    /// This session only. For file edits this switches to `acceptEdits`; for
    /// anything else it adds an allow rule that is not written down.
    Session,
}

/// The "yes, and don't ask again" answer on offer for one prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offer {
    /// The option text, as Claude Code words it.
    pub label: String,
    /// Allow rules the answer adds. Empty for the edits-this-session offer.
    pub rules: Vec<String>,
    pub scope: Scope,
}

/// The "don't ask again" offer for a call, or `None` when no rule would say
/// what was approved (a command that cannot be split cleanly).
pub fn offer(tool: &str, args: &Value, cwd: &Path) -> Option<Offer> {
    let name = toolmap::claude_name(tool);
    match toolmap::class(tool) {
        ToolClass::Shell => {
            let command = args.get("command").and_then(Value::as_str)?;
            let parts = shell::split(command);
            if parts.unparseable {
                return None;
            }
            let mut prefixes: Vec<String> = Vec::new();
            for sub in &parts.commands {
                if shell::is_read_only(sub) {
                    continue;
                }
                let prefix = command_prefix(sub)?;
                if !prefixes.contains(&prefix) {
                    prefixes.push(prefix);
                }
            }
            if prefixes.is_empty() {
                return None;
            }
            Some(Offer {
                label: format!(
                    "Yes, and don't ask again for {} commands in {}",
                    prefixes
                        .iter()
                        .map(|p| format!("{p}:*"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    cwd.display()
                ),
                rules: prefixes.iter().map(|p| format!("Bash({p}:*)")).collect(),
                scope: Scope::Project,
            })
        }
        ToolClass::Edit => Some(Offer {
            label: "Yes, allow all edits during this session".to_string(),
            rules: Vec::new(),
            scope: Scope::Session,
        }),
        ToolClass::Web if name == "WebFetch" => {
            let host = args
                .get("url")
                .and_then(Value::as_str)
                .and_then(|url| url.split("://").nth(1))
                .and_then(|rest| rest.split(['/', '?', '#']).next())
                .map(|host| host.rsplit('@').next().unwrap_or(host))
                .filter(|host| !host.is_empty())?;
            Some(Offer {
                label: format!("Yes, and don't ask again for {host}"),
                rules: vec![format!("WebFetch(domain:{host})")],
                scope: Scope::Project,
            })
        }
        _ => Some(Offer {
            label: format!("Yes, and don't ask again for {name}"),
            rules: vec![name],
            scope: Scope::Project,
        }),
    }
}

/// The part of one command a "don't ask again" rule should cover: the
/// program, plus its subcommand for the tools that have them
/// (`git commit`, `npm run`, `cargo test`).
fn command_prefix(sub: &str) -> Option<String> {
    let stripped = shell::strip_prefixes(sub);
    let mut words = stripped.split_whitespace();
    let program = words.next()?;
    if program.contains(['(', ')', '*', '"', '\'', '\\']) {
        return None;
    }
    const WITH_SUBCOMMAND: [&str; 12] = [
        "git", "npm", "pnpm", "yarn", "cargo", "docker", "kubectl", "go", "pip", "uv", "gh", "make",
    ];
    if WITH_SUBCOMMAND.contains(&program)
        && let Some(next) = words.next()
        && !next.starts_with('-')
        && next.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Some(format!("{program} {next}"));
    }
    Some(program.to_string())
}

/// What the model reads when the person said no, with what they typed.
pub fn rejection(feedback: Option<&str>) -> String {
    match feedback.map(str::trim).filter(|text| !text.is_empty()) {
        Some(text) => format!(
            "The user doesn't want to proceed with this tool use. The tool use was rejected \
             (eg. if it was a file edit, the new_string was NOT written to the file). To tell \
             you how to proceed, the user said:\n{text}"
        ),
        None => "The user doesn't want to proceed with this tool use. The tool use was rejected \
                 (eg. if it was a file edit, the new_string was NOT written to the file). STOP \
                 what you are doing and wait for the user to tell you how to proceed."
            .to_string(),
    }
}

impl Policy {
    /// What a call that needed approval and found nobody to give it says to
    /// the model, in Claude Code's words.
    pub fn refusal(&self, tool: &str, args: &Value) -> String {
        let name = toolmap::claude_name(tool);
        match toolmap::class(tool) {
            ToolClass::Edit => {
                let target = toolmap::paths(tool, args)
                    .first()
                    .map(|path| super::absolutize(path, &self.cwd).display().to_string())
                    .unwrap_or_default();
                format!(
                    "Claude requested permissions to write to {target}, but you haven't granted \
                     it yet."
                )
            }
            ToolClass::Shell => self.shell_refusal(args),
            _ => format!(
                "Claude requested permissions to use {name}, but you haven't granted it yet."
            ),
        }
    }

    fn shell_refusal(&self, args: &Value) -> String {
        let command = args.get("command").and_then(Value::as_str).unwrap_or("");
        let parts = shell::split(command);
        let pending: Vec<&String> = parts
            .commands
            .iter()
            .filter(|sub| !shell::is_read_only(sub) && !self.covers_command(sub))
            .collect();
        if parts.commands.len() > 1 && !pending.is_empty() && !parts.unparseable {
            return format!(
                "This Bash command contains multiple operations. The following parts require \
                 approval: {}",
                pending
                    .iter()
                    .map(|sub| sub.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if parts.commands.len() == 1
            && !parts.unparseable
            && shell::is_file_command(&parts.commands[0], &self.cwd)
        {
            let program = shell::strip_prefixes(&parts.commands[0]);
            let program = program.split_whitespace().next().unwrap_or("");
            let operand = parts.commands[0]
                .split_whitespace()
                .skip(1)
                .find(|word| !word.starts_with('-'))
                .unwrap_or("");
            let target = super::absolutize(operand, &self.cwd);
            return format!(
                "{program} in '{}' needs approval. The path is inside the working directories \
                 for this session ('{}'), and Claude Code asks before a shell command creates, \
                 changes or removes files there.",
                target.display(),
                self.cwd.display()
            );
        }
        "This command requires approval".to_string()
    }
}
