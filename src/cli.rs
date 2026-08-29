//! Command-line argument parsing (`clap` derive).
//!
//! Flags per `docs/architecture.md` (CLI section) and `docs/modes.md`.

use std::path::PathBuf;

use clap::Parser;

use crate::config::Mode;

/// `--max-hours` value parser: a time limit the rest of the program can
/// actually turn into a `Duration`.
///
/// clap's default `f64` parser accepts `-1`, `nan` and `inf`, and every
/// consumer of the flag ends up at `Duration::from_secs_f64`, which panics on
/// all three. `wizard --max-hours -1 -p '…'` therefore died in the middle of a
/// run instead of being refused at the command line, which is where a bad
/// number belongs. Shared with `schedule.toml`, so the flag and the file agree
/// on what a limit is.
fn parse_max_hours(raw: &str) -> Result<f64, String> {
    let hours: f64 = raw
        .parse()
        .map_err(|_| format!("`{raw}` is not a number of hours"))?;
    crate::schedule::max_hours_duration(hours).map_err(|err| err.to_string())?;
    Ok(hours)
}

/// Wizard — your sovereign agent. Self-extending. Bring any model.
#[derive(Debug, Clone, Parser)]
#[command(name = "wizard", version = crate::update::display_version(), about, long_about = None)]
pub struct Cli {
    /// Personality mode: genie (interactive TUI) or sovereign (autonomous).
    #[arg(long, value_enum)]
    pub mode: Option<Mode>,

    /// Initial task. Pre-fills the first message in genie mode; the task to
    /// complete in sovereign / evolve mode.
    #[arg(short, long)]
    pub prompt: Option<String>,

    /// Self-extension mode: run the /evolve pipeline from the CLI.
    #[arg(long)]
    pub evolve: bool,

    /// Deep evolve (tier 2): rebuild Wizard's own source. Implies --evolve.
    #[arg(long, requires = "evolve")]
    pub deep: bool,

    /// Fork Wizard to your GitHub and print a one-line installer for your
    /// variant. Requires `gh` authenticated (`gh auth login`).
    #[arg(long)]
    pub publish: bool,

    /// Start in plan mode: the agent investigates with read-only tools and
    /// presents a plan via the exit_plan tool before executing. The TUI asks
    /// for approval; headless runs and the gateway auto-approve, giving a
    /// natural plan-then-execute turn.
    #[arg(long)]
    pub plan: bool,

    /// Start in omakase (chef's-choice) mode: plan mode where the agent
    /// explores read-only, decides the approach itself, and auto-approves its
    /// own plan — no interview, no review gate. Implies `--plan`.
    #[arg(long)]
    pub omakase: bool,

    /// Time limit in hours for a sovereign-mode run.
    #[arg(long, value_parser = parse_max_hours)]
    pub max_hours: Option<f64>,

    /// Max outer loop iterations for a sovereign-mode run.
    #[arg(long = "loop", value_name = "N")]
    pub loop_limit: Option<u32>,

    /// Quality gate: a command that must exit zero before a sovereign or
    /// continuous run is allowed to finish. Repeatable; every gate must pass.
    /// A failing gate is fed back to the model as another turn instead of
    /// being accepted, and a run that ends with one still failing exits 5
    /// however it ended. Merged with the `gates` config key and the project's
    /// own `.wizard/gates.toml`. Ignored in genie mode, where a human is
    /// watching. See `docs/modes.md`.
    #[arg(long = "gate", value_name = "COMMAND")]
    pub gate: Vec<String>,

    /// Run sovereign mode perpetually: keep working toward the goal,
    /// self-directing and self-improving, until stopped (loop-control
    /// `stop` or --max-hours). Implies --mode sovereign.
    #[arg(long)]
    pub continuous: bool,

    /// Internal: this headless run was dispatched from `/dashboard`, so it
    /// registers in the session registry and persists its terminal state for
    /// the dashboard to display.
    #[arg(long, hide = true)]
    pub bg: bool,

    /// Output format for headless (sovereign `-p`) runs: `text` streams
    /// human-readable output (default), `json` emits one final JSON summary
    /// object, `stream-json` emits one JSON object per line as events
    /// arrive. Ignored by the TUI and the gateway.
    #[arg(long, value_enum, default_value_t)]
    pub output_format: crate::output::OutputFormat,

    /// Project root override (defaults to the current directory).
    #[arg(long)]
    pub cwd: Option<PathBuf>,

    /// Resume the most recent session instead of starting fresh.
    #[arg(long)]
    pub resume: bool,

    /// Re-run the first-run onboarding wizard even if a config already exists.
    #[arg(long)]
    pub onboard: bool,

    /// Run the messaging gateway (e.g. Telegram) instead of the TUI. Reads the
    /// `[gateway]` section of config.toml; a long-running headless process.
    #[arg(long)]
    pub gateway: bool,

    /// Sign in to a provider account instead of starting the TUI: `xai`
    /// (SuperGrok) or `chatgpt` (Plus/Pro/Team). OAuth in the browser; tokens
    /// are stored under ~/.wizard/. A live xAI session is left alone; delete
    /// `~/.wizard/xai_oauth.json` (or run `/login xai force`) to replace it.
    #[arg(long, value_name = "PROVIDER")]
    pub login: Option<String>,

    /// Harness bundle directory (sets `$WIZARD_HARNESS_DIR`): per-component
    /// overrides for the compiled harness defaults — system_prompt.md,
    /// tool_descriptions/, skills/, subagents/. Missing files fall back to
    /// the baked defaults. Produce a bundle with `wizard harness export`.
    #[arg(long, value_name = "DIR")]
    pub harness_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Cli {
    /// Names of top-level flags set on this invocation that a self-contained
    /// subcommand (doctor, schedule, scheduler, fleet, usage, evolve,
    /// update, sync) would silently ignore. `--cwd` is honored everywhere and
    /// excluded.
    /// [`crate::run`] turns a non-empty result into a hard error rather than
    /// silently dropping the flags.
    pub fn ignored_top_level_flags(&self) -> Vec<&'static str> {
        let mut ignored = Vec::new();
        if self.mode.is_some() {
            ignored.push("--mode");
        }
        if self.prompt.is_some() {
            ignored.push("--prompt");
        }
        if self.evolve {
            ignored.push("--evolve");
        }
        if self.deep {
            ignored.push("--deep");
        }
        if self.publish {
            ignored.push("--publish");
        }
        if self.plan {
            ignored.push("--plan");
        }
        if self.omakase {
            ignored.push("--omakase");
        }
        if self.max_hours.is_some() {
            ignored.push("--max-hours");
        }
        if self.loop_limit.is_some() {
            ignored.push("--loop");
        }
        if !self.gate.is_empty() {
            ignored.push("--gate");
        }
        if self.continuous {
            ignored.push("--continuous");
        }
        if self.bg {
            ignored.push("--bg");
        }
        if self.output_format != crate::output::OutputFormat::Text {
            ignored.push("--output-format");
        }
        if self.resume {
            ignored.push("--resume");
        }
        if self.onboard {
            ignored.push("--onboard");
        }
        if self.gateway {
            ignored.push("--gateway");
        }
        if self.login.is_some() {
            ignored.push("--login");
        }
        ignored
    }
}

