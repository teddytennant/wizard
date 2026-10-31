//! Claude Code's slash-command vocabulary on Wizard's table.
//!
//! Three kinds of thing live here, and `docs/claude-code-parity.md` says which
//! Claude Code command is which:
//!
//! - [`ALIASES`]: a Claude Code name for a command Wizard already has
//!   (`/reset` is `/clear`). Rewritten before parsing, so every surface, the
//!   agent's `run_command` gate and the table see only the real name.
//! - [`bundled`]: prompt commands (`/init`, `/review`, `/security-review`, ...)
//!   shipped as markdown in `bundled/`. They load as custom commands at the
//!   lowest priority, so a user's or project's file of the same name wins.
//! - [`COMPAT`]: commands with a Rust implementation ([`run`]) that need no new
//!   parsed shape, reached through [`SlashCommand::Compat`]. Each has a row in
//!   [`COMMANDS`](super::COMMANDS) like any other built-in.

use std::path::Path;

use super::surface::CommandSurface;
use super::{CustomCommand, SlashCommand};

/// `(claude name, wizard command, argument words put in front)`.
pub const ALIASES: &[(&str, &str, &str)] = &[
    ("reset", "clear", ""),
    ("new", "clear", ""),
    ("continue", "resume", ""),
    ("undo", "rewind", ""),
    ("checkpoint", "rewind", ""),
    ("config", "settings", ""),
    ("stats", "usage", ""),
    ("checkup", "doctor", ""),
    ("tasks", "bashes", ""),
    ("reload-plugins", "reload", ""),
    ("reload-skills", "reload", ""),
    ("subtask", "fork", ""),
    ("list-agents", "agents", ""),
    ("peers", "agents", ""),
    ("allowed-tools", "permissions", ""),
    ("bug", "feedback", ""),
    ("share", "feedback", ""),
    ("marketplace", "plugin", "marketplace "),
    ("routines", "schedule", ""),
    ("theme", "ui", ""),
    ("focus", "view", ""),
];

/// The commands [`run`] implements. Every name has a row in the table.
pub const COMPAT: &[&str] = &[
    "permissions",
    "hooks",
    "context",
    "mcp",
    "output-style",
    "release-notes",
    "skills",
    "keybindings",
    "feedback",
    "schedule",
    "logout",
    "import",
    "export",
    "copy",
    "plugin",
    "terminal-setup",
    "ide",
    "loop",
    "branch",
    "rename",
    "cd",
    "background",
    "stop",
    "statusline",
    "autocompact",
    "fast",
    "advisor",
    "color",
    "voice",
    "usage-credits",
    "passes",
    "upgrade",
    "rate-limit-options",
    "privacy-settings",
    "heapdump",
    "insights",
    "restart",
    "setup-bedrock",
    "setup-vertex",
    "autofix-pr",
    "chrome",
    "design",
    "design-sync",
    "design-login",
    "design-consent",
    "design-revoke",
    "install-github-app",
    "install-slack-app",
    "mobile",
    "powerup",
    "radio",
    "remote-env",
    "sandbox",
    "stickers",
    "teleport",
    "tui",
    "web-setup",
    "workflows",
    "claude-api",
    "dataviz",
    "plugin-authoring",
    "auto-mode-setup",
];

/// Those of [`COMPAT`] that only read state, which the agent may run itself.
const READ_ONLY: &[&str] = &[
    "permissions",
    "hooks",
    "context",
    "mcp",
    "release-notes",
    "skills",
    "schedule",
    "ide",
    "fast",
    "advisor",
    "color",
    "voice",
    "usage-credits",
    "passes",
    "upgrade",
    "rate-limit-options",
    "privacy-settings",
    "heapdump",
    "insights",
    "restart",
    "setup-bedrock",
    "setup-vertex",
    "autofix-pr",
    "chrome",
    "design",
    "design-sync",
    "design-login",
    "design-consent",
    "design-revoke",
    "install-github-app",
    "install-slack-app",
    "mobile",
    "powerup",
    "radio",
    "remote-env",
    "sandbox",
    "stickers",
    "teleport",
    "tui",
    "web-setup",
    "claude-api",
    "dataviz",
    "plugin-authoring",
    "auto-mode-setup",
];

