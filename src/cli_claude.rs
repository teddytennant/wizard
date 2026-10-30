//! Claude Code's command-line flags, accepted by Wizard.
//!
//! Each flag does one of two things, and `docs/claude-code-parity.md` lists
//! which: it maps onto something Wizard has (`--model`, `--effort`,
//! `--max-turns`, `--add-dir`, `--system-prompt`,
//! `--dangerously-skip-permissions`, ...), or it is accepted and reported on
//! stderr as not supported. Nothing is silently dropped.
//!
//! Settings that other subsystems read are published as environment variables
//! before the runtime starts ([`ClaudeFlags::publish`]), the way `--harness-dir`
//! and `--token-profile` already are. That way a subagent or a fleet worker,
//! which are Wizard processes, inherits them.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::Args;

use crate::config::{Config, ReasoningEffort, StepBudget};

/// `--input-format` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum InputFormat {
    #[default]
    Text,
    /// One JSON object per line; `{"type":"user","message":{"content":...}}`
    /// lines are the prompt.
    StreamJson,
}

#[derive(Debug, Clone, Default, Args)]
pub struct ClaudeFlags {
    /// The task, as plain words: `wizard fix the failing test`. Same as
    /// `-p "..."` but without the quotes' being required.
    #[arg(value_name = "PROMPT")]
    pub task: Vec<String>,

    /// Print the answer and exit. The prompt comes from `-p`, the words after
    /// the flags, or stdin. Runs headless whatever the saved mode is.
    #[arg(long)]
    pub print: bool,

    /// Continue the most recent conversation in this directory.
    #[arg(short = 'c', long = "continue")]
    pub continue_session: bool,

    /// Resume the session with this id. Written as `--resume <id>` and
    /// rewritten to this by the argument pre-pass.
    #[arg(long, hide = true, value_name = "ID")]
    pub resume_id: Option<String>,

    /// How `-p` reads its prompt on stdin.
    #[arg(long, value_enum, value_name = "FORMAT")]
    pub input_format: Option<InputFormat>,

    /// Replace the system prompt with this text.
    #[arg(long, value_name = "PROMPT")]
    pub system_prompt: Option<String>,

    /// Replace the system prompt with the contents of this file.
    #[arg(long, value_name = "FILE")]
    pub system_prompt_file: Option<PathBuf>,

    /// Add this text to the system prompt.
    #[arg(long, value_name = "PROMPT")]
    pub append_system_prompt: Option<String>,

    /// Add the contents of this file to the system prompt.
    #[arg(long, value_name = "FILE")]
    pub append_system_prompt_file: Option<PathBuf>,

    /// More directories the agent should work in.
    #[arg(long, num_args = 1.., value_name = "DIR")]
    pub add_dir: Vec<PathBuf>,

    /// Model for this run, instead of the active provider's.
    #[arg(long, value_name = "MODEL")]
    pub model: Option<String>,

    /// Reasoning effort for this run: low, medium, high, xhigh or max.
    #[arg(long, value_name = "LEVEL")]
    pub effort: Option<String>,

    /// Most agent steps (model calls with tool use) a turn may take.
    #[arg(long, value_name = "N")]
    pub max_turns: Option<u32>,

    /// Context size compaction counts for this run: `auto`, or a token count
    /// such as `200k` or `1m`.
    #[arg(long, value_name = "auto|TOKENS")]
    pub autocompact: Option<String>,

    /// MCP servers for this run, from JSON files or JSON strings.
    #[arg(long, num_args = 1.., value_name = "FILE_OR_JSON")]
    pub mcp_config: Vec<String>,

    /// Use only the `--mcp-config` servers.
    #[arg(long)]
    pub strict_mcp_config: bool,

    /// Define subagents for this run as JSON: `{"name": {"description": ..., "prompt": ...}}`.
    #[arg(long, value_name = "JSON")]
    pub agents: Option<String>,

    /// Load a Claude-format plugin directory for this run (repeatable).
    #[arg(long, value_name = "DIR")]
    pub plugin_dir: Vec<PathBuf>,

    /// Ask for the final answer as JSON matching this schema.
    #[arg(long, value_name = "SCHEMA")]
    pub json_schema: Option<String>,

    /// Work in a new git worktree under `.claude/worktrees/<name>`.
    #[arg(short = 'w', long, num_args = 0..=1, value_name = "NAME")]
    pub worktree: Option<Option<String>>,

    /// No automatic discovery of instruction files, `.claude` commands,
    /// agents, plugins or `.mcp.json`.
    #[arg(long)]
    pub bare: bool,

    /// `--bare`, and also none of Wizard's own project customizations.
    #[arg(long)]
    pub safe_mode: bool,

    /// Turn off custom slash commands and skills.
    #[arg(long)]
    pub disable_slash_commands: bool,

    /// More logging, to the log file.
    #[arg(long)]
    pub verbose: bool,

    /// Debug logging, optionally a filter: `--debug=agent`.
    #[arg(short = 'd', long, num_args = 0..=1, require_equals = true, value_name = "FILTER")]
    pub debug: Option<Option<String>>,

    /// Same as `--permission-mode bypassPermissions`.
    #[arg(long)]
    pub dangerously_skip_permissions: bool,

    /// Accepted for the stream-json formats, which already carry everything.
    #[arg(long, hide = true)]
    pub include_partial_messages: bool,
    #[arg(long, hide = true)]
    pub include_hook_events: bool,
    #[arg(long, hide = true)]
    pub replay_user_messages: bool,

    /// Use this session id (letters, digits, `-`, `_`, `.`) for a new session.
    /// With `--resume` or `--continue` it needs `--fork-session`.
    #[arg(long, value_name = "ID")]
    pub session_id: Option<String>,

