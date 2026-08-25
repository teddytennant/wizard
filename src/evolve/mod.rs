//! Tiered self-extension (`/evolve`) and fork-and-distribute (`/publish`).
//! See `docs/evolve.md` and `docs/market.md`.
//!
//! Tier 1 (runtime, default): write a skill, MCP server entry, scripted
//! tool, or subagent under `~/.wizard/` and activate via `/reload`.
//! Tier 2 (`--deep`): propose a diff over Wizard's own source, then take it
//! through the gate before it may replace the running binary: it must build
//! (`cargo build --release --locked`), pass the suite (`cargo test --release
//! --locked`), and smoke-test. Any rung failing reverts the patch, records the
//! failing output in `~/.wizard/evolution.jsonl`, and keeps the current
//! binary. Falls back to Tier 1 when no toolchain/source can be provisioned.

pub mod publish;
pub use publish::{PublishOutcome, PublishRequest, publish};

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncBufReadExt as _;

use crate::agent::subagent::{DEFAULT_MAX_STEPS, SubagentConfig};
use crate::cli::Cli;
use crate::config::{Config, StepBudget};
use crate::llm::{ChatMessage, ChatOptions, ChatRequest};
use crate::mcp::{McpConfig, McpServerConfig, McpTransport};
use crate::platform::exe_swap;
use crate::platform::process::ProcessGroupExt as _;
use crate::platform::shell;
use crate::tools::scripted::ScriptManifest;

/// Where deep evolve clones Wizard's source from on first use. Overridable
/// with the `WIZARD_SOURCE_REPO` environment variable (forks, mirrors,
/// air-gapped file:// remotes).
const DEFAULT_REPO_URL: &str = "https://github.com/teddytennant/wizard";

/// How many times we re-prompt the model when its reply cannot be parsed.
const PROPOSAL_ATTEMPTS: usize = 2;

/// Cap on the repository file listing included in the deep-evolve prompt.
const MAX_LISTED_FILES: usize = 400;

/// Max files whose full contents are fed to the diff-authoring turn.
const MAX_CONTEXT_FILES: usize = 8;

/// Total byte budget for file contents in the diff-authoring prompt.
const MAX_CONTEXT_BYTES: usize = 96_000;

/// Deep evolve's build step. `--locked` is on the *build* because that is the
/// first cargo invocation to touch `Cargo.lock`: whichever one runs first is
/// the one that either rejects a patch that invented a dependency or quietly
/// resolves it (and runs its build script) for everything after it.
const BUILD_ARGS: [&str; 3] = ["build", "--release", "--locked"];

/// Deep evolve's test step, sharing the build's artifacts and its lockfile
/// rule.
const TEST_ARGS: [&str; 3] = ["test", "--release", "--locked"];

/// The cargo features every deep-evolve rung is run with: the ones this binary
/// was built with, so what comes out of the gate is the same *kind* of binary
/// as the one that went in.
///
/// A `wizard-native` install is a `--features native` build. Rebuilt with
/// default features it still compiles, still passes, still installs — and
/// `wizard gui` then opens no window, because iced was never linked in. That is
/// the same silent downgrade [`native_assets`] refuses to make for `wizard
/// update` (`src/update.rs`), and deep evolve has more reason to refuse it: it
/// replaces the running binary in place. The build's failure would be loud and
/// `.prev` recovers, but recovering is not the bar.
///
/// Carried on the test rung too, not only the build: `--release` there is only
/// artifact reuse if both rungs resolve the same feature set, and a suite run
/// without `native` is not the suite for a native binary.
///
/// `cfg!` rather than a probe, because the feature set of the binary doing the
/// evolving *is* the feature set that has to come out the other end. Empty on a
/// default build, where nothing changes.
///
/// [`native_assets`]: crate::update
fn feature_args() -> &'static [&'static str] {
    if cfg!(feature = "native") {
        &["--features", "native"]
    } else {
        &[]
    }
}

/// How long `cargo test --release --locked` may run before deep evolve gives
/// up on it. Generous, because a release-mode test build of a 87k-line crate
/// on a cold cache is genuinely slow, but bounded: a patch that deadlocks a
/// test must not hang Wizard forever. [`TEST_TIMEOUT_ENV`] overrides it for
/// slow machines. `pub(crate)` so the `evolve` tool's description can be held
/// to the bound it promises the model.
pub(crate) const DEFAULT_TEST_TIMEOUT: Duration = Duration::from_secs(45 * 60);

/// How long the built binary gets to answer `--version`. A binary that starts
/// answers in milliseconds; anything near this is one that will not.
const SMOKE_TIMEOUT: Duration = Duration::from_secs(60);

/// Environment variable overriding [`DEFAULT_TEST_TIMEOUT`], in seconds. Named
/// once so the variable [`test_timeout`] reads and the one the timeout error
/// tells the user to raise cannot drift apart.
const TEST_TIMEOUT_ENV: &str = "WIZARD_EVOLVE_TEST_TIMEOUT_SECS";

/// Lines of failing build/test output kept for the error and the evolution
/// log. Enough for a few failing test names plus their assertions.
const MAX_FAILURE_LINES: usize = 80;

/// Character cap on the failure detail stored in the evolution log, so one
/// pathological run cannot bloat `evolution.jsonl`.
const MAX_FAILURE_CHARS: usize = 8_000;

/// Suffix `evolve undo` parks the binary it just displaced under
/// (`wizard.undone`), so undoing an undo is still possible. Distinct from the
/// `.prev` and `.bak` suffixes `crate::update` owns.
const UNDONE_BACKUP_SUFFIX: &str = "undone";

/// Self-extension tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvolveTier {
    /// Tier 1: runtime extension via data/config; no recompile.
    Runtime,
    /// Tier 2: rebuild Wizard's own source (`--deep`).
    Deep,
}

/// Tier-1 channel chosen by the agent for a runtime extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvolveChannel {
    /// Markdown skill listed in the system-prompt index; the body is read
    /// from disk when the skill matches (or inlined if it sets `always: true`).
    Skill,
    /// External MCP server registered in `~/.wizard/mcp.toml`.
    McpServer,
    /// Agent-authored script under `~/.wizard/tools/`.
    ScriptedTool,
    /// Named subagent definition under `~/.wizard/subagents/`.
    Subagent,
}

impl EvolveChannel {
    /// Human-readable label for status messages.
    fn label(self) -> &'static str {
        match self {
            EvolveChannel::Skill => "skill",
            EvolveChannel::McpServer => "MCP server",
            EvolveChannel::ScriptedTool => "scripted tool",
            EvolveChannel::Subagent => "subagent",
        }
    }
}

/// What the user asked `/evolve` to do.
#[derive(Debug, Clone)]
pub struct EvolveRequest {
    /// Natural-language description of the capability to add.
    pub description: String,
    pub tier: EvolveTier,
}

/// What an evolution produced.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EvolveOutcome {
    SkillAdded {
        name: String,
        path: PathBuf,
    },
    McpServerRegistered {
        name: String,
    },
    ScriptedToolAdded {
        name: String,
        path: PathBuf,
    },
    SubagentAdded {
        name: String,
    },
    /// Deep evolve built and staged a new binary; the process will
    /// `exec`-replace itself next.
    DeepRebuilt {
        binary: PathBuf,
    },
    /// Deep evolve could not proceed (no toolchain/source) and ran a Tier-1
    /// evolution instead.
    FellBackToRuntime {
        reason: String,
        outcome: Box<EvolveOutcome>,
    },
    /// The user denied the proposed change. Historical: approval gating was
    /// removed; kept so old `evolution.jsonl` records still deserialize.
    Denied,
}

/// One line of `~/.wizard/evolution.jsonl` — every evolution, both tiers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionEvent {
    pub timestamp: DateTime<Utc>,
    pub tier: EvolveTier,
    /// The user's request.
    pub description: String,
    pub outcome: EvolveOutcome,
    /// Unified diff over Wizard's source (deep evolve only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// Whether `cargo build --release --locked` succeeded (deep evolve only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_ok: Option<bool>,
}

/// JSONL record written to `~/.wizard/evolution.jsonl` when a deep evolve is
/// rejected by a gate (build, tests, smoke test) or fails to install.
///
/// Deliberately a separate shape from [`EvolutionEvent`], which describes a
/// change that actually landed: a rejected patch has no outcome, and inventing
/// one would make `evolve list`/`evolve undo` offer to undo something that was
/// never applied. Readers tell the two apart by the `"event"` key, the same
/// convention `publish` already uses in this file.
///
/// It carries the failing output on purpose. The next attempt (and the model
/// that authored the patch) can only avoid repeating a mistake it can read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeepFailureEvent {
    /// Fixed discriminator distinguishing this from [`EvolutionEvent`] lines.
    pub event: String,
    pub timestamp: DateTime<Utc>,
    /// The user's request.
    pub description: String,
    /// Which gate rejected the patch: `build`, `tests`, `smoke test`, or
    /// `install`.
    pub stage: String,
    /// Whether `cargo build --release --locked` got that far.
    pub build_ok: bool,
    /// Whether `cargo test --release --locked` got that far and passed.
    /// `None` when the build failed first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tests_ok: Option<bool>,
    /// Tail of the failing output, capped at [`MAX_FAILURE_CHARS`].
    pub detail: String,
    /// The diff that was rejected, so it is recoverable and reviewable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

/// Which deep-evolve gate rejected a patch and what it saw. Bundled so
/// [`Evolver::reject`] takes one argument rather than a queue of loose strings
/// and booleans that are easy to pass in the wrong order.
struct GateFailure<'a> {
    /// `build`, `tests`, or `smoke test`.
    stage: &'a str,
    /// The rejected diff, kept for the log.
    diff: &'a str,
    /// What the gate reported.
    err: &'a anyhow::Error,
    /// Whether the build rung was already cleared.
    build_ok: bool,
    /// Whether the test rung was cleared (`None` when it never ran).
    tests_ok: Option<bool>,
}

impl DeepFailureEvent {
    fn new(
        description: &str,
        stage: &str,
        build_ok: bool,
        tests_ok: Option<bool>,
        detail: &str,
        diff: Option<&str>,
    ) -> Self {
        Self {
            event: "deep_failed".to_string(),
            timestamp: Utc::now(),
            description: description.to_string(),
            stage: stage.to_string(),
            build_ok,
            tests_ok,
            detail: truncate(detail, MAX_FAILURE_CHARS),
            diff: diff.map(str::to_string),
        }
    }
}

/// Append one [`EvolutionEvent`] as a JSONL line to `path`, creating the
/// parent directory if needed. Backs [`Evolver::log`].
fn append_event(path: &Path, event: &EvolutionEvent) -> Result<()> {
    append_line(
        path,
        &serde_json::to_string(event).context("serializing evolution event")?,
    )
}

/// Append one [`DeepFailureEvent`] as a JSONL line to `path`.
fn append_failure(path: &Path, event: &DeepFailureEvent) -> Result<()> {
    append_line(
        path,
        &serde_json::to_string(event).context("serializing deep failure event")?,
    )
}