/// Top-level subcommands. Absent for the classic flag-driven modes.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum Command {
    /// Diagnose the environment: config, providers, MCP servers, tools,
    /// hooks, writable state dirs, checkpoints. Exits 0 when no check
    /// failed.
    Doctor {
        /// Also write a redacted bug-report bundle under
        /// ~/.wizard/bundles/doctor-<timestamp>/: the check report, the
        /// allowlist-redacted config, the newest session transcript, the
        /// usage and evolution logs, and the most recent debug logs. Secrets
        /// are stripped, but the transcript is your own text: read the bundle
        /// before you attach it to anything.
        #[arg(long)]
        bundle: bool,
    },

    /// Manage scheduled runs (~/.wizard/schedule.toml): cron entries the
    /// `wizard scheduler` daemon fires as headless wizard runs.
    Schedule {
        #[command(subcommand)]
        cmd: ScheduleCmd,
    },

    /// Run the scheduler daemon in the foreground: reload
    /// ~/.wizard/schedule.toml each pass and fire due entries as headless
    /// wizard child processes. With a subcommand, manage that daemon as a
    /// background service instead (`wizard scheduler install`); see
    /// docs/services.md.
    Scheduler {
        #[command(subcommand)]
        cmd: Option<crate::platform::service::ServiceCmd>,
    },

    /// Set the messaging gateway up (`setup`), or manage it as a background
    /// service (`install`, `start`, `stop`, `restart`, `status`, `logs`,
    /// `uninstall`). The gateway itself still runs in the foreground as
    /// `wizard --gateway`; the service verbs are how you keep it running
    /// without a terminal.
    ///
    /// `install` captures the current directory as the gateway's project root
    /// and the absolute path of this binary as ExecStart, and moves a bot
    /// token that only exists in your shell into ~/.wizard/credentials.toml
    /// (0600), never into the unit, which is world-readable. See
    /// docs/services.md.
    Gateway {
        #[command(subcommand)]
        cmd: GatewayCmd,
    },

    /// Not in this build: `wizard fleet` needs the `fleet` feature, which is
    /// on by default. Rebuild with `--features fleet`, or install a stock
    /// release binary.
    //
    // The doc comment above is what this row says when *nothing has
    // registered a fleet*, and it is the only time it is printed: `command`
    // below replaces it with `Entrypoint::about` on a build that has the
    // plugin, and hides the row entirely on one that does not. The present
    // tense belongs to whoever implements the surface — see
    // `crate::entrypoint::Subcommand::about`.
    Fleet {
        #[command(subcommand)]
        cmd: FleetCmd,
    },

    /// Open the agent dashboard: every running Wizard session on the machine.
    /// Dispatch background sessions, watch their state, peek their output, and
    /// stop them. Same view as `/dashboard` inside a session.
    Agents,

    /// Harness bundle tooling: export the compiled harness defaults
    /// (system prompt, tool descriptions, skills, subagents) as an editable
    /// bundle for external harness-evolution loops. Load one with
    /// `--harness-dir` / `$WIZARD_HARNESS_DIR`.
    Harness {
        #[command(subcommand)]
        cmd: HarnessCmd,
    },

    /// Serve Wizard's native tools over stdio as an MCP server (JSON-RPC),
    /// so any MCP client (Claude Code, Cursor, another Wizard) can call them.
    /// Self-contained: no config, no LLM. Runs until stdin closes.
    McpServe {
        /// Also advertise agent-authored scripted tools from ~/.wizard/tools/.
        #[arg(long)]
        scripted: bool,
    },

    /// Not in this build: `wizard acp` needs the `acp` feature, which is on by
    /// default. Rebuild with `--features acp`, or install a stock release
    /// binary.
    //
    // The absent text only. See the `Fleet` variant above for why.
    Acp,

    /// Open the GUI: an iced window (chat list, streaming conversation, git
    /// rail) over the same agent core as the TUI. One process — no webview, no
    /// HTTP, no port. Needs a build with `--features native`; chats are built
    /// lazily, so it opens fine without a reachable provider.
    /// See docs/native-gui.md.
    //
    // The one plugin-owned subcommand whose row stays in `--help` when
    // nothing has registered it, and the doc comment above is that row —
    // written for a reader who does *not* have a window, which is why it ends
    // by naming the flag. `native` is off by default and the window ships as
    // its own release asset, so on a stock build this is the common case
    // rather than a misconfiguration, and dropping the row would be the only
    // way most people never learn the window exists. A build that has one
    // gets `Entrypoint::about` instead, which does not tell the reader to go
    // and get something already in front of them.
    Gui {
        /// Accepted and ignored. `wizard gui --native` was how you asked for
        /// the window back when a plain `wizard gui` served a browser page
        /// instead; the page is gone, so the flag now names the only thing
        /// there is. Kept — rather than removed — because it is written into
        /// every existing alias, script and README that mentions the window,
        /// and a hard clap error is a worse answer than doing what was meant.
        #[arg(long, hide = true)]
        native: bool,
    },

    /// Roll up ~/.wizard/usage.jsonl: turns, tokens, and estimated cost per
    /// project and per provider. Self-contained; never loads config.
    Usage {
        /// Only include turns from the last N days (e.g. `--since 7d`).
        #[arg(long, value_name = "DAYS")]
        since: Option<String>,
    },

    /// Inspect and roll back self-extensions recorded in
    /// ~/.wizard/evolution.jsonl.
    Evolve {
        #[command(subcommand)]
        cmd: EvolveCmd,
    },

    /// Update Wizard in place: download the latest release binary from GitHub,
    /// verify its checksum against `checksums.txt`, and swap it in atomically
    /// (the previous binary is kept as `<name>.bak`). Self-contained: never
    /// loads config or triggers onboarding.
    Update {
        /// Report whether a newer release exists without installing anything.
        #[arg(long)]
        check: bool,

        /// Install this exact tag (e.g. `v0.5.0`) instead of the latest.
        #[arg(long, value_name = "TAG")]
        to: Option<String>,

        /// Reinstall even when the running version is already up to date.
        #[arg(long)]
        force: bool,

        /// Restore the previous binary from the pre-update `<name>.bak` backup.
        #[arg(long)]
        rollback: bool,
    },

    /// Sync config and skills across machines: pack the portable parts of
    /// ~/.wizard into a signed bundle, pull and verify one from a file or
    /// URL. Self-contained: never loads config or triggers onboarding.
    Sync {
        #[command(subcommand)]
        cmd: SyncCmd,
    },

    /// Install skills and Lua tools from the Wizard registry: search the
    /// published index, install an entry once its checksum verifies, and
    /// bring installed entries up to the published version. Self-contained:
    /// no config load, no onboarding, no LLM.
    ///
    /// Installs land beside the skills that ship inside the binary
    /// (~/.wizard/skills/<name>/SKILL.md, ~/.wizard/tools/<name>.lua), each
    /// with a receipt recording the author, version, checksum, source URL and
    /// the standard library the entry was granted. See docs/market.md.
    Skills {
        #[command(subcommand)]
        cmd: SkillsCmd,
    },

    /// Resume a conversation. Bare, this is the subcommand spelling of
    /// `wizard --resume`: reopen the most recent Wizard session recorded
    /// against this project. With `--claude` it takes the conversation from
    /// Claude Code instead, converting the chosen history into a Wizard
    /// session and continuing it here.
    ///
    /// The Claude Code side is strictly read-only: ~/.claude is another
    /// program's live state and nothing here writes a byte of it.
    Resume {
        /// Take the conversation from Claude Code (~/.claude/projects/)
        /// rather than from Wizard's own sessions. With no other flag this
        /// lists what Claude Code recorded for this directory and asks which
        /// one to continue.
        #[arg(long)]
        claude: bool,

        /// Take this Claude Code session instead of asking. Accepts the
        /// session id `--claude --list` prints, or a unique prefix of one.
        #[arg(long, value_name = "ID", requires = "claude")]
        session: Option<String>,

        /// Walk back from this line instead of the tip Claude Code would
        /// resume from. A Claude Code transcript is a DAG, not a list: an
        /// edited or rewound prompt appends a second child under the same
        /// parent, so a session can hold several conversations and this picks
        /// which one. `--claude --list` reports how many times a session
        /// forked.
        #[arg(long, value_name = "UUID", requires = "claude")]
        leaf: Option<String>,

        /// List the Claude Code sessions for this directory and exit without
        /// converting anything.
        #[arg(long, requires = "claude")]
        list: bool,
    },

    /// Not in this build: `wizard peers` needs the `mesh` feature, which is on
    /// by default. Rebuild with `--features mesh`, or install a stock release
    /// binary.
    //
    // The absent text only — see the `Fleet` variant above. What `wizard
    // peers` *is* lives in `plugins::mesh::cli::SUMMARY`, which is also the
    // first paragraph of `wizard peers --help`; the two used to be separate
    // strings in separate crates' worth of code and had already drifted.
    //
    // Everything after `peers` crosses unparsed, because the tree behind it is
    // the mesh plugin's and one of its arguments is a type core must not name:
    // `trust` takes the peer store's own three-state `clap::ValueEnum`, derived
    // on the store's type precisely so a second spelling here cannot drift into
    // a fourth state. See `crate::entrypoint::Subcommand`.
    //
    // `disable_help_flag` is load-bearing rather than tidy. Without it clap
    // answers `wizard peers --help` *here*, with a usage line reading
    // `wizard peers [ARGS]...` and no mention of the eight subcommands that
    // actually exist. Help for this tree is the plugin's to print, and these
    // three lines of rationale are `//` rather than `///` for the same reason:
    // an argument about where a boundary goes is not what somebody typing
    // `--help` came for.
    #[command(disable_help_flag = true)]
    Peers {
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
        args: Vec<String>,
    },

    /// Report the plugin set this binary was built with: what is compiled in,
    /// which backend each plugin runs on, what it registered, what it was
    /// granted, and what a rebuild would add.
    ///
    /// Read-only and self-contained: no config, no LLM, no network. See
    /// docs/plugins.md.
    //
    // A core subcommand rather than a plugin-owned one, and the distinction is
    // not a technicality. Every other row in this enum whose body ships in a
    // plugin is owned by *one* plugin, which registers an `Entrypoint` under a
    // name core looks up. This one is about all of them at once and about the
    // ones that are absent, so there is no plugin that could own it: on the
    // build where it matters most — `--no-default-features`, nothing loaded —
    // a plugin-owned `wizard plugin` would itself be missing. It reads the same
    // registry the lookups read (`crate::entrypoint::description`) rather than
    // holding a second one.
    Plugin {
        #[command(subcommand)]
        cmd: Option<PluginCmd>,
    },

    /// Install and enable the OS dependencies the `computer` tool needs for
    /// desktop control ("computer use"), then print what is left to do by
    /// hand.
    ///
    /// On Linux this installs the X11/Wayland input and capture helpers; on
    /// macOS nothing can be installed for you, so it reports which of
    /// Accessibility and Screen Recording this binary still needs granted and
    /// how to grant them. It touches no config and starts no agent.
    /// See docs/computer-use.md.
    DesktopSetup,
}

