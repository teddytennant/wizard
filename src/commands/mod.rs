//! Slash commands: the one built-in table every surface reads, the one
//! dispatcher every surface runs them through, plus custom commands and
//! `@file` references.
//!
//! [`COMMANDS`] is the single source of truth for what a *built-in* command is
//! called, what it does, and (through [`CommandSpec::tui`] / [`CommandSpec::gui`]
//! / [`CommandSpec::gateway`]) how each [`Surface`] executes it. The TUI
//! completes and dispatches from it, the GUI derives `GET /api/commands` from
//! it, the Telegram gateway answers a chat out of it, and the agent's
//! `run_command` allowlist ([`agent_commands`]) is filtered out of it. A second
//! hand-kept list on any surface is how the surfaces drift, so there is only
//! this one.
//!
//! A *plugin* command is the same thing minus the compile step: a
//! [`PluginCommand`] in the runtime registry ([`plugin`]), carrying its own
//! description, argument hint, per-surface availability and handler. The two
//! are merged by [`listing`], which is what every surface completes, helps and
//! advertises from, and [`SlashCommand::Plugin`] is how one reaches the one
//! dispatcher. The table stays the source of truth for the built-ins and the
//! registry is the source of truth for the rest; nothing consults a third list.
//!
//! What each command *does* lives in [`surface`], not on the surfaces: one
//! [`dispatch`](surface::dispatch) owns every match arm and every line of prose,
//! and a surface supplies the verbs it needs through
//! [`CommandSurface`](surface::CommandSurface). A command that genuinely cannot
//! work somewhere says so in the table ([`Execution::Unavailable`]) and is
//! refused by name; it is never a missing match arm.
//!
//! Custom commands are markdown files in `~/.wizard/commands/` and
//! `<project>/.wizard/commands/` (project files shadow global ones on a name
//! collision). The file stem is the command name; an optional `---`-fenced
//! frontmatter block (the same convention as skills) may carry a
//! `description` shown in the TUI suggestion popup. The body is a prompt
//! template: `$ARGUMENTS` expands to everything typed after the command name
//! and `$1`..`$9` to the whitespace-split positional arguments (missing
//! positions expand to the empty string).
//!
//! `@path` tokens in user input expand to the referenced file's contents in a
//! fenced code block. Both the TUI (`App::submit`) and headless `-p` runs go
//! through the same [`preprocess`] pipeline, so a prompt behaves identically
//! on every surface.

use std::path::{Path, PathBuf};

use crate::config::{Config, Mode, ProviderKind, ReasoningEffort, UltraConfig};
use crate::import_claude::ImportSelection;

pub mod plugin;
pub mod surface;

pub use plugin::{CommandFuture, CommandHandler, PluginCommand};
pub use surface::Surface;

/* ---------------------------------------------------------------------- */
/* Built-in slash commands                                                */
/* ---------------------------------------------------------------------- */

/// Parsed `/slash` command (see the README table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Help,
    Clear,
    /// `/model [tag]` — show current model, or switch to `tag`.
    Model(Option<String>),
    /// `/mode [genie|sovereign]` — show or switch mode.
    Mode(Option<Mode>),
    /// `/effort [low|medium|high|default]` — set the reasoning effort sent to
    /// models that support it. `None` opens the picker; `Some(None)` clears
    /// back to the provider default; `Some(Some(e))` sets the level.
    Effort(Option<Option<ReasoningEffort>>),
    /// `/evolve [--deep] <description>`.
    Evolve {
        deep: bool,
        description: String,
    },
    /// Reload skills, scripted tools, and MCP servers without restart.
    Reload,
    /// Toggle plan mode (also Shift+Tab): read-only investigation until a
    /// plan is approved via `exit_plan`.
    Plan,
    /// Toggle omakase (chef's-choice) mode: plan mode where the agent decides
    /// the approach itself and auto-approves its own plan — no interview, no
    /// review gate.
    Omakase,
    /// `/rewind [turn]` — restore file checkpoints and truncate history.
    /// `None` opens the turn picker; `Some` rewinds to before that turn.
    Rewind(Option<u64>),
    /// `/resume [id]` — reopen a past session and continue it. `None` opens
    /// the session picker; `Some` resumes that session id directly.
    Resume(Option<String>),
    /// `/resume-claude [id]` — take a conversation out of Claude Code's own
    /// history and continue it here. `None` opens the picker; `Some` is a
    /// Claude Code session id, or a unique prefix of one.
    ///
    /// Separate from [`SlashCommand::Resume`] rather than a flag on it,
    /// because opening one of these rows is a different act: a `/resume` row
    /// reopens a file Wizard owns, and this one reads another program's live
    /// state and writes a **new** Wizard session from it. Two gestures, two
    /// commands. `~/.claude` is only ever read — see [`crate::claude_session`].
    ResumeClaude(Option<String>),
    /// `/compact` — summarize older history into a progress note now, instead
    /// of waiting for the automatic threshold.
    Compact,
    /// `/agents` — open the subagent roster picker (browse the available
    /// subagents and what each does; Enter pre-fills a delegation request).
    Agents,
    /// Toggle the git diff sidebar.
    Diff,
    /// Toggle the compact todo band above the composer.
    Todos,
    /// Toggle the machine-wide session manager: every live Wizard session on
    /// the machine, grouped by state.
    Dashboard,
    /// Show session token usage (and cost when rates are configured).
    Cost,
    /// `/memory [read|forget <name>]` — inspect and manage the saved project
    /// memories the agent writes with the `memory` tool.
    Memory(MemoryAction),
    /// Run the environment diagnostics (same checks as `wizard doctor`).
    Doctor,
    /// Show the session status: model, provider, mode, session id, usage,
    /// todo progress, background tasks, plan mode.
    Status,
    /// `/bashes` — list background tasks (`execute` with
    /// `run_in_background`), running and finished, with id/status/command.
    Bashes,
    /// `/btw <question>` — one-shot side question against the current
    /// conversation context. The exchange is *not* appended to history or
    /// the session file (token-cheap asides mid-task).
    Btw(String),
    /// `/fork <task>` — spawn a background side quest that inherits the full
    /// conversation context (history, tools, system prompt). Runs in parallel
    /// with the main session; its report is injected into history when done.
    Fork(String),
    /// `/goal [text]` — show the standing mission goal, or set it. `None`
    /// shows the current goal; `Some` sets it (drives sovereign/continuous
    /// mode), persisting to `<project_root>/.wizard/mission.toml`.
    Goal(Option<String>),
    /// `/publish [branch]` — fork Wizard and get a one-line installer.
    Publish {
        branch: Option<String>,
    },
    /// `/fusion [config]`: toggle a council of *providers* (a panel debates,
    /// then a synthesizer answers), or open the panel configurator.
    Fusion(FusionAction),
    /// `/ultra [config]`: toggle a council of *lenses* (N read-only subagents
    /// draft the turn, a judge compares the drafts, and the main agent executes
    /// from the verdict), or open the roster editor.
    ///
    /// The two are the same primitive with different candidate sources (see
    /// [`crate::agent::ultra`]), so they stack: with both on, the lenses are
    /// dealt across the fusion panel's providers rather than each candidate
    /// re-running the whole panel.
    Ultra(UltraAction),
    /// `/provider ...` — add, remove, or switch LLM providers.
    Provider(ProviderAction),
    /// Finalize an interactive provider setup: add the provider (storing the
    /// API key in `~/.wizard/credentials.toml` when present) and switch to it.
    /// Emitted internally by the inline prompt flow, never parsed from text —
    /// hence the primitive fields (so `SlashCommand` can stay `Eq`).
    ProviderSetup {
        name: String,
        kind: ProviderKind,
        base_url: String,
        model: String,
        api_key: Option<String>,
    },
    /// `/server ...` — status / start / stop the local llama-server.
    Server(ServerAction),
    /// `/login <provider> [force]`: OAuth sign-in for providers that support it
    /// (currently `xai`). `force` replaces a session already on disk.
    Login {
        provider: String,
        force: bool,
    },
    /// `/settings` — open the in-app settings menu (a reusable picker).
    Settings,
    /// `/vim` — toggle modal (vim-style) editing of the input composer.
    Vim,
    /// `/ui [name]`: list the available interfaces (`wizard`, `claude`,
    /// `codex`, `grok`), or wear one. `None` lists; `Some` switches.
    ///
    /// Separate from [`Self::Theme`] because they are separate settings: a
    /// skin is the shape of the chrome, a theme is its palette, and wanting
    /// Codex's layout in `ember`'s colors is a reasonable thing to want.
    Ui(Option<String>),
    /// Import the selected artifacts from Claude Code (`~/.claude/`). Not a
    /// typed command; dispatched from the `/settings` import picker, which is
    /// why it carries the [`ImportSelection`].
    ImportClaude(ImportSelection),
    Quit,
    /// A command a plugin registered at runtime: the name it registered under
    /// and the rest of the typed line, verbatim.
    ///
    /// The one open variant, and the reason the other thirty-odd could stay
    /// closed. A built-in variant carries *parsed* arguments because the one
    /// dispatcher owns what they mean; a plugin's arguments mean whatever the
    /// plugin says, so the raw tail is the only honest thing to carry. The
    /// [`PluginCommand`] itself is deliberately not in here — it holds an
    /// `Arc<dyn CommandHandler>`, which is neither `PartialEq` nor `Debug`, and
    /// `SlashCommand` being both is what a dozen tests are written against. It
    /// is looked up at dispatch instead, which also means a command whose
    /// plugin unloaded between the keystroke and the dispatch is refused rather
    /// than run.
    Plugin {
        name: String,
        args: String,
    },
}

/// What a `/fusion` subcommand does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FusionAction {
    /// `/fusion` (no args): toggle the provider council on/off.
    Toggle,
    /// `/fusion config` — open the panel/synthesizer configurator.
    Config,
}