/// The `DISABLE_<NAME>_COMMAND` variable that turns a command off, if one is
/// set: Claude Code's way to take `/login`, `/logout`, `/upgrade`,
/// `/feedback` (and `/bug`), `/doctor`, `/usage-credits` (its extra-usage
/// command) and `/install-github-app` away from users of a managed install.
pub fn disabled_by(command: &str, get: &dyn Fn(&str) -> Option<String>) -> Option<&'static str> {
    let vars: &[&'static str] = match command {
        "login" => &["DISABLE_LOGIN_COMMAND"],
        "logout" => &["DISABLE_LOGOUT_COMMAND"],
        "upgrade" => &["DISABLE_UPGRADE_COMMAND"],
        "feedback" => &["DISABLE_FEEDBACK_COMMAND", "DISABLE_BUG_COMMAND"],
        "doctor" => &["DISABLE_DOCTOR_COMMAND"],
        "usage-credits" => &["DISABLE_EXTRA_USAGE_COMMAND"],
        "install-github-app" => &["DISABLE_INSTALL_GITHUB_APP_COMMAND"],
        _ => return None,
    };
    vars.iter().copied().find(|var| {
        get(var).is_some_and(|v| {
            !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "" | "0" | "false" | "no" | "off"
            )
        })
    })
}

pub fn is_compat(name: &str) -> bool {
    COMPAT.contains(&name)
}

pub fn is_alias(name: &str) -> bool {
    ALIASES.iter().any(|(alias, _, _)| *alias == name)
}

pub fn agent_may_run(name: &str) -> bool {
    READ_ONLY.contains(&name)
}

/// `/reset foo` as `/clear foo`: the line with its first word replaced by what
/// the alias stands for, or `None` when the first word is not an alias.
pub fn rewrite_alias(input: &str) -> Option<String> {
    let rest = input.trim().strip_prefix('/')?;
    let word_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let (word, tail) = rest.split_at(word_end);
    let (_, target, prefix) = ALIASES.iter().find(|(alias, _, _)| *alias == word)?;
    Some(format!("/{target} {prefix}{}", tail.trim_start()))
}

/* ---------------------------------------------------------------------- */
/* Bundled prompt commands                                                */
/* ---------------------------------------------------------------------- */

const BUNDLED: &[(&str, &str)] = &[
    ("init", include_str!("bundled/init.md")),
    (
        "security-review",
        include_str!("bundled/security-review.md"),
    ),
    ("review", include_str!("bundled/review.md")),
    ("code-review", include_str!("bundled/review.md")),
    ("pr-comments", include_str!("bundled/pr-comments.md")),
    ("simplify", include_str!("bundled/simplify.md")),
    ("recap", include_str!("bundled/recap.md")),
    ("add-dir", include_str!("bundled/add-dir.md")),
    ("debug", include_str!("bundled/debug.md")),
    ("verify", include_str!("bundled/verify.md")),
    ("update-config", include_str!("bundled/update-config.md")),
    ("run", include_str!("bundled/run.md")),
    (
        "run-skill-generator",
        include_str!("bundled/run-skill-generator.md"),
    ),
    (
        "team-onboarding",
        include_str!("bundled/team-onboarding.md"),
    ),
    (
        "fewer-permission-prompts",
        include_str!("bundled/fewer-permission-prompts.md"),
    ),
];

/// Bundled workflows that are also slash commands: the name, what the
/// arguments are called, and what to ask for when there are none.
const WORKFLOW_COMMANDS: &[(&str, &str, &str)] = &[
    ("deep-research", "question", "the question to research"),
    ("batch", "instruction", "the change to make everywhere"),
];

/// `/<name> <args>` for a bundled workflow: the prompt Claude Code's workflow
/// commands send, which tells the model to call `Workflow` with the arguments.
fn workflow_command(name: &str, noun: &str, ask: &str) -> Option<CustomCommand> {
    let (_, text) = crate::workflow::store::BUNDLED
        .iter()
        .find(|(file, _)| *file == name)?;
    let meta = crate::workflow::script::parse(text).ok()?.meta;
    let mut body = format!("Run the \"{name}\" workflow.\n{}\n", meta.description);
    if let Some(when) = &meta.when_to_use {
        body.push_str(when);
        body.push('\n');
    }
    if !meta.phases.is_empty() {
        body.push_str("\nPhases:\n");
        for phase in &meta.phases {
            match &phase.detail {
                Some(detail) => body.push_str(&format!("- {}: {detail}\n", phase.title)),
                None => body.push_str(&format!("- {}\n", phase.title)),
            }
        }
    }
    body.push_str(&format!(
        "\nThe {noun}: $ARGUMENTS\n\nIf that is empty, ask the user for {ask} and wait. Otherwise invoke: Workflow({{ name: \"{name}\", args: <the {noun} above, as a JSON string> }})\n"
    ));
    Some(CustomCommand {
        name: name.to_string(),
        description: Some(meta.description),
        template: body,
        path: std::path::PathBuf::from(format!("bundled/{name}.js")),
        pi_syntax: false,
    })
}