/// Append one already-serialized JSONL `line` to `path`, creating the parent
/// directory if needed.
///
/// The record and its newline go out in a single `write_all`, not a
/// `writeln!`: `write_fmt` issues one write per fragment, and two evolutions
/// logging at the same moment (a subagent and the session that spawned it, the
/// scheduler and an interactive run) would interleave under `O_APPEND` into
/// `{a}{b}\n\n`: one unparseable line, and both records lost to every reader
/// of `evolution.jsonl`.
fn append_line(path: &Path, line: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut record = String::with_capacity(line.len() + 1);
    record.push_str(line);
    record.push('\n');
    file.write_all(record.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// A Tier-1 extension proposed by the model: one channel plus the artifact
/// to write. Parsed from the single JSON object the planning prompt demands.
#[derive(Debug, Deserialize)]
#[serde(tag = "channel", rename_all = "snake_case")]
enum ChannelProposal {
    Skill(SkillProposal),
    McpServer(McpServerConfig),
    ScriptedTool(ScriptedToolProposal),
    Subagent(SubagentProposal),
}

impl ChannelProposal {
    fn channel(&self) -> EvolveChannel {
        match self {
            ChannelProposal::Skill(_) => EvolveChannel::Skill,
            ChannelProposal::McpServer(_) => EvolveChannel::McpServer,
            ChannelProposal::ScriptedTool(_) => EvolveChannel::ScriptedTool,
            ChannelProposal::Subagent(_) => EvolveChannel::Subagent,
        }
    }
}

#[derive(Debug, Deserialize)]
struct SkillProposal {
    name: String,
    #[serde(default)]
    description: Option<String>,
    body: String,
}

#[derive(Debug, Deserialize)]
struct ScriptedToolProposal {
    name: String,
    description: String,
    #[serde(default)]
    interpreter: Option<String>,
    /// Host runtime. `"luajit"` (default) runs in-process; omit alongside a
    /// `.lua` script_name for the same effect.
    #[serde(default)]
    runtime: Option<String>,
    /// Script file name (sanitized; derived from `name` when omitted).
    #[serde(default)]
    script_name: Option<String>,
    script_content: String,
    /// JSON Schema for the tool's arguments object.
    #[serde(default)]
    parameters: Option<Value>,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct SubagentProposal {
    name: String,
    description: String,
    system_prompt: String,
    #[serde(default)]
    tool_scope: Option<Vec<String>>,
    #[serde(default)]
    max_steps: Option<u32>,
}

/// System prompt for the Tier-1 planning turn: pick one channel, emit one
/// JSON object describing the artifact.
const TIER1_SYSTEM_PROMPT: &str = r##"You are Wizard's self-extension planner. Wizard is a local agent that can extend itself at runtime through exactly four channels. Given the user's request, choose the single best channel and respond with ONLY one JSON object — no prose, no markdown fences, no comments.

Channels and their exact JSON shapes:

1. "skill" — knowledge, guidelines, or a workflow. The prompt lists its name and description; the body is read from disk when the skill matches:
{"channel":"skill","name":"kebab-case-name","description":"one-line summary","body":"full markdown content of the skill"}

2. "mcp_server" — register an external Model Context Protocol tool server (computer use, browsers, databases, search, ...):
{"channel":"mcp_server","name":"server-name","transport":"stdio","command":"uvx","args":["mcp-package-name"],"env":{}}
or, for a remote server:
{"channel":"mcp_server","name":"server-name","transport":"http","url":"https://example.com/mcp"}

3. "scripted_tool" — a small LuaJIT script exposed as a tool. Wizard embeds LuaJIT (the just-in-time compiler); scripts run in-process, no external interpreter. Tool arguments arrive as the global Lua table `args`; the project root is the string `cwd`; helpers live under `wizard` (`wizard.read_file`, `wizard.write_file`, `wizard.json_encode`, `wizard.json_decode`, `wizard.runtime`). Print results with `print(...)` (or `return` a value). Prefer Lua. Only set an external `interpreter` (bash/python/…) when the job truly needs one:
{"channel":"scripted_tool","name":"snake_case_name","description":"what it does","runtime":"luajit","script_name":"snake_case_name.lua","script_content":"-- LuaJIT tool\nlocal n = args.n or 0\nprint(n * 2)\n","parameters":{"type":"object","properties":{"n":{"type":"number","description":"..."}},"required":["n"]},"timeout_secs":120}

4. "subagent" — a named, reusable sub-worker with its own prompt and tool scope (a 50-step ceiling by default; optional `max_steps` raises or lowers it, and 0 removes it entirely):
{"channel":"subagent","name":"reviewer","description":"what it is for","system_prompt":"You are ...","tool_scope":["read_file","search_files","git_diff"]}

Tool names available for "tool_scope": read_file, write_file, edit_file, list_files, search_files, execute, git_status, git_diff. Omit "tool_scope" (or use null) to grant the full set.

Picking a channel: use a skill for knowledge or process, an mcp_server for capabilities that live outside Wizard, a scripted_tool (LuaJIT by default) for small executable glue, and a subagent for a specialized, reusable sub-worker. Keep names short and filesystem-safe. Make the artifact complete and immediately usable. For scripted_tool always prefer Lua (`.lua`, `runtime: "luajit"`) unless the user explicitly needs a shell/Python/Node script."##;

/// System prompt for the deep-evolve (Tier 2) diff-authoring turn.
const DEEP_SYSTEM_PROMPT: &str = r#"You are Wizard's deep-evolve engineer. Wizard is a single-binary Rust 2024 agent (Ratatui TUI + multi-provider agent loop) and you are modifying its own source checkout. Produce ONE unified diff that implements the requested change.

Rules:
- Output ONLY the diff, inside a single ```diff fenced code block. No other text.
- Use standard unified diff format with `--- a/<path>` and `+++ b/<path>` headers (use /dev/null for created or deleted files) and correct `@@` hunk headers.
- Paths are relative to the repository root.
- Include at least 3 unchanged context lines around each hunk so `git apply` can locate it.
- Hunks must match the CURRENT file contents shown to you exactly, line for line. Only modify files whose contents you were shown; other paths may only appear as newly created files.
- Keep the change minimal, correct, and consistent with the existing code style. Proper error handling; no todo!() or unwrap() on fallible paths."#;

/// System prompt for the deep-evolve file-selection turn that precedes the
/// diff: pick which files' contents the diff author needs to see.
const FILE_SELECT_SYSTEM_PROMPT: &str = r#"You are Wizard's deep-evolve navigator. Wizard is a single-binary Rust 2024 agent and you are preparing to modify its own source checkout. Given a requested change and the repository's file listing, pick the files whose CURRENT CONTENTS are needed to author the change as a unified diff (the files to modify, plus closely related ones needed for context).

Respond with ONLY one JSON object — no prose, no code fences:
{"files":["src/foo.rs","src/bar.rs"]}

Rules: at most 8 files, most relevant first, and only paths that appear in the listing."#;

/// Drives the evolve pipeline. Holds config and paths; the model
/// interaction itself runs through a dedicated agent turn.
pub struct Evolver {
    config: Config,
    /// Print progress and stream model output to stdout (CLI runs).
    verbose: bool,
}

impl Evolver {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            verbose: false,
        }
    }

    /// Enable progress printing to stdout (used by the CLI entry point).
    pub fn with_verbose(mut self, verbose: bool) -> Self {
        self.verbose = verbose;
        self
    }

    /// Run one evolution end to end: have the agent pick a Tier-1 channel
    /// (or drive the deep pipeline), apply it, log it, and return the
    /// outcome. Tier-1 results become live after `/reload`.
    pub async fn run(&mut self, request: EvolveRequest) -> Result<EvolveOutcome> {
        if request.description.trim().is_empty() {
            bail!("evolution request has an empty description");
        }
        match request.tier {
            EvolveTier::Runtime => {
                let outcome = self.run_runtime(&request).await?;
                self.log_event(&request, EvolveTier::Runtime, &outcome, None, None)?;
                Ok(outcome)
            }
            EvolveTier::Deep => self.run_deep(&request).await,
        }
    }

    /// Append an event to `~/.wizard/evolution.jsonl`.
    pub fn log(&self, event: &EvolutionEvent) -> Result<()> {
        append_event(&Config::evolution_log_path()?, event)
    }

    fn log_event(
        &self,
        request: &EvolveRequest,
        tier: EvolveTier,
        outcome: &EvolveOutcome,
        diff: Option<String>,
        build_ok: Option<bool>,
    ) -> Result<()> {
        self.log(&EvolutionEvent {
            timestamp: Utc::now(),
            tier,
            description: request.description.clone(),
            outcome: outcome.clone(),
            diff,
            build_ok,
        })
    }

    // ---- Tier 1: runtime extension ----

    /// Plan and apply one runtime extension. Does not log; callers do, so
    /// the deep-evolve fallback can wrap the outcome first.
    async fn run_runtime(&self, request: &EvolveRequest) -> Result<EvolveOutcome> {
        self.status(&format!(
            "Planning a runtime extension for: {}",
            request.description
        ));
        let proposal = self.propose_channel(&request.description).await?;
        self.status(&format!(
            "\nProposed {} extension:\n{}\n",
            proposal.channel().label(),
            proposal_summary(&proposal)
        ));

        let outcome = self.apply_proposal(proposal)?;
        self.status(
            "Change written under ~/.wizard — run /reload (or restart Wizard) to activate it.",
        );
        Ok(outcome)
    }

    /// One dedicated model turn (with one retry) producing a parsed Tier-1
    /// channel proposal.
    async fn propose_channel(&self, description: &str) -> Result<ChannelProposal> {
        let messages = vec![
            ChatMessage::system(TIER1_SYSTEM_PROMPT),
            ChatMessage::user(description),
        ];
        self.propose(
            messages,
            parse_proposal,
            "Reply with ONLY the JSON object for one channel, exactly matching the documented shape — no prose, no code fences.",
        )
        .await
    }

    /// Write the proposed artifact under `~/.wizard/`.
    fn apply_proposal(&self, proposal: ChannelProposal) -> Result<EvolveOutcome> {
        match proposal {
            ChannelProposal::Skill(skill) => self.add_skill(skill),
            ChannelProposal::McpServer(server) => self.register_mcp_server(server),
            ChannelProposal::ScriptedTool(tool) => self.add_scripted_tool(tool),
            ChannelProposal::Subagent(subagent) => self.add_subagent(subagent),
        }
    }

    /// Write `~/.wizard/skills/<name>/SKILL.md` with frontmatter the skills
    /// loader understands.
    fn add_skill(&self, proposal: SkillProposal) -> Result<EvolveOutcome> {
        if proposal.body.trim().is_empty() {
            bail!("the proposed skill has an empty body");
        }
        let name = slugify(&proposal.name, '-')?;
        let dir = Config::skills_dir()?.join(&name);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join("SKILL.md");

        let mut doc = format!("---\nname: {name}\n");
        if let Some(description) = proposal.description.as_deref() {
            let description = description.replace(['\n', '\r'], " ");
            let description = description.trim();
            if !description.is_empty() {
                doc.push_str(&format!("description: {description}\n"));
            }
        }
        doc.push_str("---\n\n");
        doc.push_str(proposal.body.trim());
        doc.push('\n');

        std::fs::write(&path, doc).with_context(|| format!("writing {}", path.display()))?;
        Ok(EvolveOutcome::SkillAdded { name, path })
    }

    /// Upsert a `[[server]]` entry in `~/.wizard/mcp.toml`.
    fn register_mcp_server(&self, mut server: McpServerConfig) -> Result<EvolveOutcome> {
        server.name = slugify(&server.name, '-')?;
        match server.transport {
            McpTransport::Stdio
                if server
                    .command
                    .as_deref()
                    .is_none_or(|c| c.trim().is_empty()) =>
            {
                bail!("stdio MCP server '{}' is missing a command", server.name)
            }
            McpTransport::Http if server.url.as_deref().is_none_or(|u| u.trim().is_empty()) => {
                bail!("http MCP server '{}' is missing a url", server.name)
            }
            _ => {}
        }

        let path = Config::mcp_config_path()?;
        let mut mcp = McpConfig::load(&path)?;
        let name = server.name.clone();
        let replaced = mcp.servers.iter().any(|s| s.name == name);
        mcp.servers.retain(|s| s.name != name);
        mcp.servers.push(server);
        mcp.save(&path)?;
        if replaced {
            self.status(&format!("Replaced existing MCP server entry '{name}'."));
        }
        Ok(EvolveOutcome::McpServerRegistered { name })
    }

    /// Write the script plus its `<name>.toml` manifest under
    /// `~/.wizard/tools/`. Defaults to embedded LuaJIT (`.lua` +
    /// `runtime = "luajit"`) so evolve glue needs no external interpreter.
    fn add_scripted_tool(&self, proposal: ScriptedToolProposal) -> Result<EvolveOutcome> {
        if proposal.script_content.trim().is_empty() {
            bail!("the proposed scripted tool has an empty script");
        }
        let tool_name = slugify(&proposal.name, '_')?;
        let dir = Config::scripted_tools_dir()?;
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

        let interpreter = proposal
            .interpreter
            .as_deref()
            .map(str::trim)
            .filter(|i| !i.is_empty())
            .map(str::to_string);
        let runtime = proposal
            .runtime
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(|r| r.to_ascii_lowercase());

        // Default channel is LuaJIT: if the model omitted both runtime and a
        // non-Lua interpreter, host it in-process.
        let wants_luajit = match (runtime.as_deref(), interpreter.as_deref()) {
            (Some(r), _) if r == "luajit" || r == "lua" || r == "embedded" => true,
            (Some(r), _) if r == "external" || r == "process" || r == "shell" => false,
            (None, Some(i)) => {
                let i = i.to_ascii_lowercase();
                i.contains("luajit") || i == "lua" || i.ends_with("/lua") || i.ends_with("/luajit")
            }
            (None, None) => {
                // Peek at script_name / content shebang before defaulting.
                let name = proposal.script_name.as_deref().unwrap_or("");
                let content = proposal.script_content.trim_start();
                if name.ends_with(".lua") || content.starts_with("--") {
                    true
                } else if content.starts_with("#!")
                    || name.ends_with(".sh")
                    || name.ends_with(".py")
                    || name.ends_with(".js")
                {
                    false
                } else {
                    // Bare proposal with no signals → LuaJIT.
                    true
                }
            }
            _ => false,
        };

        let script_file = match proposal.script_name.as_deref().map(str::trim) {
            Some(name) if !name.is_empty() => sanitize_file_name(name)?,
            _ => format!(
                "{}.{}",
                slugify(&proposal.name, '-')?,
                if wants_luajit {
                    "lua"
                } else {
                    script_extension(interpreter.as_deref())
                }
            ),
        };
        let script_path = dir.join(&script_file);

        let mut content = proposal.script_content;
        if !content.ends_with('\n') {
            content.push('\n');
        }

        // External scripts with neither a shebang nor an interpreter cannot
        // run; default the interpreter to `sh` rather than write a dud tool.
        // LuaJIT tools need neither.
        let interpreter = if wants_luajit {
            None
        } else {
            interpreter.or_else(|| (!content.starts_with("#!")).then(|| "sh".to_string()))
        };
        let runtime = if wants_luajit {
            Some("luajit".to_string())
        } else {
            runtime.filter(|r| r != "luajit" && r != "lua" && r != "embedded")
        };

        std::fs::write(&script_path, content)
            .with_context(|| format!("writing {}", script_path.display()))?;
        if wants_luajit {
            // Lua tools are read by the interpreter, never exec'd, and this
            // may be overwriting an executable tool of the same name, so the
            // execute bit has to come off rather than merely not go on.
            exe_swap::clear_executable(&script_path)?;
        } else {
            exe_swap::set_executable(&script_path)?;
        }

        let parameters = match proposal.parameters {
            Some(value) if value.is_object() => value,
            _ => serde_json::json!({ "type": "object", "properties": {} }),
        };
        let manifest = ScriptManifest {
            name: tool_name.clone(),
            description: proposal.description,
            script: script_file,
            interpreter,
            runtime,
            parameters,
            timeout_secs: proposal.timeout_secs,
        };
        let manifest_path = dir.join(format!("{tool_name}.toml"));
        let raw =
            toml::to_string_pretty(&manifest).context("serializing scripted tool manifest")?;
        std::fs::write(&manifest_path, raw)
            .with_context(|| format!("writing {}", manifest_path.display()))?;

        Ok(EvolveOutcome::ScriptedToolAdded {
            name: tool_name,
            path: manifest_path,
        })
    }

    /// Write a subagent definition to `~/.wizard/subagents/<name>.toml`.
    fn add_subagent(&self, proposal: SubagentProposal) -> Result<EvolveOutcome> {
        if proposal.system_prompt.trim().is_empty() {
            bail!("the proposed subagent has an empty system prompt");
        }
        let name = slugify(&proposal.name, '-')?;
        let config = SubagentConfig {
            name: name.clone(),
            description: proposal.description,
            system_prompt: proposal.system_prompt,
            tool_scope: proposal.tool_scope,
            // A proposal that names no ceiling gets the same one a hand-written
            // subagent file gets, because that is what the docs promise and
            // what the four shipped specialists run under. Defaulting to 0
            // here would not be "unset": SubagentConfig::max_steps is a
            // transparent u32 with no skip_serializing_if, so the file this
            // writes carries a literal `max_steps = 0`, the serde default never
            // fires on it again, and an evolve-made subagent would be the only
            // one that runs unbounded. An explicit 0 from the model still means
            // unlimited, as it does in a file someone wrote by hand.
            max_steps: StepBudget::new(proposal.max_steps.unwrap_or(DEFAULT_MAX_STEPS)),
        };
        let dir = Config::wizard_dir()?.join("subagents");
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(format!("{name}.toml"));
        let raw = toml::to_string_pretty(&config).context("serializing subagent config")?;
        std::fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))?;
        Ok(EvolveOutcome::SubagentAdded { name })
    }

    // ---- Deep-evolve pipeline (tier 2) ----

    /// The full Tier-2 pipeline: source + toolchain, diff proposal, build,
    /// test suite, smoke test, install. Falls back to Tier 1 when
    /// source/toolchain cannot be provisioned. Logs its own events (it needs
    /// the diff and the gate results).
    async fn run_deep(&mut self, request: &EvolveRequest) -> Result<EvolveOutcome> {
        let prepared = self
            .ensure_source()
            .and_then(|dir| self.ensure_toolchain().map(|()| dir));
        let source_dir = match prepared {
            Ok(dir) => dir,
            Err(err) => {
                let reason = format!("{err:#}");
                self.status(&format!(
                    "Deep evolve unavailable ({reason}); falling back to a runtime (Tier 1) evolution."
                ));
                let inner = self.run_runtime(request).await?;
                let outcome = EvolveOutcome::FellBackToRuntime {
                    reason,
                    outcome: Box::new(inner),
                };
                // Log the tier that actually ran, not the one requested.
                self.log_event(request, EvolveTier::Runtime, &outcome, None, None)?;
                return Ok(outcome);
            }
        };

        self.status("Proposing a change to Wizard's own source…");
        let diff = self.propose_diff(&request.description, &source_dir).await?;
        if self.verbose {
            println!("\n{diff}");
        }

        self.apply_diff(&source_dir, &diff)?;

        // The gate a model-authored patch must clear before it is allowed to
        // replace the user's binary: it compiles, the suite still passes, and
        // the resulting binary runs. Each rung reverts the patch on failure, so
        // a rejected attempt leaves the checkout and the installed binary
        // exactly as they were.
        self.status("Building (cargo build --release --locked)… this can take a while.");
        let built = match self.build(&source_dir).await {
            Ok(binary) => binary,
            Err(err) => {
                return Err(self.reject(
                    request,
                    &source_dir,
                    GateFailure {
                        stage: "build",
                        diff: &diff,
                        err: &err,
                        build_ok: false,
                        tests_ok: None,
                    },
                ));
            }
        };
        self.status(
            "Running the test suite (cargo test --release --locked)… this can take a while.",
        );
        if let Err(err) = self.run_tests(&source_dir).await {
            return Err(self.reject(
                request,
                &source_dir,
                GateFailure {
                    stage: "tests",
                    diff: &diff,
                    err: &err,
                    build_ok: true,
                    tests_ok: Some(false),
                },
            ));
        }
        if let Err(err) = smoke_test(&built).await {
            return Err(self.reject(
                request,
                &source_dir,
                GateFailure {
                    stage: "smoke test",
                    diff: &diff,
                    err: &err,
                    build_ok: true,
                    tests_ok: Some(true),
                },
            ));
        }
        self.commit_source(&source_dir, &request.description);

        // Past the gate, the install itself is the last thing that can lie:
        // a swallowed failure here logs a successful rebuild while the next
        // launch (and the continuous loop's re-exec) still runs the old
        // binary. Propagate it and record the failure instead.
        let binary = match self.install_binary(&built) {
            Ok(binary) => binary,
            Err(err) => {
                self.log_failure(&DeepFailureEvent::new(
                    &request.description,
                    "install",
                    true,
                    Some(true),
                    &format!("{err:#}"),
                    Some(&diff),
                ));
                return Err(err.context(format!(
                    "deep evolve built and tested a new binary but could not install it; \
                     it is at {} and nothing was replaced",
                    built.display()
                )));
            }
        };
        let outcome = EvolveOutcome::DeepRebuilt {
            binary: binary.clone(),
        };
        self.log_event(request, EvolveTier::Deep, &outcome, Some(diff), Some(true))?;
        self.status(&format!("Rebuilt Wizard: {}", binary.display()));
        Ok(outcome)
    }

    /// Reject a deep evolve at `stage`: revert the working tree, record the
    /// failure (with the output that caused it) in the evolution log, and
    /// return the error the caller propagates.
    ///
    /// Every gate funnels through here so a rejected patch can never leave the
    /// checkout dirty, the binary replaced, or the log claiming success. The
    /// failing output rides along in both the log line and the error, because
    /// the error is what the model sees when it tries again.
    fn reject(
        &self,
        request: &EvolveRequest,
        source_dir: &Path,
        failure: GateFailure<'_>,
    ) -> anyhow::Error {
        self.revert_diff(source_dir);
        let stage = failure.stage;
        let detail = format!("{:#}", failure.err);
        self.log_failure(&DeepFailureEvent::new(
            &request.description,
            stage,
            failure.build_ok,
            failure.tests_ok,
            &detail,
            Some(failure.diff),
        ));
        anyhow!("{detail}").context(format!(
            "deep evolve failed the {stage} gate; the patch was reverted and the current \
             binary kept"
        ))
    }

    /// Append a [`DeepFailureEvent`] to `~/.wizard/evolution.jsonl`. Failing to
    /// log is never allowed to mask the failure being logged, so it only warns.
    fn log_failure(&self, event: &DeepFailureEvent) {
        let logged = Config::evolution_log_path()
            .and_then(|path| append_failure(&path, event))
            .is_ok();
        if !logged {
            tracing::warn!(
                stage = %event.stage,
                "could not record the deep-evolve failure in the evolution log"
            );
        }
    }

    /// Diff proposal in two steps: a file-selection turn picks the files
    /// whose contents matter (falling back to a keyword heuristic when that
    /// turn fails), then the diff-authoring turn sees those files' actual
    /// contents — without them the model hallucinates context lines and
    /// `git apply --check` rejects nearly every diff.
    async fn propose_diff(&self, description: &str, source_dir: &Path) -> Result<String> {
        let listing = source_file_listing(source_dir);
        let files = match self.select_context_files(description, &listing).await {
            Ok(files) => files,
            Err(err) => {
                self.status(&format!(
                    "File-selection turn failed ({err:#}); falling back to keyword matching."
                ));
                heuristic_context_files(description, &listing)
            }
        };
        let context = read_context_files(source_dir, &files, MAX_CONTEXT_BYTES);
        if !context.is_empty() {
            self.status(&format!(
                "Showing the diff author {} file(s): {}",
                files.len().min(MAX_CONTEXT_FILES),
                files
                    .iter()
                    .take(MAX_CONTEXT_FILES)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let user = format!(
            "Requested change to Wizard:\n{description}\n\n\
             Files in the repository (relative to its root):\n{listing}\n\n\
             {context}\
             Reply with one unified diff implementing the change."
        );
        let messages = vec![
            ChatMessage::system(DEEP_SYSTEM_PROMPT),
            ChatMessage::user(user),
        ];
        self.propose(
            messages,
            |reply| extract_diff(reply).context("no unified diff found in the reply"),
            "Reply with ONLY a unified diff inside a single ```diff fenced block.",
        )
        .await
    }

    /// One dedicated model turn (with one retry) picking the files whose
    /// contents the diff author needs.
    async fn select_context_files(&self, description: &str, listing: &str) -> Result<Vec<String>> {
        let user = format!(
            "Requested change to Wizard:\n{description}\n\n\
             Files in the repository (relative to its root):\n{listing}\n\n\
             Reply with the JSON object naming the files whose contents are needed."
        );
        let messages = vec![
            ChatMessage::system(FILE_SELECT_SYSTEM_PROMPT),
            ChatMessage::user(user),
        ];
        self.propose(
            messages,
            parse_file_selection,
            "Reply with ONLY {\"files\":[\"path\", ...]} using paths from the listing.",
        )
        .await
    }

    /// Ensure `~/.wizard/src` holds a source checkout, cloning the repo on
    /// first use. Errors when offline with no existing checkout.
    pub fn ensure_source(&self) -> Result<PathBuf> {
        let dir = Config::source_dir()?;
        if dir.join("Cargo.toml").is_file() {
            return Ok(dir);
        }
        let non_empty = dir.exists()
            && std::fs::read_dir(&dir)
                .map(|mut entries| entries.next().is_some())
                .unwrap_or(false);
        if non_empty {
            bail!(
                "{} exists but does not look like a Wizard checkout; remove it and retry",
                dir.display()
            );
        }
        if !command_exists("git") {
            bail!("`git` is required to clone Wizard's source");
        }
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        let url = repo_url();
        self.status(&format!("Cloning {url} into {}…", dir.display()));
        let output = Command::new("git")
            .args(["clone", "--depth", "1"])
            .arg(&url)
            .arg(&dir)
            .output()
            .context("running git clone")?;
        if !output.status.success() {
            bail!(
                "git clone of {url} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        if !dir.join("Cargo.toml").is_file() {
            bail!("cloned {url} but no Cargo.toml found in {}", dir.display());
        }
        Ok(dir)
    }

    /// Ensure a Rust toolchain is available, installing just-in-time via
    /// `rustup --profile minimal` when `cargo` is absent. Errors when it
    /// cannot be provisioned (the caller then falls back to Tier 1).
    pub fn ensure_toolchain(&self) -> Result<()> {
        if find_cargo().is_some() {
            return Ok(());
        }
        // rustup may be on PATH (or only as a proxy under ~/.cargo/bin) with
        // no default toolchain — try to finish that install before downloading
        // a fresh rustup. Skip on Termux: rustup's host triples target glibc
        // desktop Linux, not Android/Bionic (`pkg install rust` is the path).
        //
        // The detector is `crate::platform::host`'s, deliberately: this module
        // carried a byte-for-byte copy of it, so hardening Termux detection in
        // the one place the module docs point at (a new `TERMUX__PREFIX`, say)
        // fixed update, doctor and local_setup while leaving deep evolve to
        // pipe `sh.rustup.rs` into a shell on an Android device.
        if !crate::platform::is_termux() {
            if let Some(ru) = find_rustup() {
                self.status(
                    "Found rustup without a working cargo; running `rustup default stable`…",
                );
                let status = Command::new(&ru)
                    .args(["default", "stable"])
                    .status()
                    .context("running rustup default stable")?;
                if status.success() && find_cargo().is_some() {
                    return Ok(());
                }
            }
        } else {
            bail!(
                "no working Rust toolchain on Termux. Install with \
                 `pkg install rust git clang make pkg-config openssl`, and if a \
                 broken rustup install is shadowing it, remove `~/.cargo` and \
                 `~/.rustup` then retry"
            );
        }
        self.status("No Rust toolchain found; installing one via rustup (--profile minimal)…");
        let status = if let Some(ru) = find_rustup() {
            Command::new(ru)
                .args(["toolchain", "install", "stable", "--profile", "minimal"])
                .status()
                .context("running rustup toolchain install")?
        } else if command_exists("curl") {
            // A pipeline, so it needs a shell rather than an argv vector.
            shell::command(
                "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
                 | sh -s -- -y --profile minimal --default-toolchain stable --no-modify-path",
            )
            .status()
            .context("running the rustup installer")?
        } else {
            bail!("no Rust toolchain, and neither `rustup` nor `curl` is available to install one");
        };
        if !status.success() {
            bail!("rustup install exited with {status}");
        }
        if find_cargo().is_none() {
            bail!("rustup ran but `cargo` is still not available");
        }
        Ok(())
    }

    /// `cargo build --release --locked` in `source_dir`; returns the built
    /// binary path. The previous binary is kept beside it for rollback.
    ///
    /// `--locked` belongs on this step, not only on the test step that follows
    /// it: a build without it *reconciles* `Cargo.lock` with `Cargo.toml`
    /// (resolving, downloading, and running the new crate's `build.rs`) before
    /// anything else gets to look, so by the time `cargo test --locked` ran the
    /// evidence was already gone. Here it means a patch that invents a
    /// dependency is rejected at the first rung, before that crate's build
    /// script executes on the user's machine.
    ///
    /// The output is always captured, whatever `verbose` says, because it is
    /// the failure detail the evolution log keeps and the next attempt learns
    /// from; verbose only additionally echoes it as it arrives so the user can
    /// watch a long compile.
    pub async fn build(&self, source_dir: &std::path::Path) -> Result<PathBuf> {
        let cargo = find_cargo().context("cargo is not available (no Rust toolchain installed)")?;
        let mut cmd = tokio::process::Command::new(&cargo);
        cmd.args(BUILD_ARGS)
            .args(feature_args())
            .current_dir(source_dir)
            .env("PATH", augmented_path())
            .stdin(Stdio::null())
            // Everything cargo says to a human, diagnostics included, goes to
            // stderr; stdout only carries machine formats nothing here asks
            // for, and inheriting it would let a stray crate print into the
            // TUI's frame.
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            // Same treatment as the test rung, and for a stronger reason: this
            // is the rung that *first* executes the patch's own code. `cargo
            // build` runs the tree's `build.rs` and every proc macro before any
            // rung has reported a verdict, so a patch that edits the existing
            // build script — which `--locked` does not prevent, since it
            // invents no dependency — can simply never return. Unbounded, that
            // hung the whole deep evolve: `revert_diff` never ran, nothing was
            // written to `evolution.jsonl`, and cancelling the turn killed
            // cargo while leaving the build-script tree it forked alive.
            .kill_on_drop(true)
            .own_process_group();

        let mut child = cmd
            .spawn()
            .context("running cargo build --release --locked")?;
        let leader = child.id();
        let guard = GroupKillGuard::new(leader);
        let stderr = child
            .stderr
            .take()
            .context("capturing cargo build output")?;
        let mut lines = tokio::io::BufReader::new(stderr).lines();
        // Draining stderr and reaping cargo go under **one** deadline. Split
        // apart they are not a deadline at all: stderr reaches EOF only when
        // every process holding the write end has exited, so a build script
        // that never returns parks this task in `next_line()` forever and the
        // `child.wait()` the timeout used to wrap is never reached.
        let run = async {
            let mut captured = String::new();
            while let Some(line) = lines
                .next_line()
                .await
                .context("reading cargo build output")?
            {
                if self.verbose {
                    println!("{line}");
                }
                captured.push_str(&line);
                captured.push('\n');
            }
            let status = child
                .wait()
                .await
                .context("waiting for cargo build --release --locked")?;
            Ok::<_, anyhow::Error>((captured, status))
        };
        let (captured, status) = match tokio::time::timeout(test_timeout(), run).await {
            Ok(finished) => finished?,
            Err(_) => {
                // The guard kills the group on the way out. A build that will
                // not finish is a failed rung, not an unknown one — the same
                // call the test rung makes about its own timeout.
                bail!(
                    "cargo build --release --locked did not finish within {:?}; the patch's own \
                     build script or proc macros may not terminate. Treated as a failed build.",
                    test_timeout()
                );
            }
        };
        guard.disarm();
        if !status.success() {
            bail!(
                "cargo build --release --locked failed:\n{}",
                tail_lines(&captured, MAX_FAILURE_LINES)
            );
        }

        let binary = source_dir.join("target").join("release").join("wizard");
        if !binary.is_file() {
            bail!(
                "build succeeded but the binary is missing at {}",
                binary.display()
            );
        }
        Ok(binary)
    }

    /// `cargo test --release --locked` in `source_dir`: the gate that decides
    /// whether a model-authored patch may replace the user's binary.
    ///
    /// Without it "it compiles and prints a version string" is the entire bar,
    /// which any plausible-looking patch clears while quietly breaking the
    /// agent loop. `--release` reuses the artifacts the preceding build just
    /// produced; `--locked` is carried here as well as on the build so the
    /// lockfile is checked again after whatever the build did.
    ///
    /// Output is always captured, never inherited, because the failing tail is
    /// what the next attempt learns from.
    pub async fn run_tests(&self, source_dir: &std::path::Path) -> Result<()> {
        let cargo = find_cargo().context("cargo is not available (no Rust toolchain installed)")?;
        let timeout = test_timeout();
        let mut cmd = tokio::process::Command::new(&cargo);
        cmd.args(TEST_ARGS)
            .args(feature_args())
            .current_dir(source_dir)
            .env("PATH", augmented_path())
            // A test that reads stdin would otherwise block until the timeout.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Reaches cargo itself when the future below is dropped; the test
            // binaries it forked need the process-group kill.
            .kill_on_drop(true)
            // Give cargo a process group of its own so a timeout can kill
            // everything it started, the same way `tools::shell` does for the
            // shell tool.
            .own_process_group();

        let mut child = cmd
            .spawn()
            .context("running cargo test --release --locked")?;
        // Kept before the future takes ownership of the child: the group id is
        // what the kill needs, and by then there is no handle left.
        let leader = child.id();
        // Drained as it arrives rather than collected at the end, so a run that
        // is killed still has something to report. `wait_with_output` hands
        // back nothing when the future holding it is dropped, which is exactly
        // the case — a suite that hung — where what it printed on the way is
        // the only evidence of *which* test hung.
        let stdout = child.stdout.take().context("capturing cargo test output")?;
        let stderr = child.stderr.take().context("capturing cargo test output")?;
        let captured = std::sync::Mutex::new(String::new());
        let run = {
            // Armed for every exit from this scope except the one that reaped
            // cargo itself. The timeout is not the only way this future dies:
            // the TUI's interrupt aborts the turn that is awaiting it, a
            // subagent's step budget drops it, and the runtime drops it on
            // shutdown. In all of those `kill_on_drop` reaches exactly the one
            // process tokio spawned, and the rustc and `wizard-<hash>` test
            // binaries cargo forked keep running with nothing left that knows
            // their group id.
            let guard = GroupKillGuard::new(leader);
            // Draining both pipes and reaping cargo go under one deadline, for
            // the reason the build rung spells out: a pipe reaches EOF only
            // when every process holding its write end has exited, so a
            // separate `wait` would never be reached.
            let finish = async {
                tokio::try_join!(drain_into(stdout, &captured), drain_into(stderr, &captured))?;
                child
                    .wait()
                    .await
                    .context("running cargo test --release --locked")
            };
            match tokio::time::timeout(timeout, finish).await {
                Ok(result) => {
                    let status = result?;
                    // Reaped, so from here the pid may be recycled and must
                    // never be signalled again. There is no await between the
                    // reap and this line, so no cancellation can land inside
                    // the window.
                    guard.disarm();
                    TestRun::Finished {
                        success: status.success(),
                        output: taken(&captured),
                    }
                }
                Err(_) => {
                    // Nothing has been killed yet. `timeout` dropped `finish`,
                    // but that future only *borrows* `child` — the `Child`
                    // itself outlives this scope, so `kill_on_drop` has not
                    // fired and cargo is still running. It would not be enough
                    // on its own anyway: a patch whose test deadlocks leaves
                    // the `wizard-<hash>` test binary cargo forked reparented
                    // to init, still holding whatever it hung on, and every
                    // later timed-out evolve leaks another. The guard's `Drop`,
                    // at the end of this scope, is what ends the run — it
                    // SIGKILLs the whole process group, cargo included.
                    TestRun::TimedOut {
                        secs: timeout.as_secs(),
                        output: taken(&captured),
                    }
                }
            }
        };
        test_verdict(run)
    }

    /// Replace the running process with `binary`. On success this never
    /// returns; how a process becomes another program is
    /// [`crate::platform::process::exec_replace`]'s business.
    pub fn exec_replace(binary: &std::path::Path) -> Result<std::convert::Infallible> {
        crate::platform::process::exec_replace(binary)
    }

    /// Pipe `diff` to `git apply` in `source_dir` (a `--check` pass first,
    /// then for real).
    fn apply_diff(&self, source_dir: &Path, diff: &str) -> Result<()> {
        for check in [true, false] {
            let mut cmd = Command::new("git");
            cmd.arg("-C")
                .arg(source_dir)
                .args(["apply", "--whitespace=nowarn"]);
            if check {
                cmd.arg("--check");
            }
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = cmd.spawn().context("spawning git apply")?;
            child
                .stdin
                .take()
                .context("opening git apply stdin")?
                .write_all(diff.as_bytes())
                .context("writing diff to git apply")?;
            let output = child.wait_with_output().context("waiting for git apply")?;
            if !output.status.success() {
                bail!(
                    "git apply{} failed:\n{}",
                    if check { " --check" } else { "" },
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
        }
        Ok(())
    }

    /// Best-effort revert of an applied-but-unbuildable diff so the next
    /// deep evolve starts from a clean tree.
    fn revert_diff(&self, source_dir: &Path) {
        let checkout = Command::new("git")
            .arg("-C")
            .arg(source_dir)
            .args(["checkout", "--", "."])
            .status();
        let clean = Command::new("git")
            .arg("-C")
            .arg(source_dir)
            .args(["clean", "-fdq"])
            .status();
        let ok = checkout.map(|s| s.success()).unwrap_or(false)
            && clean.map(|s| s.success()).unwrap_or(false);
        if ok {
            self.status("Reverted the applied diff in the source checkout.");
        } else {
            tracing::warn!(
                source_dir = %source_dir.display(),
                "failed to revert the applied diff; the checkout may be dirty"
            );
        }
    }

    /// Best-effort commit of a successful deep evolve so the checkout stays
    /// clean for the next one and the change is recoverable from history.
    fn commit_source(&self, source_dir: &Path, description: &str) {
        let added = Command::new("git")
            .arg("-C")
            .arg(source_dir)
            .args(["add", "-A"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !added {
            tracing::warn!("git add failed in the source checkout; skipping commit");
            return;
        }
        let subject = description.lines().next().unwrap_or(description);
        let message = format!("evolve(deep): {subject}");
        let committed = Command::new("git")
            .arg("-C")
            .arg(source_dir)
            .args([
                "-c",
                "user.name=Wizard",
                "-c",
                "user.email=wizard@localhost",
                "commit",
                "-m",
            ])
            .arg(&message)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !committed {
            tracing::warn!("git commit failed in the source checkout");
        }
    }

    /// Install `built` over the currently running executable, keeping the
    /// prior binary beside it as `<name>.prev` for rollback, and return the
    /// path the caller should exec.
    ///
    /// Fallible on purpose. A failed swap used to degrade to "run the built
    /// binary from the source tree", which reads as success to every caller
    /// while `wizard` on `PATH` (and the continuous loop's re-exec of
    /// `current_exe`) keeps running the old binary. The only paths that return
    /// the build output are the ones where nothing needed installing.
    fn install_binary(&self, built: &Path) -> Result<PathBuf> {
        self.install_to(built, resolved_exe())
    }

    /// [`Evolver::install_binary`] with the install target already resolved,
    /// so a test can say "the running executable could not be located" without
    /// arranging for `/proc/self/exe` to read back as deleted.
    fn install_to(&self, built: &Path, exe: Result<PathBuf>) -> Result<PathBuf> {
        let exe = exe.with_context(|| {
            format!(
                "could not locate the running executable to install over; \
                 the rebuilt binary is at {}. Install it by hand with: \
                 sudo install -m755 {} $(command -v wizard)",
                built.display(),
                built.display()
            )
        })?;
        // Already running from the build output (e.g. after a prior in-place
        // deep evolve): nothing to install.
        if is_same_binary(&exe, built) {
            return Ok(built.to_path_buf());
        }
        self.install_over(built, &exe)
    }

    /// Install `built` over `exe` and report where to find the way back.
    ///
    /// Split from [`Evolver::install_binary`] so the install itself can be
    /// tested against a scratch destination instead of the running test
    /// binary.
    fn install_over(&self, built: &Path, exe: &Path) -> Result<PathBuf> {
        let backup = exe_swap::install_executable(built, exe, crate::update::EVOLVE_BACKUP_SUFFIX)
            .with_context(|| {
                format!(
                    "installing the rebuilt binary over {}; install it by hand with: \
                 sudo install -m755 {} {}",
                    exe.display(),
                    built.display(),
                    exe.display()
                )
            })?;
        self.status(&format!(
            "Installed the new binary over {}. To roll back: mv {} {}",
            exe.display(),
            backup.display(),
            exe.display()
        ));
        Ok(exe.to_path_buf())
    }

    // ---- Model interaction ----

    /// Run a dedicated model turn and parse the reply, re-prompting once
    /// with `retry_hint` when parsing fails.
    async fn propose<T>(
        &self,
        mut messages: Vec<ChatMessage>,
        parse: impl Fn(&str) -> Result<T>,
        retry_hint: &str,
    ) -> Result<T> {
        let mut last_err: Option<anyhow::Error> = None;
        for _ in 0..PROPOSAL_ATTEMPTS {
            let reply = self.complete(&messages).await?;
            match parse(&reply) {
                Ok(value) => return Ok(value),
                Err(err) => {
                    messages.push(ChatMessage::assistant(reply));
                    messages.push(ChatMessage::user(format!(
                        "That response could not be used ({err:#}). {retry_hint}"
                    )));
                    last_err = Some(err);
                }
            }
        }
        Err(last_err
            .unwrap_or_else(|| anyhow!("the model produced no reply"))
            .context("the model did not produce a usable evolution proposal"))
    }

    /// Stream one completion from the active provider and return the
    /// accumulated text.
    async fn complete(&self, messages: &[ChatMessage]) -> Result<String> {
        let active = self.config.active();
        let client = active
            .build()
            .with_context(|| format!("building provider '{}'", active.name))?;
        let request = ChatRequest {
            model: active.model,
            messages: messages.to_vec(),
            tools: Vec::new(),
            stream: true,
            options: Some(ChatOptions {
                // Low temperature: we want a parseable artifact, not prose.
                temperature: Some(0.3),
                num_ctx: None,
                reasoning_effort: None,
            }),
        };
        let mut stream = client.chat_stream(request).await?;
        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if let Some(message) = chunk.message
                && !message.text().is_empty()
            {
                if self.verbose {
                    print!("{}", message.text());
                    let _ = std::io::stdout().flush();
                }
                text.push_str(&message.text());
            }
            if chunk.done {
                break;
            }
        }
        if self.verbose && !text.is_empty() {
            println!();
        }
        Ok(text)
    }

    /// Print a progress line when verbose (CLI); always trace it.
    fn status(&self, message: &str) {
        tracing::info!(target: "wizard::evolve", "{message}");
        if self.verbose {
            println!("{message}");
        }
    }
}

/// CLI entry point for `wizard --publish`: forks Wizard to the user's GitHub
/// and prints the fork URL and one-line installer to stdout.
pub async fn run_publish_cli(config: Config, cli: Cli) -> Result<()> {
    use publish::PublishRequest;

    let branch = cli.prompt.clone().and_then(|p| {
        let p = p.trim().to_string();
        (!p.is_empty()).then_some(p)
    });

    let req = PublishRequest { branch };

    let outcome = publish::publish(&config, req, true).await?;
    println!("Fork:    {}", outcome.fork_url);
    println!("Branch:  {}", outcome.branch);
    if let Some(sha) = &outcome.commit {
        println!("Commit:  {sha}");
    }
    println!("\nInstall one-liner:\n{}", outcome.install_one_liner);
    Ok(())
}

/// CLI entry point for `wizard evolve list|undo`: inspect and roll back the
/// evolution history in `~/.wizard/evolution.jsonl`. Self-contained — no
/// config load, no LLM.
pub fn run_history_cli(cmd: crate::cli::EvolveCmd) -> Result<i32> {
    let path = Config::evolution_log_path()?;
    match cmd {
        crate::cli::EvolveCmd::List => list_events(&path),
        crate::cli::EvolveCmd::Undo { n } => undo_event(&path, n),
    }
}

/// Read every parsable event from the evolution log; a missing file is an
/// empty history, malformed lines are skipped with a warning.
fn read_events(path: &Path) -> Result<Vec<EvolutionEvent>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).context(format!("reading {}", path.display())),
    };
    let mut events = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<EvolutionEvent>(line) {
            Ok(event) => events.push(event),
            // `publish` and [`DeepFailureEvent`] share this file and mark
            // themselves with an `"event"` key. They are not evolutions with
            // an outcome to list or undo, so skip them without the noise a
            // genuinely corrupt line deserves.
            Err(err) => {
                let foreign = serde_json::from_str::<Value>(line)
                    .map(|value| value.get("event").is_some())
                    .unwrap_or(false);
                if !foreign {
                    tracing::warn!("skipping malformed evolution line: {err}");
                }
            }
        }
    }
    Ok(events)
}