/// What an `/ultra` subcommand does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UltraAction {
    /// `/ultra` (no args): toggle the lens council on/off.
    Toggle,
    /// `/ultra config` — open the lens/judge roster editor.
    Config,
    /// Save the roster chosen at that editor. Not a typed command: the picker
    /// emits it on Enter, which is why it carries the whole [`UltraConfig`]
    /// rather than a name. Every field of it is `Eq`, so `SlashCommand` stays
    /// `Eq`.
    Apply(UltraConfig),
}

/// What a `/provider` subcommand does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderAction {
    /// `/provider` (no args) — open the interactive two-level picker (switch
    /// providers, or add a new one).
    Menu,
    /// `/provider list` — show configured providers.
    List,
    /// `/provider use <name>` — switch the active provider.
    Use(String),
    /// `/provider add <name> <kind> <base_url> <model> [API_KEY_ENV]`.
    Add {
        name: String,
        kind: ProviderKind,
        base_url: String,
        model: String,
        api_key_env: Option<String>,
    },
    /// `/provider remove <name>`.
    Remove(String),
}

/// Parse the arguments to `/provider` (everything after the command word).
fn parse_provider(args: &[&str]) -> Result<SlashCommand, String> {
    let action = match args.first().copied() {
        None => ProviderAction::Menu,
        Some("list") => ProviderAction::List,
        Some("use") => match args.get(1) {
            Some(name) => ProviderAction::Use((*name).to_string()),
            None => return Err("usage: /provider use <name>".to_string()),
        },
        Some("add") => {
            if args.len() < 5 {
                // The kinds are listed from what is installed, not from a
                // literal. The literal that used to be here had drifted:
                // `chatgptoauth` was a valid kind the config file loaded and
                // this line never mentioned.
                let kinds = crate::llm::registry::kinds()
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("|");
                return Err(format!(
                    "usage: /provider add <name> <{kinds}> <base_url> <model> [API_KEY_ENV]"
                ));
            }
            // Validated against what is registered rather than against a
            // second copy of the same list.
            let kind = ProviderKind::new(args[2]);
            if crate::llm::registry::installed(&kind).is_none() {
                return Err(crate::llm::registry::unknown(&kind).to_string());
            }
            ProviderAction::Add {
                name: args[1].to_string(),
                kind,
                base_url: args[3].to_string(),
                model: args[4].to_string(),
                api_key_env: args.get(5).map(|s| s.to_string()),
            }
        }
        Some("remove") => match args.get(1) {
            Some(name) => ProviderAction::Remove((*name).to_string()),
            None => return Err("usage: /provider remove <name>".to_string()),
        },
        Some(other) => {
            return Err(format!(
                "unknown /provider subcommand '{other}' (list|use|add|remove)"
            ));
        }
    };
    Ok(SlashCommand::Provider(action))
}

/// What a `/memory` subcommand does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryAction {
    /// `/memory` (no args) — list the saved memories: name, type, description.
    List,
    /// `/memory read <name>` — show one memory's content.
    Read(String),
    /// `/memory forget <name>` — delete one memory.
    Forget(String),
}

/// Parse the arguments to `/memory` (everything after the command word).
fn parse_memory(args: &[&str]) -> Result<SlashCommand, String> {
    let action = match args.first().copied() {
        None => MemoryAction::List,
        Some("read") => match args.get(1) {
            Some(name) => MemoryAction::Read((*name).to_string()),
            None => return Err("usage: /memory read <name>".to_string()),
        },
        Some("forget") => match args.get(1) {
            Some(name) => MemoryAction::Forget((*name).to_string()),
            None => return Err("usage: /memory forget <name>".to_string()),
        },
        Some(other) => {
            return Err(format!(
                "unknown /memory subcommand '{other}' — use /memory, /memory read <name>, or /memory forget <name>"
            ));
        }
    };
    Ok(SlashCommand::Memory(action))
}

/// What a `/server` subcommand does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerAction {
    /// `/server` / `/server status` — health of the local llama-server.
    Status,
    /// `/server start` — start llama-server for the active provider.
    Start,
    /// `/server stop` — stop the llama-server Wizard started.
    Stop,
}

/// Parse the arguments to `/server` (everything after the command word).
fn parse_server(args: &[&str]) -> Result<SlashCommand, String> {
    let action = match args.first().copied() {
        None | Some("status") => ServerAction::Status,
        Some("start") => ServerAction::Start,
        Some("stop") => ServerAction::Stop,
        Some(other) => {
            return Err(format!(
                "unknown /server subcommand '{other}' (status|start|stop)"
            ));
        }
    };
    Ok(SlashCommand::Server(action))
}

impl SlashCommand {
    /// Parse a `/...` input line. `None` when `input` is not a slash
    /// command; `Some(Err(msg))` for an unknown command or bad arguments.
    pub fn parse(input: &str) -> Option<Result<Self, String>> {
        let input = input.trim();
        let rest = input.strip_prefix('/')?;
        let mut parts = rest.split_whitespace();
        let command = parts.next().unwrap_or("");
        let args: Vec<&str> = parts.collect();

        let parsed = match command {
            "help" => Ok(Self::Help),
            "clear" => Ok(Self::Clear),
            "model" => Ok(Self::Model(args.first().map(|s| s.to_string()))),
            "mode" => match args.first() {
                None => Ok(Self::Mode(None)),
                Some(&"genie") => Ok(Self::Mode(Some(Mode::Genie))),
                Some(&"sovereign") => Ok(Self::Mode(Some(Mode::Sovereign))),
                Some(other) => Err(format!("unknown mode '{other}' (genie|sovereign)")),
            },
            "genie" => Ok(Self::Mode(Some(Mode::Genie))),
            "sovereign" => Ok(Self::Mode(Some(Mode::Sovereign))),
            "effort" => match args.first().map(|s| s.to_ascii_lowercase()).as_deref() {
                None => Ok(Self::Effort(None)),
                Some("low") => Ok(Self::Effort(Some(Some(ReasoningEffort::Low)))),
                Some("medium") | Some("med") => {
                    Ok(Self::Effort(Some(Some(ReasoningEffort::Medium))))
                }
                Some("high") => Ok(Self::Effort(Some(Some(ReasoningEffort::High)))),
                Some("default") | Some("off") | Some("none") => Ok(Self::Effort(Some(None))),
                Some(other) => Err(format!(
                    "unknown effort '{other}' (low|medium|high|default)"
                )),
            },
            "evolve" => {
                let deep = args.first() == Some(&"--deep");
                let description = if deep { &args[1..] } else { &args[..] }.join(" ");
                if description.is_empty() {
                    Err("usage: /evolve [--deep] <what to add>".to_string())
                } else {
                    Ok(Self::Evolve { deep, description })
                }
            }
            "reload" => Ok(Self::Reload),
            "plan" => Ok(Self::Plan),
            "omakase" => Ok(Self::Omakase),
            "rewind" => match args.first() {
                None => Ok(Self::Rewind(None)),
                Some(arg) => arg
                    .parse::<u64>()
                    .map(|turn| Self::Rewind(Some(turn)))
                    .map_err(|_| "usage: /rewind [turn]".to_string()),
            },
            "resume" => Ok(Self::Resume(args.first().map(|s| s.to_string()))),
            "resume-claude" => Ok(Self::ResumeClaude(args.first().map(|s| s.to_string()))),
            "compact" => Ok(Self::Compact),
            "agents" => Ok(Self::Agents),
            "diff" => Ok(Self::Diff),
            "todos" => Ok(Self::Todos),
            "dashboard" => Ok(Self::Dashboard),
            "cost" => Ok(Self::Cost),
            "memory" => parse_memory(&args),
            "doctor" => Ok(Self::Doctor),
            "status" => Ok(Self::Status),
            "bashes" => Ok(Self::Bashes),
            "btw" => {
                // Keep the question intact (spaces, punctuation) rather than
                // rejoining whitespace-split tokens — the whole rest of the
                // line is the question.
                let rest = input.trim().strip_prefix('/').unwrap_or(input).trim_start();
                let question = rest
                    .strip_prefix("btw")
                    .unwrap_or("")
                    .trim_start()
                    .to_string();
                if question.is_empty() {
                    Err("usage: /btw <question>".to_string())
                } else {
                    Ok(Self::Btw(question))
                }
            }
            "fork" => {
                // Same whole-rest-of-line rule as `/btw`: the side-quest brief
                // keeps its spaces and punctuation.
                let rest = input.trim().strip_prefix('/').unwrap_or(input).trim_start();
                let task = rest
                    .strip_prefix("fork")
                    .unwrap_or("")
                    .trim_start()
                    .to_string();
                if task.is_empty() {
                    Err("usage: /fork <task>".to_string())
                } else {
                    Ok(Self::Fork(task))
                }
            }
            "goal" => {
                let text = args.join(" ");
                if text.is_empty() {
                    Ok(Self::Goal(None))
                } else {
                    Ok(Self::Goal(Some(text)))
                }
            }
            "publish" => Ok(Self::Publish {
                branch: args.first().map(|s| s.to_string()),
            }),
            "provider" => parse_provider(&args),
            "fusion" => match args.first().copied() {
                None => Ok(Self::Fusion(FusionAction::Toggle)),
                Some("config") => Ok(Self::Fusion(FusionAction::Config)),
                Some(other) => Err(format!(
                    "unknown /fusion subcommand '{other}' — use /fusion or /fusion config"
                )),
            },
            "ultra" => match args.first().copied() {
                None => Ok(Self::Ultra(UltraAction::Toggle)),
                Some("config") => Ok(Self::Ultra(UltraAction::Config)),
                Some(other) => Err(format!(
                    "unknown /ultra subcommand '{other}' — use /ultra or /ultra config"
                )),
            },
            "server" => parse_server(&args),
            "login" => match args.as_slice() {
                [provider] => Ok(Self::Login {
                    provider: (*provider).to_string(),
                    force: false,
                }),
                [provider, "force"] => Ok(Self::Login {
                    provider: (*provider).to_string(),
                    force: true,
                }),
                _ => Err("usage: /login xai [force]".to_string()),
            },
            "settings" => Ok(Self::Settings),
            "vim" => Ok(Self::Vim),
            // Joined so `/ui claude code` is the same request as `/ui claude`:
            // people type the product name, not the key.
            "ui" => Ok(Self::Ui((!args.is_empty()).then(|| args.join(" ")))),
            "quit" | "q" | "exit" => Ok(Self::Quit),
            // Not a built-in word. Ask the runtime registry before giving up,
            // so a plugin command is resolved by the same parser every surface
            // already calls rather than by a second lookup each surface would
            // have to remember to do. The built-in arms are matched first and
            // the registry refuses their names ([`plugin::install`]), so this
            // can never shadow one.
            other => match plugin::get(other) {
                Some(_) => Ok(Self::Plugin {
                    name: other.to_string(),
                    // The whole rest of the line, spaces and punctuation
                    // intact: a plugin's argument grammar is its own.
                    args: rest
                        .strip_prefix(other)
                        .unwrap_or_default()
                        .trim_start()
                        .to_string(),
                }),
                None => Err(format!("unknown command '/{other}' — try /help")),
            },
        };
        Some(parsed)
    }