/// `wizard plugin` subcommands.
///
/// Four verbs over one question — "what is in this binary" — split by who is
/// asking. `list` and `show` are for somebody holding a build they did not
/// make; `missing` and `profiles` are for somebody about to make one.
///
/// `--json` is per-verb rather than a top-level flag because clap puts a
/// top-level flag *before* the subcommand (`wizard plugin --json list`), which
/// is the wrong way round for something people will type from memory.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum PluginCmd {
    /// One row per plugin the kernel loaded: backend, source, and what it
    /// registered. The default when no verb is given.
    List {
        /// Emit JSON instead of a table.
        #[arg(long)]
        json: bool,
    },

    /// One plugin in full: version, backend, the capabilities it declared and
    /// what each one grants, and every tool, command, provider and entrypoint
    /// it registered. Exits 1 if this build does not have it.
    Show {
        /// The plugin's name, as `wizard plugin list` prints it.
        name: String,
        /// Emit JSON instead of a report.
        #[arg(long)]
        json: bool,
    },

    /// Plugin features this build does not have, and the flag that brings each
    /// one back.
    Missing {
        /// Emit JSON instead of a listing.
        #[arg(long)]
        json: bool,
    },

    /// The named build profiles, what each is for, and which one this binary
    /// is.
    Profiles {
        /// Emit JSON instead of a listing.
        #[arg(long)]
        json: bool,
    },
}

/// `wizard skills` subcommands. Self-contained like `sync`: they read the
/// published index (cached under ~/.wizard/registry, so search keeps working
/// offline) and write only under ~/.wizard/skills and ~/.wizard/tools.
///
/// Installing a tool is running its author's code, so the trust decision is
/// part of this surface rather than a detail behind it.
/// [`crate::registry_client::decide_trust`] refuses an entry that declares
/// capabilities unless a human said yes to the exact author, version,
/// checksum and capability list that
/// [`crate::registry_client::grant_prompt`] prints, and the entry then runs
/// under [`crate::tools::lua::Stdlib::Full`] rather than the sandbox.
/// `--grant-full-stdlib` is that yes, given up front; it is spelled out
/// rather than called `--yes` because a flag in somebody's shell history has
/// to say what it accepted, and it still prints the grant so what was handed
/// over is on the screen and not only in the flag.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum SkillsCmd {
    /// Search the published index, best match first. Every term has to match
    /// something (name, tag, author or description), so extra terms narrow
    /// rather than widen. Works offline against the cached index.
    Search {
        /// Terms to match.
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,

        /// Only skills: markdown listed in the system-prompt index; the body
        /// is read from disk when the skill matches.
        #[arg(long, conflicts_with = "tools")]
        skills: bool,

        /// Only tools: LuaJIT scripts the model can call.
        #[arg(long)]
        tools: bool,
    },

    /// Install one entry by name, after verifying its published checksum.
    ///
    /// Refuses rather than guessing when a name is published as both a skill
    /// and a tool; say which with `--skills` or `--tools`. Refuses a name that
    /// a built-in tool or bundled skill already has, and never overwrites
    /// something you wrote yourself.
    Install {
        /// Entry name as published (`wizard skills search <name>` shows it).
        name: String,

        /// Resolve the name as a skill.
        #[arg(long, conflicts_with = "tools")]
        skills: bool,

        /// Resolve the name as a tool.
        #[arg(long)]
        tools: bool,

        /// Accept, before being asked, that this entry's author may run code
        /// on this machine with your privileges under the full LuaJIT
        /// standard library (os.execute, io.open, os.getenv). Only entries
        /// whose manifest declares capabilities are affected; everything else
        /// installs sandboxed either way. The grant is all or nothing and is
        /// still printed in full.
        #[arg(long)]
        grant_full_stdlib: bool,
    },

    /// Bring registry installs up to the published version. A new version is
    /// never taken silently when the name changed hands or when the install
    /// holds a full-stdlib grant: those are reported and left alone. Exits
    /// non-zero only when an update genuinely failed.
    Update {
        /// Only this entry. Default: everything installed from the registry.
        name: Option<String>,

        /// Re-grant the full LuaJIT standard library for entries that need
        /// one, without being asked per entry. Same meaning, and same
        /// consequences, as on `install`.
        #[arg(long)]
        grant_full_stdlib: bool,
    },

    /// List what is installed from the registry: author, version, and which
    /// standard library each entry runs under. Read from the receipts beside
    /// the installs themselves, so deleting an install deletes its record.
    List,
}