/// Short outcome label for `evolve list`.
fn outcome_label(outcome: &EvolveOutcome) -> String {
    match outcome {
        EvolveOutcome::SkillAdded { name, .. } => format!("skill '{name}'"),
        EvolveOutcome::McpServerRegistered { name } => format!("mcp server '{name}'"),
        EvolveOutcome::ScriptedToolAdded { name, .. } => format!("scripted tool '{name}'"),
        EvolveOutcome::SubagentAdded { name } => format!("subagent '{name}'"),
        EvolveOutcome::DeepRebuilt { binary } => format!("deep rebuild → {}", binary.display()),
        EvolveOutcome::FellBackToRuntime { outcome, .. } => {
            format!("fallback: {}", outcome_label(outcome))
        }
        EvolveOutcome::Denied => "denied".to_string(),
    }
}

/// `wizard evolve list`: numbered history, most recent first (#1 newest —
/// the number `evolve undo` takes).
fn list_events(path: &Path) -> Result<i32> {
    let events = read_events(path)?;
    if events.is_empty() {
        println!("no evolutions recorded yet ({})", path.display());
        return Ok(0);
    }
    for (i, event) in events.iter().rev().enumerate() {
        let tier = match event.tier {
            EvolveTier::Runtime => "runtime",
            EvolveTier::Deep => "deep",
        };
        println!(
            "#{:<3} {}  {tier:<7}  {:<40}  {}",
            i + 1,
            event.timestamp.format("%Y-%m-%d %H:%M"),
            outcome_label(&event.outcome),
            truncate(&event.description, 70).replace('\n', " ")
        );
    }
    Ok(0)
}