    /// Whether the agent may invoke this command itself (via the `run_command`
    /// tool), and if not, the reason to report back to the model.
    ///
    /// Allowed: read-only status/info commands and state changes the agent can
    /// sensibly apply to its own session (effort, model, mode, goal, planning
    /// modes, reload, compact, and the UI toggles). Refused: commands that need
    /// a human at an interactive picker (the no-argument forms), that end or
    /// rewind the session, or that reach outside it to set up providers.
    pub fn agent_runnable(&self) -> Result<(), String> {
        use SlashCommand::*;
        match self {
            // State the agent can set on itself, plus read-only info toggles.
            Model(Some(_))
            | Mode(Some(_))
            | Effort(Some(_))
            | Goal(_)
            | Diff
            | Todos
            | Dashboard
            | Cost
            // Every `/memory` action — list, read, forget — is one the `memory`
            // tool already grants the agent, so a gate here would be theater.
            | Memory(_)
            | Doctor
            | Status
            | Bashes
            | Compact
            | Reload
            | Plan
            | Omakase
            | Settings
            | Vim
            | Help
            | Fusion(FusionAction::Toggle)
            | Ultra(UltraAction::Toggle) => Ok(()),

            // A side question is the user's aside; the agent already has the
            // conversation and should not spend a turn asking itself.
            Btw(_) => Err(
                "`/btw` is a user side-question; answer from context yourself".into(),
            ),

            // A fork is the user's parallel side quest; the agent already has
            // `spawn_subagent` for its own background work.
            Fork(_) => Err(
                "`/fork` is a user side-quest; use `spawn_subagent` for your own \
                 background work"
                    .into(),
            ),

            // Interactive pickers: there is no human at the keyboard mid-turn,
            // so require the argument that names the choice directly.
            Model(None) => Err("name a model, e.g. `/model claude-sonnet-5`".into()),
            Mode(None) => Err("name a mode, e.g. `/mode sovereign`".into()),
            Effort(None) => Err("name a level, e.g. `/effort high`".into()),
            Fusion(FusionAction::Config) => {
                Err("`/fusion config` opens an interactive editor; use `/fusion` to toggle".into())
            }
            Ultra(UltraAction::Config | UltraAction::Apply(_)) => {
                Err("`/ultra config` opens an interactive editor; use `/ultra` to toggle".into())
            }
            Agents => Err(
                "`/agents` opens a picker for the user; spawn subagents with the spawn tool".into(),
            ),

            // Session-ending, destructive, or external-setup commands are off
            // limits to the agent.
            Quit => Err("refusing to quit the session on the user's behalf".into()),
            Clear => Err("refusing to clear the conversation on the user's behalf".into()),
            Rewind(_) => Err("`/rewind` restores checkpoints and is the user's call".into()),
            Resume(_) => Err("`/resume` switches sessions and is the user's call".into()),
            ResumeClaude(_) => Err(
                "`/resume-claude` switches sessions and is the user's call".into(),
            ),
            Evolve { .. } => {
                Err("`/evolve` is a heavyweight self-edit; leave it to the user".into())
            }
            Publish { .. } => Err("`/publish` forks the tool; leave it to the user".into()),
            Provider(_) | ProviderSetup { .. } => {
                Err("provider setup is the user's call; use `/model` to switch models".into())
            }
            Server(_) => {
                Err("`/server` manages the local model server; leave it to the user".into())
            }
            Login { .. } => Err("`/login` is an interactive sign-in; leave it to the user".into()),
            Ui(_) => Err("`/ui` restyles the user's terminal; leave it to them".into()),
            ImportClaude(_) => {
                Err("`/settings` import is driven from a picker; leave it to the user".into())
            }

            // Plugin commands are not on the agent's allowlist, and this is a
            // deliberate stop rather than an oversight. Every `Ok` above is an
            // argument about *that command's* semantics — is it read-only, does
            // it need a human at a picker, does it reach outside the session —
            // and a plugin cannot make that argument on its own behalf: an
            // `agent_runnable = true` field would be a plugin grading its own
            // homework, and the tool that dispatches it would be handing the
            // model a name whose blast radius nobody assessed. When a plugin
            // command should be model-callable, the plugin registers a *tool*,
            // which is the API that already has a capability grant attached.
            Plugin { name, .. } => Err(format!(
                "`/{name}` comes from a plugin; ask the plugin's tool instead"
            )),
        }
    }

    /// The table row this command belongs to, by name. Aliases fold into the
    /// command they are aliases *of* (`/genie` into `/mode`, `/q` into
    /// `/quit`), and the two variants the pickers emit rather than the parser
    /// (`ProviderSetup`, `ImportClaude`) fold into the command that opened the
    /// picker. So every parsed built-in has exactly one row, and
    /// [`dispatch`](surface::dispatch) can ask the table who runs it. A
    /// [`SlashCommand::Plugin`] names itself instead, which is why this returns
    /// a borrow rather than the `&'static str` it used to: a registered name is
    /// a `String` with a runtime lifetime.
    pub fn name(&self) -> &str {
        use SlashCommand::*;
        match self {
            Help => "help",
            Clear => "clear",
            Model(_) => "model",
            Mode(_) => "mode",
            Effort(_) => "effort",
            Evolve { .. } => "evolve",
            Reload => "reload",
            Plan => "plan",
            Omakase => "omakase",
            Rewind(_) => "rewind",
            Resume(_) => "resume",
            ResumeClaude(_) => "resume-claude",
            Compact => "compact",
            Agents => "agents",
            Diff => "diff",
            Todos => "todos",
            Dashboard => "dashboard",
            Cost => "cost",
            Memory(_) => "memory",
            Doctor => "doctor",
            Status => "status",
            Bashes => "bashes",
            Btw(_) => "btw",
            Fork(_) => "fork",
            Goal(_) => "goal",
            Publish { .. } => "publish",
            Fusion(_) => "fusion",
            Ultra(_) => "ultra",
            Provider(_) | ProviderSetup { .. } => "provider",
            Server(_) => "server",
            Login { .. } => "login",
            Settings | ImportClaude(_) => "settings",
            Vim => "vim",
            Ui(_) => "ui",
            Quit => "quit",
            Plugin { name, .. } => name,
        }
    }

    /// The [`CommandSpec`] for this command, or `None` for a
    /// [`SlashCommand::Plugin`] — the one variant with no table row, because
    /// its row is a [`PluginCommand`] in the runtime registry instead.
    ///
    /// Still total over the built-ins: every other variant's
    /// [`name`](Self::name) is a row of [`COMMANDS`], which
    /// `every_command_has_a_table_row` holds to. Prefer
    /// [`execution`](Self::execution) where the question is "may this surface
    /// run it", so the answer covers both kinds of command.
    pub fn spec(&self) -> Option<&'static CommandSpec> {
        spec(self.name())
    }

    /// How `surface` runs this command, whoever owns it.
    ///
    /// The one gate [`dispatch`](surface::dispatch) and the window's `route`
    /// ask, so a plugin command's "TUI only" is enforced by the same line of
    /// code that enforces `/vim`'s. A plugin command whose plugin has unloaded
    /// resolves to [`Execution::Unavailable`]: there is nothing left to run,
    /// and refusing by name is what a surface already knows how to say.
    pub fn execution(&self, surface: Surface) -> Execution {
        match self {
            Self::Plugin { name, .. } => plugin::get(name)
                .map(|command| command.execution(surface))
                .unwrap_or(Execution::Unavailable),
            other => other
                .spec()
                .map(|spec| spec.execution(surface))
                // Unreachable while `every_command_has_a_table_row` passes.
                // Refusing beats panicking on a surface a user is typing at.
                .unwrap_or(Execution::Unavailable),
        }
    }
}

/* ---------------------------------------------------------------------- */
/* The built-in command table                                             */
/* ---------------------------------------------------------------------- */

/// How one surface runs a built-in command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Execution {
    /// Applied to the live [`Agent`](crate::agent::Agent). In the window it is
    /// queued on the chat's worker ([`crate::gui::tasks::TaskManager`]) and
    /// answers as the same [`AgentEvent`](crate::agent::AgentEvent)s a turn
    /// carries — a notice, a context reading, an error.
    Agent,
    /// The surface's own: a picker, a panel, an overlay, a list. There is
    /// nothing to ask the agent for, and asking would only be a round trip to
    /// a no-op.
    Ui,
    /// Nowhere to land here: a browser has no modal editor to toggle and no
    /// process to exit, and a chat has no panel to draw or picker to choose
    /// from. Offered nowhere on that surface and refused honestly when invoked
    /// anyway. Declared, never a missing match arm.
    Unavailable,
}

impl Execution {
    /// The `where` value on the wire (`GET /api/commands`).
    pub fn wire(self) -> &'static str {
        match self {
            Execution::Agent => "server",
            Execution::Ui => "client",
            Execution::Unavailable => "unavailable",
        }
    }
}