/// `wizard sync` subcommands. Self-contained like update: no config load
/// (pull reads `[sync].source` from config.toml directly), no onboarding,
/// no LLM. Bundles are ed25519-signed; trust is pinned on first use in
/// `~/.wizard/sync/trusted_keys` (compare fingerprints via `wizard sync key`).
#[derive(Debug, Clone, clap::Subcommand)]
pub enum SyncCmd {
    /// Create a signed bundle of portable ~/.wizard state: config.toml,
    /// mcp.toml, system_prompt.md, and the skills/, commands/, subagents/,
    /// and tools/ directories. Credentials stay out unless explicitly
    /// included.
    Pack {
        /// Output path (default: `wizard-sync-<YYYYMMDD>.tar.gz` in the
        /// current directory).
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,

        /// Also pack credentials.toml and xai_oauth.json. The bundle then
        /// contains API keys — transfer it privately. The bundle file is
        /// written with 0600 permissions.
        #[arg(long)]
        include_credentials: bool,
    },

    /// Fetch, verify, and apply a bundle (file path or http(s) URL). Falls
    /// back to `[sync].source` in config.toml when no source is given.
    /// Additive only: replaced files are backed up under
    /// ~/.wizard/sync/backups/, nothing is deleted.
    Pull {
        /// Bundle to pull: a local file path (`~` expands) or an http(s) URL.
        source: Option<String>,

        /// Verify the bundle and show what would change without writing
        /// anything.
        #[arg(long)]
        dry_run: bool,
    },

    /// Print this machine's sync public key and fingerprint (generating the
    /// keypair on first use). Compare the fingerprint against what `pull`
    /// reports on the other machine.
    Key,
}

/// `wizard evolve` subcommands. Self-contained: they read
/// `~/.wizard/evolution.jsonl` and touch the recorded artifacts directly —
/// no config load, no LLM.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum EvolveCmd {
    /// List recorded evolutions, most recent first (#1 is the newest).
    List,

    /// Undo evolution #N from `wizard evolve list`: delete the files a
    /// runtime evolution created, or restore the `.prev` binary for a deep
    /// one. Refuses when the artifacts are already gone.
    Undo {
        /// Entry number as shown by `wizard evolve list`.
        n: usize,
    },
}

/// `wizard gateway` subcommands: one gateway-specific verb plus the shared
/// service verbs.
///
/// The service verbs are *flattened* rather than nested under a `service`
/// word, so `wizard gateway install` keeps meaning exactly what it always
/// meant — the spelling in every existing doc, unit and shell history — while
/// `setup` sits beside it. Flattening is also what keeps `setup` off
/// [`crate::platform::service::ServiceCmd`], which is defined once and
/// answered by `wizard scheduler` too: a scheduler has no bot to talk to and
/// no chat id to discover, so a `Setup` variant there would be a verb that
/// exists only to be rejected.
#[derive(Debug, Clone, Copy, clap::Subcommand)]
pub enum GatewayCmd {
    /// Guided first run: find or ask for a bot token, check it against
    /// Telegram, discover your chat id by having you message the bot, and —
    /// with your say-so — write that id into gateway.allowed_chat_ids.
    /// Interactive: it needs a terminal and refuses without one.
    Setup,
    /// The shared service verbs (`install`, `start`, `stop`, `restart`,
    /// `status`, `logs`, `uninstall`).
    #[command(flatten)]
    Service(crate::platform::service::ServiceCmd),
}

/// `wizard harness` subcommands. Self-contained: no config load,
/// no onboarding, no LLM.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum HarnessCmd {
    /// Write the compiled harness defaults into `dir` as a bundle:
    /// `system_prompt.md`, `tool_descriptions/<tool>.md`,
    /// `skills/<name>/SKILL.md`, `subagents/<name>.toml`, plus a generated
    /// `HARNESS.md` describing each component.
    Export {
        /// Target directory (created if missing; existing files overwritten).
        dir: PathBuf,
    },
}

/// `wizard fleet` subcommands. `run` loads config (the coordinator drives a
/// real agent for planning and synthesis); `status` and `stop` only touch
/// the project's `.wizard/fleet/` directory.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum FleetCmd {
    /// Plan the mission, spawn up to N parallel workers over git worktrees,
    /// supervise them, then synthesize (merge the fleet branches).
    Run {
        /// Number of parallel workers.
        #[arg(short = 'n', long = "workers", value_name = "N")]
        n: usize,

        /// Mission prompt, decomposed into independent tasks by a planning
        /// turn.
        #[arg(short, long)]
        prompt: String,
    },

    /// Show the fleet state: mission, status, and a per-task table.
    Status,

    /// Ask a running fleet to wind down (writes the stop sentinel; the
    /// coordinator kills its workers on the next supervision tick).
    Stop,
}

/// `wizard schedule` subcommands. Self-contained:
/// they edit `~/.wizard/schedule.toml` directly and never load
/// `~/.wizard/config.toml` or trigger onboarding.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum ScheduleCmd {
    /// Add an entry; validates the cron expression and prints the next
    /// fire time.
    Add {
        /// Unique entry name; `[a-zA-Z0-9_-]+` only.
        name: String,

        /// Standard 5-field cron expression (minute hour day month weekday),
        /// evaluated in local time.
        #[arg(long)]
        cron: String,

        /// Task prompt handed to the spawned headless wizard run.
        #[arg(long)]
        prompt: String,

        /// Directory the run executes in (must exist).
        #[arg(long)]
        cwd: PathBuf,

        /// Wall-clock cap in hours for the spawned run.
        #[arg(long, value_parser = parse_max_hours)]
        max_hours: Option<f64>,

        /// Run mode for the job: `sovereign` (default) or `continuous`.
        #[arg(long, default_value = "sovereign")]
        mode: String,
    },

    /// List entries with their next fire times.
    List,

    /// Remove an entry by name.
    Remove {
        /// Entry name as shown by `wizard schedule list`.
        name: String,
    },

    /// Enable a disabled entry (the daemon fires it again).
    Enable {
        /// Entry name as shown by `wizard schedule list`.
        name: String,
    },

    /// Disable an entry without removing it (kept in the file, never fired).
    Disable {
        /// Entry name as shown by `wizard schedule list`.
        name: String,
    },

    /// Run one entry's job immediately in the foreground (same child
    /// command the daemon would spawn); exits with the child's exit code.
    Run {
        /// Entry name as shown by `wizard schedule list`.
        name: String,
    },
}

/* ---------------------------------------------------------------------- */
/* Help, built from what this build actually has                          */
/* ---------------------------------------------------------------------- */

/// What `--help` does with a subcommand nothing has registered.
///
/// Two answers, because the two cases are genuinely different and the
/// difference is already written down in [`crate::entrypoint::absent`]: `acp`,
/// `fleet` and `mesh` are on by default and in every published binary, so a
/// build without one is a build somebody made that way on purpose, and the
/// row is noise. `native` is off by default and the window ships as its own
/// release asset, so a build without it is the normal case and the row is how
/// most people find out there is a window at all.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WhenAbsent {
    /// Drop the row. The `clap` variant still parses, so `wizard acp` still
    /// answers — with [`crate::entrypoint::absent`], which names the flag.
    Drop,
    /// Keep the row, with core's own text. That text is the doc comment on
    /// the variant, so this arm does nothing at all: it is here to be named
    /// at the one call site that means it.
    Keep,
}

