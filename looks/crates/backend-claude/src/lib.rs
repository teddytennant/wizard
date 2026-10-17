//! Backend that drives the real Claude Code CLI through `claude -p` stream-json.
//!
//! ```text
//! claude -p --input-format stream-json --output-format stream-json --verbose \
//!        --include-partial-messages --permission-mode <mode> \
//!        --allow-dangerously-skip-permissions --settings '{"showThinkingSummaries":true}' \
//!        (--session-id <uuid> | --resume <id>) [--model m] [--effort e]
//! ```
//!
//! One long-lived process per session. Prompts go in as
//! `{"type":"user","message":{"role":"user","content":"..."}}` lines, everything else
//! (interrupt, model, mode, effort, handshake) is a `control_request`. See the module docs
//! of [`mapper`] (stream to events), [`history`] (transcripts on disk) and the notes below.
//!
//! # Wire facts that are easy to get wrong (observed on claude 2.1.289)
//!
//! * **Nothing is printed until the first prompt**, not even `system/init`. Config comes from
//!   a `control_request` `initialize` (commands, models with supported effort levels, current
//!   permission mode), `get_settings` and `get_context_usage` (`maxTokens` is the real
//!   window and `model` the resolved id). We pick the session id ourselves with `--session-id`.
//! * `system/init` repeats at the start of **every** turn. A turn ends with exactly one
//!   `result`, but turns also start on their own (finished background subagent, local slash
//!   command), so `TurnStart` is not tied to a `Prompt`.
//! * **Interrupt** is `{"subtype":"interrupt"}`. The process survives and takes the next
//!   prompt. The turn ends with `result` `subtype:"error_during_execution"`,
//!   `is_error:true`, `terminal_reason:"aborted_streaming"` or `"aborted_tools"`, which is a
//!   cancel, not an error. A running tool gets an `is_error` tool_result.
//! * **Partial messages** (`stream_event`) exist for the main thread only. Each content block
//!   is also sent whole as its own `assistant` message (same `message.id`), so text from
//!   those is skipped when the id was streamed. `thinking` blocks are empty unless
//!   `showThinkingSummaries` is set (we set it). `message_delta.usage` is cumulative.
//! * **Subagent tool is `Agent`** (older: `Task`). Its inner messages arrive whole with
//!   `parent_tool_use_id` set. By default it runs in the background: the tool_result only says
//!   "Async agent launched", the turn ends, the inner calls stream in afterwards, and
//!   `system/task_notification` completes the call before the CLI starts another turn by
//!   itself. We set `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1` so it is one foreground turn;
//!   `OPENC_CLAUDE_BACKGROUND=1` turns that off and the mapper handles both.
//! * **Todos** are `TaskCreate`/`TaskUpdate` calls (state kept in the mapper), or
//!   `TodoWrite` with the whole list when `CLAUDE_CODE_ENABLE_TASKS=0`.
//! * There is **no Glob or Grep tool** in this version (searches run through `Bash`); the
//!   kinds are still mapped for older CLIs.
//! * **Slash commands**: built-ins that run locally (`/context`, `/usage`, `/model`, `/mcp`)
//!   answer with a synthetic `assistant` message (`model:"<synthetic>"`) and a `result` with
//!   `num_turns:0`; no model call. `/clear` keeps the process but changes `session_id`
//!   (the id in `conversation_reset` is not the one later messages carry). Unknown commands
//!   are sent to the model as plain text. `/compact` emits `compact_boundary`.
//! * **Errors arrive as a normal `result`**: API failures and plan limits have
//!   `is_error:true` (often still `subtype:"success"`) and exit code stays 0. A plan limit
//!   reads "You've hit your ...". `system/api_retry` precedes them while retrying.
//! * **Permissions**: with no host answering prompts, a tool that needs approval is denied
//!   at once: `tool_result` with `is_error:true` and text like "Permission for this tool use
//!   was denied...", plus `system/permission_denied` and `result.permission_denials`.
//!   openc starts in `bypassPermissions`, so this only shows after `SetMode`.
//! * `total_cost_usd` is cumulative per process, so a restart carries an offset.
//! * Control requests: `set_model` (alias, `alias[1m]` or full id; unknown ids answer with an
//!   error), `set_permission_mode` (`bypassPermissions` only works if the process was started
//!   with `--allow-dangerously-skip-permissions`), `apply_flag_settings`
//!   `{"effortLevel":..}` for effort.