/// The prompt commands that ship in the binary.
pub fn bundled() -> Vec<CustomCommand> {
    let mut commands: Vec<CustomCommand> = BUNDLED
        .iter()
        .map(|(name, raw)| {
            let (meta, body) = crate::skills::split_frontmatter(raw);
            CustomCommand {
                name: (*name).to_string(),
                description: meta.description,
                template: body,
                path: std::path::PathBuf::from(format!("bundled/{name}.md")),
                pi_syntax: false,
            }
        })
        .collect();
    for (name, noun, ask) in WORKFLOW_COMMANDS {
        if let Some(command) = workflow_command(name, noun, ask) {
            commands.push(command);
        }
    }
    // The reference lives next to the runtime it describes.
    commands.push(CustomCommand {
        name: "workflow-authoring".to_string(),
        description: Some("load the reference for writing Workflow tool scripts".to_string()),
        template: crate::workflow::AUTHORING.to_string(),
        path: std::path::PathBuf::from("bundled/workflow-authoring.md"),
        pi_syntax: false,
    });
    commands
}

/* ---------------------------------------------------------------------- */
/* Commands with a body                                                   */
/* ---------------------------------------------------------------------- */

/// Run one of [`COMPAT`].
pub async fn run<S: CommandSurface + Send + ?Sized>(name: &str, args: &str, surface: &mut S) {
    let root = surface.project_root();
    let args = args.trim();
    match name {
        "permissions" => match super::views::permissions(&surface.snapshot(), &root, args) {
            Ok(text) => surface.notice(text),
            Err(message) => surface.error(message),
        },
        "hooks" => surface.notice(super::views::hooks(&root)),
        "loop" => match super::loop_cmd::run(args, &root) {
            Ok(text) => surface.notice(text),
            Err(message) => surface.error(message),
        },
        "branch" => {
            match super::session_cmds::branch(surface.snapshot().session.as_deref(), args) {
                Ok((id, message)) => {
                    surface.notice(message);
                    surface.resume(id).await;
                }
                Err(message) => surface.error(message),
            }
        }
        "rename" => reply(
            surface,
            super::session_cmds::rename(surface.snapshot().session.as_deref(), args),
        ),
        "cd" => reply(surface, super::session_cmds::change_dir(args, &root)),
        "background" => reply(
            surface,
            super::session_cmds::background(surface.snapshot().session.as_deref(), args, &root),
        ),
        "stop" => reply(surface, super::session_cmds::stop(args)),
        "workflows" => {
            // Bare `/workflows` opens the panel where the surface has one.
            if args.is_empty() && surface.open(super::surface::Chooser::Workflows).await {
                return;
            }
            reply(surface, crate::workflow::command::run(args, &root));
        }
        "statusline" => reply(surface, super::misc::statusline(args, &root)),
        "autocompact" => reply(surface, super::misc::autocompact(args)),
        "fast" => reply(surface, super::misc::fast(args)),
        "sandbox" => surface.notice(super::misc::sandbox(&root)),
        "privacy-settings" => surface.notice(super::misc::privacy()),
        other if super::misc::not_available(other).is_some() => {
            surface.notice(
                super::misc::not_available(other)
                    .unwrap_or_default()
                    .to_string(),
            );
        }
        "context" => {
            let text = context_report(&surface.snapshot(), &root);
            surface.notice(text);
        }
        "mcp" => match args {
            "reload" => surface.reload().await,
            _ => surface.notice(mcp_report(&root)),
        },
        "output-style" => {
            if args.is_empty() {
                surface.notice(crate::claude_dir::styles::report(&root));
            } else {
                match crate::claude_dir::styles::set_active(&root, args) {
                    Ok(name) => {
                        surface.reload().await;
                        surface.notice(format!("output style: {name}"));
                    }
                    Err(message) => surface.error(message),
                }
            }
        }
        "release-notes" => surface.notice(release_notes()),
        "skills" => surface.notice(skills_report()),
        "keybindings" => match keybindings(args) {
            Ok(text) => surface.notice(text),
            Err(message) => surface.error(message),
        },
        "feedback" => surface.notice(feedback_text(args)),
        "schedule" => surface.notice(schedule_report()),
        "logout" => match logout(args) {
            Ok(text) => surface.notice(text),
            Err(message) => surface.error(message),
        },
        "import" => match args {
            "" | "claude" => {
                surface
                    .import_claude(crate::import_claude::ImportSelection {
                        mcp: true,
                        commands: true,
                        verbs: true,
                    })
                    .await
            }
            other => surface.error(format!(
                "can only import from Claude Code (`/import claude`), not '{other}'"
            )),
        },
        "export" => export(args, surface),
        "copy" => copy(args, surface),
        "plugin" => plugin(args, &root, surface).await,
        "terminal-setup" => surface.notice(terminal_setup()),
        "ide" => surface.notice(ide_text()),
        other => surface.error(format!("'/{other}' is not a compat command")),
    }
}