/// One row per CLI subcommand whose body ships in a plugin: what it answers
/// to, what the plugin says it is on this build, and what to do when nothing
/// answers.
///
/// Core enumerating its four plugin-owned subcommands, which it already does
/// twice — once as `clap` variants above, once as dispatch arms in
/// [`crate::run`] — and for the same reason: parsing `wizard fleet run -n 3`
/// is core's job whether or not a fleet is compiled in, so the variants stay,
/// and something has to join each one to the lookup that finds its body. The
/// argument type is part of that join ([`crate::entrypoint::installed`] is a
/// `TypeId` downcast), which is why this is four written-out lookups and not
/// a loop over a table of names.
fn plugin_subcommands() -> [(&'static str, Option<&'static str>, WhenAbsent); 4] {
    use crate::entrypoint::{self, installed, installed_subcommand};

    [
        (
            entrypoint::GUI,
            installed::<crate::config::Config>(entrypoint::GUI).map(|entry| entry.about()),
            WhenAbsent::Keep,
        ),
        (
            entrypoint::ACP,
            installed::<crate::config::Config>(entrypoint::ACP).map(|entry| entry.about()),
            WhenAbsent::Drop,
        ),
        (
            entrypoint::FLEET,
            installed::<FleetCmd>(entrypoint::FLEET).map(|entry| entry.about()),
            WhenAbsent::Drop,
        ),
        (
            entrypoint::PEERS,
            installed_subcommand(entrypoint::PEERS).map(|entry| entry.about()),
            WhenAbsent::Drop,
        ),
    ]
}

/// The `clap` command this binary actually has, as opposed to the one the
/// derive describes.
///
/// The derive cannot know: whether `wizard acp` does anything is a property of
/// the plugin set, which is a runtime lookup. So the four rows above are
/// folded in here — the description a registered surface gave itself replaces
/// core's, and a row with nothing behind it is dropped or kept per its policy.
///
/// Everything else in the tree keeps using `Cli::parse` / `Cli::try_parse_from`
/// on the derived command, because everything else is *parsing*, and parsing
/// does not change: `wizard acp` is accepted on every build and answers with a
/// sentence rather than a usage error. This is only the listing.
pub fn command() -> clap::Command {
    use clap::CommandFactory;

    let mut cmd = Cli::command();
    for (name, about, absent) in plugin_subcommands() {
        cmd = cmd.mut_subcommand(name, |sub| match (about, absent) {
            (Some(about), _) => sub.about(about),
            (None, WhenAbsent::Drop) => sub.hide(true),
            (None, WhenAbsent::Keep) => sub,
        });
    }
    cmd
}

/// Parse this process's arguments, with help that reflects the plugin set.
///
/// # Why this is not just `Cli::parse()`
///
/// Two things the derived command cannot do, and one thing it must not be
/// made to do.
///
/// **The listing.** [`command`] needs the plugin set, and reaching the plugin
/// set means building the process kernel. Doing that on every invocation
/// would be wrong rather than merely wasteful: [`crate::plugins::boot`] sets
/// the project root *before* the kernel is built, so that a sandboxed
/// plugin's file helpers are confined to `--cwd` and not to wherever the
/// process happened to start. A kernel forced into existence at parse time
/// would be confined to the wrong directory, which is worse than not being
/// confined at all because it looks like it is working. So the first parse is
/// the plain derived one and the plugin-aware command is built only on the
/// path that prints help and exits — a path with no `--cwd` left to honour.
/// `clap` decides which path that is, not a scan of the argument list for
/// `-h`: `wizard -p help` is a prompt, not a help request, and the cost of
/// guessing wrong is the confinement above.
///
/// **`wizard help <plugin-subcommand>`.** `wizard peers --help` already
/// reaches the plugin — core's variant is `trailing_var_arg` with
/// `disable_help_flag`, so the flag crosses unparsed with everything else and
/// the plugin's own `clap::Parser` prints its own tree. `wizard help peers`
/// is `clap`'s help *subcommand*, which `disable_help_flag` does not reach, so
/// it printed core's `wizard peers [ARGS]...` usage line instead — a real
/// answer to the wrong question.
///
/// It is rewritten here, into the spelling that already works. The
/// alternative was to teach `clap` to route it, which means core holding a
/// `clap::Command` for the plugin's tree: either mirrored, which is what
/// [`crate::entrypoint::Subcommand`] exists to refuse (`trust` takes the peer
/// store's own `ValueEnum` and a second spelling of it can drift into a
/// fourth trust state), or handed over by the plugin, which puts `clap` in a
/// kernel service signature and still ends with `clap` rendering a *second*
/// copy of the help the plugin renders itself. Two spellings of one request
/// should not print two documents, and the way to guarantee that is for one
/// spelling to become the other.
///
/// The rewrite fires only when the word after `help` is a name some plugin
/// registered a [`crate::entrypoint::Subcommand`] under. `wizard help doctor`,
/// `wizard help` and `wizard help nonsense` are `clap`'s, exactly as before.
pub fn parse() -> Cli {
    use clap::CommandFactory;

    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    match Cli::command().try_get_matches_from(argv.clone()) {
        Ok(matches) => from_matches(&matches),
        Err(err) if displays_help(&err) => {
            match forward_help_to_plugin(&argv) {
                // `wizard help peers` -> `wizard peers --help`, which parses
                // cleanly (the flag is just another trailing argument) and
                // reaches the plugin through the ordinary dispatch arm.
                Some(rewritten) => from_matches(&command().get_matches_from(rewritten)),
                None => from_matches(&command().get_matches_from(argv)),
            }
        }
        Err(err) => err.exit(),
    }
}

/// Whether this `clap` error is "print help and exit 0" rather than "the
/// arguments are wrong".
///
/// The help paths are the only ones worth building the plugin-aware command
/// for, and they are also the only ones where doing so is free: they exit
/// without running anything, so nothing downstream depends on the kernel not
/// having been built yet.
fn displays_help(err: &clap::Error) -> bool {
    use clap::error::ErrorKind;
    matches!(
        err.kind(),
        ErrorKind::DisplayHelp | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    )
}

/// `["wizard", "help", "peers", ..]` becomes `["wizard", "peers", "--help", ..]`
/// when a plugin has registered a subcommand tree under that name, and
/// [`None`] otherwise.
fn forward_help_to_plugin(argv: &[std::ffi::OsString]) -> Option<Vec<std::ffi::OsString>> {
    let mut rest = argv.iter();
    let binary = rest.next()?;
    if rest.next()? != "help" {
        return None;
    }
    let name = rest.next()?;
    crate::entrypoint::installed_subcommand(name.to_str()?)?;

    let mut rewritten = vec![binary.clone(), name.clone(), "--help".into()];
    rewritten.extend(rest.cloned());
    Some(rewritten)
}

