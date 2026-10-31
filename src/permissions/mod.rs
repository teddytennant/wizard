//! Permission rules and modes, in Claude Code's vocabulary.
//!
//! Rules are `Tool` or `Tool(specifier)` strings in three lists, `deny`,
//! `ask` and `allow`. A call is checked against them in that order and the
//! first list with a match decides, whatever the specificity: an allow rule
//! cannot carve an exception out of a deny.
//!
//! Wizard's own `default` runs anything no rule objects to. Naming a mode
//! (`--permission-mode default`, `acceptEdits`, `plan`) opts in to Claude
//! Code's: writes, commands and other side effects are asked about, by a
//! `PermissionRequest` hook first and then by whoever is at the surface. See
//! `docs/permissions.md`.

mod persist;
mod prompt;
mod rule;
mod shell;

use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use persist::{persist_allow, settings_local_path};
pub use prompt::{Offer, Scope, offer, rejection};
pub use rule::{Rule, absolutize};

use crate::hooks::toolmap::{self, ToolClass};
use crate::tools::ToolAccess;

/// How strictly tool calls are checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PermissionMode {
    /// Wizard's usual behavior: calls no rule objects to run.
    #[default]
    #[serde(rename = "default")]
    Default,
    /// File edits run; other side effects need an allow rule.
    #[serde(rename = "acceptEdits")]
    AcceptEdits,
    /// Read-only tools only.
    #[serde(rename = "plan")]
    Plan,
    /// Only read-only tools and calls an allow rule names; nothing is asked.
    #[serde(rename = "dontAsk")]
    DontAsk,
    /// Nothing is checked except deny and ask rules.
    #[serde(rename = "bypassPermissions")]
    BypassPermissions,
    /// Claude Code's classifier mode. Wizard has no classifier, so this is
    /// `default`.
    #[serde(rename = "auto")]
    Auto,
}

impl PermissionMode {
    /// The spelling Claude Code uses in `settings.json` and hook payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionMode::Default => "default",
            PermissionMode::AcceptEdits => "acceptEdits",
            PermissionMode::Plan => "plan",
            PermissionMode::DontAsk => "dontAsk",
            PermissionMode::BypassPermissions => "bypassPermissions",
            PermissionMode::Auto => "auto",
        }
    }
}

impl FromStr for PermissionMode {
    type Err = String;

    /// Accepts `acceptEdits`, `accept-edits` and `accept_edits` alike.
    fn from_str(text: &str) -> Result<Self, String> {
        let key: String = text
            .chars()
            .filter(|c| !matches!(c, '-' | '_'))
            .collect::<String>()
            .to_ascii_lowercase();
        Ok(match key.as_str() {
            "default" | "manual" => PermissionMode::Default,
            "acceptedits" => PermissionMode::AcceptEdits,
            "plan" => PermissionMode::Plan,
            "dontask" => PermissionMode::DontAsk,
            "bypasspermissions" | "bypass" => PermissionMode::BypassPermissions,
            "auto" => PermissionMode::Auto,
            _ => {
                return Err(format!(
                    "unknown permission mode `{text}` (default, acceptEdits, plan, dontAsk, bypassPermissions, auto)"
                ));
            }
        })
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the policy says about one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Refused, with the text the model reads.
    Deny(String),
    /// Needs an approver. With nobody to ask, this is a refusal; a
    /// `PermissionRequest` hook is the approver.
    Ask(String),
}

/// The rules and mode in force for a session.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    pub mode: PermissionMode,
    pub allow: Vec<Rule>,
    pub deny: Vec<Rule>,
    pub ask: Vec<Rule>,
    pub cwd: PathBuf,
    /// A mode was named, so `default` means Claude Code's `default`: ask
    /// before writes, commands and the like. Off, `default` is Wizard's own
    /// and runs whatever no rule objects to.
    pub claude_defaults: bool,
}

impl Policy {
    /// An empty policy rooted at `cwd`.
    pub fn new(cwd: PathBuf) -> Self {
        Policy {
            cwd,
            ..Policy::default()
        }
    }

    /// True when no rule is set and the mode is the default: checking would
    /// change nothing.
    pub fn is_inert(&self) -> bool {
        self.allow.is_empty()
            && self.deny.is_empty()
            && self.ask.is_empty()
            && !self.claude_defaults
            && matches!(self.mode, PermissionMode::Default | PermissionMode::Auto)
    }