fn reply<S: CommandSurface + Send + ?Sized>(surface: &mut S, outcome: Result<String, String>) {
    match outcome {
        Ok(text) => surface.notice(text),
        Err(message) => surface.error(message),
    }
}

fn context_report(session: &super::surface::SessionSnapshot, root: &Path) -> String {
    let instructions = crate::instructions::load(root).map_or(0, |text| text.len());
    let skills = crate::agent::load_skills().len();
    let commands = super::load(root).len();
    let servers = crate::config::Config::mcp_config_path()
        .and_then(|path| crate::mcp::McpConfig::load(&path))
        .map_or(0, |config| config.servers.len());
    let next = session
        .context_tokens
        .map_or("not known right now".to_string(), |n| format!("{n} tokens"));
    format!(
        "context\n  model: {}\n  next request: {next}\n  session so far: {} prompt + {} completion tokens\n  \
         instructions: {} KB   skills: {skills}   commands: {commands}   MCP servers: {servers}",
        session.model,
        session.prompt_tokens,
        session.completion_tokens,
        instructions.div_ceil(1024),
    )
}

fn mcp_report(root: &Path) -> String {
    let mut out = String::from("MCP servers\n");
    let configured = crate::config::Config::mcp_config_path()
        .and_then(|path| crate::mcp::McpConfig::load(&path))
        .map(|config| config.servers)
        .unwrap_or_default();
    let found = crate::claude_dir::mcp::discover(root);
    let line = |server: &crate::mcp::McpServerConfig, source: &str| {
        let target = server
            .url
            .clone()
            .or_else(|| {
                server
                    .command
                    .as_ref()
                    .map(|c| format!("{c} {}", server.args.join(" ")))
            })
            .unwrap_or_default();
        format!("  {:<18} {:<8} {}\n", server.name, source, target.trim())
    };
    for server in &configured {
        out.push_str(&line(server, "wizard"));
    }
    for server in &found.servers {
        if !configured.iter().any(|have| have.name == server.name) {
            let source = found
                .origins
                .get(&server.name)
                .copied()
                .unwrap_or("--mcp-config");
            out.push_str(&line(server, source));
        }
    }
    for name in &found.pending {
        out.push_str(&format!(
            "  {name:<18} waiting: .mcp.json runs commands, so it loads once the project is trusted (WIZARD_TRUST_PROJECT=1)\n"
        ));
    }
    for name in &found.unsupported {
        out.push_str(&format!("  {name:<18} skipped: SSE or malformed entry\n"));
    }
    if configured.is_empty() && found.servers.is_empty() && found.pending.is_empty() {
        out.push_str("  none; add them to ~/.wizard/mcp.toml or a project .mcp.json\n");
    }
    out.push_str("\n/mcp reload reconnects.");
    out
}

fn release_notes() -> String {
    let log = include_str!("../../CHANGELOG.md");
    let mut sections = log.split("\n## ");
    let _preamble = sections.next();
    match sections.find(|section| !section.starts_with("[Unreleased]")) {
        Some(latest) => format!("## {}", latest.trim_end()),
        None => "no release notes in this build".to_string(),
    }
}