    /// A display name for the session, shown by `/resume` and accepted by
    /// `--resume <name>`.
    #[arg(short = 'n', long, value_name = "NAME")]
    pub name: Option<String>,

    /// With `--resume` or `--continue`: start a new session that holds a copy
    /// of that conversation instead of continuing the original.
    #[arg(long)]
    pub fork_session: bool,

    /// Do not keep the session: the file is removed when a `--print` run ends.
    #[arg(long)]
    pub no_session_persistence: bool,

    /// With `--print`: models to switch to, in order, when the first one keeps
    /// failing. Comma separated.
    #[arg(long, value_name = "MODELS")]
    pub fallback_model: Option<String>,

    /// With `--print`: stop the run once its estimated cost passes this many
    /// dollars.
    #[arg(long, value_name = "USD")]
    pub max_budget_usd: Option<f64>,

    /// Extra settings for this run: a Claude Code `settings.json` path or a
    /// JSON string. Read after the project's files.
    #[arg(long, value_name = "FILE_OR_JSON")]
    pub settings: Option<String>,

    /// Run the session as this agent: its system prompt is added to the
    /// session's. Names a built-in, `.claude/agents` or `--agents` agent.
    #[arg(long, value_name = "NAME")]
    pub agent: Option<String>,
}

/// Environment variables the flags publish; read by the loaders.
pub mod env {
    pub const APPEND_SYSTEM_PROMPT: &str = "WIZARD_APPEND_SYSTEM_PROMPT";
    pub const SYSTEM_PROMPT_FILE: &str = "WIZARD_SYSTEM_PROMPT";
    pub const BARE: &str = "WIZARD_BARE";
    pub const SAFE_MODE: &str = "WIZARD_SAFE_MODE";
    pub const NO_SLASH_COMMANDS: &str = "WIZARD_NO_SLASH_COMMANDS";
    pub const PLUGIN_DIRS: &str = "WIZARD_PLUGIN_DIRS";
    pub const MCP_CONFIG: &str = "WIZARD_MCP_CONFIG";
    pub const STRICT_MCP: &str = "WIZARD_STRICT_MCP_CONFIG";
    pub const EXTRA_AGENTS: &str = "WIZARD_EXTRA_AGENTS";
    /// The file `--settings` names, or a private copy of its JSON text.
    pub const SETTINGS_FILE: &str = "WIZARD_SETTINGS_FILE";
    /// Set by `--no-session-persistence`.
    pub const EPHEMERAL_SESSION: &str = "WIZARD_EPHEMERAL_SESSION";
}

/// Separator between values in the list-valued variables. A control
/// character, because JSON strings and paths can hold anything else.
pub const SEP: char = '\u{1f}';

impl ClaudeFlags {
    /// The words after the flags, as one prompt.
    pub fn task_text(&self) -> Option<String> {
        let text = self.task.join(" ");
        (!text.trim().is_empty()).then_some(text)
    }

    /// Flags that were accepted and cannot be honored in this run, as a
    /// stderr line each. The session flags work everywhere; these three only
    /// have a meaning for a headless run, so they are reported only when the
    /// terminal UI is about to open.
    pub fn unsupported(&self) -> Vec<String> {
        use std::io::IsTerminal;
        let mut out = Vec::new();
        if self.print || !std::io::stdout().is_terminal() {
            return out;
        }
        let mut say = |flag: &str| {
            out.push(format!(
                "wizard: {flag} only applies to headless runs (-p with no terminal); ignored"
            ));
        };
        if self.no_session_persistence {
            say("--no-session-persistence");
        }
        if self.fallback_model.is_some() {
            say("--fallback-model");
        }
        if self.max_budget_usd.is_some() {
            say("--max-budget-usd");
        }
        out
    }