/// `wizard evolve undo <n>`: revert evolution #n from `evolve list`.
/// Conservative: refuses with a clear message when the recorded artifacts
/// are already gone rather than guessing.
fn undo_event(path: &Path, n: usize) -> Result<i32> {
    let events = read_events(path)?;
    if n == 0 || n > events.len() {
        bail!(
            "no evolution #{n} — the history has {} entr{} (see `wizard evolve list`)",
            events.len(),
            if events.len() == 1 { "y" } else { "ies" }
        );
    }
    let event = &events[events.len() - n];
    undo_outcome(&event.outcome)?;
    println!("undid evolution #{n}: {}", event.description);
    Ok(0)
}

/// Revert one recorded outcome. Tier 1 = delete the created artifacts;
/// deep = restore the `.prev` binary.
fn undo_outcome(outcome: &EvolveOutcome) -> Result<()> {
    match outcome {
        EvolveOutcome::SkillAdded { name, path } => {
            if !path.is_file() {
                bail!(
                    "skill '{name}' is already gone ({} does not exist)",
                    path.display()
                );
            }
            std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
            // The per-skill directory only held SKILL.md; drop it when empty.
            if let Some(dir) = path.parent() {
                let _ = std::fs::remove_dir(dir);
            }
            println!("removed {} — /reload (or restart) to apply", path.display());
        }
        EvolveOutcome::McpServerRegistered { name } => {
            let path = Config::mcp_config_path()?;
            let mut mcp = McpConfig::load(&path)?;
            let before = mcp.servers.len();
            mcp.servers.retain(|server| &server.name != name);
            if mcp.servers.len() == before {
                bail!(
                    "MCP server '{name}' is not registered in {} (already removed?)",
                    path.display()
                );
            }
            mcp.save(&path)?;
            println!("unregistered MCP server '{name}' from {}", path.display());
        }
        EvolveOutcome::ScriptedToolAdded { name, path } => {
            if !path.is_file() {
                bail!(
                    "scripted tool '{name}' is already gone ({} does not exist)",
                    path.display()
                );
            }
            // The manifest names the script file that sits beside it.
            if let Ok(raw) = std::fs::read_to_string(path)
                && let Ok(manifest) = toml::from_str::<ScriptManifest>(&raw)
            {
                let script = path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(&manifest.script);
                if script.is_file() {
                    std::fs::remove_file(&script)
                        .with_context(|| format!("removing {}", script.display()))?;
                    println!("removed {}", script.display());
                }
            }
            std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
            println!("removed {} — /reload (or restart) to apply", path.display());
        }
        EvolveOutcome::SubagentAdded { name } => {
            let path = Config::wizard_dir()?
                .join("subagents")
                .join(format!("{name}.toml"));
            if !path.is_file() {
                bail!(
                    "subagent '{name}' is already gone ({} does not exist)",
                    path.display()
                );
            }
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            println!("removed {} — /reload (or restart) to apply", path.display());
        }
        EvolveOutcome::DeepRebuilt { binary } => {
            let file_name = binary
                .file_name()
                .and_then(|n| n.to_str())
                .context("the recorded binary path has no file name")?;
            let prev = binary.with_file_name(format!(
                "{file_name}.{}",
                crate::update::EVOLVE_BACKUP_SUFFIX
            ));
            if !prev.is_file() {
                bail!(
                    "no rollback binary at {} — cannot undo this deep evolve",
                    prev.display()
                );
            }
            // Through `install_executable`, like every other swap: it stages a
            // copy beside the binary, backs the live one up (that backup is
            // the `.undone` copy kept in case the rollback was itself a
            // mistake), and renames the staged file into place. Doing it here
            // by hand meant a rename-aside followed by a rename-back, and an
            // undo interrupted between the two left no `wizard` on PATH at
            // all, only `wizard.undone` and `wizard.prev`.
            let undone = exe_swap::install_executable(&prev, binary, UNDONE_BACKUP_SUFFIX)
                .with_context(|| {
                    format!("restoring {} over {}", prev.display(), binary.display())
                })?;
            // The rollback copy has been consumed: it is the live binary now,
            // and the build it displaced is the `.undone` one. Leaving `.prev`
            // behind would offer a second undo that only restores what is
            // already installed.
            let _ = std::fs::remove_file(&prev);
            println!(
                "restored the previous binary at {} — restart wizard to run it{}",
                binary.display(),
                if undone.is_file() {
                    format!(" (the undone build is kept at {})", undone.display())
                } else {
                    String::new()
                }
            );
        }
        EvolveOutcome::FellBackToRuntime { outcome, .. } => undo_outcome(outcome)?,
        EvolveOutcome::Denied => bail!("that evolution was denied; nothing was ever applied"),
    }
    Ok(())
}

/// CLI entry point for `wizard --evolve [-p "..."] [--deep]`: runs one
/// evolution without the full TUI, printing progress to stdout.
pub async fn run_cli(config: Config, cli: Cli) -> Result<()> {
    let description = match cli.prompt.as_deref().map(str::trim) {
        Some(prompt) if !prompt.is_empty() => prompt.to_string(),
        _ => prompt_for_description()?,
    };
    let tier = if cli.deep {
        EvolveTier::Deep
    } else {
        EvolveTier::Runtime
    };
    let request = EvolveRequest { description, tier };

    let mut evolver = Evolver::new(config).with_verbose(true);
    let outcome = evolver.run(request).await?;
    print_outcome(&outcome);

    if let EvolveOutcome::DeepRebuilt { binary } = &outcome {
        println!("Restarting Wizard with the new binary…");
        Evolver::exec_replace(binary)?; // never returns on success
    }
    Ok(())
}

/// Ask for a description interactively when `-p` was not given.
fn prompt_for_description() -> Result<String> {
    if !std::io::stdin().is_terminal() {
        bail!("no evolution description provided; pass one with -p \"...\"");
    }
    print!("What capability should Wizard add? ");
    std::io::stdout().flush().context("flushing stdout")?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading the description")?;
    let description = line.trim().to_string();
    if description.is_empty() {
        bail!("no evolution description provided");
    }
    Ok(description)
}

/// Print a user-facing summary of an outcome (recurses into fallbacks).
fn print_outcome(outcome: &EvolveOutcome) {
    match outcome {
        EvolveOutcome::SkillAdded { name, path } => println!(
            "Skill '{name}' added at {} — /reload (or restart) to activate.",
            path.display()
        ),
        EvolveOutcome::McpServerRegistered { name } => {
            println!("MCP server '{name}' registered in ~/.wizard/mcp.toml — /reload to connect.")
        }
        EvolveOutcome::ScriptedToolAdded { name, path } => println!(
            "Scripted tool '{name}' added ({}) — /reload to activate.",
            path.display()
        ),
        EvolveOutcome::SubagentAdded { name } => {
            println!("Subagent '{name}' configured — /reload to activate.")
        }
        EvolveOutcome::DeepRebuilt { binary } => {
            println!("Deep evolve complete: {}", binary.display())
        }
        EvolveOutcome::FellBackToRuntime { reason, outcome } => {
            println!("Deep evolve fell back to a runtime extension: {reason}");
            print_outcome(outcome);
        }
        EvolveOutcome::Denied => println!("Evolution denied; no changes were applied."),
    }
}

/// Human-readable proposal preview printed before the change is applied.
fn proposal_summary(proposal: &ChannelProposal) -> String {
    match proposal {
        ChannelProposal::Skill(skill) => format!(
            "skill '{}' — {}\n\n{}",
            skill.name,
            skill.description.as_deref().unwrap_or("(no description)"),
            truncate(&skill.body, 2000)
        ),
        ChannelProposal::McpServer(server) => format!(
            "MCP server '{}':\n{}",
            server.name,
            toml::to_string_pretty(server).unwrap_or_else(|_| format!("{server:?}"))
        ),
        ChannelProposal::ScriptedTool(tool) => format!(
            "scripted tool '{}' — {}\n\n{}",
            tool.name,
            tool.description,
            truncate(&tool.script_content, 2000)
        ),
        ChannelProposal::Subagent(subagent) => format!(
            "subagent '{}' — {}\n\nsystem prompt:\n{}",
            subagent.name,
            subagent.description,
            truncate(&subagent.system_prompt, 2000)
        ),
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let head: String = text.chars().take(max_chars).collect();
        format!("{head}…")
    }
}

/// Parse the file-selection reply: `{"files":[...]}` (tolerating prose and
/// fences around the JSON, like every other proposal parse).
fn parse_file_selection(reply: &str) -> Result<Vec<String>> {
    let value = extract_json_object(reply)?;
    let files = value
        .get("files")
        .and_then(Value::as_array)
        .context("the reply has no \"files\" array")?;
    let out: Vec<String> = files
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .filter(|path| !path.trim().is_empty())
        .collect();
    if out.is_empty() {
        bail!("the \"files\" array named no usable paths");
    }
    Ok(out)
}

/// Fallback file selection when the model turn fails: rank listed source
/// files by how many words of the description appear in their path.
fn heuristic_context_files(description: &str, listing: &str) -> Vec<String> {
    let words: Vec<String> = description
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| word.len() >= 4)
        .map(str::to_string)
        .collect();
    let mut scored: Vec<(usize, &str)> = listing
        .lines()
        .filter(|path| path.ends_with(".rs") || *path == "Cargo.toml")
        .map(|path| {
            let lower = path.to_lowercase();
            let score = words
                .iter()
                .filter(|word| lower.contains(word.as_str()))
                .count();
            (score, path)
        })
        .filter(|(score, _)| *score > 0)
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    scored
        .into_iter()
        .take(MAX_CONTEXT_FILES)
        .map(|(_, path)| path.to_string())
        .collect()
}

/// Read the selected files (skipping absolute or traversal paths and
/// anything unreadable) under a total byte budget, rendered as a prompt
/// section. Files are included whole or not at all: a truncated file would
/// make the model author hunks against lines it never saw.
fn read_context_files(source_dir: &Path, files: &[String], budget: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    let mut included = 0usize;
    for rel in files {
        if included == MAX_CONTEXT_FILES {
            break;
        }
        let rel_path = Path::new(rel);
        if rel_path.is_absolute()
            || rel_path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(source_dir.join(rel_path)) else {
            continue;
        };
        if used + content.len() > budget {
            continue;
        }
        used += content.len();
        included += 1;
        out.push_str(&format!("--- current contents of {rel} ---\n{content}\n"));
    }
    if out.is_empty() {
        String::new()
    } else {
        format!("Current contents of the most relevant files:\n\n{out}")
    }
}

/// Parse the model's reply into a Tier-1 channel proposal.
fn parse_proposal(reply: &str) -> Result<ChannelProposal> {
    let value = extract_json_object(reply)?;
    serde_json::from_value(value)
        .map_err(|err| anyhow!("the proposal JSON did not match any channel shape: {err}"))
}

/// Find the first JSON object in a model reply, tolerating prose,
/// `<think>` blocks, and code fences around it.
fn extract_json_object(text: &str) -> Result<Value> {
    let text = strip_thinking(text);
    for block in fenced_blocks(&text) {
        if let Some(value) = first_json_object(&block) {
            return Ok(value);
        }
    }
    first_json_object(&text).context("no JSON object found in the model's reply")
}

/// Scan for `{` and try to parse one JSON object starting there (trailing
/// text after the object is fine).
fn first_json_object(text: &str) -> Option<Value> {
    let mut start = 0;
    let mut attempts = 0;
    while let Some(offset) = text[start..].find('{') {
        let index = start + offset;
        let mut iter = serde_json::Deserializer::from_str(&text[index..]).into_iter::<Value>();
        if let Some(Ok(value)) = iter.next()
            && value.is_object()
        {
            return Some(value);
        }
        start = index + 1;
        attempts += 1;
        if attempts >= 50 {
            break;
        }
    }
    None
}

/// Contents of ``` fenced blocks, with a short language tag line stripped.
fn fenced_blocks(text: &str) -> Vec<String> {
    text.split("```")
        .enumerate()
        .filter(|(i, _)| i % 2 == 1)
        .map(|(_, block)| match block.split_once('\n') {
            Some((first, rest)) if first.trim().len() <= 20 && !first.contains('{') => {
                rest.to_string()
            }
            _ => block.to_string(),
        })
        .collect()
}