fn skills_report() -> String {
    let skills = crate::agent::load_skills();
    if skills.is_empty() {
        return "no skills; put SKILL.md folders in ~/.wizard/skills, ~/.claude/skills or .claude/skills"
            .to_string();
    }
    let mut out = format!("skills ({})\n", skills.len());
    for skill in &skills {
        let tokens =
            (skill.meta.description.as_deref().map_or(0, str::len) + skill.name.len()).div_ceil(4);
        out.push_str(&format!(
            "  {:<24} ~{tokens:>4} tok in the prompt  {}\n",
            skill.name,
            skill.path.display()
        ));
    }
    out
}

const KEYBINDINGS_TEMPLATE: &str = "{\n  \"bindings\": {}\n}\n";

fn keybindings(args: &str) -> Result<String, String> {
    let path = crate::config::Config::wizard_dir()
        .map_err(|err| err.to_string())?
        .join("keybindings.json");
    if args == "init" {
        if path.exists() {
            return Ok(format!("{} already exists", path.display()));
        }
        std::fs::write(&path, KEYBINDINGS_TEMPLATE).map_err(|err| err.to_string())?;
        return Ok(format!("created {}", path.display()));
    }
    let state = if path.exists() {
        "exists"
    } else {
        "not created; `/keybindings init` writes a template"
    };
    Ok(format!("keybindings file: {} ({state})", path.display()))
}

fn feedback_text(report: &str) -> String {
    let url = env!("CARGO_PKG_REPOSITORY");
    let url = if url.is_empty() {
        "https://github.com/teddytennant/wizard"
    } else {
        url
    };
    let body = if report.is_empty() {
        String::new()
    } else {
        format!("\n\nYour note, ready to paste:\n{report}")
    };
    format!(
        "Report bugs and ideas at {url}/issues. Run `wizard doctor --bundle` first and attach \
         the output.{body}"
    )
}

fn schedule_report() -> String {
    let path = crate::config::Config::wizard_dir().map(|dir| dir.join("schedule.toml"));
    let entries = path
        .as_ref()
        .ok()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|raw| raw.parse::<toml::Table>().ok());
    let mut out = String::from("scheduled runs\n");
    let list = entries
        .as_ref()
        .and_then(|doc| doc.values().find_map(toml::Value::as_array));
    match list {
        Some(list) if !list.is_empty() => {
            for entry in list {
                let field = |key: &str| entry.get(key).and_then(toml::Value::as_str).unwrap_or("-");
                out.push_str(&format!(
                    "  {:<16} {:<18} {}\n",
                    field("name"),
                    field("cron"),
                    field("prompt")
                ));
            }
        }
        _ => out.push_str("  none\n"),
    }
    out.push_str("\nManage them with `wizard schedule` (docs/scheduler.md).");
    out
}

/// `/logout <account>`, also `wizard auth logout`.
pub fn sign_out(account: &str) -> Result<String, String> {
    logout(account)
}

fn logout(args: &str) -> Result<String, String> {
    let file = match args {
        "xai" => "xai_oauth.json",
        "chatgpt" => "chatgpt_oauth.json",
        "" => return Err("name the account to sign out of: /logout xai or /logout chatgpt".into()),
        other => return Err(format!("unknown account '{other}' (xai|chatgpt)")),
    };
    let path = crate::config::Config::wizard_dir()
        .map_err(|err| err.to_string())?
        .join(file);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(format!("signed out: removed {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok(format!("not signed in to {args}"))
        }
        Err(err) => Err(format!("{}: {err}", path.display())),
    }
}

fn export<S: CommandSurface + ?Sized>(args: &str, surface: &mut S) {
    let Some(turns) = surface.transcript() else {
        return surface.error("this surface has no conversation to export".into());
    };
    let text: String = turns
        .iter()
        .map(|(who, text)| format!("{who}:\n{text}\n\n"))
        .collect();
    if args.is_empty() {
        if surface.copy_to_clipboard(&text) {
            surface.notice(format!("copied {} turns to the clipboard", turns.len()));
        } else {
            surface.error("no clipboard here; give a file name: /export <file>".into());
        }
        return;
    }
    let path = surface.project_root().join(args);
    match std::fs::write(&path, text) {
        Ok(()) => surface.notice(format!("wrote {} turns to {}", turns.len(), path.display())),
        Err(err) => surface.error(format!("{}: {err}", path.display())),
    }
}