    /// Decide one call. `access` is the tool's own claim about what it
    /// touches ([`ToolAccess`]).
    pub fn check(&self, tool: &str, access: ToolAccess, args: &Value) -> Decision {
        let name = toolmap::claude_name(tool);
        let what = describe(tool, args);

        if self.deny.iter().any(|rule| self.denies(rule, tool, args)) {
            return Decision::Deny(format!("Permission to use {name}{what} has been denied."));
        }
        if self
            .ask
            .iter()
            .any(|rule| rule.matches_call(tool, args, &self.cwd, true))
        {
            return Decision::Ask(format!("{name}{what} requires approval (ask rule)."));
        }
        let read_only = access == ToolAccess::ReadOnly;
        match self.mode {
            PermissionMode::Default if self.claude_defaults => {
                if read_only || !prompt::asks_in_claude(tool) || self.allowed(tool, args) {
                    Decision::Allow
                } else {
                    Decision::Ask(format!("{name}{what} needs approval."))
                }
            }
            PermissionMode::Default | PermissionMode::Auto | PermissionMode::BypassPermissions => {
                Decision::Allow
            }
            PermissionMode::Plan => {
                if read_only {
                    Decision::Allow
                } else {
                    Decision::Deny(format!(
                        "Permission to use {name}{what} has been denied because the session is in plan mode."
                    ))
                }
            }
            PermissionMode::DontAsk => {
                if read_only || self.allowed(tool, args) {
                    Decision::Allow
                } else {
                    Decision::Deny(format!(
                        "Permission to use {name}{what} has been denied because Wizard is in dontAsk mode and no allow rule matches."
                    ))
                }
            }
            PermissionMode::AcceptEdits => {
                if read_only
                    || !prompt::asks_in_claude(tool)
                    || self.edits_inside(tool, access, args)
                    || self.runs_file_commands(tool, args)
                    || self.allowed(tool, args)
                {
                    Decision::Allow
                } else {
                    Decision::Ask(format!("{name}{what} needs approval in acceptEdits mode."))
                }
            }
        }
    }

    /// An edit whose every path is inside the working directory, which
    /// `acceptEdits` runs. A path-less edit (a tool that does not say what it
    /// touches) counts: the mode has always let those through.
    fn edits_inside(&self, tool: &str, access: ToolAccess, args: &Value) -> bool {
        if access != ToolAccess::Edit && toolmap::class(tool) != ToolClass::Edit {
            return false;
        }
        toolmap::paths(tool, args)
            .iter()
            .all(|path| shell::inside(&self.cwd, path))
    }

    /// A shell command made only of read-only pieces and file commands
    /// (`touch`, `mkdir`, `mv`, `cp`, `rm`, `rmdir`, `sed`) on paths inside the
    /// working directory, which `acceptEdits` runs. A `cd` anywhere in it
    /// makes the paths ambiguous, so it asks.
    fn runs_file_commands(&self, tool: &str, args: &Value) -> bool {
        if toolmap::class(tool) != ToolClass::Shell {
            return false;
        }
        let Some(command) = args.get("command").and_then(Value::as_str) else {
            return false;
        };
        let parts = shell::split(command);
        if parts.unparseable
            || parts.commands.is_empty()
            || shell::changes_directory(&parts.commands)
        {
            return false;
        }
        parts
            .commands
            .iter()
            .all(|sub| shell::is_read_only(sub) || shell::is_file_command(sub, &self.cwd))
    }

    /// A deny rule applies to the call, directly or through `Read`: a `Read`
    /// deny on a path also stops edits to it.
    fn denies(&self, rule: &Rule, tool: &str, args: &Value) -> bool {
        if rule.matches_call(tool, args, &self.cwd, true) {
            return true;
        }
        rule.tool == "Read"
            && toolmap::class(tool) == ToolClass::Edit
            && rule.spec.is_some()
            && toolmap::paths(tool, args).iter().any(|path| {
                Rule {
                    tool: "Edit".to_string(),
                    ..rule.clone()
                }
                .matches_call(
                    "write_file",
                    &serde_json::json!({ "path": path }),
                    &self.cwd,
                    true,
                )
            })
    }

    /// An `ask` rule names this call. A prompt the mode raised (Claude's
    /// `default` asking about a write) can be answered by a hook's `allow`;
    /// one an `ask` rule raised cannot.
    pub fn ask_rule_matches(&self, tool: &str, args: &Value) -> bool {
        self.ask
            .iter()
            .any(|rule| rule.matches_call(tool, args, &self.cwd, true))
    }

    /// An allow rule covers this one shell subcommand.
    fn covers_command(&self, sub: &str) -> bool {
        self.allow.iter().any(|rule| {
            rule.names("execute") && (rule.is_whole_tool() || rule.matches_command(sub, false))
        })
    }

    /// Add an allow rule to the ones in force, without writing it anywhere.
    pub fn allow_rule(&mut self, raw: &str, source: &str) {
        match Rule::parse(raw, &self.cwd) {
            Ok(rule) => self.allow.push(rule.from_source(source)),
            Err(why) => tracing::warn!("{source}: {why}"),
        }
    }