/// One built-in slash command. Drives the TUI's suggestion popup and ghost-text
/// prediction, the GUI's command menu, and the allowlist the agent's
/// `run_command` tool dispatches through — one table, so no two surfaces can
/// drift into offering different commands.
///
/// The per-surface [`Execution`] columns are named fields rather than a list,
/// so a third surface is a field every one of these rows has to answer for
/// before the crate compiles again.
#[derive(Debug)]
pub struct CommandSpec {
    pub name: &'static str,
    /// Argument hint shown after the name (e.g. `[tag]`).
    pub args: &'static str,
    pub description: &'static str,
    /// Completion appends a trailing space and waits for arguments instead
    /// of submitting immediately.
    pub takes_args: bool,
    /// How the terminal UI runs it.
    pub tui: Execution,
    /// How the GUI — the iced window — runs it. Still named `gui` because the
    /// column was the browser GUI's and the window inherited it rather than
    /// growing a duplicate; see [`Surface::Gui`](crate::commands::Surface::Gui).
    pub gui: Execution,
    /// How the Telegram gateway runs it. All agent and no window: a chat can
    /// carry any answer that is text, and nothing that is a picker, a panel or
    /// a keystroke (see [`Surface::Gateway`]).
    pub gateway: Execution,
    /// A valid argument for the commands whose bare form opens an interactive
    /// picker (`/model`, `/mode`, `/effort`), which [`SlashCommand::agent_runnable`]
    /// refuses for want of the choice a human would have made. [`agent_commands`]
    /// parses `/name` plus this before asking the gate, so a picker command is
    /// not mistaken for one the agent may never run. Empty when the bare form
    /// already parses into what the agent would ask for.
    pub agent_arg: &'static str,
}

/// All slash commands, in display order.
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "model",
        args: "[tag]",
        description: "pick or switch the model",
        takes_args: false,
        tui: Execution::Agent,
        gui: Execution::Agent,
        gateway: Execution::Agent,
        agent_arg: "gpt-5",
    },
    CommandSpec {
        name: "mode",
        args: "[genie|sovereign]",
        description: "pick or switch personality mode",
        takes_args: false,
        tui: Execution::Agent,
        gui: Execution::Agent,
        gateway: Execution::Agent,
        agent_arg: "sovereign",
    },
    CommandSpec {
        name: "genie",
        args: "",
        description: "switch to genie mode",
        takes_args: false,
        tui: Execution::Agent,
        gui: Execution::Agent,
        gateway: Execution::Agent,
        agent_arg: "",
    },
    CommandSpec {
        name: "sovereign",
        args: "",
        description: "switch to sovereign mode",
        takes_args: false,
        tui: Execution::Agent,
        gui: Execution::Agent,
        gateway: Execution::Agent,
        agent_arg: "",
    },
    CommandSpec {
        name: "effort",
        args: "[low|medium|high|default]",
        description: "set reasoning effort (Grok 4.x, OpenAI o-series / gpt-5)",
        takes_args: false,
        tui: Execution::Agent,
        gui: Execution::Agent,
        gateway: Execution::Agent,
        agent_arg: "high",
    },
    CommandSpec {
        name: "plan",
        args: "",
        description: "toggle plan mode: read-only until a plan is approved",
        takes_args: false,
        tui: Execution::Agent,
        gui: Execution::Agent,
        gateway: Execution::Agent,
        agent_arg: "",
    },
    CommandSpec {
        name: "omakase",
        args: "",
        description: "toggle omakase: chef's-choice plan mode, the agent decides",
        takes_args: false,
        tui: Execution::Agent,
        gui: Execution::Agent,
        gateway: Execution::Agent,
        agent_arg: "",
    },
    CommandSpec {
        name: "rewind",
        args: "[turn]",
        description: "rewind files and conversation to before a turn",
        takes_args: false,
        tui: Execution::Agent,
        gui: Execution::Agent,
        // Bare `/rewind` degrades to the list of candidate turns as text
        // (`dispatch` falls back to it wherever there is no picker), and
        // `/rewind <turn>` is typeable in a chat.
        gateway: Execution::Agent,
        agent_arg: "",
    },
    CommandSpec {
        name: "resume",
        args: "",
        description: "reopen and continue a past session",
        takes_args: false,
        tui: Execution::Agent,
        gui: Execution::Ui,
        // The one picker command with no argument to fall back on: the table
        // advertises none, and `Chooser::Resume` deliberately has no usage
        // line because the list of past sessions *is* the command. Nothing to
        // show a chat but a menu it cannot render.
        gateway: Execution::Unavailable,
        agent_arg: "",
    },
    CommandSpec {
        name: "resume-claude",
        args: "",
        description: "continue a conversation from Claude Code",
        takes_args: false,
        tui: Execution::Ui,
        gui: Execution::Ui,
        // Unavailable for the same reason `/resume` is, and one more: the
        // import writes a session and then continues the conversation in it,
        // which is not something to do on a chat message with no picker behind
        // it to show what is being taken.
        gateway: Execution::Unavailable,
        agent_arg: "",
    },
    CommandSpec {
        name: "compact",
        args: "",
        description: "summarize older history into a progress note now",
        takes_args: false,
        tui: Execution::Agent,
        gui: Execution::Agent,
        gateway: Execution::Agent,
        agent_arg: "",
    },
    CommandSpec {
        name: "btw",
        args: "<question>",
        description: "ask a side question without adding it to the conversation",
        takes_args: true,
        tui: Execution::Agent,
        gui: Execution::Agent,
        gateway: Execution::Agent,
        agent_arg: "",
    },
    CommandSpec {
        name: "fork",
        args: "<task>",
        description: "spawn a background side quest that inherits full conversation context",
        takes_args: true,
        tui: Execution::Agent,
        gui: Execution::Agent,
        gateway: Execution::Agent,
        agent_arg: "",
    },
    CommandSpec {
        name: "agents",
        args: "",
        description: "browse subagents and delegate to one",
        takes_args: false,
        tui: Execution::Ui,
        gui: Execution::Agent,
        // No picker, so `dispatch` answers with the roster itself — which is
        // the same text the picker is drawn over.
        gateway: Execution::Agent,
        agent_arg: "",
    },
    CommandSpec {
        name: "evolve",
        args: "[--deep] <desc>",
        description: "self-extend: add a skill, tool, or MCP server",
        takes_args: true,
        tui: Execution::Agent,
        gui: Execution::Agent,
        gateway: Execution::Agent,
        agent_arg: "",
    },
    CommandSpec {
        name: "publish",
        args: "[branch]",
        description: "fork & publish your Wizard, get a one-line installer",
        takes_args: false,
        tui: Execution::Agent,
        gui: Execution::Agent,
        // Long, but its whole result is a one-line installer: text.
        gateway: Execution::Agent,
        agent_arg: "",
    },
    CommandSpec {
        name: "provider",
        args: "",
        description: "add or switch LLM providers (interactive)",
        takes_args: false,
        tui: Execution::Ui,
        gui: Execution::Ui,
        // Bare `/provider` is a two-level picker, and the argument forms would
        // have the operator paste an API key into a Telegram chat, where it
        // stays in the message history on someone else's servers.
        gateway: Execution::Unavailable,
        agent_arg: "",
    },
    CommandSpec {
        name: "fusion",
        args: "[config]",
        description: "toggle model fusion, or configure the panel",
        takes_args: false,
        tui: Execution::Agent,
        gui: Execution::Agent,
        gateway: Execution::Agent,
        agent_arg: "",
    },
    CommandSpec {
        name: "ultra",
        args: "[config]",
        description: "toggle mixture of agents, or configure the roster",
        takes_args: false,
        tui: Execution::Agent,
        gui: Execution::Agent,
        gateway: Execution::Agent,
        agent_arg: "",
    },
    CommandSpec {
        name: "server",
        args: "[status|start|stop]",
        description: "manage the local llama-server",
        takes_args: false,
        tui: Execution::Agent,
        gui: Execution::Agent,
        // The llama-server runs on the operator's machine, which is where the
        // gateway runs too; status/start/stop all report back as text.
        gateway: Execution::Agent,
        agent_arg: "",
    },
    CommandSpec {
        name: "login",
        args: "<xai> [force]",
        description: "sign in to a provider account (xAI OAuth)",
        takes_args: true,
        tui: Execution::Ui,
        gui: Execution::Ui,
        // OAuth opens a browser and waits for a callback on the machine. The
        // person in the chat is not sitting at it.
        gateway: Execution::Unavailable,
        agent_arg: "",
    },
    CommandSpec {
        name: "diff",
        args: "",
        description: "toggle the git diff sidebar",
        takes_args: false,
        tui: Execution::Ui,
        gui: Execution::Ui,
        // A sidebar toggle: its entire effect is on a panel nobody here can see.
        gateway: Execution::Unavailable,
        agent_arg: "",
    },
    CommandSpec {
        name: "todos",
        args: "",
        description: "toggle the todo list above the input",
        takes_args: false,
        tui: Execution::Ui,
        gui: Execution::Ui,
        // Same — a band above a composer. `/status` carries the counts instead.
        gateway: Execution::Unavailable,
        agent_arg: "",
    },
    CommandSpec {
        name: "dashboard",
        args: "",
        description: "session manager: all live wizard sessions on this machine",
        takes_args: false,
        tui: Execution::Ui,
        gui: Execution::Ui,
        // A live, machine-wide session view. There is no screen to hold it.
        gateway: Execution::Unavailable,
        agent_arg: "",
    },
    CommandSpec {
        name: "cost",
        args: "",
        description: "show session token usage and cost",
        takes_args: false,
        tui: Execution::Agent,
        gui: Execution::Agent,
        gateway: Execution::Agent,
        agent_arg: "",
    },
    CommandSpec {
        name: "memory",
        args: "[read|forget <name>]",
        description: "list, show, or forget saved project memories",
        takes_args: false,
        tui: Execution::Agent,
        gui: Execution::Agent,
        gateway: Execution::Agent,
        agent_arg: "",
    },
    CommandSpec {
        name: "status",
        args: "",
        description: "show session status: model, usage, todos, tasks",
        takes_args: false,
        tui: Execution::Agent,
        gui: Execution::Agent,
        gateway: Execution::Agent,
        agent_arg: "",
    },
    CommandSpec {
        name: "bashes",
        args: "",
        description: "list background tasks: id, status, command",
        takes_args: false,
        tui: Execution::Agent,
        gui: Execution::Agent,
        gateway: Execution::Agent,
        agent_arg: "",
    },
    CommandSpec {
        name: "goal",
        args: "[text]",
        description: "show the standing goal, or set one and start working on it",
        takes_args: false,
        tui: Execution::Agent,
        gui: Execution::Agent,
        gateway: Execution::Agent,
        agent_arg: "",
    },
    CommandSpec {
        name: "settings",
        args: "",
        description: "open the settings menu (change config anytime)",
        takes_args: false,
        tui: Execution::Ui,
        gui: Execution::Ui,
        // An in-app menu; every entry of it is a choice made at a picker.
        gateway: Execution::Unavailable,
        agent_arg: "",
    },
    CommandSpec {
        name: "vim",
        args: "",
        description: "toggle vim-style modal editing of the input line",
        takes_args: false,
        tui: Execution::Ui,
        gui: Execution::Unavailable,
        // Modal editing of a composer this surface does not have.
        gateway: Execution::Unavailable,
        agent_arg: "",
    },
    CommandSpec {
        name: "ui",
        args: "[wizard|codex|grok]",
        description: "list interfaces, or wear one (codex, grok build)",
        takes_args: false,
        tui: Execution::Ui,
        // The window draws its own widgets; there is no terminal chrome in it
        // for a skin to reshape.
        gui: Execution::Unavailable,
        // Telegram has no chrome of ours at all.
        gateway: Execution::Unavailable,
        agent_arg: "",
    },
    CommandSpec {
        name: "doctor",
        args: "",
        description: "diagnose config, providers, MCP, hooks, state dirs",
        takes_args: false,
        tui: Execution::Agent,
        gui: Execution::Agent,
        gateway: Execution::Agent,
        agent_arg: "",
    },
    CommandSpec {
        name: "reload",
        args: "",
        description: "reload skills, scripted tools, and MCP servers",
        takes_args: false,
        tui: Execution::Agent,
        gui: Execution::Agent,
        gateway: Execution::Agent,
        agent_arg: "",
    },
    CommandSpec {
        name: "clear",
        args: "",
        description: "clear the conversation",
        takes_args: false,
        tui: Execution::Agent,
        gui: Execution::Ui,
        // A chat can be handed a fresh session as easily as a window can, and
        // it is how a gateway conversation is ended (see `/quit`).
        gateway: Execution::Agent,
        agent_arg: "",
    },
    CommandSpec {
        name: "help",
        args: "",
        description: "show available commands and keys",
        takes_args: false,
        tui: Execution::Ui,
        gui: Execution::Agent,
        // Derived from this table and sent as a message. The gateway has no
        // client half to run it instead, so it belongs to the process holding
        // the agent, like everything else here.
        gateway: Execution::Agent,
        agent_arg: "",
    },
    CommandSpec {
        name: "quit",
        args: "",
        description: "exit wizard",
        takes_args: false,
        tui: Execution::Ui,
        gui: Execution::Unavailable,
        // The gateway is a long-running service shared by every allow-listed
        // chat: one message must not take it down for the rest. Ending a
        // conversation is `/clear`; stopping the service is done on the
        // machine that runs it.
        gateway: Execution::Unavailable,
        agent_arg: "",
    },
    CommandSpec {
        name: "exit",
        args: "",
        description: "exit wizard",
        takes_args: false,
        tui: Execution::Ui,
        gui: Execution::Unavailable,
        // The alias, and the same reasoning.
        gateway: Execution::Unavailable,
        agent_arg: "",
    },
];