pub mod driver;
pub mod guard;
pub mod history;
pub mod mapper;
mod proc;
pub use proc::set_guard_exe;
pub mod sessions;
mod tools;

use agent_core::{Backend, BackendHandle};
use anyhow::Result;
use driver::{fresh_uuid, Opts};
use proc::{Launch, Proc, SessionArg};
use std::path::PathBuf;
use tokio::sync::mpsc::unbounded_channel;

pub use driver::MODES;

/// Launch settings. [`ClaudeOptions::from_env`] reads:
/// `OPENC_CLAUDE_BIN` (default `claude` on PATH), `OPENC_CLAUDE_MODEL`, `OPENC_CLAUDE_EFFORT`,
/// `OPENC_CLAUDE_MODE` (default `bypassPermissions`), `OPENC_CLAUDE_ARGS` (extra argv, split
/// on whitespace) and `OPENC_CLAUDE_BACKGROUND` (set to allow background subagents).
#[derive(Clone, Debug)]
pub struct ClaudeOptions {
    pub bin: std::ffi::OsString,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: String,
    pub extra: Vec<String>,
    pub background: bool,
    /// How long `Cancel` waits for the CLI to end the turn before it restarts the process
    /// (same session). Default 6s.
    pub cancel_grace: std::time::Duration,
}

impl ClaudeOptions {
    pub fn new(cwd: PathBuf) -> Self {
        ClaudeOptions {
            bin: "claude".into(),
            cwd,
            model: None,
            effort: None,
            mode: "bypassPermissions".into(),
            extra: Vec::new(),
            background: false,
            cancel_grace: std::time::Duration::from_secs(6),
        }
    }

    pub fn from_env(cwd: PathBuf) -> Self {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let mut o = Self::new(cwd);
        if let Some(b) = std::env::var_os("OPENC_CLAUDE_BIN").filter(|b| !b.is_empty()) {
            o.bin = b;
        }
        o.model = var("OPENC_CLAUDE_MODEL");
        o.effort = var("OPENC_CLAUDE_EFFORT");
        if let Some(m) = var("OPENC_CLAUDE_MODE") {
            o.mode = m;
        }
        if let Some(a) = var("OPENC_CLAUDE_ARGS") {
            o.extra = a.split_whitespace().map(str::to_string).collect();
        }
        o.background = var("OPENC_CLAUDE_BACKGROUND").is_some();
        o
    }
}

pub struct ClaudeBackend;

impl ClaudeBackend {
    /// Like [`Backend::spawn`] with explicit options instead of the environment.
    pub fn spawn_with(opts: ClaudeOptions, resume: Option<String>) -> Result<BackendHandle> {
        let replay = resume.is_some();
        let session = match resume {
            Some(id) => SessionArg::Resume(id),
            None => SessionArg::New(fresh_uuid()),
        };
        let launch = Launch {
            bin: opts.bin.clone(),
            cwd: opts.cwd.clone(),
            session: session.clone(),
            model: opts.model.clone(),
            effort: opts.effort.clone(),
            mode: opts.mode.clone(),
            extra: opts.extra.clone(),
            background: opts.background,
        };
        // Started here, not in the task, so a missing binary is an error from spawn().
        let first = Proc::start(&launch)?;
        let (tx, req_rx) = unbounded_channel();
        let (ev_tx, rx) = unbounded_channel();
        let o = Opts {
            bin: opts.bin,
            cwd: opts.cwd,
            model: opts.model,
            effort: opts.effort,
            mode: opts.mode,
            extra: opts.extra,
            background: opts.background,
            cancel_grace: opts.cancel_grace,
        };
        agent_core::spawn_supervised(
            "the claude driver",
            ev_tx.clone(),
            driver::run(o, first, session, replay, req_rx, ev_tx),
        );
        Ok(BackendHandle { tx, rx })
    }
}

impl Backend for ClaudeBackend {
    fn spawn(cwd: PathBuf, resume: Option<String>) -> Result<BackendHandle> {
        Self::spawn_with(ClaudeOptions::from_env(cwd), resume)
    }
}