    /// Some allow rule covers the whole call. For a compound shell command
    /// every subcommand needs its own cover, and a command that could not be
    /// split is never covered by a scoped rule.
    fn allowed(&self, tool: &str, args: &Value) -> bool {
        if self
            .allow
            .iter()
            .any(|rule| rule.names(tool) && rule.is_whole_tool())
        {
            return true;
        }
        if toolmap::class(tool) != ToolClass::Shell {
            return self
                .allow
                .iter()
                .any(|rule| rule.matches_call(tool, args, &self.cwd, false));
        }
        let Some(command) = args.get("command").and_then(Value::as_str) else {
            return false;
        };
        let parts = shell::split(command);
        if parts.unparseable || parts.commands.is_empty() {
            return false;
        }
        parts.commands.iter().all(|sub| {
            shell::is_read_only(sub)
                || self
                    .allow
                    .iter()
                    .any(|rule| rule.names(tool) && rule.matches_command(sub, false))
        })
    }

    /// Rules grouped for `/permissions`.
    pub fn summary(&self) -> String {
        let mut out = format!("mode: {}\n", self.mode);
        if self.claude_defaults && self.mode == PermissionMode::Default {
            out.push_str("asks before file edits, commands, the web and MCP tools\n");
        }
        for (label, rules) in [
            ("deny", &self.deny),
            ("ask", &self.ask),
            ("allow", &self.allow),
        ] {
            if rules.is_empty() {
                continue;
            }
            out.push_str(&format!("{label}:\n"));
            for rule in rules {
                out.push_str(&format!("  {}  ({})\n", rule.raw, rule.source));
            }
        }
        if self.is_inert() {
            out.push_str("no rules set; every tool call runs\n");
        }
        out
    }
}

/// Every piece of the command line is read-only (`ls`, `grep`, `git log`),
/// with nothing that could not be split. Plan mode lets these through.
pub fn is_read_only_command(command: &str) -> bool {
    let parts = shell::split(command);
    !parts.unparseable
        && !parts.commands.is_empty()
        && parts.commands.iter().all(|sub| shell::is_read_only(sub))
}

/// ` with command <cmd>` or ` on <path>`, for messages.
fn describe(tool: &str, args: &Value) -> String {
    if let Some(command) = args.get("command").and_then(Value::as_str) {
        return format!(" with command {command}");
    }
    match toolmap::paths(tool, args).first() {
        Some(path) => format!(" on {path}"),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests;

/// The policy for a session: every settings layer, `[permissions]` in
/// `config.toml`, and this run's flags. Deny wins whichever list it came
/// from, so merging is concatenation.
pub fn build(
    config: &crate::config::Config,
    settings: &crate::config::claude::ClaudeSettings,
    cwd: &std::path::Path,
) -> Policy {
    let mut policy = Policy::new(cwd.to_path_buf());
    // Permissions are an option. Nothing asked for them, so every call runs, as
    // it always did: a Claude Code `permissions` block on its own does not count.
    if !opted_in(config, settings) {
        return policy;
    }
    policy.allow = settings.allow.clone();
    policy.ask = settings.ask.clone();
    policy.deny = settings.deny.clone();
    let mut add = |list: &[String], into: fn(&mut Policy) -> &mut Vec<Rule>, source: &str| {
        for raw in list {
            match Rule::parse(raw, cwd) {
                Ok(rule) => into(&mut policy).push(rule.from_source(source)),
                Err(why) => tracing::warn!("{source}: {why}"),
            }
        }
    };
    add(&config.permissions.allow, |p| &mut p.allow, "config.toml");
    add(&config.permissions.ask, |p| &mut p.ask, "config.toml");
    add(&config.permissions.deny, |p| &mut p.deny, "config.toml");
    add(
        &config.run_permissions.allow,
        |p| &mut p.allow,
        "--allowedTools",
    );
    add(
        &config.run_permissions.deny,
        |p| &mut p.deny,
        "--disallowedTools",
    );
    let named = config
        .run_permissions
        .mode
        .or(config.permissions.mode)
        .or(settings.default_mode);
    let mode = named.unwrap_or_default();
    // Naming a mode is what turns Claude's prompts on. `auto` stays Wizard's
    // own default: it is Claude's classifier mode and there is no classifier.
    policy.claude_defaults = matches!(named, Some(PermissionMode::Default | PermissionMode::Plan));
    // Plan mode is the session's own loop (`config.plan_first`, `exit_plan`);
    // a rule-level plan mode would keep refusing writes after the plan is
    // approved.
    policy.mode = if mode == PermissionMode::Plan {
        PermissionMode::Default
    } else {
        mode
    };
    policy
}

/// Whether this run asked for permissions: a flag, `[permissions]` in
/// `config.toml`, or a permissions block in one of Wizard's own settings files.
pub fn opted_in(
    config: &crate::config::Config,
    settings: &crate::config::claude::ClaudeSettings,
) -> bool {
    let run = &config.run_permissions;
    let own = &config.permissions;
    run.mode.is_some()
        || !run.allow.is_empty()
        || !run.deny.is_empty()
        || own.mode.is_some()
        || !own.allow.is_empty()
        || !own.ask.is_empty()
        || !own.deny.is_empty()
        || settings.permissions_opt_in
}