/// The [`CommandSpec`] for a built-in command word, if there is one.
pub fn spec(name: &str) -> Option<&'static CommandSpec> {
    COMMANDS.iter().find(|spec| spec.name == name)
}

/// The built-in commands `surface` runs the given way.
pub fn commands_for(
    surface: Surface,
    execution: Execution,
) -> impl Iterator<Item = &'static CommandSpec> {
    COMMANDS
        .iter()
        .filter(move |spec| spec.execution(surface) == execution)
}

/// One row of the palette on one surface, whoever owns it.
///
/// The merged view of [`COMMANDS`] and the [`plugin`] registry, and the only
/// thing a surface should build its completion, its help or its advertised menu
/// out of. Owned `String`s rather than borrows because half the rows come from
/// behind an `RwLock` and the other half are `&'static` — a type that could
/// express both would be a lifetime puzzle for no gain, and this is built once
/// per keystroke at most.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub name: String,
    /// Argument hint shown after the name.
    pub args: String,
    pub description: String,
    /// Completion appends a trailing space and waits for arguments.
    pub takes_args: bool,
    /// How this surface runs it — [`Execution::Unavailable`] included, because
    /// the window lists those dimmed rather than hiding them.
    pub execution: Execution,
    /// A plugin registered it rather than [`COMMANDS`]. Nothing branches on
    /// this today — the whole point is that a surface does not have to — but it
    /// is what a `/help` that ever groups by origin would read, and it is what
    /// the tests assert a plugin row actually is.
    pub from_plugin: bool,
}

/// Every command `surface` knows about: the built-ins in table order, then the
/// plugin-registered ones in name order.
///
/// Built-ins first and plugins after, rather than interleaved alphabetically,
/// because the table's order is a designed order (`/model`, `/mode`, `/effort`
/// … then the rare ones) and dropping a plugin's `/deploy` into the middle of it
/// would scramble a list people navigate by position.
pub fn listing(surface: Surface) -> Vec<Listing> {
    let builtin = COMMANDS.iter().map(|spec| Listing {
        name: spec.name.to_string(),
        args: spec.args.to_string(),
        description: spec.description.to_string(),
        takes_args: spec.takes_args,
        execution: spec.execution(surface),
        from_plugin: false,
    });
    let registered = plugin::all().into_iter().map(move |command| Listing {
        execution: command.execution(surface),
        name: command.name,
        args: command.args,
        description: command.description,
        takes_args: command.takes_args,
        from_plugin: true,
    });
    builtin.chain(registered).collect()
}

/// [`listing`] minus what `surface` cannot run. What completion offers and what
/// `/help` lists.
pub fn available(surface: Surface) -> Vec<Listing> {
    listing(surface)
        .into_iter()
        .filter(|row| row.execution != Execution::Unavailable)
        .collect()
}

/// Whether `name` is a command word this build knows — a built-in, one of the
/// parser's aliases, or a plugin registration.
///
/// The question the TUI asks to decide whether `/word` with bad arguments earns
/// a usage notice or falls through to the model as an ordinary prompt.
pub fn is_known(name: &str) -> bool {
    plugin::is_builtin(name) || plugin::get(name).is_some()
}

/// The commands the agent may queue through `run_command` on a surface that
/// runs [`Execution::Agent`] commands against the Agent (the GUI): those
/// entries, minus the ones [`SlashCommand::agent_runnable`] refuses whatever
/// their arguments.
///
/// Derived from the one table and the one gate — never a second list — so the
/// tool cannot promise the model a command the executor does not run, nor
/// refuse one it does.
pub fn agent_commands() -> &'static [&'static str] {
    static NAMES: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    NAMES.get_or_init(|| {
        commands_for(Surface::Gui, Execution::Agent)
            .filter(|spec| spec.agent_runnable())
            .map(|spec| spec.name)
            .collect()
    })
}

impl CommandSpec {
    /// How `surface` runs this command.
    pub fn execution(&self, surface: Surface) -> Execution {
        match surface {
            Surface::Tui => self.tui,
            Surface::Gui => self.gui,
            Surface::Gateway => self.gateway,
        }
    }

    /// Whether the agent may queue this command at all — whether *some*
    /// invocation of it passes [`SlashCommand::agent_runnable`]. Argument-level
    /// gating stays in that gate; this is its by-name half, which is all a
    /// dispatch allowlist can express.
    fn agent_runnable(&self) -> bool {
        let line = format!("/{} {}", self.name, self.agent_arg);
        matches!(
            SlashCommand::parse(&line),
            Some(Ok(command)) if command.agent_runnable().is_ok()
        )
    }
}

/* ---------------------------------------------------------------------- */
/* Custom commands and @file references                                   */
/* ---------------------------------------------------------------------- */

/// One loaded custom command.
#[derive(Debug, Clone)]
pub struct CustomCommand {
    /// Command name (the file stem): `/name` invokes it.
    pub name: String,
    /// Frontmatter `description`, shown in the suggestion popup.
    pub description: Option<String>,
    /// Prompt template with `$ARGUMENTS` / `$1`..`$9` placeholders.
    pub template: String,
    /// File it was loaded from.
    pub path: PathBuf,
}

impl CustomCommand {
    /// Whether the template references any argument placeholder — drives the
    /// `[args]` hint and Enter-to-complete behavior in the TUI.
    pub fn expects_args(&self) -> bool {
        let bytes = self.template.as_bytes();
        self.template.match_indices('$').any(|(i, _)| {
            let rest = &bytes[i + 1..];
            rest.starts_with(b"ARGUMENTS")
                || rest.first().is_some_and(|b| (b'1'..=b'9').contains(b))
        })
    }
}

/// Load custom commands from the canonical roots: `~/.wizard/commands/`,
/// then `<project>/.wizard/commands/` (project shadows global).
pub fn load(project_root: &Path) -> Vec<CustomCommand> {
    let mut dirs = Vec::new();
    match Config::wizard_dir() {
        Ok(dir) => dirs.push(dir.join("commands")),
        Err(err) => tracing::warn!("could not resolve ~/.wizard for commands: {err}"),
    }
    dirs.push(project_root.join(".wizard").join("commands"));
    load_from_dirs(&dirs)
}