    /// The models `--fallback-model` names, in the order to try them.
    pub fn fallback_models(&self) -> Vec<String> {
        use std::io::IsTerminal;
        let from_settings = std::env::current_dir()
            .ok()
            .and_then(|root| {
                crate::config::claude::load(&root, crate::config::claude::Trust::Refused)
                    .fallback_model
            })
            .filter(|_| self.print || !std::io::stdout().is_terminal());
        self.fallback_model
            .clone()
            .or(from_settings)
            .as_deref()
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// Every agent `--agent` can name: the built-ins, `.claude/agents`
    /// (user, project and plugins) and `--agents`.
    pub fn known_agents(&self) -> Vec<crate::agent::subagent::SubagentConfig> {
        let root = std::env::current_dir().unwrap_or_default();
        let mut agents = crate::agent::subagent::builtin_configs();
        agents.extend(crate::claude_dir::agents::available(&root));
        if let Some(json) = &self.agents {
            agents.extend(crate::claude_dir::agents::from_json(json));
        }
        agents
    }

    /// The system-prompt text `--agent` adds: the agent's own prompt, framed
    /// as the role this session plays. `None` when no agent was named or none
    /// matches (the caller reports that, see [`prepare`]).
    pub fn agent_prompt(&self) -> Option<String> {
        let name = self.agent.as_deref()?;
        // The last definition wins, the way a project shadows the user's.
        // Claude Code's three built-ins all run the one built-in worker here.
        let name = match name {
            "general-purpose" | "Explore" | "Plan" => "worker",
            other => other,
        };
        let agent = self
            .known_agents()
            .into_iter()
            .rev()
            .find(|agent| agent.name == name)?;
        Some(format!(
            "You are running as the '{}' agent: {}\n\n{}",
            agent.name,
            agent.description.trim(),
            agent.system_prompt.trim()
        ))
    }

    /// Text added to the system prompt for this run: the `--agent` role,
    /// `--append-system-prompt` (file text first), the schema request, and
    /// `--add-dir`.
    pub fn appended_prompt(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if let Some(prompt) = self.agent_prompt() {
            parts.push(prompt);
        }
        if let Some(path) = &self.append_system_prompt_file {
            match std::fs::read_to_string(path) {
                Ok(text) => parts.push(text.trim().to_string()),
                Err(err) => eprintln!("wizard: cannot read {}: {err}", path.display()),
            }
        }
        if let Some(text) = &self.append_system_prompt {
            parts.push(text.trim().to_string());
        }
        if !self.add_dir.is_empty() {
            let dirs: Vec<String> = self
                .add_dir
                .iter()
                .map(|dir| {
                    dir.canonicalize()
                        .unwrap_or_else(|_| dir.clone())
                        .display()
                        .to_string()
                })
                .collect();
            parts.push(format!(
                "Additional working directories for this session: {}. Use absolute paths for them.",
                dirs.join(", ")
            ));
        }
        if let Some(schema) = &self.json_schema {
            parts.push(format!(
                "Your final message must be a single JSON value that validates against this JSON \
                 Schema, with no text around it:\n{schema}"
            ));
        }
        parts.retain(|part| !part.is_empty());
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    /// Publish what other subsystems read as environment variables.
    ///
    /// # Safety
    ///
    /// Writes the process environment, so it must run before any other thread
    /// exists: first thing in `main`, before the tokio runtime is built.
    pub unsafe fn publish(&self) {
        let set = |key: &str, value: OsString| unsafe { std::env::set_var(key, value) };
        if let Some(text) = self.appended_prompt() {
            set(env::APPEND_SYSTEM_PROMPT, text.into());
        }
        if let Some(path) = &self.system_prompt_file {
            set(env::SYSTEM_PROMPT_FILE, path.clone().into_os_string());
        } else if let Some(text) = &self.system_prompt
            && let Some(path) = write_private_temp("system-prompt", "md", text)
        {
            set(env::SYSTEM_PROMPT_FILE, path.into_os_string());
        }
        if self.bare || self.safe_mode {
            set(env::BARE, "1".into());
        }
        if self.safe_mode {
            set(env::SAFE_MODE, "1".into());
        }
        if self.disable_slash_commands {
            set(env::NO_SLASH_COMMANDS, "1".into());
        }
        if !self.plugin_dir.is_empty()
            && let Ok(joined) = std::env::join_paths(&self.plugin_dir)
        {
            set(env::PLUGIN_DIRS, joined);
        }
        if !self.mcp_config.is_empty() {
            set(
                env::MCP_CONFIG,
                self.mcp_config.join(&SEP.to_string()).into(),
            );
        }
        if self.strict_mcp_config {
            set(env::STRICT_MCP, "1".into());
        }
        if let Some(json) = &self.agents {
            set(env::EXTRA_AGENTS, json.into());
        }
        if let Some(settings) = &self.settings
            && let Some(path) = settings_file(settings)
        {
            set(env::SETTINGS_FILE, path.into_os_string());
        }
        if self.no_session_persistence {
            set(env::EPHEMERAL_SESSION, "1".into());
        }
        unsafe { crate::config::key_helper::export() };
        for (name, value) in
            crate::config::claude_env::publish(&|name: &str| std::env::var(name).ok())
        {
            if std::env::var_os(name).is_none() {
                set(name, value.into());
            }
        }
        if self.verbose || self.debug.is_some() {
            let filter = match &self.debug {
                Some(Some(filter)) => format!("wizard={filter}"),
                Some(None) => "debug".to_string(),
                None => "info".to_string(),
            };
            if std::env::var_os(crate::logging::FILTER_ENV).is_none() {
                set(crate::logging::FILTER_ENV, filter.into());
            }
        }
    }

    /// Apply the run-scoped overrides to the loaded config.
    pub fn apply_to(&self, config: &mut Config) {
        let get = |name: &str| std::env::var(name).ok();
        crate::config::claude_env::apply(config, &get);
        let mut settings = std::env::current_dir()
            .map(|root| crate::config::claude::load(&root, crate::config::claude::Trust::Refused))
            .unwrap_or_default();
        for (alias, model) in crate::config::claude_env::model_aliases(&get) {
            settings.model_overrides.entry(alias).or_insert(model);
        }
        if let Some(model) = &self.model {
            let model = settings.resolve_model(model);
            match settings.refuse_model(&model) {
                Some(why) => eprintln!("wizard: --model ignored: {why}"),
                None => set_model(config, &model),
            }
        }
        let from_env = crate::config::claude_env::effort(&get);
        let effort = self
            .effort
            .as_deref()
            .or(from_env.as_deref())
            .or(settings.effort_level.as_deref());
        if let Some(effort) = effort.and_then(parse_effort) {
            config.reasoning_effort = Some(effort);
        }
        if let Some(window) = self.autocompact.as_deref().and_then(parse_window) {
            config.max_context_tokens = window;
        }
        if let Some(turns) = self
            .max_turns
            .or_else(|| crate::config::claude_env::max_turns(&get))
        {
            config.max_steps = StepBudget::new(turns);
        }
    }

    /// Whether this invocation asks for a headless run.
    pub fn wants_print(&self) -> bool {
        self.print
    }
}

/// Point the active provider at `model` for this run.
pub fn set_model(config: &mut Config, model: &str) {
    let at = config
        .active_provider
        .as_ref()
        .and_then(|name| config.providers.iter().position(|p| &p.name == name))
        .unwrap_or(0);
    if let Some(provider) = config.providers.get_mut(at) {
        provider.model = model.to_string();
        provider.model_pinned = Some(true);
    }
}

/// `--autocompact` as a token count: `200k`, `1m`, or plain digits. `auto`
/// and anything unreadable leave the configured window alone.
fn parse_window(raw: &str) -> Option<u32> {
    let text = raw.trim().to_ascii_lowercase();
    let (digits, scale) = match text.strip_suffix('k') {
        Some(rest) => (rest, 1_000),
        None => match text.strip_suffix('m') {
            Some(rest) => (rest, 1_000_000),
            None => (text.as_str(), 1),
        },
    };
    digits.parse::<u32>().ok()?.checked_mul(scale)
}

fn parse_effort(raw: &str) -> Option<ReasoningEffort> {
    match raw.to_ascii_lowercase().as_str() {
        "low" => Some(ReasoningEffort::Low),
        "medium" | "med" => Some(ReasoningEffort::Medium),
        "high" => Some(ReasoningEffort::High),
        "xhigh" | "max" => Some(ReasoningEffort::Xhigh),
        _ => None,
    }
}

/// The file `--settings` points at: the path itself, or a private temp file
/// holding the text when the value is a JSON object.
fn settings_file(value: &str) -> Option<PathBuf> {
    if value.trim_start().starts_with('{') {
        return write_private_temp("settings", "json", value);
    }
    let path = PathBuf::from(value);
    Some(path.canonicalize().unwrap_or(path))
}

fn write_private_temp(tag: &str, extension: &str, text: &str) -> Option<PathBuf> {
    use std::io::Write;
    let path =
        std::env::temp_dir().join(format!("wizard-{tag}-{}.{extension}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).ok()?;
    file.write_all(text.as_bytes()).ok()?;
    Some(path)
}

/* ---------------------------------------------------------------------- */
/* Argument pre-pass                                                      */
/* ---------------------------------------------------------------------- */

/// Rewrite the two spellings Claude Code accepts that Wizard's derive cannot
/// express:
///
/// - `-p` with no value (end of line, or another flag next) is `--print`. In
///   Wizard `-p` takes the task, and `-p --output-format json "q"` used to
///   read `--output-format` as the task.
/// - `--resume <id>` / `-r <id>` becomes `--resume --resume-id <id>`.
///
/// `known_flag` says whether a `-x`/`--long[=v]` token is a flag Wizard has.
pub fn normalize_args(
    argv: Vec<OsString>,
    known_flag: impl Fn(&str) -> bool,
    subcommands: &[String],
) -> Vec<OsString> {
    let mut out: Vec<OsString> = Vec::with_capacity(argv.len() + 2);
    let mut i = 0;
    let mut after_dashes = false;
    while i < argv.len() {
        let token = argv[i].clone();
        let text = token.to_str().unwrap_or("");
        if after_dashes || i == 0 {
            out.push(token);
            i += 1;
            continue;
        }
        if text == "--" {
            after_dashes = true;
            out.push(token);
            i += 1;
            continue;
        }
        let next = argv.get(i + 1).and_then(|n| n.to_str());
        match text {
            // Claude Code's short form of `--version`.
            "-v" => out.push("--version".into()),
            "-p" | "--prompt" => {
                let valueless = match next {
                    None => true,
                    Some(next) => next.starts_with('-') && next != "-" && known_flag(next),
                };
                out.push(if valueless { "--print".into() } else { token });
            }
            "-r" | "--resume" => match next {
                Some(id) if !id.starts_with('-') && !subcommands.iter().any(|s| s == id) => {
                    out.push("--resume".into());
                    out.push("--resume-id".into());
                    out.push(id.into());
                    i += 1;
                }
                _ => out.push(token),
            },
            _ => out.push(token),
        }
        i += 1;
    }
    out
}

/// How many values a flag takes.
#[derive(Clone, Copy)]
enum Takes {
    Nothing,
    One,
    /// A value only when the next word is not another flag.
    Optional,
    /// Every following word that is not a flag.
    Many,
}

/// Claude Code flags for features Wizard does not have (cloud and remote
/// sessions, Chrome and IDE links, teammates, ...). They are removed from the
/// command line with a warning rather than failing the whole run, so a script
/// written for `claude` still starts. The permission flags are not here: the
/// permissions subsystem owns those.
const UNSUPPORTED: &[(&str, Takes, &str)] = &[
    ("--betas", Takes::Many, "Anthropic beta headers"),
    ("--brief", Takes::Nothing, "no SendUserMessage tool"),
    (
        "--chrome",
        Takes::Nothing,
        "no Chrome integration; see the computer tool",
    ),
    ("--no-chrome", Takes::Nothing, "no Chrome integration"),
    (
        "--ide",
        Takes::Nothing,
        "editors connect over ACP: wizard acp",
    ),
    ("--cloud", Takes::Optional, "no cloud sessions"),
    ("--remote", Takes::Nothing, "no cloud sessions"),
    ("--environment", Takes::One, "no cloud sessions"),
    ("--ref", Takes::One, "no cloud sessions"),
    ("--remote-control", Takes::Optional, "no remote control"),
    ("--rc", Takes::Optional, "no remote control"),
    (
        "--remote-control-session-name-prefix",
        Takes::One,
        "no remote control",
    ),
    ("--teleport", Takes::Optional, "no cloud sessions"),
    (
        "--background",
        Takes::Nothing,
        "start background sessions from wizard agents",
    ),
    ("--desktop", Takes::Nothing, "use wizard gui"),
    (
        "--tmux",
        Takes::Optional,
        "worktrees do not open tmux sessions",
    ),
    (
        "--from-pr",
        Takes::Optional,
        "sessions are not linked to pull requests",
    ),
    ("--file", Takes::Many, "no file resources"),
    ("--init", Takes::Nothing, "no setup hooks"),
    ("--init-only", Takes::Nothing, "no setup hooks"),
    ("--maintenance", Takes::Nothing, "no setup hooks"),
    (
        "--ax-screen-reader",
        Takes::Nothing,
        "no screen-reader mode",
    ),
    (
        "--tools",
        Takes::Many,
        "the tool set is fixed; --allowedTools and --disallowedTools narrow what runs",
    ),
    (
        "--permission-prompts",
        Takes::One,
        "headless runs never wait for a prompt",
    ),
    (
        "--permission-prompt-tool",
        Takes::One,
        "no permission prompt tool",
    ),
    (
        "--allow-dangerously-skip-permissions",
        Takes::Nothing,
        "--permission-mode bypassPermissions is always available",
    ),
    ("--advisor", Takes::One, "see /fusion and /ultra"),
    (
        "--prompt-suggestions",
        Takes::Optional,
        "no prompt suggestions",
    ),
    ("--teammate-mode", Takes::One, "no teammates"),
    ("--channels", Takes::Many, "no channels"),
    (
        "--dangerously-load-development-channels",
        Takes::Many,
        "no channels",
    ),
    (
        "--forward-subagent-text",
        Takes::Nothing,
        "stream-json already labels subagent events",
    ),
    (
        "--exclude-dynamic-system-prompt-sections",
        Takes::Nothing,
        "the prompt layout is fixed",
    ),
    (
        "--system-prompt-snapshot",
        Takes::One,
        "the prompt is built per session",
    ),
    (
        "--append-subagent-system-prompt",
        Takes::One,
        "subagent prompts come from their definitions",
    ),
    (
        "--append-subagent-system-prompt-file",
        Takes::One,
        "subagent prompts come from their definitions",
    ),
    ("--plugin-url", Takes::One, "use /plugin install <url>"),
    ("--debug-file", Takes::One, "logs go to ~/.wizard/logs"),
    (
        "--setting-sources",
        Takes::One,
        "Wizard has no Claude settings layers",
    ),
    ("--restricted", Takes::Nothing, "no restricted mode"),
];

/// Drop the [`UNSUPPORTED`] flags `known_flag` does not recognise, returning
/// the remaining arguments and one warning per flag dropped.
pub fn strip_unsupported(
    argv: Vec<OsString>,
    known_flag: impl Fn(&str) -> bool,
) -> (Vec<OsString>, Vec<String>) {
    let mut out = Vec::with_capacity(argv.len());
    let mut warnings = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        let token = argv[i].to_string_lossy().into_owned();
        if i == 0 {
            out.push(argv[0].clone());
            i = 1;
            continue;
        }
        if token == "--" {
            out.extend(argv[i..].iter().cloned());
            break;
        }
        let name = token.split('=').next().unwrap_or(&token);
        let entry = UNSUPPORTED.iter().find(|(flag, _, _)| *flag == name);
        let Some((flag, takes, why)) = entry.filter(|_| !known_flag(&token)) else {
            out.push(argv[i].clone());
            i += 1;
            continue;
        };
        warnings.push(format!("wizard: {flag} is not supported ({why}); ignored"));
        let inline_value = token.contains('=');
        i += 1;
        let is_value = |j: usize| {
            argv.get(j)
                .and_then(|a| a.to_str())
                .is_some_and(|a| !a.starts_with('-'))
        };
        match takes {
            _ if inline_value => {}
            Takes::Nothing => {}
            Takes::One => i += usize::from(i < argv.len()),
            Takes::Optional => i += usize::from(is_value(i)),
            Takes::Many => {
                while is_value(i) {
                    i += 1;
                }
            }
        }
    }
    (out, warnings)
}

/// The closest subcommand to a lone word that is not one, so
/// `wizard doctr` suggests `doctor` instead of starting an agent on "doctr".
pub fn did_you_mean(word: &str, subcommands: &[String]) -> Option<String> {
    if word.contains(char::is_whitespace) || word.len() < 3 {
        return None;
    }
    subcommands
        .iter()
        .map(|name| (edit_distance(word, name), name))
        .filter(|(distance, name)| *distance > 0 && *distance <= 2 && name.len() > 3)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, name)| name.clone())
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            let next = (row[j] + 1).min(row[j + 1] + 1).min(prev + cost);
            prev = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

/// Settle what the flags mean for the rest of the run: warn about the ones
/// that cannot be honored, fold `-c` and `--resume-id` into `--resume`, turn
/// the words after the flags into the prompt, and for `--print` read the
/// prompt (or extra context) from stdin.
pub fn prepare(cli: &mut crate::cli::Cli) -> anyhow::Result<()> {
    use std::io::IsTerminal;
    for line in cli.claude.unsupported() {
        eprintln!("{line}");
    }
    if cli.claude.continue_session || cli.claude.resume_id.is_some() {
        cli.resume = true;
    }
    if let Some(name) = &cli.claude.agent
        && cli.claude.agent_prompt().is_none()
    {
        let mut known: Vec<String> = cli
            .claude
            .known_agents()
            .into_iter()
            .map(|agent| agent.name)
            .collect();
        known.sort();
        known.dedup();
        anyhow::bail!(
            "--agent '{name}' is not a known agent (known: {})",
            known.join(", ")
        );
    }
    if cli.prompt.is_none()
        && let Some(task) = cli.claude.task_text()
    {
        if cli.command.is_none() && cli.claude.task.len() == 1 {
            use clap::CommandFactory;
            let names: Vec<String> = crate::cli::Cli::command()
                .get_subcommands()
                .map(|sub| sub.get_name().to_string())
                .collect();
            if let Some(name) = did_you_mean(&task, &names) {
                anyhow::bail!(
                    "unrecognized subcommand '{task}'. Did you mean `wizard {name}`? (to run it as a task, use -p \"{task}\")"
                );
            }
        }
        cli.prompt = Some(task);
    }
    if cli.claude.print {
        // `--input-format stream-json` is a conversation: the headless runner
        // reads stdin itself, one turn per user message.
        if cli.claude.input_format != Some(InputFormat::StreamJson) {
            if !std::io::stdin().is_terminal() {
                let piped = read_stdin_for(std::time::Duration::from_secs(3));
                let piped = Some(piped).filter(|text| !text.trim().is_empty());
                cli.prompt = match (piped, cli.prompt.take()) {
                    (Some(piped), Some(prompt)) => Some(format!("{piped}\n\n{prompt}")),
                    (Some(piped), None) => Some(piped),
                    (None, prompt) => prompt,
                };
            }
            if cli.prompt.is_none() {
                anyhow::bail!(
                    "--print needs a prompt: pass it as an argument, with -p, or on stdin"
                );
            }
        }
    }
    if let Some(name) = &cli.claude.worktree {
        let root = match &cli.cwd {
            Some(dir) => dir.clone(),
            None => std::env::current_dir()?,
        };
        let path = create_worktree(&root, name.as_deref()).map_err(anyhow::Error::msg)?;
        cli.cwd = Some(path);
    }
    choose_session(cli)?;
    Ok(())
}

/// Settle `--resume <id or name>`, `--session-id`, `--name` and
/// `--fork-session` into one concrete session, named by `resume_id` from here
/// on, so the terminal and the headless runner open it the same way.
fn choose_session(cli: &mut crate::cli::Cli) -> anyhow::Result<()> {
    use crate::session_flags::{Request, select};
    let flags = &cli.claude;
    if flags.fork_session && flags.resume_id.is_none() && !flags.continue_session && !cli.resume {
        eprintln!("wizard: --fork-session needs --resume or --continue; ignored");
    }
    let request = Request {
        resume: flags.resume_id.as_deref(),
        latest: flags.continue_session || (cli.resume && flags.resume_id.is_none()),
        session_id: flags.session_id.as_deref(),
        name: flags.name.as_deref(),
        fork: flags.fork_session,
    };
    if request.latest && request.resume.is_none() {
        let root = match &cli.cwd {
            Some(dir) => dir.clone(),
            None => std::env::current_dir()?,
        };
        tell_about_claude_sessions(&root);
    }
    if request.is_empty() {
        return Ok(());
    }
    // Sessions are per project, and the runner has not changed directory yet.
    let here = std::env::current_dir()?;
    if let Some(dir) = &cli.cwd {
        std::env::set_current_dir(dir)?;
    }
    let chosen = crate::config::Config::sessions_dir().and_then(|dir| select(&dir, request));
    std::env::set_current_dir(here)?;
    if let Some(session) = chosen? {
        cli.claude.resume_id = Some(session.id);
        cli.resume = true;
    }
    Ok(())
}

/// `--resume` with nothing of Wizard's to reopen here: say what Claude Code
/// recorded for this directory, since that is the history a switcher means.
fn tell_about_claude_sessions(root: &std::path::Path) {
    let here = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let has_wizard_session = crate::config::Config::sessions_dir().is_ok_and(|dir| {
        crate::agent::session::summaries(&dir)
            .iter()
            .any(|session| {
                session.cwd.as_deref().is_some_and(|cwd| {
                    std::path::Path::new(cwd)
                        .canonicalize()
                        .is_ok_and(|cwd| cwd == here)
                })
            })
    });
    if has_wizard_session {
        return;
    }
    let rows = crate::session_registry::claude_chats(&root.display().to_string());
    if rows.is_empty() {
        return;
    }
    eprintln!(
        "wizard: no Wizard session here to resume; Claude Code recorded {} for this directory:",
        rows.len()
    );
    for row in rows.iter().take(5) {
        eprintln!("  {}  {}", row.id, row.title);
    }
    eprintln!("`wizard resume --claude` continues one (`--list` shows them all).");
}

/// Everything on stdin until it closes, or whatever arrived within `wait` if
/// nothing closes it (an inherited pipe nobody writes to).
fn read_stdin_for(wait: std::time::Duration) -> String {
    use std::io::Read;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::stdin().read_to_string(&mut text);
        let _ = tx.send(text);
    });
    rx.recv_timeout(wait).unwrap_or_default()
}