/// Remove `<think>`/`<thinking>` blocks some models emit inline.
fn strip_thinking(text: &str) -> String {
    let mut out = text.to_string();
    for tag in ["think", "thinking"] {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        while let Some(start) = out.find(&open) {
            match out[start..].find(&close) {
                Some(end) => out.replace_range(start..start + end + close.len(), ""),
                None => {
                    out.truncate(start);
                    break;
                }
            }
        }
    }
    out
}

/// Extract a unified diff from a model reply: a ```diff fenced block, or a
/// bare diff starting at the first `diff --git` / `--- ` line.
fn extract_diff(text: &str) -> Option<String> {
    let text = strip_thinking(text);
    for block in fenced_blocks(&text) {
        let block = block.trim();
        if looks_like_diff(block) {
            return Some(format!("{block}\n"));
        }
    }

    let mut index = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end();
        if trimmed.starts_with("diff --git") || trimmed.starts_with("--- ") {
            let candidate = text[index..].trim_end().trim_end_matches("```").trim_end();
            if looks_like_diff(candidate) {
                return Some(format!("{candidate}\n"));
            }
        }
        index += line.len();
    }
    None
}

fn looks_like_diff(text: &str) -> bool {
    let mut has_header = false;
    let mut has_hunk = false;
    for line in text.lines() {
        if line.starts_with("--- ") || line.starts_with("diff --git") {
            has_header = true;
        }
        if line.starts_with("@@") {
            has_hunk = true;
        }
    }
    has_header && has_hunk
}

/// Reduce a free-form name to a lowercase filesystem-safe slug joined by
/// `sep`. Errors when nothing usable remains (defends against path
/// traversal and junk names from the model).
fn slugify(raw: &str, sep: char) -> Result<String> {
    let mut out = String::new();
    let mut prev_sep = true;
    for c in raw.trim().chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_sep = false;
        } else if !prev_sep {
            out.push(sep);
            prev_sep = true;
        }
    }
    let out = out.trim_matches(sep).to_string();
    if out.is_empty() {
        bail!("'{raw}' does not reduce to a usable name");
    }
    Ok(out)
}

/// Reduce a proposed script file name to a safe basename (no directories,
/// no traversal, conservative character set).
fn sanitize_file_name(raw: &str) -> Result<String> {
    let name: String = Path::new(raw)
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| {
            n.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                        c
                    } else {
                        '-'
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let name = name.trim_matches(['.', '-']).to_string();
    if name.is_empty() {
        bail!("'{raw}' is not a usable file name");
    }
    Ok(name)
}

/// Pick a script extension from the interpreter name.
fn script_extension(interpreter: Option<&str>) -> &'static str {
    match interpreter {
        Some(i) if i.contains("python") => "py",
        Some(i) if i.contains("node") || i.contains("deno") || i.contains("bun") => "js",
        Some(i)
            if i.contains("luajit")
                || i == "lua"
                || i.ends_with("/lua")
                || i.ends_with("/luajit") =>
        {
            "lua"
        }
        _ => "sh",
    }
}

fn repo_url() -> String {
    std::env::var("WIZARD_SOURCE_REPO").unwrap_or_else(|_| DEFAULT_REPO_URL.to_string())
}

/// How many times a `--version` probe is retried when the *spawn* failed for a
/// reason that says nothing about the program.
const PROBE_ATTEMPTS: u32 = 4;

/// Pause between those attempts. Short: the window being waited out is a
/// `fork` that has not reached its `exec` yet, which is microseconds.
const PROBE_BACKOFF: std::time::Duration = std::time::Duration::from_millis(20);

/// `true` when `program --version` exits successfully.
///
/// Retries when the spawn itself failed transiently, which is a different fact
/// from the program running and reporting failure and must not be collapsed
/// into it. The one that bites is `ETXTBSY`: a file written and made
/// executable a moment ago cannot be `exec`ed while any process still holds a
/// write descriptor to it, and a *concurrent* `fork` elsewhere in the process
/// briefly inherits one — the child holds the copy until its own `exec`
/// completes. Rust opens with `O_CLOEXEC`, so the window is short, but it is
/// not zero and it is entered whenever something else spawns at the wrong
/// microsecond.
///
/// The cost of not retrying is not a flaky test, it is `wizard evolve`
/// deciding "cargo is broken" and refusing to build over a race that had
/// nothing to do with cargo. A real "cargo does not work" answer survives four
/// attempts unchanged, because a program that runs and exits non-zero is
/// `Ok(status)` and returns immediately.
fn probe_runs(mut command: impl FnMut() -> Command) -> bool {
    for attempt in 0..PROBE_ATTEMPTS {
        match command()
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(status) => return status.success(),
            Err(error) if !worth_retrying(error.kind()) => return false,
            Err(_) if attempt + 1 == PROBE_ATTEMPTS => return false,
            Err(_) => std::thread::sleep(PROBE_BACKOFF),
        }
    }
    false
}

/// Whether a failure to *start* a program might answer differently in a
/// moment.
///
/// Only these three. Not found, permission denied and the rest are answers:
/// the program really cannot be run, waiting will not change that, and
/// retrying would turn a fast "no" into four sleeps and the same "no".
fn worth_retrying(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::ExecutableFileBusy
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::Interrupted
    )
}

/// `true` when `cmd --version` runs successfully.
fn command_exists(cmd: &str) -> bool {
    probe_runs(|| Command::new(cmd))
}

/// `true` when `path --version` exits successfully. A rustup *proxy* can
/// exist on `PATH` while no default toolchain is configured — `which cargo`
/// succeeds, `cargo --version` does not (Termux + leftover `~/.cargo/bin`).
fn cargo_binary_works(path: &std::path::Path) -> bool {
    if !path.is_file() {
        return false;
    }
    probe_runs(|| Command::new(path))
}

/// Locate `rustup` on `PATH` or under `~/.cargo/bin`.
fn find_rustup() -> Option<PathBuf> {
    if let Some(path_os) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_os) {
            let candidate = dir.join("rustup");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let candidate = dirs::home_dir()?.join(".cargo").join("bin").join("rustup");
    candidate.is_file().then_some(candidate)
}

/// Locate a working `cargo`: each `PATH` entry first (so a Termux/distro
/// cargo wins over a broken rustup shim in `~/.cargo/bin`), then
/// `~/.cargo/bin` as a last resort for a just-in-time rustup install that
/// is not yet on `PATH` in this process.
fn find_cargo() -> Option<PathBuf> {
    if let Some(path_os) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_os) {
            let candidate = dir.join("cargo");
            if cargo_binary_works(&candidate) {
                return Some(candidate);
            }
        }
    }
    let candidate = dirs::home_dir()?.join(".cargo").join("bin").join("cargo");
    cargo_binary_works(&candidate).then_some(candidate)
}

/// `PATH` with the directory of the chosen `cargo` prepended, and
/// `~/.cargo/bin` available when that is the working toolchain. When a
/// non-rustup cargo wins (Termux `pkg install rust`), `~/.cargo/bin` is
/// *not* prepended — a broken rustup shim there must not shadow `rustc`.
fn augmented_path() -> std::ffi::OsString {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let mut paths: Vec<PathBuf> = std::env::split_paths(&current).collect();
    if let Some(cargo) = find_cargo()
        && let Some(dir) = cargo.parent()
    {
        let dir = dir.to_path_buf();
        paths.retain(|p| p != &dir);
        paths.insert(0, dir.clone());
        // Drop ~/.cargo/bin when it is not the chosen toolchain's dir,
        // so a leftover rustup proxy cannot win for rustc/clippy/etc.
        if let Some(home) = dirs::home_dir() {
            let cargo_bin = home.join(".cargo").join("bin");
            if cargo_bin != dir {
                paths.retain(|p| p != &cargo_bin);
            }
        }
        return std::env::join_paths(paths).unwrap_or(current);
    }
    if let Some(home) = dirs::home_dir() {
        let cargo_bin = home.join(".cargo").join("bin");
        if !paths.contains(&cargo_bin) {
            paths.push(cargo_bin);
        }
    }
    std::env::join_paths(paths).unwrap_or(current)
}

/// The running executable, resolved through symlinks.
///
/// Canonicalizing is the whole point, and `crate::update` does the same for
/// the same reason. A managed install is usually a symlink
/// (`/usr/local/bin/wizard` into a Homebrew cellar, a Nix store path); without
/// resolving it the swap renames a fresh regular file over the *link*, so the
/// package manager's real binary keeps running at the old version and its view
/// of the world is now wrong. It also makes the path absolute: on a
/// `./wizard`, `dest.parent()` would be `""` and the staged copy would land
/// wherever the agent's cwd happened to be.
///
/// An error rather than a `None` that the caller shrugs off, matching
/// `crate::update`. The failure is not hypothetical: once anything has renamed
/// over the running binary (which is exactly what an install does), Linux
/// reports `/proc/self/exe` as `<path> (deleted)` for the still-running
/// process and `canonicalize` fails with ENOENT. Treating that as "nothing
/// needed installing" reported a rebuild as landed while the binary on `PATH`
/// was still the old one, and left `evolve undo` pointing at a `wizard.prev`
/// nobody wrote.
fn resolved_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the current executable")?;
    exe.canonicalize()
        .with_context(|| format!("canonicalizing {}", exe.display()))
}

/// Whether two paths name the same binary on disk.
///
/// Both sides are resolved, and a path that cannot be resolved is never "the
/// same" as anything: two missing paths comparing equal as text used to be
/// read as "already installed", which skipped the install and reported
/// success.
fn is_same_binary(left: &Path, right: &Path) -> bool {
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// SIGKILL the whole process group led by `leader`, when there is one.
///
/// tokio's `kill_on_drop` reaches exactly one process, the one it spawned. A
/// cargo run is a tree (rustc, build scripts, the test binaries themselves),
/// so killing the leader alone leaves the interesting half of it running. The
/// group kill is the same one `tools::shell` and `tools::tasks` make, for the
/// same reason; the `Option` is only because `tokio::process::Child::id`
/// returns none once the child has been reaped, and a run that already ended
/// has nothing to kill.
fn kill_process_group(leader: Option<u32>) {
    if let Some(pid) = leader {
        crate::platform::process::kill_group(pid);
    }
}

/// Kills the process group it holds when it goes out of scope, unless
/// [`GroupKillGuard::disarm`] said the run is over.
///
/// Putting cargo in a group of its own is what makes the whole tree killable,
/// and it is also what takes cargo *out* of the terminal's foreground group:
/// nothing else on the machine will end that tree on Wizard's behalf any more.
/// So every way out of the run has to deliver the kill, not just the timeout
/// arm, and a `Drop` is the only spelling of "every way out" that a later edit
/// cannot forget. The one exception the guard cannot cover is a signal that
/// kills Wizard outright (`SIGKILL`, or a `SIGINT` in a terminal that owns a
/// non-TUI run): destructors do not run then, and the group survives. That is
/// the same tradeoff `tools::shell` and `tools::tasks` make for their own
/// spawns.
///
/// The group id is only safe to signal while the child has not been reaped:
/// after that the pid can be recycled, and a stale group kill would land on
/// somebody else's processes.
struct GroupKillGuard(Option<u32>);

impl GroupKillGuard {
    fn new(leader: Option<u32>) -> Self {
        Self(leader)
    }

    /// The child was reaped; there is nothing left to kill and the pid is no
    /// longer ours to signal.
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for GroupKillGuard {
    fn drop(&mut self) {
        kill_process_group(self.0.take());
    }
}

fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// Relative paths of the source files (skipping `.git` and `target`),
/// sorted and capped, for the deep-evolve prompt.
fn source_file_listing(root: &Path) -> String {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = entry.file_name();
                if name == ".git" || name == "target" {
                    continue;
                }
                stack.push(path);
            } else if let Ok(rel) = path.strip_prefix(root) {
                files.push(rel.display().to_string());
            }
        }
    }
    files.sort();
    files.truncate(MAX_LISTED_FILES);
    files.join("\n")
}