/// Load `*.md` commands from `dirs` in order; later directories shadow
/// earlier ones on a name collision. Missing directories are skipped;
/// unreadable files are logged and skipped. The result is sorted by name.
pub fn load_from_dirs(dirs: &[PathBuf]) -> Vec<CustomCommand> {
    let mut by_name: std::collections::BTreeMap<String, CustomCommand> =
        std::collections::BTreeMap::new();
    for dir in dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                tracing::warn!("could not read {}: {err}", dir.display());
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let raw = match std::fs::read_to_string(&path) {
                Ok(raw) => raw,
                Err(err) => {
                    tracing::warn!("could not read {}: {err}", path.display());
                    continue;
                }
            };
            let (meta, body) = crate::skills::split_frontmatter(&raw);
            by_name.insert(
                name.to_string(),
                CustomCommand {
                    name: name.to_string(),
                    description: meta.description,
                    template: body,
                    path,
                },
            );
        }
    }
    by_name.into_values().collect()
}

/// Expand `$ARGUMENTS` and `$1`..`$9` in `template`. A single pass over the
/// template, so placeholder-like text inside the arguments themselves is
/// never re-expanded.
pub fn expand_template(template: &str, args: &str) -> String {
    let args = args.trim();
    let positional: Vec<&str> = args.split_whitespace().collect();
    let mut out = String::with_capacity(template.len() + args.len());
    let mut rest = template;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        if let Some(tail) = after.strip_prefix("ARGUMENTS") {
            out.push_str(args);
            rest = tail;
        } else if let Some(digit) = after.chars().next().filter(|c| ('1'..='9').contains(c)) {
            let index = digit as usize - '1' as usize;
            if let Some(arg) = positional.get(index) {
                out.push_str(arg);
            }
            rest = &after[1..];
        } else {
            out.push('$');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// If `input` is `/name [args...]` for one of `commands`, expand its
/// template. `None` when the input is not a custom-command invocation.
pub fn expand_custom(input: &str, commands: &[CustomCommand]) -> Option<String> {
    let rest = input.trim().strip_prefix('/')?;
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let command = commands.iter().find(|command| command.name == name)?;
    Some(expand_template(&command.template, args))
}

/// Byte cap applied to one `@file` expansion.
pub const MAX_FILE_REF_BYTES: usize = 50_000;

/// Extensions treated as images. These expand to a short placeholder in the
/// prompt text and are collected as attachment paths for vision models.
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

/// Result of expanding custom commands and `@file` / image references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preprocessed {
    /// Expanded prompt text (file contents inlined; images as `[image: name]`).
    pub text: String,
    /// Absolute paths of image files referenced via `@path` (and any other
    /// image attachments the caller may merge in before the agent turn).
    pub images: Vec<PathBuf>,
}

impl Preprocessed {
    /// Text-only result with no attachments.
    pub fn text_only(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
        }
    }
}

/// Whether `path` looks like a supported image file (by extension).
pub fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|ext| IMAGE_EXTENSIONS.contains(&ext.as_str()))
}

/// Expand `@path` tokens in `input` to fenced code blocks with the file's
/// contents (capped at [`MAX_FILE_REF_BYTES`], with a truncation note).
///
/// A token expands only when `@` starts a whitespace-delimited token and the
/// rest resolves to an existing file (relative to `project_root`, absolute,
/// or `~/`-prefixed). Everything else — `@@escaped` tokens, email-like
/// `user@host`, `@missing-paths` — passes through unchanged. Image files
/// expand to a short `[image: name]` placeholder and their absolute paths are
/// collected in [`Preprocessed::images`] for vision-capable providers.
pub fn expand_file_refs(input: &str, project_root: &Path) -> Preprocessed {
    let mut out = String::with_capacity(input.len());
    let mut images = Vec::new();
    let mut rest = input;
    while !rest.is_empty() {
        // Copy leading whitespace verbatim, then take one token.
        let token_start = rest
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(rest.len());
        out.push_str(&rest[..token_start]);
        rest = &rest[token_start..];
        if rest.is_empty() {
            break;
        }
        let token_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let token = &rest[..token_end];
        match expand_token(token, project_root) {
            Some(TokenExpansion::Text(expanded)) => out.push_str(&expanded),
            Some(TokenExpansion::Image { placeholder, path }) => {
                out.push_str(&placeholder);
                images.push(path);
            }
            None => out.push_str(token),
        }
        rest = &rest[token_end..];
    }
    Preprocessed { text: out, images }
}

/// Result of expanding one `@` token.
enum TokenExpansion {
    Text(String),
    Image { placeholder: String, path: PathBuf },
}

/// Expand one whitespace-delimited token, or `None` to pass it through.
fn expand_token(token: &str, project_root: &Path) -> Option<TokenExpansion> {
    let path_part = token.strip_prefix('@')?;
    // `@@path` is the escape hatch and a lone `@` is not a reference.
    if path_part.is_empty() || path_part.starts_with('@') {
        return None;
    }
    let path = resolve(path_part, project_root);
    if !path.is_file() {
        return None;
    }
    if is_image_path(&path) {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(path_part);
        let absolute = path.canonicalize().unwrap_or_else(|_| path.clone());
        return Some(TokenExpansion::Image {
            placeholder: format!("[image: {name}]"),
            path: absolute,
        });
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        // Unreadable / non-UTF-8: leave the token for the model to act on.
        Err(_) => return None,
    };
    let (content, truncated) = cap_bytes(&raw, MAX_FILE_REF_BYTES);
    let fence = fence_for(content);
    let mut block = format!("{fence}{path_part}\n{content}");
    if !content.ends_with('\n') {
        block.push('\n');
    }
    if truncated {
        block.push_str("… [truncated at 50KB]\n");
    }
    block.push_str(&fence);
    Some(TokenExpansion::Text(block))
}

/// Resolve a `@`-reference against the project root, expanding a leading `~`.
fn resolve(path: &str, project_root: &Path) -> PathBuf {
    let expanded = shellexpand::tilde(path);
    let candidate = Path::new(expanded.as_ref());
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        project_root.join(candidate)
    }
}

/// Truncate to at most `max` bytes on a char boundary. Returns the slice and
/// whether anything was dropped.
fn cap_bytes(raw: &str, max: usize) -> (&str, bool) {
    if raw.len() <= max {
        return (raw, false);
    }
    let mut cut = max;
    while cut > 0 && !raw.is_char_boundary(cut) {
        cut -= 1;
    }
    (&raw[..cut], true)
}

/// A backtick fence one longer than the longest run inside `content`
/// (minimum three), so embedded fences cannot break the block.
fn fence_for(content: &str) -> String {
    let mut longest = 0usize;
    let mut run = 0usize;
    for c in content.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat((longest + 1).max(3))
}