fn copy<S: CommandSurface + ?Sized>(args: &str, surface: &mut S) {
    let nth: usize = match args {
        "" => 1,
        n => match n.parse::<usize>() {
            Ok(n) if n > 0 => n,
            _ => {
                return surface.error("usage: /copy [N]  (the Nth latest reply, default 1)".into());
            }
        },
    };
    let Some(turns) = surface.transcript() else {
        return surface.error("this surface has no conversation to copy from".into());
    };
    let reply = turns
        .iter()
        .rev()
        .filter(|(who, text)| who == "assistant" && !text.trim().is_empty())
        .nth(nth - 1);
    match reply {
        None => surface.error("no such reply yet".into()),
        Some((_, text)) => {
            if surface.copy_to_clipboard(text) {
                surface.notice(format!("copied {} characters", text.chars().count()));
            } else {
                surface.error("no clipboard available here".into());
            }
        }
    }
}

async fn plugin<S: CommandSurface + Send + ?Sized>(args: &str, root: &Path, surface: &mut S) {
    let words: Vec<String> = args.split_whitespace().map(str::to_string).collect();
    let root = root.to_path_buf();
    let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
        use crate::claude_dir::plugins as p;
        let words: Vec<&str> = words.iter().map(String::as_str).collect();
        match words.as_slice() {
            [] | ["list"] => Ok(p::report(&root)),
            ["install", spec] => p::install(spec),
            ["uninstall" | "remove", name] => p::uninstall(name),
            ["enable", name] => p::set_enabled(name, true),
            ["disable", name] => p::set_enabled(name, false),
            ["validate", dir] => p::validate(Path::new(dir)),
            ["marketplace"] | ["marketplace", "list"] => Ok(p::marketplace_list()),
            ["marketplace", "add", source] => p::marketplace_add(source),
            ["marketplace", "remove", name] => p::marketplace_remove(name),
            ["marketplace", "update"] => p::marketplace_update(),
            _ => Err("usage: /plugin [list|install <spec>|uninstall <name>|enable <name>|disable <name>|validate <dir>|marketplace add|list|remove|update]".to_string()),
        }
    })
    .await
    .unwrap_or_else(|err| Err(err.to_string()));
    match result {
        Ok(text) => {
            let changed = args.starts_with("install")
                || args.starts_with("uninstall")
                || args.starts_with("remove")
                || args.starts_with("enable")
                || args.starts_with("disable");
            surface.notice(text);
            if changed {
                surface.reload().await;
            }
        }
        Err(message) => surface.error(message),
    }
}

fn terminal_setup() -> String {
    let term = std::env::var("TERM_PROGRAM").unwrap_or_else(|_| "unknown".into());
    format!(
        "terminal: {term}\nWizard needs no terminal setup: it asks the terminal for the Kitty \
         keyboard protocol where there is one, and Alt+Enter or a trailing backslash starts a new \
         line anywhere else. Clipboard copies use OSC 52 over SSH and tmux."
    )
}

fn ide_text() -> String {
    "Wizard talks to editors over ACP: point Zed, Neovim or Emacs at `wizard acp`. See docs/acp.md."
        .to_string()
}