/// The prompt a `--input-format stream-json` stdin carries: every user
/// message's text, in order. Lines that are not JSON, or not user messages,
/// are skipped.
pub fn prompt_from_stream_json(stdin: &str) -> Option<String> {
    let mut texts = Vec::new();
    for line in stdin.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }
        let content = value.get("message").and_then(|m| m.get("content"));
        match content {
            Some(serde_json::Value::String(text)) => texts.push(text.clone()),
            Some(serde_json::Value::Array(blocks)) => {
                let joined: Vec<&str> = blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect();
                texts.push(joined.join("\n"));
            }
            _ => {}
        }
    }
    let joined = texts.join("\n\n");
    (!joined.trim().is_empty()).then_some(joined)
}

/// Create the git worktree `--worktree` asks for and return its path.
pub fn create_worktree(root: &std::path::Path, name: Option<&str>) -> Result<PathBuf, String> {
    let name = match name {
        Some(name) if !name.trim().is_empty() => name.trim().to_string(),
        _ => format!(
            "wt-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        ),
    };
    if name.contains(['/', '\\']) || name.starts_with('.') {
        return Err(format!("worktree name '{name}' must be a plain word"));
    }
    let path = root.join(".claude").join("worktrees").join(&name);
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["worktree", "add", "-b", &name])
        .arg(&path)
        .output()
        .map_err(|err| format!("could not run git: {err}"))?;
    if out.status.success() {
        return Ok(path);
    }
    // The branch may already exist from an earlier run: reuse it.
    let retry = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["worktree", "add"])
        .arg(&path)
        .arg(&name)
        .output()
        .map_err(|err| format!("could not run git: {err}"))?;
    if retry.status.success() {
        Ok(path)
    } else {
        Err(format!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    fn known(flag: &str) -> bool {
        let long = flag.split('=').next().unwrap_or(flag);
        ["--output-format", "--print", "--model", "-c", "--plan"].contains(&long)
    }

    fn norm(list: &[&str]) -> Vec<String> {
        let subs = vec!["doctor".to_string(), "resume".to_string()];
        normalize_args(args(list), known, &subs)
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn bare_dash_p_is_print() {
        assert_eq!(
            norm(&["wizard", "-p", "--output-format", "json", "q"]),
            ["wizard", "--print", "--output-format", "json", "q"]
        );
        assert_eq!(norm(&["wizard", "q", "-p"]), ["wizard", "q", "--print"]);
    }

    #[test]
    fn dash_p_with_a_task_keeps_the_task_even_when_it_starts_with_a_dash() {
        assert_eq!(norm(&["wizard", "-p", "do it"]), ["wizard", "-p", "do it"]);
        assert_eq!(
            norm(&["wizard", "-p", "- a list item"]),
            ["wizard", "-p", "- a list item"]
        );
        assert_eq!(
            norm(&["wizard", "-p", "--not-a-flag"]),
            ["wizard", "-p", "--not-a-flag"]
        );
    }

    #[test]
    fn resume_with_an_id_splits_and_without_one_stays() {
        assert_eq!(
            norm(&["wizard", "--resume", "abc-123"]),
            ["wizard", "--resume", "--resume-id", "abc-123"]
        );
        assert_eq!(
            norm(&["wizard", "-r", "x", "q"]),
            ["wizard", "--resume", "--resume-id", "x", "q"]
        );
        assert_eq!(norm(&["wizard", "--resume"]), ["wizard", "--resume"]);
        assert_eq!(
            norm(&["wizard", "--resume", "--plan"]),
            ["wizard", "--resume", "--plan"]
        );
        assert_eq!(
            norm(&["wizard", "--resume", "doctor"]),
            ["wizard", "--resume", "doctor"]
        );
    }

    #[test]
    fn dash_v_is_version_and_the_last_permission_flags_are_stripped() {
        assert_eq!(norm(&["wizard", "-v"]), ["wizard", "--version"]);
        assert_eq!(
            strip(&["wizard", "--tools", "Bash", "Edit", "--model", "m"]),
            (vec!["wizard".into(), "--model".into(), "m".into()], 1)
        );
        assert_eq!(
            strip(&["wizard", "--permission-prompts", "none", "q"]),
            (vec!["wizard".into(), "q".into()], 1)
        );
        assert_eq!(
            strip(&["wizard", "--allow-dangerously-skip-permissions", "q"]),
            (vec!["wizard".into(), "q".into()], 1)
        );
    }

    #[test]
    fn autocompact_takes_a_token_count_and_auto_leaves_the_window_alone() {
        assert_eq!(parse_window("200k"), Some(200_000));
        assert_eq!(parse_window("1M"), Some(1_000_000));
        assert_eq!(parse_window("150000"), Some(150_000));
        assert_eq!(parse_window("auto"), None);
        let cli = parse(&["wizard", "--autocompact", "300k", "-p", "x"]);
        let mut config = Config::default();
        cli.claude.apply_to(&mut config);
        assert_eq!(config.max_context_tokens, 300_000);
    }

    #[test]
    fn nothing_after_double_dash_is_rewritten() {
        assert_eq!(norm(&["wizard", "--", "-p"]), ["wizard", "--", "-p"]);
    }

    fn strip(list: &[&str]) -> (Vec<String>, usize) {
        let (out, warnings) = strip_unsupported(args(list), known);
        (
            out.into_iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect(),
            warnings.len(),
        )
    }

    #[test]
    fn flags_for_features_wizard_lacks_are_removed_with_their_values() {
        assert_eq!(
            strip(&["wizard", "--betas", "a", "b", "--model", "m", "q"]),
            (
                vec!["wizard".into(), "--model".into(), "m".into(), "q".into()],
                1
            )
        );
        assert_eq!(
            strip(&["wizard", "--brief", "q"]),
            (vec!["wizard".into(), "q".into()], 1)
        );
        assert_eq!(
            strip(&["wizard", "--from-pr=12", "q"]),
            (vec!["wizard".into(), "q".into()], 1)
        );
        assert_eq!(
            strip(&["wizard", "--cloud", "--model", "m"]),
            (vec!["wizard".into(), "--model".into(), "m".into()], 1),
            "an optional value is not taken from a following flag"
        );
        assert_eq!(
            strip(&["wizard", "--", "--brief"]).1,
            0,
            "after -- nothing is a flag"
        );
        assert_eq!(strip(&["wizard", "--plan"]).1, 0);
    }

    #[test]
    fn a_flag_wizard_knows_is_never_stripped() {
        let (out, warnings) = strip_unsupported(args(&["wizard", "--model", "m"]), |_| true);
        assert!(warnings.is_empty());
        assert_eq!(out.len(), 3);
        let (_, warnings) = strip_unsupported(args(&["wizard", "--brief"]), |_| true);
        assert!(
            warnings.is_empty(),
            "a flag the derive knows wins over the table"
        );
    }

    fn parse(list: &[&str]) -> crate::cli::Cli {
        use clap::Parser;
        let argv = crate::cli::normalize(args(list));
        crate::cli::Cli::try_parse_from(argv).expect("parses")
    }

    #[test]
    fn claude_style_invocations_parse() {
        let cli = parse(&["wizard", "-p", "--output-format", "json", "what is 2+2"]);
        assert!(cli.claude.print);
        assert_eq!(cli.claude.task_text().as_deref(), Some("what is 2+2"));
        assert_eq!(cli.output_format, crate::output::OutputFormat::Json);

        let cli = parse(&["wizard", "-c", "--model", "m", "--max-turns", "3", "go"]);
        assert!(cli.claude.continue_session);
        assert_eq!(cli.claude.model.as_deref(), Some("m"));
        assert_eq!(cli.claude.max_turns, Some(3));

        let cli = parse(&["wizard", "--resume", "abc", "--bare"]);
        assert!(cli.resume && cli.claude.bare);
        assert_eq!(cli.claude.resume_id.as_deref(), Some("abc"));

        let cli = parse(&["wizard", "-w", "--append-system-prompt", "be brief", "x"]);
        assert_eq!(cli.claude.worktree, Some(None));
        assert_eq!(cli.claude.append_system_prompt.as_deref(), Some("be brief"));

        let cli = parse(&["wizard", "--add-dir", "../a", "../b", "--print", "q"]);
        assert_eq!(cli.claude.add_dir.len(), 2);
    }

    #[test]
    fn existing_invocations_keep_their_meaning() {
        let cli = parse(&["wizard", "-p", "do it"]);
        assert_eq!(cli.prompt.as_deref(), Some("do it"));
        assert!(!cli.claude.print);
        let cli = parse(&["wizard", "doctor"]);
        assert!(matches!(
            cli.command,
            Some(crate::cli::Command::Doctor { .. })
        ));
        assert!(cli.claude.task.is_empty());
        let cli = parse(&["wizard", "fix", "the", "bug"]);
        assert_eq!(cli.claude.task_text().as_deref(), Some("fix the bug"));
        assert!(cli.command.is_none());
        let cli = parse(&["wizard", "--resume"]);
        assert!(cli.resume && cli.claude.resume_id.is_none());
    }

    #[test]
    fn a_typo_of_a_subcommand_gets_a_suggestion() {
        let subs = vec!["doctor".to_string(), "gateway".to_string()];
        assert_eq!(did_you_mean("doctr", &subs).as_deref(), Some("doctor"));
        assert_eq!(did_you_mean("fix the bug", &subs), None);
        assert_eq!(did_you_mean("refactor", &subs), None);
        assert_eq!(
            did_you_mean("doctor", &subs),
            None,
            "an exact name is a subcommand"
        );
    }

    #[test]
    fn stream_json_input_yields_the_user_text() {
        let input = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"first\"}}\n\
                     {\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"second\"}]}}\n\
                     {\"type\":\"system\"}\nnot json\n";
        assert_eq!(
            prompt_from_stream_json(input).as_deref(),
            Some("first\n\nsecond")
        );
        assert_eq!(prompt_from_stream_json("{\"type\":\"result\"}"), None);
    }

    #[test]
    fn run_overrides_land_on_the_active_provider() {
        use crate::config::{ProviderConfig, ProviderKind};
        let provider = |name: &str| ProviderConfig {
            name: name.to_string(),
            kind: ProviderKind::OLLAMA,
            base_url: "http://x".into(),
            model: "old".into(),
            ..ProviderConfig::default()
        };
        let mut config = Config {
            providers: vec![provider("a"), provider("b")],
            active_provider: Some("b".into()),
            ..Config::default()
        };
        let flags = ClaudeFlags {
            model: Some("new".into()),
            effort: Some("max".into()),
            max_turns: Some(7),
            ..ClaudeFlags::default()
        };
        flags.apply_to(&mut config);
        assert_eq!(config.providers[0].model, "old");
        assert_eq!(config.providers[1].model, "new");
        assert_eq!(config.reasoning_effort, Some(ReasoningEffort::Xhigh));
        assert_eq!(config.max_steps, StepBudget::new(7));
    }

    #[test]
    fn the_appended_prompt_joins_file_text_schema_and_dirs() {
        let flags = ClaudeFlags {
            append_system_prompt: Some("be brief".into()),
            json_schema: Some("{\"type\":\"object\"}".into()),
            add_dir: vec![PathBuf::from("/nonexistent-dir-for-test")],
            ..ClaudeFlags::default()
        };
        let text = flags.appended_prompt().expect("something to add");
        assert!(text.starts_with("be brief"));
        assert!(text.contains("/nonexistent-dir-for-test"));
        assert!(text.contains("\"type\":\"object\""));
        assert!(ClaudeFlags::default().appended_prompt().is_none());
    }

    #[test]
    fn headless_only_flags_are_named_when_the_terminal_ui_would_open() {
        let flags = ClaudeFlags {
            max_budget_usd: Some(1.0),
            no_session_persistence: true,
            ..ClaudeFlags::default()
        };
        // Under `cargo test` stdout is not a terminal, so the headless path
        // is the one in force and nothing is reported.
        assert!(flags.unsupported().is_empty());
        assert!(ClaudeFlags::default().unsupported().is_empty());
        let models = ClaudeFlags {
            fallback_model: Some("a, b,,c".into()),
            ..ClaudeFlags::default()
        };
        assert_eq!(models.fallback_models(), ["a", "b", "c"]);
    }
}