/// `Cli` out of matches `clap` has already accepted.
///
/// The `expect` is not optimism: `ArgMatches` came from this very
/// `clap::Command`, so a failure here is a derive bug rather than user input,
/// and `clap`'s own `Parser::parse` does the same thing for the same reason.
fn from_matches(matches: &clap::ArgMatches) -> Cli {
    use clap::FromArgMatches;
    Cli::from_arg_matches(matches).expect("clap accepted these arguments")
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::*;
    use crate::config::Mode;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("wizard").chain(args.iter().copied()))
    }

    #[test]
    fn clap_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn defaults_when_no_args() {
        let cli = parse(&[]).expect("bare invocation parses");
        assert_eq!(cli.mode, None);
        assert_eq!(cli.prompt, None);
        assert!(!cli.evolve);
        assert!(!cli.deep);
        assert!(!cli.plan);
        assert_eq!(cli.max_hours, None);
        assert_eq!(cli.loop_limit, None);
        assert!(cli.gate.is_empty());
        assert!(!cli.continuous);
        assert_eq!(cli.output_format, crate::output::OutputFormat::Text);
        assert_eq!(cli.cwd, None);
        assert!(!cli.resume);
        assert!(!cli.onboard);
        assert!(!cli.gateway);
        assert_eq!(cli.login, None);
        assert!(cli.command.is_none(), "bare wizard has no subcommand");
    }

    #[test]
    fn login_flag_takes_a_provider() {
        let cli = parse(&["--login", "xai"]).expect("--login xai parses");
        assert_eq!(cli.login.as_deref(), Some("xai"));

        let err = parse(&["--login"]).expect_err("--login without a provider is rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn parses_all_documented_flags() {
        let cli = parse(&[
            "--mode",
            "sovereign",
            "-p",
            "add tests",
            "--plan",
            "--max-hours",
            "1.5",
            "--loop",
            "10",
            "--continuous",
            "--cwd",
            "/tmp/project",
            "--resume",
            "--onboard",
            "--gateway",
        ])
        .expect("full flag set parses");
        assert_eq!(cli.mode, Some(Mode::Sovereign));
        assert_eq!(cli.prompt.as_deref(), Some("add tests"));
        assert!(cli.plan);
        assert_eq!(cli.max_hours, Some(1.5));
        assert_eq!(cli.loop_limit, Some(10));
        assert!(cli.continuous);
        assert_eq!(
            cli.cwd.as_deref(),
            Some(std::path::Path::new("/tmp/project"))
        );
        assert!(cli.resume);
        assert!(cli.onboard);
        assert!(cli.gateway);
    }

    /// A time limit that cannot become a `Duration` is refused at the command
    /// line, not carried into the run.
    ///
    /// `--max-hours` had clap's stock `f64` parser, which happily produces
    /// `-1`, `nan` and `inf`; every consumer then calls
    /// `Duration::from_secs_f64`, which panics on all three. The failure
    /// landed after the config was loaded and the agent was built, which is a
    /// long way from the typo that caused it.
    #[test]
    fn an_impossible_max_hours_is_refused_by_the_parser() {
        for bad in ["-1", "0", "nan", "inf", "-inf", "1e9", "not-a-number"] {
            let arg = format!("--max-hours={bad}");
            assert!(parse(&[arg.as_str()]).is_err(), "{arg} must be refused");
        }
        let cli = parse(&["--max-hours=1.5"]).expect("a real limit still parses");
        assert_eq!(cli.max_hours, Some(1.5));
    }

    /// `--gate` accumulates, and keeps whole command lines intact.
    ///
    /// The two ways to get this wrong are both silent: a non-repeatable flag
    /// would keep only the last gate, and a space-splitting one would turn
    /// `cargo test --lib` into three gates, two of which cannot run. Either
    /// way the run reports a clean pass on gates that never happened.
    #[test]
    fn the_gate_flag_repeats_and_keeps_its_command_line_whole() {
        let cli = parse(&["--gate", "cargo fmt --check", "--gate", "cargo test --lib"])
            .expect("repeated gates parse");
        assert_eq!(cli.gate, vec!["cargo fmt --check", "cargo test --lib"]);
        assert!(
            cli.ignored_top_level_flags().contains(&"--gate"),
            "a subcommand that cannot honour a gate must say so, not drop it"
        );
    }

    #[test]
    fn long_prompt_flag_works() {
        let cli = parse(&["--prompt", "task"]).expect("long form parses");
        assert_eq!(cli.prompt.as_deref(), Some("task"));
    }

    #[test]
    fn evolve_flags() {
        let cli = parse(&["--evolve", "-p", "add a skill"]).expect("evolve parses");
        assert!(cli.evolve);
        assert!(!cli.deep);

        let cli = parse(&["--evolve", "--deep", "-p", "new panel"]).expect("deep evolve parses");
        assert!(cli.evolve);
        assert!(cli.deep);
    }

    #[test]
    fn deep_requires_evolve() {
        let err = parse(&["--deep"]).expect_err("--deep alone must be rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn output_format_parses_all_values() {
        use crate::output::OutputFormat;
        for (raw, expected) in [
            ("text", OutputFormat::Text),
            ("json", OutputFormat::Json),
            ("stream-json", OutputFormat::StreamJson),
        ] {
            let cli = parse(&["--output-format", raw]).expect("format parses");
            assert_eq!(cli.output_format, expected);
        }
        let err = parse(&["--output-format", "yaml"]).expect_err("unknown format rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn rejects_unknown_mode() {
        let err = parse(&["--mode", "warlock"]).expect_err("unknown mode must be rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn doctor_parses_as_a_subcommand() {
        let cli = parse(&["doctor"]).expect("doctor parses");
        assert!(matches!(
            cli.command,
            Some(Command::Doctor { bundle: false })
        ));
    }

    #[test]
    fn doctor_bundle_flag_parses() {
        // The bundle entry point is advertised as `wizard doctor --bundle` in
        // the module docs and the README; before this field existed clap
        // rejected the flag with "unexpected argument" and the whole feature
        // was unreachable from the CLI.
        let cli = parse(&["doctor", "--bundle"]).expect("doctor --bundle parses");
        assert!(matches!(
            cli.command,
            Some(Command::Doctor { bundle: true })
        ));
    }

    #[test]
    fn mcp_serve_parses_as_a_subcommand() {
        let cli = parse(&["mcp-serve"]).expect("mcp-serve parses");
        assert!(matches!(
            cli.command,
            Some(Command::McpServe { scripted: false })
        ));
        let cli = parse(&["mcp-serve", "--scripted"]).expect("--scripted parses");
        assert!(matches!(
            cli.command,
            Some(Command::McpServe { scripted: true })
        ));
    }

    #[test]
    fn acp_parses_as_a_subcommand() {
        let cli = parse(&["acp"]).expect("acp parses");
        assert!(matches!(cli.command, Some(Command::Acp)));
    }

    #[test]
    fn update_parses_as_a_subcommand() {
        let cli = parse(&["update"]).expect("update parses");
        assert!(matches!(
            cli.command,
            Some(Command::Update {
                check: false,
                to: None,
                force: false,
                rollback: false,
            })
        ));

        let cli = parse(&[
            "update",
            "--check",
            "--to",
            "v0.5.0",
            "--force",
            "--rollback",
        ])
        .expect("update flags parse");
        let Some(Command::Update {
            check,
            to,
            force,
            rollback,
        }) = cli.command
        else {
            panic!("expected update");
        };
        assert!(check);
        assert_eq!(to.as_deref(), Some("v0.5.0"));
        assert!(force);
        assert!(rollback);
    }

    #[test]
    fn sync_subcommands_parse() {
        let cli = parse(&["sync", "pack"]).expect("sync pack parses");
        assert!(matches!(
            cli.command,
            Some(Command::Sync {
                cmd: SyncCmd::Pack {
                    out: None,
                    include_credentials: false,
                }
            })
        ));

        let cli = parse(&[
            "sync",
            "pack",
            "--out",
            "/tmp/b.tar.gz",
            "--include-credentials",
        ])
        .expect("sync pack flags parse");
        let Some(Command::Sync {
            cmd:
                SyncCmd::Pack {
                    out,
                    include_credentials,
                },
        }) = cli.command
        else {
            panic!("expected sync pack");
        };
        assert_eq!(out, Some(PathBuf::from("/tmp/b.tar.gz")));
        assert!(include_credentials);

        let cli = parse(&["sync", "pull"]).expect("sync pull without a source parses");
        assert!(matches!(
            cli.command,
            Some(Command::Sync {
                cmd: SyncCmd::Pull {
                    source: None,
                    dry_run: false,
                }
            })
        ));

        let cli = parse(&["sync", "pull", "~/b.tar.gz", "--dry-run"])
            .expect("sync pull with a source parses");
        let Some(Command::Sync {
            cmd: SyncCmd::Pull { source, dry_run },
        }) = cli.command
        else {
            panic!("expected sync pull");
        };
        assert_eq!(source.as_deref(), Some("~/b.tar.gz"));
        assert!(dry_run);

        let cli = parse(&["sync", "key"]).expect("sync key parses");
        assert!(matches!(
            cli.command,
            Some(Command::Sync { cmd: SyncCmd::Key })
        ));

        let err = parse(&["sync"]).expect_err("bare sync requires a subcommand");
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand,
            "got: {err}"
        );
    }

    #[test]
    fn scheduler_parses_as_a_subcommand() {
        let cli = parse(&["scheduler"]).expect("scheduler parses");
        assert!(matches!(
            cli.command,
            Some(Command::Scheduler { cmd: None })
        ));
    }

    /// A surface that can be managed as a service answers the same seven
    /// verbs, because `ServiceCmd` is defined once in `platform::service`
    /// rather than per surface. This pins that, and pins the one shape that
    /// differs: `scheduler` still runs in the foreground with no subcommand,
    /// where `gateway` alone is a usage error rather than a silent no-op.
    ///
    /// `gateway` reaches them through `GatewayCmd::Service`, flattened, so the
    /// spellings below are the ones every existing doc and unit already uses:
    /// adding `setup` beside them must not have turned `install` into
    /// `service install`.
    #[test]
    fn a_managed_surface_answers_the_service_verbs() {
        use crate::platform::service::ServiceCmd;

        assert!(matches!(
            parse(&["gateway", "install"]).expect("install").command,
            Some(Command::Gateway {
                cmd: GatewayCmd::Service(ServiceCmd::Install)
            })
        ));
        assert!(matches!(
            parse(&["gateway", "logs", "-f", "-n", "200"])
                .expect("logs")
                .command,
            Some(Command::Gateway {
                cmd: GatewayCmd::Service(ServiceCmd::Logs {
                    follow: true,
                    lines: 200
                })
            })
        ));
        assert!(matches!(
            parse(&["scheduler", "status"]).expect("status").command,
            Some(Command::Scheduler {
                cmd: Some(ServiceCmd::Status)
            })
        ));
        assert!(parse(&["gateway"]).is_err());
    }

    /// `setup` is the gateway's own verb: it parses under `gateway` and
    /// nowhere else. The scheduler answering it would be a promise nothing
    /// keeps — there is no scheduler token to check and no chat to discover —
    /// which is why it lives on `GatewayCmd` rather than on the shared
    /// `ServiceCmd`.
    #[test]
    fn gateway_setup_parses_and_the_scheduler_has_no_such_verb() {
        assert!(matches!(
            parse(&["gateway", "setup"]).expect("setup").command,
            Some(Command::Gateway {
                cmd: GatewayCmd::Setup
            })
        ));
        // No arguments: everything it needs it asks for.
        assert!(parse(&["gateway", "setup", "--token", "x"]).is_err());
        assert!(
            parse(&["scheduler", "setup"]).is_err(),
            "the shared service verbs did not grow a gateway-only one"
        );
    }

    #[test]
    fn schedule_add_parses_with_defaults() {
        let cli = parse(&[
            "schedule",
            "add",
            "nightly",
            "--cron",
            "0 3 * * *",
            "--prompt",
            "tidy up",
            "--cwd",
            "/tmp/proj",
        ])
        .expect("schedule add parses");
        let Some(Command::Schedule {
            cmd:
                ScheduleCmd::Add {
                    name,
                    cron,
                    prompt,
                    cwd,
                    max_hours,
                    mode,
                },
        }) = cli.command
        else {
            panic!("expected schedule add");
        };
        assert_eq!(name, "nightly");
        assert_eq!(cron, "0 3 * * *");
        assert_eq!(prompt, "tidy up");
        assert_eq!(cwd, PathBuf::from("/tmp/proj"));
        assert_eq!(max_hours, None);
        assert_eq!(mode, "sovereign");
    }

    #[test]
    fn schedule_add_requires_cron_prompt_and_cwd() {
        let err = parse(&["schedule", "add", "nightly"]).expect_err("missing args rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn schedule_list_remove_and_run_parse() {
        let cli = parse(&["schedule", "list"]).expect("schedule list parses");
        assert!(matches!(
            cli.command,
            Some(Command::Schedule {
                cmd: ScheduleCmd::List
            })
        ));

        let cli = parse(&["schedule", "remove", "nightly"]).expect("schedule remove parses");
        let Some(Command::Schedule {
            cmd: ScheduleCmd::Remove { name },
        }) = cli.command
        else {
            panic!("expected schedule remove");
        };
        assert_eq!(name, "nightly");

        let cli = parse(&["schedule", "run", "nightly"]).expect("schedule run parses");
        let Some(Command::Schedule {
            cmd: ScheduleCmd::Run { name },
        }) = cli.command
        else {
            panic!("expected schedule run");
        };
        assert_eq!(name, "nightly");
    }

    #[test]
    fn fleet_run_parses_workers_and_prompt() {
        let cli = parse(&["fleet", "run", "-n", "3", "-p", "improve coverage"])
            .expect("fleet run parses");
        let Some(Command::Fleet {
            cmd: FleetCmd::Run { n, prompt },
        }) = cli.command
        else {
            panic!("expected fleet run");
        };
        assert_eq!(n, 3);
        assert_eq!(prompt, "improve coverage");

        let cli =
            parse(&["fleet", "run", "--workers", "2", "--prompt", "x"]).expect("long forms parse");
        assert!(matches!(
            cli.command,
            Some(Command::Fleet {
                cmd: FleetCmd::Run { n: 2, .. }
            })
        ));
    }

    #[test]
    fn fleet_run_requires_workers_and_prompt() {
        let err = parse(&["fleet", "run", "-p", "x"]).expect_err("missing -n rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        let err = parse(&["fleet", "run", "-n", "2"]).expect_err("missing -p rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn fleet_status_and_stop_parse() {
        let cli = parse(&["fleet", "status"]).expect("fleet status parses");
        assert!(matches!(
            cli.command,
            Some(Command::Fleet {
                cmd: FleetCmd::Status
            })
        ));
        let cli = parse(&["fleet", "stop"]).expect("fleet stop parses");
        assert!(matches!(
            cli.command,
            Some(Command::Fleet {
                cmd: FleetCmd::Stop
            })
        ));
    }

    #[test]
    fn schedule_enable_and_disable_parse() {
        let cli = parse(&["schedule", "enable", "nightly"]).expect("schedule enable parses");
        let Some(Command::Schedule {
            cmd: ScheduleCmd::Enable { name },
        }) = cli.command
        else {
            panic!("expected schedule enable");
        };
        assert_eq!(name, "nightly");

        let cli = parse(&["schedule", "disable", "nightly"]).expect("schedule disable parses");
        let Some(Command::Schedule {
            cmd: ScheduleCmd::Disable { name },
        }) = cli.command
        else {
            panic!("expected schedule disable");
        };
        assert_eq!(name, "nightly");
    }

    #[test]
    fn usage_parses_with_optional_since() {
        let cli = parse(&["usage"]).expect("usage parses");
        assert!(matches!(cli.command, Some(Command::Usage { since: None })));

        let cli = parse(&["usage", "--since", "7d"]).expect("usage --since parses");
        let Some(Command::Usage { since }) = cli.command else {
            panic!("expected usage");
        };
        assert_eq!(since.as_deref(), Some("7d"));
    }

    #[test]
    fn evolve_list_and_undo_parse() {
        let cli = parse(&["evolve", "list"]).expect("evolve list parses");
        assert!(matches!(
            cli.command,
            Some(Command::Evolve {
                cmd: EvolveCmd::List
            })
        ));

        let cli = parse(&["evolve", "undo", "2"]).expect("evolve undo parses");
        assert!(matches!(
            cli.command,
            Some(Command::Evolve {
                cmd: EvolveCmd::Undo { n: 2 }
            })
        ));
    }

    #[test]
    fn skills_subcommands_parse() {
        let cli = parse(&["skills", "search", "todo", "list"]).expect("skills search parses");
        let Some(Command::Skills {
            cmd:
                SkillsCmd::Search {
                    query,
                    skills,
                    tools,
                },
        }) = cli.command
        else {
            panic!("expected skills search");
        };
        // Several bare words are one query, so `skills search todo list` does
        // not need quoting to mean what it reads as.
        assert_eq!(query, vec!["todo".to_string(), "list".to_string()]);
        assert!(!skills);
        assert!(!tools);

        let cli = parse(&["skills", "install", "slugify"]).expect("skills install parses");
        assert!(matches!(
            cli.command,
            Some(Command::Skills {
                cmd: SkillsCmd::Install {
                    grant_full_stdlib: false,
                    ..
                }
            })
        ));

        let cli = parse(&["skills", "update"]).expect("bare skills update parses");
        assert!(matches!(
            cli.command,
            Some(Command::Skills {
                cmd: SkillsCmd::Update {
                    name: None,
                    grant_full_stdlib: false,
                }
            })
        ));

        let cli = parse(&["skills", "list"]).expect("skills list parses");
        assert!(matches!(
            cli.command,
            Some(Command::Skills {
                cmd: SkillsCmd::List
            })
        ));

        let err = parse(&["skills"]).expect_err("bare skills requires a subcommand");
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand,
            "got: {err}"
        );
    }

    #[test]
    fn the_full_stdlib_grant_is_spelled_out_and_is_never_the_default() {
        // Installing a tool that declares capabilities runs a stranger's code
        // with the user's privileges. The flag that accepts that has to say so
        // where it is typed and where it is later read back out of a shell
        // history, so there is deliberately no `-y` and no `--yes`.
        let cli = parse(&["skills", "install", "slugify", "--grant-full-stdlib"])
            .expect("the grant flag parses");
        assert!(matches!(
            cli.command,
            Some(Command::Skills {
                cmd: SkillsCmd::Install {
                    grant_full_stdlib: true,
                    ..
                }
            })
        ));
        for bypass in ["-y", "--yes", "--force", "--trust"] {
            parse(&["skills", "install", "slugify", bypass])
                .expect_err(&format!("{bypass} must not be a way to skip the grant"));
        }
    }

    #[test]
    fn a_skills_query_may_not_be_both_kinds_at_once() {
        // `None` means "look at both and refuse a name published as each"
        // rather than "pick one", so asking for both kinds explicitly is a
        // contradiction rather than a synonym for the default.
        let err = parse(&["skills", "search", "todo", "--skills", "--tools"])
            .expect_err("--skills --tools is a contradiction");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
        let err = parse(&["skills", "install", "todo", "--skills", "--tools"])
            .expect_err("--skills --tools is a contradiction");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn resume_parses_bare_and_with_claude() {
        let cli = parse(&["resume"]).expect("bare resume parses");
        assert!(matches!(
            cli.command,
            Some(Command::Resume {
                claude: false,
                session: None,
                leaf: None,
                list: false,
            })
        ));

        let cli = parse(&["resume", "--claude", "--list"]).expect("resume --claude --list parses");
        assert!(matches!(
            cli.command,
            Some(Command::Resume {
                claude: true,
                list: true,
                ..
            })
        ));

        let cli = parse(&["resume", "--claude", "--session", "abc", "--leaf", "u-1"])
            .expect("session and leaf parse");
        let Some(Command::Resume { session, leaf, .. }) = cli.command else {
            panic!("expected resume");
        };
        assert_eq!(session.as_deref(), Some("abc"));
        assert_eq!(leaf.as_deref(), Some("u-1"));
    }

    #[test]
    fn the_claude_only_flags_require_claude() {
        // `--session` and `--leaf` name things that only exist in a Claude
        // Code transcript. Accepting them without `--claude` would read as
        // "resume this Wizard session", which is `/resume` and something else
        // entirely.
        for args in [
            &["resume", "--session", "abc"][..],
            &["resume", "--leaf", "u-1"],
            &["resume", "--list"],
        ] {
            let err = parse(args).expect_err("must require --claude");
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::MissingRequiredArgument,
                "{args:?}: {err}"
            );
        }
    }

    #[test]
    fn peers_takes_its_whole_argument_list_unparsed() {
        // Everything after `peers` reaches the plugin verbatim, hyphens
        // included. This is the whole of core's half of `wizard peers`: the
        // eight subcommands, the trust states and their validation live in
        // `plugins::mesh::cli`, which is where the store's own enum is.
        for (args, expected) in [
            (vec!["peers", "list"], vec!["list"]),
            (vec!["peers", "address"], vec!["address"]),
            (vec!["peers", "add", "wiz1abc"], vec!["add", "wiz1abc"]),
            (
                vec!["peers", "trust", "wiz1abc", "trusted"],
                vec!["trust", "wiz1abc", "trusted"],
            ),
            (
                vec!["peers", "watch", "wiz1abc", "--limit", "3"],
                vec!["watch", "wiz1abc", "--limit", "3"],
            ),
            // `--help` is the interesting one: swallowing it here would print
            // core's one-line description of a tree it cannot describe.
            (vec!["peers", "--help"], vec!["--help"]),
        ] {
            let cli = parse(&args).expect("peers parses");
            let Some(Command::Peers { args: passed }) = cli.command else {
                panic!("expected peers, got {:?}", cli.command);
            };
            assert_eq!(passed, expected, "{args:?}");
        }

        // No arguments at all is legal here and is the plugin's error to
        // report, not this parser's: a required-subcommand check in core
        // would be core knowing how many subcommands there are.
        let cli = parse(&["peers"]).expect("bare peers parses");
        assert!(matches!(cli.command, Some(Command::Peers { args }) if args.is_empty()));
    }

    #[test]
    fn ignored_top_level_flags_names_everything_but_cwd() {
        let cli = parse(&["--cwd", "/tmp", "doctor"]).expect("parses");
        assert!(cli.ignored_top_level_flags().is_empty(), "--cwd is honored");

        let cli = parse(&["--plan", "--max-hours", "2", "fleet", "status"]).expect("parses");
        assert_eq!(cli.ignored_top_level_flags(), vec!["--plan", "--max-hours"]);

        let cli = parse(&["--output-format", "json", "doctor"]).expect("parses");
        assert_eq!(cli.ignored_top_level_flags(), vec!["--output-format"]);
    }
}