impl SlashCommand {
    /// A Claude Code command Wizard implements in [`run`].
    pub(super) fn compat(name: &str, args: &str) -> Self {
        Self::Compat {
            name: name.to_string(),
            args: args.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disable_command_variables_turn_a_command_off() {
        let set = |var: &'static str| move |name: &str| (name == var).then(|| "1".to_string());
        assert_eq!(
            disabled_by("login", &set("DISABLE_LOGIN_COMMAND")),
            Some("DISABLE_LOGIN_COMMAND")
        );
        assert_eq!(
            disabled_by("feedback", &set("DISABLE_BUG_COMMAND")),
            Some("DISABLE_BUG_COMMAND")
        );
        assert_eq!(disabled_by("login", &set("DISABLE_LOGOUT_COMMAND")), None);
        assert_eq!(disabled_by("model", &set("DISABLE_LOGIN_COMMAND")), None);
        assert_eq!(disabled_by("login", &|_| Some("0".to_string())), None);
    }

    #[test]
    fn an_alias_is_rewritten_to_the_real_command_keeping_its_arguments() {
        assert_eq!(rewrite_alias("/reset"), Some("/clear ".into()));
        assert_eq!(
            rewrite_alias("/subtask read   the docs"),
            Some("/fork read   the docs".into())
        );
        assert_eq!(
            rewrite_alias("/marketplace add owner/repo"),
            Some("/plugin marketplace add owner/repo".into())
        );
        assert_eq!(rewrite_alias("/clear"), None);
        assert_eq!(rewrite_alias("not a command"), None);
    }

    #[test]
    fn every_alias_points_at_a_command_the_table_has() {
        for (alias, target, _) in ALIASES {
            assert!(
                super::super::spec(target).is_some(),
                "/{alias} points at /{target}, which has no table row"
            );
            assert!(
                super::super::spec(alias).is_none(),
                "/{alias} has its own row and is also an alias"
            );
        }
    }

    #[test]
    fn every_compat_name_has_a_row_and_parses_to_itself() {
        for name in COMPAT {
            assert!(super::super::spec(name).is_some(), "/{name} has no row");
            match SlashCommand::parse(&format!("/{name} x y")) {
                Some(Ok(SlashCommand::Compat { name: got, args })) => {
                    assert_eq!(&got, name);
                    assert_eq!(args, "x y");
                }
                other => panic!("/{name} parsed to {other:?}"),
            }
        }
    }

    #[test]
    fn the_agent_may_only_run_the_read_only_compat_commands() {
        for name in COMPAT {
            let parsed = SlashCommand::parse(&format!("/{name}")).unwrap().unwrap();
            assert_eq!(
                parsed.agent_runnable().is_ok(),
                READ_ONLY.contains(name),
                "/{name}"
            );
        }
        assert!(READ_ONLY.iter().all(|name| COMPAT.contains(name)));
    }

    #[test]
    fn bundled_prompts_load_with_descriptions_and_no_stray_placeholders() {
        let all = bundled();
        for name in [
            "init",
            "review",
            "code-review",
            "security-review",
            "pr-comments",
            "simplify",
            "recap",
            "add-dir",
            "workflow-authoring",
        ] {
            let command = all.iter().find(|c| c.name == name).expect(name);
            assert!(command.description.is_some(), "/{name} has a description");
            assert!(!command.template.trim().is_empty());
        }
        let review = all.iter().find(|c| c.name == "review").unwrap();
        assert!(review.expand("123").contains("Target: 123"));
    }

    #[test]
    fn workflow_commands_tell_the_model_to_call_the_workflow_tool() {
        let all = bundled();
        let research = all.iter().find(|c| c.name == "deep-research").unwrap();
        let prompt = research.expand("is zinc worth taking");
        assert!(
            prompt.starts_with("Run the \"deep-research\" workflow."),
            "{prompt}"
        );
        assert!(
            prompt.contains("Phases:\n- Scope: split the question"),
            "{prompt}"
        );
        assert!(prompt.contains("The question: is zinc worth taking"));
        assert!(prompt.contains("Workflow({ name: \"deep-research\", args:"));
        let batch = all.iter().find(|c| c.name == "batch").unwrap();
        assert!(
            batch
                .expand("rename Foo")
                .contains("The instruction: rename Foo")
        );
        assert!(!COMPAT.contains(&"batch") && !COMPAT.contains(&"deep-research"));
    }

    #[test]
    fn a_project_file_shadows_a_bundled_command() {
        let tmp = crate::claude_dir::testing::Tmp::new("shadow");
        tmp.write(
            ".claude/commands/init.md",
            "---\ndescription: mine\n---\nmy init",
        );
        let commands = super::super::load(tmp.path());
        let init = commands.iter().find(|c| c.name == "init").expect("init");
        assert_eq!(init.template, "my init");
    }

    #[test]
    fn claude_commands_load_with_folder_namespaces() {
        let tmp = crate::claude_dir::testing::Tmp::new("ns");
        tmp.write(
            ".claude/commands/hi.md",
            "---\ndescription: say hi\n---\nSay hi to $ARGUMENTS",
        );
        tmp.write(".claude/commands/team/deploy.md", "deploy $1");
        let commands = super::super::load(tmp.path());
        let hi = commands.iter().find(|c| c.name == "hi").expect("hi");
        assert_eq!(hi.expand("bob"), "Say hi to bob");
        let deploy = commands
            .iter()
            .find(|c| c.name == "team:deploy")
            .expect("namespaced");
        assert_eq!(deploy.expand("prod"), "deploy prod");
    }

    #[test]
    fn release_notes_is_the_newest_changelog_section() {
        let notes = release_notes();
        assert!(notes.starts_with("## "), "{notes}");
    }
}