/// The one shared preprocessing pipeline for user prompts: expand a custom
/// `/command` invocation (when `input` is one), then `@file` references.
/// Used by both the TUI submit path and headless `-p` runs.
pub fn preprocess(input: &str, commands: &[CustomCommand], project_root: &Path) -> Preprocessed {
    let expanded = expand_custom(input, commands).unwrap_or_else(|| input.to_string());
    expand_file_refs(&expanded, project_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, content: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), content).unwrap();
    }

    // --- the built-in table ---

    /// Every row is a word the parser knows. A row the parser answers "unknown
    /// command" to is a menu entry that cannot be typed; a command with no row
    /// is one only whoever already knew about it can find.
    #[test]
    fn every_row_is_a_command_the_parser_knows() {
        let mut seen = std::collections::HashSet::new();
        for spec in COMMANDS {
            assert!(
                seen.insert(spec.name),
                "/{} is in the table twice",
                spec.name
            );
            let line = format!("/{} {}", spec.name, spec.agent_arg);
            match SlashCommand::parse(&line) {
                // `/evolve` and friends parse only with the argument they exist
                // to carry; what matters here is that the word is known.
                Some(Err(message)) => assert!(
                    !message.contains("unknown command"),
                    "/{} is offered but the parser does not know it",
                    spec.name
                ),
                Some(Ok(_)) => {}
                None => panic!("/{} is not a slash command", spec.name),
            }
        }
    }

    /// The agent's allowlist is the intersection of what a surface runs against
    /// the Agent and what the gate lets the model ask for — derived, so it cannot
    /// drift from either.
    #[test]
    fn the_agent_allowlist_is_the_server_table_minus_what_the_gate_refuses() {
        let allowed = agent_commands();
        assert!(allowed.contains(&"goal"), "the agent may set the mission");
        assert!(allowed.contains(&"model"), "and switch its own model");
        assert!(allowed.contains(&"effort"));
        assert!(allowed.contains(&"status"));

        // Runs here, but is the user's call: restoring checkpoints, forking the
        // tool, rebuilding the binary, or taking the local model server down.
        for refused in ["rewind", "publish", "evolve", "server", "agents"] {
            assert_eq!(
                spec(refused).map(|spec| spec.gui),
                Some(Execution::Agent),
                "/{refused} is executed against the agent"
            );
            assert!(
                !allowed.contains(&refused),
                "/{refused} is the user's call, not the agent's"
            );
        }

        // Nothing the page owns, or the terminal owns, is offered to the agent
        // here — it would be queued and then answered with an error it never
        // reads.
        for spec in COMMANDS {
            if spec.gui != Execution::Agent {
                assert!(
                    !allowed.contains(&spec.name),
                    "/{} does not run against the Agent",
                    spec.name
                );
            }
        }
    }

    /// Every parsed command folds onto a row of the table, so
    /// [`SlashCommand::spec`] is total and [`dispatch`] can always ask who runs
    /// it. A variant with no row would reach the dispatcher with nothing to
    /// gate it, which is the drift this whole module exists to prevent.
    #[test]
    fn every_command_has_a_table_row() {
        let commands = [
            SlashCommand::Help,
            SlashCommand::Clear,
            SlashCommand::Model(None),
            SlashCommand::Mode(None),
            SlashCommand::Effort(None),
            SlashCommand::Evolve {
                deep: false,
                description: "x".into(),
            },
            SlashCommand::Reload,
            SlashCommand::Plan,
            SlashCommand::Omakase,
            SlashCommand::Rewind(None),
            SlashCommand::Resume(None),
            SlashCommand::ResumeClaude(None),
            SlashCommand::Compact,
            SlashCommand::Agents,
            SlashCommand::Diff,
            SlashCommand::Todos,
            SlashCommand::Dashboard,
            SlashCommand::Cost,
            SlashCommand::Memory(MemoryAction::List),
            SlashCommand::Doctor,
            SlashCommand::Status,
            SlashCommand::Bashes,
            SlashCommand::Btw("x".into()),
            SlashCommand::Fork("x".into()),
            SlashCommand::Goal(None),
            SlashCommand::Publish { branch: None },
            SlashCommand::Fusion(FusionAction::Toggle),
            SlashCommand::Ultra(UltraAction::Toggle),
            SlashCommand::Provider(ProviderAction::Menu),
            SlashCommand::ProviderSetup {
                name: "x".into(),
                kind: ProviderKind::OLLAMA,
                base_url: "u".into(),
                model: "m".into(),
                api_key: None,
            },
            SlashCommand::Server(ServerAction::Status),
            SlashCommand::Login {
                provider: "xai".into(),
                force: false,
            },
            SlashCommand::Settings,
            SlashCommand::ImportClaude(ImportSelection::default()),
            SlashCommand::Vim,
            SlashCommand::Quit,
        ];
        for command in &commands {
            assert!(
                spec(command.name()).is_some(),
                "{command:?} names /{}, which is not in the table",
                command.name()
            );
        }
    }

    /// A picker command's bare form is refused for want of the choice a human
    /// would have made at the picker — which is a *usage* refusal, not a policy
    /// one. The allowlist has to tell them apart, or `/model` looks like `/quit`.
    #[test]
    fn a_picker_command_is_not_mistaken_for_one_the_agent_may_never_run() {
        assert!(
            SlashCommand::parse("/model")
                .unwrap()
                .unwrap()
                .agent_runnable()
                .is_err()
        );
        assert!(
            SlashCommand::parse("/model gpt-5")
                .unwrap()
                .unwrap()
                .agent_runnable()
                .is_ok()
        );
        assert!(agent_commands().contains(&"model"));
    }

    #[test]
    fn non_slash_input_is_not_a_command_and_unknown_words_error() {
        assert_eq!(SlashCommand::parse("just a prompt"), None);
        assert_eq!(SlashCommand::parse(""), None);
        assert_eq!(
            SlashCommand::parse("  /help  "),
            Some(Ok(SlashCommand::Help))
        );
        assert!(
            matches!(SlashCommand::parse("/frobnicate"), Some(Err(message)) if message.contains("unknown command"))
        );
    }

    #[test]
    fn effort_parses_levels_aliases_and_any_case() {
        let parse = |line: &str| SlashCommand::parse(line).expect("a slash command");
        assert_eq!(parse("/effort"), Ok(SlashCommand::Effort(None)));
        assert_eq!(
            parse("/effort HIGH"),
            Ok(SlashCommand::Effort(Some(Some(ReasoningEffort::High))))
        );
        assert_eq!(
            parse("/effort med"),
            Ok(SlashCommand::Effort(Some(Some(ReasoningEffort::Medium))))
        );
        assert_eq!(parse("/effort off"), Ok(SlashCommand::Effort(Some(None))));
        assert_eq!(
            parse("/effort default"),
            Ok(SlashCommand::Effort(Some(None)))
        );
        assert!(
            matches!(parse("/effort extreme"), Err(message) if message.contains("unknown effort"))
        );
    }

    #[test]
    fn evolve_requires_a_description_and_takes_deep() {
        let parse = |line: &str| SlashCommand::parse(line).expect("a slash command");
        assert_eq!(
            parse("/evolve add a linter tool"),
            Ok(SlashCommand::Evolve {
                deep: false,
                description: "add a linter tool".to_string(),
            })
        );
        assert_eq!(
            parse("/evolve --deep browser control"),
            Ok(SlashCommand::Evolve {
                deep: true,
                description: "browser control".to_string(),
            })
        );
        assert!(matches!(parse("/evolve"), Err(message) if message.contains("usage")));
        assert!(matches!(parse("/evolve --deep"), Err(message) if message.contains("usage")));
    }

    #[test]
    fn rewind_takes_a_turn_number_or_opens_the_picker() {
        let parse = |line: &str| SlashCommand::parse(line).expect("a slash command");
        assert_eq!(parse("/rewind"), Ok(SlashCommand::Rewind(None)));
        assert_eq!(parse("/rewind 3"), Ok(SlashCommand::Rewind(Some(3))));
        assert!(matches!(parse("/rewind three"), Err(message) if message.contains("usage")));
        assert!(matches!(parse("/rewind -1"), Err(message) if message.contains("usage")));
    }

    #[test]
    /// The two kinds it names are plugins, so the parse it exercises only
    /// exists on a build that has them. The arity and unknown-kind halves
    /// below hold either way and are covered by
    /// `an_unregistered_kind_parses_and_fails_later`.
    #[cfg(all(feature = "provider-ollama", feature = "provider-openai"))]
    fn provider_add_parses_kind_and_arity() {
        let parse = |line: &str| SlashCommand::parse(line).expect("a slash command");
        assert_eq!(
            parse("/provider"),
            Ok(SlashCommand::Provider(ProviderAction::Menu))
        );
        assert_eq!(
            parse("/provider add local ollama http://localhost:11434 qwen3:8b"),
            Ok(SlashCommand::Provider(ProviderAction::Add {
                name: "local".to_string(),
                kind: ProviderKind::OLLAMA,
                base_url: "http://localhost:11434".to_string(),
                model: "qwen3:8b".to_string(),
                api_key_env: None,
            }))
        );
        assert_eq!(
            parse("/provider add or openrouter https://openrouter.ai/api/v1 auto OPENROUTER_KEY"),
            Ok(SlashCommand::Provider(ProviderAction::Add {
                name: "or".to_string(),
                kind: ProviderKind::OPENROUTER,
                base_url: "https://openrouter.ai/api/v1".to_string(),
                model: "auto".to_string(),
                api_key_env: Some("OPENROUTER_KEY".to_string()),
            }))
        );
        assert!(
            matches!(parse("/provider add local ollama"), Err(message) if message.contains("usage"))
        );
        assert!(
            matches!(parse("/provider add x bogus url model"), Err(message) if message.contains("unknown provider kind"))
        );
        assert!(matches!(parse("/provider use"), Err(message) if message.contains("usage")));
        assert!(
            matches!(parse("/provider frob"), Err(message) if message.contains("unknown /provider subcommand"))
        );
    }

    /// `/memory` lists, `/memory read <name>` shows one, `/memory forget
    /// <name>` deletes one. A subcommand without the name it needs is a usage
    /// error, not a memory named nothing.
    #[test]
    fn memory_parses_its_three_forms() {
        let parse = |line: &str| SlashCommand::parse(line).expect("a slash command");
        assert_eq!(
            parse("/memory"),
            Ok(SlashCommand::Memory(MemoryAction::List))
        );
        assert_eq!(
            parse("/memory read subagent-panes"),
            Ok(SlashCommand::Memory(MemoryAction::Read(
                "subagent-panes".to_string()
            )))
        );
        assert_eq!(
            parse("/memory forget subagent-panes"),
            Ok(SlashCommand::Memory(MemoryAction::Forget(
                "subagent-panes".to_string()
            )))
        );
        assert_eq!(
            parse("/memory read"),
            Err("usage: /memory read <name>".to_string())
        );
        assert_eq!(
            parse("/memory forget"),
            Err("usage: /memory forget <name>".to_string())
        );
        assert!(
            matches!(parse("/memory purge"), Err(message) if message.contains("unknown /memory subcommand"))
        );
    }

    /// `/btw` keeps the whole rest of the line (spaces and all) as the
    /// question, and refuses a bare `/btw` with a usage error.
    #[test]
    fn btw_keeps_the_full_question_and_refuses_an_empty_one() {
        let parse = |line: &str| SlashCommand::parse(line).expect("a slash command");
        assert_eq!(
            parse("/btw what is the default timeout?"),
            Ok(SlashCommand::Btw("what is the default timeout?".into()))
        );
        assert_eq!(
            parse("/btw   keep  internal   spaces?"),
            Ok(SlashCommand::Btw("keep  internal   spaces?".into()))
        );
        assert_eq!(parse("/btw"), Err("usage: /btw <question>".to_string()));
        assert_eq!(parse("/btw   "), Err("usage: /btw <question>".to_string()));
        // A side question is the user's call — the agent may not queue one.
        assert!(SlashCommand::Btw("hi".into()).agent_runnable().is_err());
    }

    /// `/fork` keeps the whole rest of the line as the side-quest brief, and
    /// refuses a bare `/fork` with a usage error. The agent may not invoke it
    /// (it already has `spawn_subagent`).
    #[test]
    fn fork_keeps_the_full_task_and_refuses_an_empty_one() {
        let parse = |line: &str| SlashCommand::parse(line).expect("a slash command");
        assert_eq!(
            parse("/fork read the docs and summarize auth"),
            Ok(SlashCommand::Fork(
                "read the docs and summarize auth".into()
            ))
        );
        assert_eq!(
            parse("/fork   keep  internal   spaces"),
            Ok(SlashCommand::Fork("keep  internal   spaces".into()))
        );
        assert_eq!(parse("/fork"), Err("usage: /fork <task>".to_string()));
        assert_eq!(parse("/fork   "), Err("usage: /fork <task>".to_string()));
        assert!(SlashCommand::Fork("hi".into()).agent_runnable().is_err());
    }

    #[test]
    fn the_wire_value_says_who_executes_a_command() {
        assert_eq!(Execution::Agent.wire(), "server");
        assert_eq!(Execution::Ui.wire(), "client");
        assert_eq!(Execution::Unavailable.wire(), "unavailable");
        assert_eq!(spec("goal").map(|spec| spec.gui), Some(Execution::Agent));
        assert_eq!(spec("diff").map(|spec| spec.gui), Some(Execution::Ui));
        assert_eq!(
            spec("vim").map(|spec| spec.gui),
            Some(Execution::Unavailable)
        );
        assert!(spec("frobnicate").is_none());
    }

    /// The terminal runs every command it offers. Only the *browser* has rows
    /// with nowhere to land, and the columns say which, so a third surface
    /// declares its gaps here rather than discovering them as silence.
    #[test]
    fn each_surface_declares_its_gaps_in_the_table() {
        for spec in COMMANDS {
            assert_ne!(
                spec.tui,
                Execution::Unavailable,
                "/{} is offered in the terminal, so the terminal runs it",
                spec.name
            );
        }
        let missing: Vec<&str> = commands_for(Surface::Gui, Execution::Unavailable)
            .map(|spec| spec.name)
            .collect();
        assert_eq!(missing, ["vim", "ui", "quit", "exit"]);
    }

    // --- loading ---

    #[test]
    fn loads_commands_from_md_files_with_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("commands");
        write(
            &dir,
            "review.md",
            "---\ndescription: review the diff\n---\nReview this: $ARGUMENTS",
        );
        write(&dir, "notes.txt", "not a command");
        let commands = load_from_dirs(&[dir]);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "review");
        assert_eq!(commands[0].description.as_deref(), Some("review the diff"));
        assert_eq!(commands[0].template, "Review this: $ARGUMENTS");
        assert!(commands[0].expects_args());
    }

    #[test]
    fn command_without_frontmatter_has_no_description() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("commands");
        write(&dir, "ship.md", "Commit and push everything.");
        let commands = load_from_dirs(&[dir]);
        assert_eq!(commands[0].name, "ship");
        assert_eq!(commands[0].description, None);
        assert!(!commands[0].expects_args());
    }

    #[test]
    fn project_commands_shadow_global_on_name_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path().join("global");
        let project = tmp.path().join("project");
        write(&global, "deploy.md", "global deploy");
        write(&global, "lint.md", "global lint");
        write(&project, "deploy.md", "project deploy");
        let commands = load_from_dirs(&[global, project]);
        assert_eq!(commands.len(), 2);
        let deploy = commands.iter().find(|c| c.name == "deploy").unwrap();
        assert_eq!(deploy.template, "project deploy");
        assert!(commands.iter().any(|c| c.name == "lint"));
    }

    #[test]
    fn missing_directories_load_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let commands = load_from_dirs(&[tmp.path().join("absent")]);
        assert!(commands.is_empty());
    }

    // --- template expansion ---

    #[test]
    fn arguments_placeholder_takes_everything_after_the_name() {
        assert_eq!(
            expand_template("Fix: $ARGUMENTS", "the login bug now"),
            "Fix: the login bug now"
        );
    }

    #[test]
    fn positional_placeholders_split_on_whitespace() {
        assert_eq!(
            expand_template("from $1 to $2 ($ARGUMENTS)", "main release"),
            "from main to release (main release)"
        );
    }

    #[test]
    fn missing_positionals_expand_to_empty() {
        assert_eq!(expand_template("a=$1 b=$2 c=$3", "only"), "a=only b= c=");
    }

    #[test]
    fn dollar_in_arguments_is_not_reexpanded() {
        assert_eq!(expand_template("run $1", "$2"), "run $2");
        assert_eq!(
            expand_template("say $ARGUMENTS", "$1 literal"),
            "say $1 literal"
        );
    }

    #[test]
    fn bare_dollar_and_unknown_placeholders_pass_through() {
        assert_eq!(
            expand_template("price $0 and $x end$", "y"),
            "price $0 and $x end$"
        );
    }

    #[test]
    fn expand_custom_matches_by_name() {
        let commands = vec![CustomCommand {
            name: "review".into(),
            description: None,
            template: "Review $ARGUMENTS carefully.".into(),
            path: PathBuf::new(),
        }];
        assert_eq!(
            expand_custom("/review src/app.rs", &commands).as_deref(),
            Some("Review src/app.rs carefully.")
        );
        assert_eq!(expand_custom("/other x", &commands), None);
        assert_eq!(expand_custom("not a command", &commands), None);
    }

    // --- @file references ---

    #[test]
    fn existing_file_expands_to_a_fenced_block() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("notes.md"), "hello world\n").unwrap();
        let out = expand_file_refs("see @notes.md please", tmp.path());
        assert_eq!(out.text, "see ```notes.md\nhello world\n``` please");
        assert!(out.images.is_empty());
    }

    #[test]
    fn missing_file_token_passes_through() {
        let tmp = tempfile::tempdir().unwrap();
        let out = expand_file_refs("see @missing.md please", tmp.path());
        assert_eq!(out.text, "see @missing.md please");
        assert!(out.images.is_empty());
    }

    #[test]
    fn double_at_escapes_and_emails_pass_through() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("real.md"), "x").unwrap();
        let out = expand_file_refs("@@real.md and user@host.com and a lone @", tmp.path());
        assert_eq!(out.text, "@@real.md and user@host.com and a lone @");
    }

    #[test]
    fn oversized_file_is_capped_with_a_truncation_note() {
        let tmp = tempfile::tempdir().unwrap();
        let big = "x".repeat(MAX_FILE_REF_BYTES + 1000);
        std::fs::write(tmp.path().join("big.txt"), &big).unwrap();
        let out = expand_file_refs("@big.txt", tmp.path());
        assert!(out.text.contains("… [truncated at 50KB]"));
        assert!(out.text.len() < big.len() + 200);
    }

    #[test]
    fn resolve_handles_tilde_absolute_and_relative_paths() {
        let root = Path::new("/project");
        if let Some(home) = dirs::home_dir() {
            assert_eq!(resolve("~/notes.md", root), home.join("notes.md"));
        }
        assert_eq!(resolve("/etc/hosts", root), PathBuf::from("/etc/hosts"));
        assert_eq!(resolve("src/lib.rs", root), root.join("src/lib.rs"));
    }

    #[test]
    fn absolute_paths_resolve_as_is() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("abs.txt");
        std::fs::write(&file, "absolute").unwrap();
        let input = format!("@{}", file.display());
        let out = expand_file_refs(&input, Path::new("/elsewhere"));
        assert!(out.text.contains("absolute"));
    }

    #[test]
    fn image_extensions_attach_path_and_placeholder() {
        let tmp = tempfile::tempdir().unwrap();
        let shot = tmp.path().join("shot.png");
        std::fs::write(&shot, [0x89u8, b'P', b'N', b'G']).unwrap();
        let out = expand_file_refs("look at @shot.png", tmp.path());
        assert_eq!(out.text, "look at [image: shot.png]");
        assert_eq!(out.images.len(), 1);
        assert_eq!(
            out.images[0],
            shot.canonicalize().unwrap_or(shot),
            "image path is absolute"
        );
    }

    #[test]
    fn cap_lands_on_a_char_boundary() {
        // '€' is 3 bytes; a cap that falls mid-char must back off, not panic.
        let raw = "€€€";
        let (cut, truncated) = cap_bytes(raw, 4);
        assert_eq!(cut, "€");
        assert!(truncated);
        let (whole, truncated) = cap_bytes(raw, 9);
        assert_eq!(whole, raw);
        assert!(!truncated);
    }

    #[test]
    fn directories_pass_through() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("subdir")).unwrap();
        let out = expand_file_refs("@subdir", tmp.path());
        assert_eq!(out.text, "@subdir");
        assert!(out.images.is_empty());
    }

    #[test]
    fn embedded_fences_get_a_longer_fence() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("doc.md"), "```rust\ncode\n```\n").unwrap();
        let out = expand_file_refs("@doc.md", tmp.path());
        assert!(out.text.starts_with("````doc.md\n"), "got: {}", out.text);
        assert!(out.text.ends_with("````"), "got: {}", out.text);
    }

    #[test]
    fn multiline_input_preserves_whitespace() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "F").unwrap();
        let out = expand_file_refs("line one\n  @f.txt\nline three", tmp.path());
        assert_eq!(out.text, "line one\n  ```f.txt\nF\n```\nline three");
    }

    // --- the shared pipeline ---

    #[test]
    fn preprocess_expands_commands_then_file_refs() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("ctx.txt"), "context").unwrap();
        let commands = vec![CustomCommand {
            name: "with-ctx".into(),
            description: None,
            template: "Use @ctx.txt for $ARGUMENTS".into(),
            path: PathBuf::new(),
        }];
        let out = preprocess("/with-ctx the task", &commands, tmp.path());
        assert!(out.text.contains("context"), "got: {}", out.text);
        assert!(out.text.ends_with("for the task"), "got: {}", out.text);
        assert!(out.images.is_empty());
    }

    #[test]
    fn preprocess_passes_plain_prompts_through() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            preprocess("just a prompt", &[], tmp.path()),
            Preprocessed::text_only("just a prompt")
        );
        assert_eq!(
            preprocess("/unknown cmd", &[], tmp.path()),
            Preprocessed::text_only("/unknown cmd")
        );
    }

    #[test]
    fn preprocess_collects_image_attachments() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.png"), b"png").unwrap();
        std::fs::write(tmp.path().join("b.webp"), b"webp").unwrap();
        let out = preprocess("compare @a.png and @b.webp", &[], tmp.path());
        assert!(out.text.contains("[image: a.png]"));
        assert!(out.text.contains("[image: b.webp]"));
        assert_eq!(out.images.len(), 2);
    }
}