/// Run `binary --version` and check it exits 0 printing a `wizard …`
/// version line, before trusting it to replace the running executable.
async fn smoke_test(binary: &Path) -> Result<()> {
    // Bounded and group-killed, like the two rungs before it. This starts the
    // freshly built *model-authored* binary, so "it never returns" is a thing
    // it can choose to do — and it ran blocking, on a tokio worker thread, with
    // no timeout at all. A `--version` that answers is the whole test, so the
    // budget can be short.
    let child = tokio::process::Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .own_process_group()
        .spawn()
        .with_context(|| format!("running {} --version", binary.display()))?;
    let guard = GroupKillGuard::new(child.id());
    let output = match tokio::time::timeout(SMOKE_TIMEOUT, child.wait_with_output()).await {
        Ok(result) => {
            let output =
                result.with_context(|| format!("running {} --version", binary.display()))?;
            guard.disarm();
            output
        }
        Err(_) => bail!(
            "{} --version did not answer within {SMOKE_TIMEOUT:?}",
            binary.display()
        ),
    };
    if !output.status.success() {
        bail!(
            "{} --version exited with {}",
            binary.display(),
            output.status
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.trim_start().starts_with("wizard") {
        bail!(
            "{} --version printed {:?} instead of a wizard version",
            binary.display(),
            stdout.trim()
        );
    }
    Ok(())
}

/// Append every line `reader` produces to `sink`, so the text survives the
/// future being dropped. Two of these share one buffer: cargo's diagnostics go
/// to stderr and the test harness's results to stdout, and the failing tail is
/// whichever of them spoke last.
async fn drain_into<R>(reader: R, sink: &std::sync::Mutex<String>) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = tokio::io::BufReader::new(reader).lines();
    while let Some(line) = lines
        .next_line()
        .await
        .context("reading cargo test output")?
    {
        let mut sink = sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sink.push_str(&line);
        sink.push('\n');
    }
    Ok(())
}

/// The captured output so far. Poison is stepped over rather than panicked on:
/// a lock poisoned by a drained pipe is still holding the only account of what
/// the suite printed.
fn taken(sink: &std::sync::Mutex<String>) -> String {
    sink.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// How a finished (or abandoned) `cargo test` run came back.
#[derive(Debug)]
enum TestRun {
    /// The suite ran to completion, with its combined stdout+stderr.
    Finished { success: bool, output: String },
    /// The suite was still running when the timeout expired and was killed,
    /// with whatever it had printed by then.
    TimedOut { secs: u64, output: String },
}

/// The verdict on a test run, split out from [`Evolver::run_tests`] so the
/// rule is one testable decision: only a clean exit passes.
///
/// A timeout counts as a failure, never a pass. The tempting reading of "we
/// could not tell" is that the patch is probably fine; the honest one is that
/// a patch which hangs the suite is exactly the patch that must not replace a
/// working binary.
fn test_verdict(run: TestRun) -> Result<()> {
    match run {
        TestRun::Finished { success: true, .. } => Ok(()),
        TestRun::Finished { output, .. } => bail!(
            "cargo test --release --locked failed:\n{}",
            tail_lines(&output, MAX_FAILURE_LINES)
        ),
        // The tail matters most here: it names the test that was still
        // running, which is the one thing the next attempt needs and the one
        // thing a bare "did not finish" withholds.
        TestRun::TimedOut { secs, output } => {
            let tail = tail_lines(&output, MAX_FAILURE_LINES);
            let tail = if tail.trim().is_empty() {
                "(it printed nothing before the kill)".to_string()
            } else {
                tail
            };
            bail!(
                "cargo test --release --locked did not finish within {secs}s and was killed; \
                 a timeout counts as a failing test suite, so the patch is rejected \
                 (raise {TEST_TIMEOUT_ENV} if this machine is simply slow). Last output before \
                 the kill:\n{tail}"
            )
        }
    }
}

/// Bound on the deep-evolve test step: [`DEFAULT_TEST_TIMEOUT`], or
/// [`TEST_TIMEOUT_ENV`] when it parses to a non-zero number of seconds. A
/// garbled or zero value falls back to the default rather than disabling the
/// bound.
fn test_timeout() -> Duration {
    parse_test_timeout(std::env::var(TEST_TIMEOUT_ENV).ok().as_deref())
}

/// Pure half of [`test_timeout`].
fn parse_test_timeout(raw: Option<&str>) -> Duration {
    raw.map(str::trim)
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_TEST_TIMEOUT)
}

/// What an [`EvolveOutcome`] means, in the one line every surface reports it
/// with. Runtime-tier changes land on disk but are not live until the tools
/// are reloaded, so each says so.
pub fn describe_outcome(outcome: &EvolveOutcome) -> String {
    match outcome {
        EvolveOutcome::SkillAdded { name, path } => {
            format!(
                "evolve: added skill '{name}' at {} — run /reload to activate",
                path.display()
            )
        }
        EvolveOutcome::McpServerRegistered { name } => {
            format!("evolve: registered MCP server '{name}' — run /reload to activate")
        }
        EvolveOutcome::ScriptedToolAdded { name, path } => {
            format!(
                "evolve: added scripted tool '{name}' at {} — run /reload to activate",
                path.display()
            )
        }
        EvolveOutcome::SubagentAdded { name } => {
            format!("evolve: added subagent '{name}' — run /reload to activate")
        }
        EvolveOutcome::DeepRebuilt { binary } => {
            format!(
                "evolve: deep rebuild succeeded ({}) — restart wizard to run the new binary",
                binary.display()
            )
        }
        EvolveOutcome::FellBackToRuntime { reason, outcome } => {
            format!(
                "evolve: fell back to runtime tier ({reason}); {}",
                describe_outcome(outcome)
            )
        }
        EvolveOutcome::Denied => "evolve: change denied".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;

    #[test]
    fn slugify_normalizes_names() {
        assert_eq!(
            slugify("Conventional Commits!", '-').unwrap(),
            "conventional-commits"
        );
        assert_eq!(slugify("  ../../Evil Name  ", '-').unwrap(), "evil-name");
        assert_eq!(slugify("mermaid PNG", '_').unwrap(), "mermaid_png");
        assert!(slugify("///", '-').is_err());
    }

    #[test]
    fn sanitize_file_name_strips_directories() {
        assert_eq!(sanitize_file_name("../../evil.sh").unwrap(), "evil.sh");
        assert_eq!(sanitize_file_name("tool name.sh").unwrap(), "tool-name.sh");
        assert!(sanitize_file_name("..").is_err());
    }

    #[test]
    fn strips_thinking_blocks() {
        let text = "<think>secret plan</think>{\"a\":1}";
        assert_eq!(strip_thinking(text), "{\"a\":1}");
        let unterminated = "<think>never closed";
        assert_eq!(strip_thinking(unterminated), "");
    }

    #[test]
    fn extracts_json_from_fenced_reply_with_prose() {
        let reply = "Here you go:\n```json\n{\"channel\":\"skill\",\"name\":\"x\",\"body\":\"b\"}\n```\nDone.";
        let value = extract_json_object(reply).unwrap();
        assert_eq!(value["channel"], "skill");
    }

    #[test]
    fn extracts_bare_json_with_trailing_text() {
        let reply = "{\"channel\":\"subagent\",\"name\":\"r\",\"description\":\"d\",\"system_prompt\":\"p\"} hope that helps";
        let value = extract_json_object(reply).unwrap();
        assert_eq!(value["channel"], "subagent");
    }

    #[test]
    fn parses_each_channel_proposal() {
        let skill: ChannelProposal = serde_json::from_str(
            r#"{"channel":"skill","name":"commits","description":"d","body":"b"}"#,
        )
        .unwrap();
        assert_eq!(skill.channel(), EvolveChannel::Skill);

        let mcp: ChannelProposal = serde_json::from_str(
            r#"{"channel":"mcp_server","name":"computer-use","transport":"stdio","command":"uvx","args":["mcp-computer-use"]}"#,
        )
        .unwrap();
        assert_eq!(mcp.channel(), EvolveChannel::McpServer);

        let tool: ChannelProposal = serde_json::from_str(
            r##"{"channel":"scripted_tool","name":"mermaid_png","description":"d","script_content":"#!/bin/sh\necho hi","parameters":{"type":"object"}}"##,
        )
        .unwrap();
        assert_eq!(tool.channel(), EvolveChannel::ScriptedTool);

        let sub: ChannelProposal = serde_json::from_str(
            r#"{"channel":"subagent","name":"reviewer","description":"d","system_prompt":"p","tool_scope":["read_file"],"max_steps":10}"#,
        )
        .unwrap();
        assert_eq!(sub.channel(), EvolveChannel::Subagent);
    }

    #[test]
    fn an_evolved_subagent_gets_the_documented_step_ceiling() {
        // A subagent proposal that names no `max_steps` must land on the same
        // 50-step ceiling as a hand-written file, and it has to be *in the
        // file*: `SubagentConfig::max_steps` serialises unconditionally, so the
        // serde default that supplies 50 for an absent key never fires on
        // anything evolve writes. Writing 0 here made `/evolve`-created
        // subagents the only unbounded ones on the system, while
        // docs/evolve.md promised the opposite.
        let evolver = Evolver::new(Config::default());
        let outcome = evolver
            .add_subagent(SubagentProposal {
                name: "reviewer".into(),
                description: "audits diffs".into(),
                system_prompt: "You review diffs.".into(),
                tool_scope: Some(vec!["read_file".into()]),
                max_steps: None,
            })
            .expect("write the subagent");
        let EvolveOutcome::SubagentAdded { name } = outcome else {
            panic!("expected SubagentAdded, got {outcome:?}");
        };
        let path = Config::wizard_dir()
            .unwrap()
            .join("subagents")
            .join(format!("{name}.toml"));
        let raw = std::fs::read_to_string(&path).unwrap();
        let written: SubagentConfig = toml::from_str(&raw).unwrap();
        assert_eq!(
            written.max_steps.cap(),
            Some(DEFAULT_MAX_STEPS),
            "an evolved subagent must be bounded:\n{raw}"
        );

        // An explicit 0 still means unlimited, the same as in a file someone
        // wrote by hand — the default is for silence, not an override.
        let outcome = evolver
            .add_subagent(SubagentProposal {
                name: "marathon".into(),
                description: "runs until done".into(),
                system_prompt: "You keep going.".into(),
                tool_scope: None,
                max_steps: Some(0),
            })
            .expect("write the subagent");
        let EvolveOutcome::SubagentAdded { name } = outcome else {
            panic!("expected SubagentAdded, got {outcome:?}");
        };
        let raw = std::fs::read_to_string(
            Config::wizard_dir()
                .unwrap()
                .join("subagents")
                .join(format!("{name}.toml")),
        )
        .unwrap();
        let written: SubagentConfig = toml::from_str(&raw).unwrap();
        assert_eq!(written.max_steps.cap(), None, "{raw}");
    }

    #[test]
    fn the_subagent_prompt_states_the_ceiling_the_writer_applies() {
        // The planner prompt and add_subagent are one contract: the model is
        // told what an omitted `max_steps` will mean, and the doc quotes the
        // same number. A prompt that says "no step ceiling by default" while
        // the writer applies one (or the reverse) makes the model reach for an
        // explicit value to get the behaviour it already had.
        let stated = format!("{DEFAULT_MAX_STEPS}-step ceiling by default");
        assert!(
            TIER1_SYSTEM_PROMPT.contains(&stated),
            "the evolve prompt must promise the ceiling add_subagent writes ({stated})"
        );
        let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/evolve.md"))
            .expect("read docs/evolve.md");
        assert!(
            doc.contains(&format!("default ceiling of {DEFAULT_MAX_STEPS} steps")),
            "docs/evolve.md must state the same ceiling ({DEFAULT_MAX_STEPS})"
        );
    }

    #[test]
    fn extracts_diff_from_fenced_block() {
        let reply = "Sure:\n```diff\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n```\n";
        let diff = extract_diff(reply).unwrap();
        assert!(diff.starts_with("--- a/src/main.rs"));
        assert!(diff.ends_with('\n'));
        assert!(diff.contains("@@ -1,2 +1,2 @@"));
    }

    #[test]
    fn extracts_bare_diff() {
        let reply = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n";
        let diff = extract_diff(reply).unwrap();
        assert!(diff.starts_with("diff --git"));
    }

    #[test]
    fn rejects_non_diff_text() {
        assert!(extract_diff("no patch here, sorry").is_none());
    }

    #[test]
    fn script_extension_matches_interpreter() {
        assert_eq!(script_extension(Some("python3")), "py");
        assert_eq!(script_extension(Some("node")), "js");
        assert_eq!(script_extension(Some("bash")), "sh");
        assert_eq!(script_extension(Some("luajit")), "lua");
        assert_eq!(script_extension(None), "sh");
    }

    #[test]
    fn add_scripted_tool_defaults_to_embedded_luajit() {
        let config = Config::default();
        let evolver = Evolver::new(config);
        let outcome = evolver
            .add_scripted_tool(ScriptedToolProposal {
                name: "double_it".into(),
                description: "double a number".into(),
                interpreter: None,
                runtime: None,
                script_name: None,
                script_content: "print((args.n or 0) * 2)".into(),
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": { "n": { "type": "number" } }
                })),
                timeout_secs: None,
            })
            .expect("write lua tool");
        let EvolveOutcome::ScriptedToolAdded { name, path } = outcome else {
            panic!("expected ScriptedToolAdded, got {outcome:?}");
        };
        assert_eq!(name, "double_it");
        let manifest_raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            manifest_raw.contains("luajit") || manifest_raw.contains(".lua"),
            "manifest should mark LuaJIT:\n{manifest_raw}"
        );
        let manifest: ScriptManifest = toml::from_str(&manifest_raw).unwrap();
        assert!(
            manifest.script.ends_with(".lua"),
            "script file should be .lua, got {}",
            manifest.script
        );
        assert_eq!(manifest.runtime.as_deref(), Some("luajit"));
        assert!(manifest.interpreter.is_none());

        // And it actually runs through the embedded JIT.
        let tool = crate::tools::scripted::ScriptedTool::load(&path).unwrap();
        let cwd = path.parent().unwrap().to_path_buf();
        let out = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(tool.execute(
                serde_json::json!({ "n": 21 }),
                &crate::tools::ToolContext::new(&cwd),
            ))
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("42"), "{}", out.content);
    }

    #[cfg(unix)]
    #[test]
    fn a_lua_tool_replacing_a_shell_tool_loses_the_execute_bit() {
        // Same tool name, twice: first an external shell script (written 0755
        // because the kernel runs it), then a LuaJIT tool (read by the
        // embedded interpreter). The second write lands on the first file, and
        // a Lua source file that is still 0755 is a file something will
        // eventually try to exec.
        let evolver = Evolver::new(Config::default());
        let shell_tool = evolver
            .add_scripted_tool(ScriptedToolProposal {
                name: "swappable".into(),
                description: "shell first".into(),
                interpreter: Some("sh".into()),
                runtime: None,
                script_name: Some("swappable.script".into()),
                script_content: "echo hi\n".into(),
                parameters: None,
                timeout_secs: None,
            })
            .expect("write the shell tool");
        let EvolveOutcome::ScriptedToolAdded { path, .. } = shell_tool else {
            panic!("expected ScriptedToolAdded");
        };
        let script = path.with_file_name("swappable.script");
        assert!(exe_swap::is_executable(&script), "{}", script.display());

        evolver
            .add_scripted_tool(ScriptedToolProposal {
                name: "swappable".into(),
                description: "lua second".into(),
                interpreter: None,
                runtime: Some("luajit".into()),
                script_name: Some("swappable.script".into()),
                script_content: "print('hi')".into(),
                parameters: None,
                timeout_secs: None,
            })
            .expect("write the lua tool");
        assert!(
            !exe_swap::is_executable(&script),
            "a Lua tool must not keep the execute bit of the tool it replaced"
        );
        // What exactly `clear_executable` leaves behind is that function's
        // property, asserted in `platform::exe_swap`'s own tests. What matters
        // here is only that this module calls it on the replacing write.

        let _ = std::fs::remove_file(&script);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn add_scripted_tool_keeps_external_shell_when_asked() {
        let config = Config::default();
        let evolver = Evolver::new(config);
        let outcome = evolver
            .add_scripted_tool(ScriptedToolProposal {
                name: "echo_shell".into(),
                description: "shell echo".into(),
                interpreter: Some("sh".into()),
                runtime: None,
                script_name: Some("echo_shell.sh".into()),
                script_content: "#!/bin/sh\necho hi\n".into(),
                parameters: None,
                timeout_secs: None,
            })
            .expect("write shell tool");
        let EvolveOutcome::ScriptedToolAdded { path, .. } = outcome else {
            panic!("expected ScriptedToolAdded");
        };
        let manifest: ScriptManifest =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(manifest.script.ends_with(".sh"));
        assert_eq!(manifest.interpreter.as_deref(), Some("sh"));
        assert!(manifest.runtime.is_none());
    }

    #[test]
    fn tail_lines_keeps_the_end() {
        let text = "1\n2\n3\n4";
        assert_eq!(tail_lines(text, 2), "3\n4");
        assert_eq!(tail_lines(text, 10), text);
    }

    /// Temp dir removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("wizard-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sample_event(description: &str, outcome: EvolveOutcome) -> EvolutionEvent {
        EvolutionEvent {
            timestamp: Utc::now(),
            tier: EvolveTier::Runtime,
            description: description.to_string(),
            outcome,
            diff: None,
            build_ok: None,
        }
    }

    #[test]
    fn parses_file_selection_with_prose_and_fences() {
        let files = parse_file_selection(
            "Sure:\n```json\n{\"files\":[\"src/cli.rs\",\"src/lib.rs\"]}\n```",
        )
        .unwrap();
        assert_eq!(files, vec!["src/cli.rs", "src/lib.rs"]);

        assert!(parse_file_selection("{\"files\":[]}").is_err());
        assert!(parse_file_selection("{\"paths\":[\"x\"]}").is_err());
        assert!(parse_file_selection("no json").is_err());
    }

    #[test]
    fn heuristic_selection_ranks_paths_by_description_words() {
        let listing = "Cargo.toml\nREADME.md\nsrc/cli.rs\nsrc/schedule.rs\nsrc/usage.rs";
        let files = heuristic_context_files("add a schedule pause command", listing);
        assert_eq!(files, vec!["src/schedule.rs"]);

        // Nothing matches: empty selection, not a panic.
        assert!(heuristic_context_files("zzz", listing).is_empty());

        // Non-source files are never selected.
        let files = heuristic_context_files("update the readme", listing);
        assert!(files.is_empty(), "{files:?}");
    }

    #[test]
    fn context_files_are_read_whole_under_a_budget() {
        let tmp = TempDir::new();
        std::fs::create_dir_all(tmp.0.join("src")).unwrap();
        std::fs::write(tmp.0.join("src/small.rs"), "fn small() {}\n").unwrap();
        std::fs::write(tmp.0.join("src/big.rs"), "x".repeat(10_000)).unwrap();

        let files = vec![
            "src/big.rs".to_string(),
            "src/small.rs".to_string(),
            "src/absent.rs".to_string(),
            "/etc/passwd".to_string(),
            "../outside.rs".to_string(),
        ];
        let out = read_context_files(&tmp.0, &files, 1_000);
        assert!(out.contains("src/small.rs"), "small file fits: {out}");
        assert!(out.contains("fn small()"));
        assert!(
            !out.contains("src/big.rs"),
            "over-budget file skipped whole"
        );
        assert!(!out.contains("absent"), "missing file skipped");
        assert!(!out.contains("passwd"), "absolute path rejected");
        assert!(!out.contains("outside"), "traversal rejected");

        assert_eq!(read_context_files(&tmp.0, &[], 1_000), "");
    }

    #[test]
    fn outcome_labels_are_compact() {
        assert_eq!(
            outcome_label(&EvolveOutcome::SkillAdded {
                name: "commits".to_string(),
                path: PathBuf::from("/x"),
            }),
            "skill 'commits'"
        );
        assert_eq!(
            outcome_label(&EvolveOutcome::FellBackToRuntime {
                reason: "offline".to_string(),
                outcome: Box::new(EvolveOutcome::SubagentAdded {
                    name: "reviewer".to_string(),
                }),
            }),
            "fallback: subagent 'reviewer'"
        );
    }

    #[test]
    fn read_events_skips_malformed_lines_and_missing_files() {
        let tmp = TempDir::new();
        let log = tmp.0.join("evolution.jsonl");
        assert!(read_events(&log).unwrap().is_empty(), "missing = empty");

        let good = serde_json::to_string(&sample_event("ok", EvolveOutcome::Denied)).unwrap();
        std::fs::write(&log, format!("{good}\nnot json\n{good}\n")).unwrap();
        let events = read_events(&log).unwrap();
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn undo_skill_removes_the_file_and_refuses_when_gone() {
        let tmp = TempDir::new();
        let dir = tmp.0.join("skills").join("commits");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("SKILL.md");
        std::fs::write(&path, "body").unwrap();

        let outcome = EvolveOutcome::SkillAdded {
            name: "commits".to_string(),
            path: path.clone(),
        };
        undo_outcome(&outcome).expect("undo removes the skill");
        assert!(!path.exists());
        assert!(!dir.exists(), "empty skill dir removed too");

        let err = undo_outcome(&outcome).unwrap_err();
        assert!(err.to_string().contains("already gone"), "{err}");
    }

    #[test]
    fn undo_scripted_tool_removes_script_and_manifest() {
        let tmp = TempDir::new();
        let script = tmp.0.join("hello.sh");
        std::fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
        let manifest = ScriptManifest {
            name: "hello".to_string(),
            description: "d".to_string(),
            script: "hello.sh".to_string(),
            interpreter: None,
            runtime: None,
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            timeout_secs: None,
        };
        let manifest_path = tmp.0.join("hello.toml");
        std::fs::write(&manifest_path, toml::to_string_pretty(&manifest).unwrap()).unwrap();

        undo_outcome(&EvolveOutcome::ScriptedToolAdded {
            name: "hello".to_string(),
            path: manifest_path.clone(),
        })
        .expect("undo removes the tool");
        assert!(!manifest_path.exists());
        assert!(
            !script.exists(),
            "script referenced by the manifest removed"
        );
    }

    #[test]
    fn undo_deep_restores_the_prev_binary() {
        let tmp = TempDir::new();
        let binary = tmp.0.join("wizard");
        let prev = tmp.0.join("wizard.prev");
        std::fs::write(&binary, "new build").unwrap();
        std::fs::write(&prev, "old build").unwrap();

        undo_outcome(&EvolveOutcome::DeepRebuilt {
            binary: binary.clone(),
        })
        .expect("undo restores .prev");
        assert_eq!(std::fs::read_to_string(&binary).unwrap(), "old build");
        assert!(!prev.exists());
        assert_eq!(
            std::fs::read_to_string(tmp.0.join("wizard.undone")).unwrap(),
            "new build",
            "the undone build is kept aside"
        );
        // The restore went through `install_executable` rather than a pair of
        // renames: `wizard.prev` was written non-executable above, and only
        // the shared swap chmods what it stages. An undo interrupted halfway
        // must never be able to leave the user with no binary at that path at
        // all.
        assert!(
            exe_swap::is_executable(&binary),
            "the restored binary is executable"
        );

        // A second undo has no .prev left: refuse.
        let err = undo_outcome(&EvolveOutcome::DeepRebuilt { binary }).unwrap_err();
        assert!(err.to_string().contains("no rollback binary"), "{err}");
    }

    #[test]
    fn evolution_event_round_trips_through_jsonl() {
        let event = EvolutionEvent {
            timestamp: Utc::now(),
            tier: EvolveTier::Deep,
            description: "add a status panel".to_string(),
            outcome: EvolveOutcome::DeepRebuilt {
                binary: PathBuf::from("/tmp/wizard-new"),
            },
            diff: Some("--- a/x\n+++ b/x\n".to_string()),
            build_ok: Some(true),
        };
        let line = serde_json::to_string(&event).unwrap();
        let parsed: EvolutionEvent = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed.tier, EvolveTier::Deep);
        assert_eq!(parsed.description, "add a status panel");
        assert_eq!(parsed.diff.as_deref(), Some("--- a/x\n+++ b/x\n"));
        assert_eq!(parsed.build_ok, Some(true));
        match parsed.outcome {
            EvolveOutcome::DeepRebuilt { binary } => {
                assert_eq!(binary, PathBuf::from("/tmp/wizard-new"));
            }
            other => panic!("wrong outcome variant: {other:?}"),
        }
    }

    #[test]
    fn runtime_event_omits_deep_only_fields() {
        let line = serde_json::to_string(&sample_event(
            "learn conventional commits",
            EvolveOutcome::SkillAdded {
                name: "commits".to_string(),
                path: PathBuf::from("/tmp/skills/commits/SKILL.md"),
            },
        ))
        .unwrap();
        assert!(!line.contains("\"diff\""), "absent diff is not serialized");
        assert!(
            !line.contains("\"build_ok\""),
            "absent build_ok is not serialized"
        );
        assert!(
            line.contains("\"kind\":\"skill_added\""),
            "outcome is kind-tagged"
        );
    }

    #[test]
    fn append_event_creates_parents_and_appends_lines() {
        let tmp = TempDir::new();
        let log = tmp.0.join("nested").join("evolution.jsonl");

        append_event(
            &log,
            &sample_event(
                "first",
                EvolveOutcome::SubagentAdded {
                    name: "reviewer".to_string(),
                },
            ),
        )
        .unwrap();
        append_event(&log, &sample_event("second", EvolveOutcome::Denied)).unwrap();

        let raw = std::fs::read_to_string(&log).unwrap();
        let events: Vec<EvolutionEvent> = raw
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].description, "first");
        assert_eq!(events[1].description, "second");
        assert!(matches!(events[1].outcome, EvolveOutcome::Denied));
    }

    #[test]
    fn a_failing_test_suite_rejects_the_patch() {
        // The whole point of the gate: the patch compiled, so the build rung
        // passed, and the suite is what catches it.
        let err = test_verdict(TestRun::Finished {
            success: false,
            output: "running 3 tests\ntest agent::loops ... FAILED\nassertion failed: ok\n"
                .to_string(),
        })
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("cargo test --release --locked failed"),
            "{err}"
        );
        // The failing output rides along so the next attempt can learn from it.
        assert!(err.contains("test agent::loops ... FAILED"), "{err}");

        assert!(
            test_verdict(TestRun::Finished {
                success: true,
                output: "test result: ok. 900 passed".to_string(),
            })
            .is_ok()
        );
    }

    #[test]
    fn a_test_timeout_counts_as_a_failure_never_a_pass() {
        let err = test_verdict(TestRun::TimedOut {
            secs: 2700,
            output: "running 900 tests\ntest agent::loops_forever has been running for over 60 \
                     seconds\n"
                .to_string(),
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("did not finish within 2700s"), "{err}");
        assert!(err.contains("rejected"), "{err}");
        // The tail is what the next attempt reads, and on a timeout it is the
        // only thing that says *which* test hung. docs/evolve.md promises it
        // for this arm as well as the failure one.
        assert!(err.contains("agent::loops_forever"), "{err}");
    }

    #[test]
    fn a_timeout_that_captured_nothing_says_so() {
        // The empty case has to read as "it printed nothing", not as a message
        // that trails off after a colon.
        let err = test_verdict(TestRun::TimedOut {
            secs: 60,
            output: String::new(),
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("printed nothing before the kill"), "{err}");
    }

    #[test]
    fn the_test_timeout_is_bounded_and_overridable() {
        assert_eq!(parse_test_timeout(None), DEFAULT_TEST_TIMEOUT);
        assert_eq!(parse_test_timeout(Some(" 600 ")), Duration::from_secs(600));
        // Neither garbage nor zero may disable the bound.
        assert_eq!(parse_test_timeout(Some("soon")), DEFAULT_TEST_TIMEOUT);
        assert_eq!(parse_test_timeout(Some("0")), DEFAULT_TEST_TIMEOUT);
    }

    /// A git checkout with one committed file, for the revert tests.
    fn git_checkout(dir: &Path) -> bool {
        if !command_exists("git") {
            return false;
        }
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        std::fs::write(dir.join("src.rs"), "fn main() {}\n").unwrap();
        git(&["init", "-q"])
            && git(&["config", "user.email", "wizard@localhost"])
            && git(&["config", "user.name", "Wizard"])
            && git(&["add", "-A"])
            && git(&["commit", "-qm", "base"])
    }

    #[test]
    fn deep_evolve_reverts_and_records_the_patch_that_failed_its_tests() {
        let tmp = TempDir::new();
        if !git_checkout(&tmp.0) {
            eprintln!("skipping: git is unavailable");
            return;
        }
        // The patch applied cleanly and compiled; only the suite rejected it.
        std::fs::write(tmp.0.join("src.rs"), "fn main() { panic!() }\n").unwrap();
        std::fs::write(tmp.0.join("new_file.rs"), "// added by the patch\n").unwrap();

        let evolver = Evolver::new(Config::default());
        let request = EvolveRequest {
            description: format!("deep gate regression {}", uuid::Uuid::new_v4()),
            tier: EvolveTier::Deep,
        };
        let failure = test_verdict(TestRun::Finished {
            success: false,
            output: "test evolve::gate ... FAILED".to_string(),
        })
        .unwrap_err();
        let err = evolver.reject(
            &request,
            &tmp.0,
            GateFailure {
                stage: "tests",
                diff: "--- a/src.rs\n+++ b/src.rs\n",
                err: &failure,
                build_ok: true,
                tests_ok: Some(false),
            },
        );

        // 1. The working tree is back to the committed state: the user's
        //    checkout is not left holding a patch that fails its tests.
        assert_eq!(
            std::fs::read_to_string(tmp.0.join("src.rs")).unwrap(),
            "fn main() {}\n"
        );
        assert!(
            !tmp.0.join("new_file.rs").exists(),
            "files the patch added are cleaned up too"
        );

        // 2. The error says the patch was rejected and carries the output.
        let text = format!("{err:#}");
        assert!(text.contains("failed the tests gate"), "{text}");
        assert!(text.contains("test evolve::gate ... FAILED"), "{text}");

        // 3. The evolution log records the failure (with the test output) and
        //    never an outcome claiming a rebuild happened.
        let log = Config::evolution_log_path().unwrap();
        let raw = std::fs::read_to_string(&log).unwrap();
        let recorded: DeepFailureEvent = raw
            .lines()
            .filter_map(|line| serde_json::from_str::<DeepFailureEvent>(line).ok())
            .find(|event| event.description == request.description)
            .expect("the failure is in the evolution log");
        assert_eq!(recorded.event, "deep_failed");
        assert_eq!(recorded.stage, "tests");
        assert!(recorded.build_ok);
        assert_eq!(recorded.tests_ok, Some(false));
        assert!(recorded.detail.contains("test evolve::gate ... FAILED"));
        assert!(recorded.diff.is_some(), "the rejected diff is kept");

        // 4. And `evolve list` / `evolve undo` do not see it as an evolution:
        //    nothing landed, so there is nothing to undo.
        let events = read_events(&log).unwrap();
        assert!(
            !events.iter().any(|e| e.description == request.description),
            "a rejected patch is not a listed evolution"
        );
    }

    /// A busy executable is retried; a failing one is not.
    ///
    /// The distinction is the whole point of [`probe_runs`]: "could not start
    /// it" and "started it and it said no" are different answers, and only the
    /// first is worth waiting on. Driven through a counting closure rather
    /// than by racing a real `fork`, because the race this defends against is
    /// exactly the kind that does not reproduce on demand.
    #[test]
    fn a_probe_retries_a_busy_binary_and_gives_up_on_a_failing_one() {
        // The classification, which is the part that decides whether anything
        // is waited on at all. ETXTBSY is the one this exists for.
        assert!(worth_retrying(std::io::ErrorKind::ExecutableFileBusy));
        assert!(worth_retrying(std::io::ErrorKind::WouldBlock));
        assert!(worth_retrying(std::io::ErrorKind::Interrupted));
        for answered in [
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::InvalidInput,
        ] {
            assert!(
                !worth_retrying(answered),
                "{answered:?} is an answer, not a race"
            );
        }

        // A program that runs and exits non-zero is answered on the first
        // attempt: no sleeping, no retrying, because it was not busy.
        let mut tries = 0;
        let failing = || {
            tries += 1;
            shell::command("exit 1")
        };
        assert!(!probe_runs(failing));
        assert_eq!(
            tries, 1,
            "a program that ran and failed must not be retried"
        );

        // And a binary that is simply absent is also one attempt, not four:
        // `NotFound` is an answer.
        let mut missing_tries = 0;
        let missing = || {
            missing_tries += 1;
            Command::new("/nonexistent/wizard-probe-target")
        };
        assert!(!probe_runs(missing));
        assert_eq!(missing_tries, 1, "an absent binary must not be retried");
    }

    #[test]
    fn cargo_binary_works_rejects_failing_shim() {
        let tmp = TempDir::new();
        let bad = tmp.0.join("cargo");
        // Resolved rather than `#!/bin/sh`, for the reason `write_cargo_shim`
        // gives: a shim that cannot exec would read as "cargo is broken" here
        // whatever the code under test did.
        let shebang = shell::shebang();
        std::fs::write(&bad, format!("{shebang}\necho broken >&2\nexit 1\n")).unwrap();
        exe_swap::set_executable(&bad).unwrap();
        assert!(!cargo_binary_works(&bad));
        assert!(!cargo_binary_works(&tmp.0.join("missing")));

        let good = tmp.0.join("good-cargo");
        std::fs::write(&good, format!("{shebang}\necho 'cargo 1.0'\n")).unwrap();
        exe_swap::set_executable(&good).unwrap();
        assert!(cargo_binary_works(&good));
    }

    #[test]
    fn concurrent_appends_never_interleave_into_one_line() {
        let tmp = TempDir::new();
        let log = tmp.0.join("evolution.jsonl");
        // Two evolutions logging at the same moment is ordinary: a subagent
        // evolves while the session that spawned it does, or the scheduler
        // fires mid-turn. `writeln!` wrote the record and its newline
        // separately, so under `O_APPEND` two of them landed as `{a}{b}\n\n`
        // and both were lost to `evolve list` and `evolve undo`.
        let writers = 8;
        let per_writer = 25;
        std::thread::scope(|scope| {
            for writer in 0..writers {
                let log = log.clone();
                scope.spawn(move || {
                    for round in 0..per_writer {
                        append_event(
                            &log,
                            &sample_event(
                                &format!("writer {writer} round {round}"),
                                EvolveOutcome::Denied,
                            ),
                        )
                        .expect("append");
                    }
                });
            }
        });

        let raw = std::fs::read_to_string(&log).expect("readable");
        for line in raw.lines() {
            serde_json::from_str::<EvolutionEvent>(line)
                .unwrap_or_else(|err| panic!("torn line {line:?}: {err}"));
        }
        assert_eq!(
            raw.lines().count(),
            writers * per_writer,
            "one line per event, no matter who was writing"
        );
    }

    #[test]
    fn every_gate_stage_reverts_the_patch_and_records_what_it_saw() {
        // The tests rung is covered above; these are the other two, with the
        // build/test state `run_deep` hands `reject` for each. A regression
        // that dropped the revert or the log line from the build path would
        // leave the user's checkout holding a patch that does not compile, and
        // leave nothing in `evolution.jsonl` for the next attempt to read.
        let cases = [
            (
                "build",
                false,
                None,
                "error[E0599]: no method named `evolve` found",
            ),
            (
                "smoke test",
                true,
                Some(true),
                "target/release/wizard --version exited with signal 11",
            ),
        ];
        for (stage, build_ok, tests_ok, detail) in cases {
            let tmp = TempDir::new();
            if !git_checkout(&tmp.0) {
                eprintln!("skipping: git is unavailable");
                return;
            }
            std::fs::write(tmp.0.join("src.rs"), "fn main() { not rust }\n").unwrap();
            std::fs::write(tmp.0.join("new_file.rs"), "// added by the patch\n").unwrap();

            let evolver = Evolver::new(Config::default());
            let request = EvolveRequest {
                description: format!("deep {stage} gate regression {}", uuid::Uuid::new_v4()),
                tier: EvolveTier::Deep,
            };
            let failure = anyhow!("{detail}");
            let err = evolver.reject(
                &request,
                &tmp.0,
                GateFailure {
                    stage,
                    diff: "--- a/src.rs\n+++ b/src.rs\n",
                    err: &failure,
                    build_ok,
                    tests_ok,
                },
            );

            assert_eq!(
                std::fs::read_to_string(tmp.0.join("src.rs")).unwrap(),
                "fn main() {}\n",
                "the {stage} gate leaves the checkout as it found it"
            );
            assert!(!tmp.0.join("new_file.rs").exists());

            let text = format!("{err:#}");
            assert!(text.contains(&format!("failed the {stage} gate")), "{text}");
            assert!(text.contains(detail), "{text}");

            let log = Config::evolution_log_path().unwrap();
            let raw = std::fs::read_to_string(&log).unwrap();
            let recorded: DeepFailureEvent = raw
                .lines()
                .filter_map(|line| serde_json::from_str::<DeepFailureEvent>(line).ok())
                .find(|event| event.description == request.description)
                .expect("the failure is in the evolution log");
            assert_eq!(recorded.stage, stage);
            assert_eq!(recorded.build_ok, build_ok);
            assert_eq!(recorded.tests_ok, tests_ok);
            assert!(recorded.detail.contains(detail), "{}", recorded.detail);
            assert!(recorded.diff.is_some(), "the rejected diff is kept");
        }
    }

    /// A crate whose `Cargo.toml` names a dependency its `Cargo.lock` does not:
    /// what a patch that invents a dependency leaves behind. The dependency is
    /// a path, so the mismatch is detectable with no network and no registry.
    fn crate_with_a_stale_lockfile(dir: &Path) {
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::create_dir_all(dir.join("helper").join("src")).unwrap();
        std::fs::write(
            dir.join("helper").join("src").join("lib.rs"),
            "pub fn help() {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("helper").join("Cargo.toml"),
            "[package]\nname = \"helper\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            // The `native` feature exists here for the same reason the real
            // crate has one: `feature_args` passes `--features native` on a
            // native build, and cargo rejects a flag naming a feature the
            // package does not declare before it ever looks at the lockfile.
            // The probe has to be the shape of the thing being built.
            "[workspace]\n\n[package]\nname = \"lockfile-probe\"\nversion = \"0.1.0\"\n\
             \nedition = \"2021\"\n\n[features]\ndefault = []\nnative = []\n\
             \n[dependencies]\nhelper = { path = \"helper\" }\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("Cargo.lock"),
            "version = 3\n\n[[package]]\nname = \"lockfile-probe\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
    }

    #[test]
    fn the_build_gate_refuses_a_patch_that_would_rewrite_the_lockfile() {
        if find_cargo().is_none() {
            eprintln!("skipping: cargo is unavailable");
            return;
        }
        let tmp = TempDir::new();
        crate_with_a_stale_lockfile(&tmp.0);
        let lock_before = std::fs::read_to_string(tmp.0.join("Cargo.lock")).unwrap();

        // Verbose on purpose: this is the CLI path (`wizard --evolve --deep`),
        // which used to inherit the compiler's streams and hand `reject` the
        // literal string "see output above" as the whole failure detail.
        let evolver = Evolver::new(Config::default()).with_verbose(true);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let outcome = runtime.block_on(async {
            // Bounded so a machine whose cargo is blocked on a package-cache
            // lock reports that rather than hanging the suite.
            tokio::time::timeout(Duration::from_secs(180), evolver.build(&tmp.0)).await
        });
        let Ok(result) = outcome else {
            eprintln!("skipping: cargo did not answer within the bound");
            return;
        };

        let err = format!("{:#}", result.expect_err("--locked rejects the stale lock"));
        // Split the module's own "cargo build --release --locked failed:"
        // prefix off first. That prefix names the flag whatever went wrong, so
        // asserting `--locked` against the whole message would pass for a
        // build that failed for any reason at all, including one where the
        // flag had been dropped. What has to be true is that *cargo* refused,
        // and said so because of the lockfile.
        let (_, cargo_said) = err
            .split_once("failed:\n")
            .unwrap_or_else(|| panic!("no captured cargo output in: {err}"));
        assert!(cargo_said.contains("lock file"), "{err}");
        assert!(cargo_said.contains("--locked"), "{err}");
        // The point of putting `--locked` on the build rather than only on the
        // test step: the lockfile is still the committed one, so nothing was
        // resolved, downloaded, or had its build script run before the gate
        // got a look.
        assert_eq!(
            std::fs::read_to_string(tmp.0.join("Cargo.lock")).unwrap(),
            lock_before,
            "a rejected patch never gets its lockfile written for it"
        );
    }

    #[test]
    fn installing_the_rebuilt_binary_swaps_it_in_or_fails_loudly() {
        let tmp = TempDir::new();
        let built = tmp.0.join("built-wizard");
        std::fs::write(&built, "new build").unwrap();
        let bin = tmp.0.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let installed = bin.join("wizard");
        std::fs::write(&installed, "old build").unwrap();

        let evolver = Evolver::new(Config::default());
        let landed = evolver
            .install_over(&built, &installed)
            .expect("the swap succeeds");
        assert_eq!(
            landed, installed,
            "the caller execs what is installed, not the build output"
        );
        assert_eq!(std::fs::read_to_string(&installed).unwrap(), "new build");
        assert_eq!(
            std::fs::read_to_string(bin.join("wizard.prev")).unwrap(),
            "old build",
            "the way back is kept beside it"
        );

        // A swap that cannot happen is an error. Returning the build output
        // instead reads as success to every caller while `wizard` on PATH (and
        // the continuous loop's re-exec) keeps running the old binary.
        let unwritable = tmp.0.join("no-such-dir").join("wizard");
        let err = format!(
            "{:#}",
            evolver
                .install_over(&built, &unwritable)
                .expect_err("a failed swap is not a success")
        );
        assert!(err.contains("installing the rebuilt binary over"), "{err}");
        assert!(err.contains("sudo install -m755"), "{err}");
    }

    #[test]
    fn the_install_target_is_resolved_through_symlinks() {
        let tmp = TempDir::new();
        let real = tmp.0.join("wizard");
        std::fs::write(&real, "build").unwrap();
        assert!(is_same_binary(&real, &real));

        // The managed-install case: /usr/local/bin/wizard is a symlink to the
        // binary that is actually running, and a swap that did not resolve it
        // would replace the link with a regular file and leave the real binary
        // at the old version.
        let link = tmp.0.join("wizard-link");
        crate::platform::paths::symlink(&real, &link).unwrap();
        assert!(is_same_binary(&link, &real));

        let other = tmp.0.join("other");
        std::fs::write(&other, "build").unwrap();
        assert!(
            !is_same_binary(&real, &other),
            "the same bytes are not the same file"
        );
        assert!(
            !is_same_binary(&tmp.0.join("gone"), &tmp.0.join("gone")),
            "two paths that do not resolve are not 'the same binary'"
        );

        // And what `install_binary` actually installs over is absolute, so the
        // staged copy never lands relative to whatever cwd the agent had.
        let exe = resolved_exe().expect("the test binary is on disk");
        assert!(exe.is_absolute(), "{}", exe.display());
    }

    #[test]
    fn an_unresolvable_executable_fails_the_install_instead_of_reporting_one() {
        // `resolved_exe` used to answer `None` here and the install read that
        // as "nothing needed installing", so a deep evolve whose binary never
        // moved was recorded as `DeepRebuilt` and printed "Rebuilt Wizard"
        // while `wizard` on PATH stayed at the old version and `evolve undo`
        // pointed at a `wizard.prev` nobody had written. The condition is
        // ordinary: once anything has renamed over the running binary, Linux
        // reports `/proc/self/exe` as `<path> (deleted)` and canonicalizing it
        // fails with ENOENT.
        let tmp = TempDir::new();
        let built = tmp.0.join("built-wizard");
        std::fs::write(&built, "new build").unwrap();

        let evolver = Evolver::new(Config::default());
        let err = format!(
            "{:#}",
            evolver
                .install_to(&built, Err(anyhow!("canonicalizing /proc/self/exe")))
                .expect_err("an install that did not happen is not a success")
        );
        assert!(
            err.contains("could not locate the running executable"),
            "{err}"
        );
        // And it says how to finish the job by hand, naming the binary the
        // build did produce.
        assert!(err.contains("sudo install -m755"), "{err}");
        assert!(err.contains(&built.display().to_string()), "{err}");

        // The one case that legitimately returns the build output: already
        // running from it, so there is nothing to install.
        let landed = evolver
            .install_to(&built, Ok(built.clone()))
            .expect("already installed");
        assert_eq!(landed, built);
    }

    #[test]
    fn the_termux_probe_is_the_platform_layers_one() {
        // This module carried a byte-for-byte copy of `platform::host::is_termux`
        // and called it from `ensure_toolchain`, so hardening the detector in
        // the file the module docs point at left deep evolve running the
        // `sh.rustup.rs` installer on an Android device. Structural, because
        // the divergence is only observable on Termux: what has to hold is
        // that there is one probe, not that this host is or is not Termux.
        let source = include_str!("mod.rs");
        let (production, _) = source
            .split_once("#[cfg(test)]\nmod tests {")
            .expect("this module ends with its test module");
        for needle in ["TERMUX_VERSION", "com.termux"] {
            assert!(
                !production.contains(needle),
                "the Termux probe belongs to crate::platform::host, not here: found {needle}"
            );
        }
        assert_eq!(
            production.matches("crate::platform::is_termux()").count(),
            1,
            "one call, to the platform detector"
        );
    }

    #[test]
    fn both_cargo_rungs_carry_the_running_binarys_features() {
        // A native install's deep evolve must not rebuild default features over
        // itself: the new binary compiles and passes and then `wizard gui`
        // opens nothing.
        if cfg!(feature = "native") {
            assert_eq!(feature_args(), ["--features", "native"]);
        } else {
            assert!(feature_args().is_empty());
        }
        // `cfg!` is resolved at compile time, so one run of the suite can only
        // ever see one of those branches — which is why the wiring is checked
        // as text as well. Both rungs, because `--release` on the test step is
        // artifact reuse only if it resolves the same features as the build.
        let source = include_str!("mod.rs");
        let (production, _) = source
            .split_once("#[cfg(test)]\nmod tests {")
            .expect("this module ends with its test module");
        for rung in ["cmd.args(BUILD_ARGS)", "cmd.args(TEST_ARGS)"] {
            assert!(
                production.contains(&format!("{rung}\n            .args(feature_args())")),
                "{rung} must carry feature_args()"
            );
        }
        assert_eq!(
            production.matches(".args(feature_args())").count(),
            2,
            "every cargo invocation in the gate, and only those"
        );
    }

    const KILL_PROBE_ENV: &str = "WIZARD_EVOLVE_KILL_PROBE_DIR";
    const KILL_PROBE_PREFIX: &str = "grandchild-survived=";

    /// A stand-in `cargo` at `<dir>/cargo`: it answers `--version` (which is
    /// how [`find_cargo`] decides a toolchain works), and on any other
    /// invocation announces itself, forks a grandchild that outlives the
    /// timeout, and sleeps far past it. The grandchild is the `wizard-<hash>`
    /// test binary cargo forks in the real thing, and its marker is how we see
    /// it survive; `started` is the positive control that says the shim was
    /// reached at all.
    #[cfg(unix)]
    fn write_cargo_shim(dir: &Path) {
        let shim = dir.join("cargo");
        std::fs::write(
            &shim,
            format!(
                "{}\n\
                 case \"$1\" in --version) echo 'cargo 1.0.0'; exit 0;; esac\n\
                 touch started\n\
                 (sleep 5 && touch grandchild-survived) &\n\
                 sleep 30\n",
                // Not `#!/bin/sh`: Termux has no `/bin`, and a shim that
                // cannot exec turns this probe into a real `cargo test` run in
                // an empty directory, which fails for the wrong reason.
                shell::shebang()
            ),
        )
        .unwrap();
        exe_swap::set_executable(&shim).unwrap();
    }

    /// Drives the real [`Evolver::run_tests`] against that shim and reports
    /// whether the grandchild survived. Inert unless the parent set
    /// [`KILL_PROBE_ENV`]: it needs `PATH` and the timeout variable set for a
    /// whole process, which is only sound in a child of our own.
    #[cfg(unix)]
    #[test]
    fn timed_out_run_probe() {
        let Some(dir) = std::env::var_os(KILL_PROBE_ENV) else {
            return;
        };
        let dir = PathBuf::from(dir);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let verdict = runtime.block_on(async {
            let evolver = Evolver::new(Config::default());
            // Bounded at two seconds by the parent, so this comes back
            // almost immediately with the timeout verdict.
            let err = format!(
                "{:#}",
                evolver
                    .run_tests(&dir)
                    .await
                    .expect_err("the shim never finishes")
            );
            // The failure has to be the timeout, not "no cargo on this PATH":
            // otherwise nothing was ever spawned and the verdict below would
            // be a green light for a run that did not happen.
            assert!(err.contains("did not finish within 2s"), "{err}");
            assert!(dir.join("started").exists(), "the shim never ran: {err}");
            // Past the point where a surviving grandchild would write its
            // marker (it sleeps five seconds; the kill landed after two).
            tokio::time::sleep(Duration::from_secs(5)).await;
            dir.join("grandchild-survived").exists()
        });
        println!("{KILL_PROBE_PREFIX}{verdict}");
    }

    #[cfg(unix)]
    #[test]
    fn a_timed_out_test_run_kills_what_cargo_forked() {
        // Through `run_tests` itself, not through a hand-rolled command with
        // its own process-group call: the thing that has to hold is that *the
        // production path* both puts cargo in a group of its own and kills
        // that group when the timeout fires. A test that spawns its own
        // process group only proves the kill primitive works, and stays green
        // when the two lines that matter are deleted from `run_tests`.
        let tmp = TempDir::new();
        let bin = tmp.0.join("bin");
        let source = tmp.0.join("source");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        write_cargo_shim(&bin);

        let survived =
            kill_probe_verdict("timed_out_run_probe", KILL_PROBE_ENV, &source, &bin, "2");
        assert_eq!(
            survived, "false",
            "a process the run forked outlived the timeout kill"
        );
    }

    const BUILD_PROBE_ENV: &str = "WIZARD_EVOLVE_BUILD_PROBE_DIR";

    /// The build rung's own deadline, against the same shim. Inert unless the
    /// parent set [`BUILD_PROBE_ENV`], for the same reason as
    /// [`timed_out_run_probe`].
    #[cfg(unix)]
    #[test]
    fn timed_out_build_probe() {
        let Some(dir) = std::env::var_os(BUILD_PROBE_ENV) else {
            return;
        };
        let dir = PathBuf::from(dir);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let verdict = runtime.block_on(async {
            let evolver = Evolver::new(Config::default());
            let err = format!(
                "{:#}",
                evolver
                    .build(&dir)
                    .await
                    .expect_err("the shim never finishes")
            );
            assert!(err.contains("did not finish within 2s"), "{err}");
            assert!(dir.join("started").exists(), "the shim never ran: {err}");
            tokio::time::sleep(Duration::from_secs(5)).await;
            dir.join("grandchild-survived").exists()
        });
        println!("{KILL_PROBE_PREFIX}{verdict}");
    }

    #[cfg(unix)]
    #[test]
    fn a_build_that_never_finishes_hits_the_timeout() {
        // The build rung drains cargo's stderr to a line reader and then waits
        // on the child. With the timeout wrapped around the wait alone it was
        // unreachable code: stderr only EOFs once every holder of the write
        // end is gone, so a build script that hangs (and the grandchild this
        // shim forks, which inherits the pipe) parks the drain forever and the
        // whole deep evolve hangs with it — no revert, no `evolution.jsonl`
        // entry. What has to hold is that `build` *returns*, with the timeout
        // verdict, and takes the tree cargo forked with it.
        let tmp = TempDir::new();
        let bin = tmp.0.join("bin");
        let source = tmp.0.join("source");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        write_cargo_shim(&bin);

        let survived =
            kill_probe_verdict("timed_out_build_probe", BUILD_PROBE_ENV, &source, &bin, "2");
        assert_eq!(
            survived, "false",
            "a process the build forked outlived the timeout kill"
        );
    }

    /// Runs one of the kill probes in a child process with the shim first on
    /// `PATH`, and returns the verdict it printed.
    #[cfg(unix)]
    fn kill_probe_verdict(
        probe: &str,
        probe_env: &str,
        source: &Path,
        bin: &Path,
        run_timeout_secs: &str,
    ) -> String {
        let path = match std::env::var_os("PATH") {
            Some(existing) => {
                let mut dirs = vec![bin.to_path_buf()];
                dirs.extend(std::env::split_paths(&existing));
                std::env::join_paths(dirs).expect("join PATH")
            }
            None => bin.as_os_str().to_os_string(),
        };
        let output = std::process::Command::new(std::env::current_exe().expect("test binary path"))
            .args(["--exact", &format!("evolve::tests::{probe}"), "--nocapture"])
            .env(probe_env, source)
            .env(TEST_TIMEOUT_ENV, run_timeout_secs)
            .env("PATH", path)
            .output()
            .expect("run the kill probe");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(output.status.success(), "probe failed:\n{stdout}");
        stdout
            .lines()
            .find_map(|line| line.strip_prefix(KILL_PROBE_PREFIX))
            .unwrap_or_else(|| panic!("probe printed no verdict:\n{stdout}"))
            .to_string()
    }

    const CANCEL_PROBE_ENV: &str = "WIZARD_EVOLVE_CANCEL_PROBE_DIR";

    /// The other way a run ends: the future is dropped from outside, long
    /// before its own timeout. Inert unless the parent set
    /// [`CANCEL_PROBE_ENV`], for the same reason as [`timed_out_run_probe`].
    #[cfg(unix)]
    #[test]
    fn cancelled_run_probe() {
        let Some(dir) = std::env::var_os(CANCEL_PROBE_ENV) else {
            return;
        };
        let dir = PathBuf::from(dir);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let verdict = runtime.block_on(async {
            let evolver = Evolver::new(Config::default());
            // The run's own budget is ten minutes here, so nothing inside
            // `run_tests` fires: this drops the future from outside, which is
            // what the TUI's interrupt, a subagent running out of steps and a
            // runtime shutdown all do to it.
            let outcome =
                tokio::time::timeout(Duration::from_secs(2), evolver.run_tests(&dir)).await;
            assert!(
                outcome.is_err(),
                "the shim never finishes, so run_tests must not have returned"
            );
            assert!(dir.join("started").exists(), "the shim never ran");
            // Past the point where a surviving grandchild would write its
            // marker (it sleeps five seconds; the cancel landed after two).
            tokio::time::sleep(Duration::from_secs(5)).await;
            dir.join("grandchild-survived").exists()
        });
        println!("{KILL_PROBE_PREFIX}{verdict}");
    }

    #[cfg(unix)]
    #[test]
    fn a_cancelled_test_run_kills_what_cargo_forked_too() {
        // The timeout arm is not the only exit. Putting cargo in a process
        // group of its own also takes it *out* of the terminal's foreground
        // group, so nothing else on the machine will end that tree on Wizard's
        // behalf; if the only group kill hangs off the timeout, every other
        // cancellation leaks the whole `cargo test` tree, which on a release
        // build is every core on the box for the next forty-five minutes.
        let tmp = TempDir::new();
        let bin = tmp.0.join("bin");
        let source = tmp.0.join("source");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        write_cargo_shim(&bin);

        let survived = kill_probe_verdict(
            "cancelled_run_probe",
            CANCEL_PROBE_ENV,
            &source,
            &bin,
            "600",
        );
        assert_eq!(
            survived, "false",
            "a process the run forked outlived the cancellation"
        );
    }

    /// Prints the timeout [`test_timeout`] resolves to, for
    /// [`the_test_timeout_reads_the_variable_it_documents`]. Inert unless the
    /// parent set [`TIMEOUT_PROBE_ENV`].
    #[test]
    fn timeout_probe() {
        if std::env::var_os(TIMEOUT_PROBE_ENV).is_none() {
            return;
        }
        println!("{TIMEOUT_PROBE_PREFIX}{}", test_timeout().as_secs());
    }

    const TIMEOUT_PROBE_ENV: &str = "WIZARD_EVOLVE_TIMEOUT_PROBE";
    const TIMEOUT_PROBE_PREFIX: &str = "timeout-secs=";

    #[test]
    fn the_test_timeout_reads_the_variable_it_documents() {
        // `parse_test_timeout` is covered above; what is not is the wiring,
        // and a typo in the variable name would pass every other test in this
        // file. The read happens in a child process rather than through
        // `set_var`, which is unsound against the threads this suite runs on.
        let exe = std::env::current_exe().expect("test binary path");
        let resolve = |value: Option<&str>| -> u64 {
            let mut cmd = std::process::Command::new(&exe);
            cmd.args(["--exact", "evolve::tests::timeout_probe", "--nocapture"])
                .env(TIMEOUT_PROBE_ENV, "1");
            match value {
                Some(value) => cmd.env(TEST_TIMEOUT_ENV, value),
                None => cmd.env_remove(TEST_TIMEOUT_ENV),
            };
            let output = cmd.output().expect("run the timeout probe");
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            assert!(output.status.success(), "probe failed:\n{stdout}");
            stdout
                .lines()
                .find_map(|line| line.strip_prefix(TIMEOUT_PROBE_PREFIX))
                .and_then(|secs| secs.parse().ok())
                .unwrap_or_else(|| panic!("probe printed no timeout:\n{stdout}"))
        };

        assert_eq!(resolve(Some("1200")), 1_200);
        assert_eq!(resolve(None), DEFAULT_TEST_TIMEOUT.as_secs());
        assert_eq!(resolve(Some("0")), DEFAULT_TEST_TIMEOUT.as_secs());

        // And the name the user is told to raise is the name that is read.
        let timed_out = test_verdict(TestRun::TimedOut {
            secs: 1,
            output: String::new(),
        })
        .unwrap_err()
        .to_string();
        assert!(timed_out.contains(TEST_TIMEOUT_ENV), "{timed_out}");
    }
}
