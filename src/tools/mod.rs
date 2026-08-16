//! Tool system: the [`Tool`] trait implemented by native tools
//! ([`file`], [`shell`], [`git`]), agent-authored [`scripted`] tools, and
//! MCP tools (`crate::mcp`). All three present a uniform interface through
//! [`registry::ToolRegistry`], so the model calls them identically.

pub mod code;
pub mod command;
pub mod compact;
pub mod computer;
pub mod evolve;
pub mod file;
pub mod git;
pub mod image;
pub mod interview;
pub mod lua;
pub mod manual;
pub mod memory;
pub mod plan;
pub mod publish;
pub mod registry;
pub mod scripted;
pub mod shell;
pub mod spill;
pub mod subagent_tasks;
pub mod tasks;
pub mod todo;
pub mod web;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::llm::{Image, ToolSpec};

/// Where a tool comes from. Affects display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    /// Compiled into the binary.
    Native,
    /// Agent-authored script under `~/.wizard/tools/`.
    Scripted,
    /// Served by an external MCP server.
    Mcp,
}

/// How a tool touches the world. Drives the plan-mode read-only gate and
/// checkpoint snapshots of `Edit`-class targets — never prompting.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToolAccess {
    /// Observes only (reads files, queries state).
    ReadOnly,
    /// Modifies a file at a resolvable path.
    Edit,
    /// Runs commands or has other side effects.
    Execute,
}

/// Which of Wizard's own slash commands the attached surface will actually run
/// when the agent queues one through `run_command`.
///
/// A live `events` channel implies none of them: headless and gateway runs have
/// one too, streaming to a printer that cannot apply a command. The tool gates
/// on this so it never reports success for work that would never run — and, on a
/// surface that implements a subset, refuses the rest *in the tool result*,
/// which is the only place the model reads before the turn ends.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CommandDispatch {
    /// Nothing drains the queue: headless, gateway, subagents.
    #[default]
    None,
    /// Every command [`SlashCommand::agent_runnable`](crate::commands::SlashCommand)
    /// allows — the interactive TUI, which has a surface for all of them.
    All,
    /// Only these command names, in a surface whose executor implements a subset
    /// of them (the GUI: `/vim` and the interactive pickers have nowhere to land
    /// in a browser).
    Only(&'static [&'static str]),
}

impl CommandDispatch {
    /// Whether the attached surface will run the command called `name` (no
    /// leading slash), or the reason it will not.
    pub fn accepts(self, name: &str) -> Result<(), String> {
        match self {
            CommandDispatch::None => Err(
                "slash commands are only available in an interactive Wizard session, \
                 not in this run"
                    .to_string(),
            ),
            CommandDispatch::All => Ok(()),
            CommandDispatch::Only(names) if names.contains(&name) => Ok(()),
            // Say *why*, from the one table, when the table knows: "nowhere to
            // run" is false of a command the user can run right there in the
            // page, and the model deserves the real reason rather than a shrug
            // it will read as a bug and retry around.
            CommandDispatch::Only(names) => Err(match crate::commands::spec(name).map(|s| s.gui) {
                Some(crate::commands::Execution::Ui) => format!(
                    "'/{name}' belongs to the user's window, not the agent — they open it, you cannot"
                ),
                Some(crate::commands::Execution::Unavailable) => {
                    format!("'/{name}' runs only in a terminal session, and this one is not")
                }
                _ => {
                    let offered: Vec<String> =
                        names.iter().map(|name| format!("/{name}")).collect();
                    format!(
                        "'/{name}' has nowhere to run on this surface; it runs only {}",
                        offered.join(", ")
                    )
                }
            }),
        }
    }
}

/// Per-call execution context.
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// Project root all relative paths resolve against.
    pub cwd: PathBuf,
    /// Session-wide registry of background shell tasks.
    pub tasks: Arc<tasks::TaskRegistry>,
    /// Session-wide registry of background subagent runs (`spawn_subagent`
    /// with `background: true`).
    pub subagents: Arc<subagent_tasks::SubagentTaskRegistry>,
    /// The agent's working todo list, shared by every call in the session.
    pub todos: Arc<Mutex<todo::TodoList>>,
    /// Event channel of the turn currently dispatching, injected by the
    /// dispatcher so tools that converse with the surface (`exit_plan`'s
    /// approval round-trip) can reach it. `None` outside the dispatch
    /// pipeline (subagents, direct registry execution).
    pub events: Option<tokio::sync::mpsc::Sender<crate::agent::AgentEvent>>,
    /// The running turn's cancel handle, set by the agent at construction. A
    /// tool that can park for as long as a human takes to answer has to observe
    /// this itself: the agent loop only checks cancellation *between* tool
    /// calls, so Ctrl-C during a two-minute install would otherwise do nothing
    /// until the turn task was aborted out from under the child. `None` outside
    /// an agent.
    pub cancel: Option<crate::agent::CancelHandle>,
    /// Whether a human is attached to this run and can answer a command that
    /// prompts on stdin. Set by the surface's agent builder;
    /// [`ConsoleAccess::None`] everywhere else, which is the behaviour every
    /// command had before consoles existed.
    pub console: ConsoleAccess,
    /// Settings for the native web tools (`[web]` in `config.toml`), set by
    /// the agent at construction; defaults elsewhere.
    pub web: Arc<crate::config::WebConfig>,
    /// Per-file checkpoint store, set by the agent at construction. The
    /// dispatcher and the subagent loop snapshot `Edit`-class targets into
    /// it before execution. `None` outside an agent (direct registry
    /// execution in tests).
    pub checkpoints: Option<Arc<crate::checkpoint::CheckpointStore>>,
    /// Where images produced during this session are written, set by the agent
    /// at construction (`~/.wizard/images/<session>/`). The agent loop and the
    /// subagent loop persist through it before announcing an image to the
    /// surfaces. `None` outside an agent (direct registry execution in tests),
    /// in which case images still reach the model but land nowhere on disk.
    pub images: Option<Arc<crate::images::ImageStore>>,
    /// The slash commands the surface behind this run will dispatch when the
    /// agent queues one via `run_command`. Set by the surface's agent builder;
    /// [`CommandDispatch::None`] everywhere else.
    pub command_dispatch: CommandDispatch,
    /// The agent's token counters, set by the agent at construction. Shared
    /// (not owned) because the spend a tool delegates to a model is the
    /// parent's spend: [`crate::agent::subagent::spawn`] records every
    /// subagent model call here, so `/cost` and the status bar account for a
    /// fan-out (`spawn_subagent`, every `/ultra` candidate and judge) instead
    /// of reporting the main loop alone. `None` outside an agent.
    pub usage: Option<Arc<crate::usage::UsageTracker>>,
}

impl ToolContext {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            tasks: Arc::new(tasks::TaskRegistry::new()),
            subagents: Arc::new(subagent_tasks::SubagentTaskRegistry::new()),
            todos: Arc::new(Mutex::new(todo::TodoList::new())),
            events: None,
            cancel: None,
            console: ConsoleAccess::None,
            web: Arc::new(crate::config::WebConfig::default()),
            checkpoints: None,
            images: None,
            command_dispatch: CommandDispatch::None,
            usage: None,
        }
    }

    /// Declare which queued slash commands the attached surface will run
    /// (see [`CommandDispatch`]). Anything but `None` enables `run_command`.
    ///
    /// Nothing calls this. Every surface declares it after the agent exists,
    /// through [`crate::agent::Agent::set_command_dispatch`] — the TUI, ACP
    /// and the browser GUI all do. Kept as the builder-time spelling of the
    /// same field for a caller assembling a registry by hand, and named here
    /// so the next reader does not go looking for the builder call that wires
    /// the real surfaces.
    pub fn with_command_dispatch(mut self, dispatch: CommandDispatch) -> Self {
        self.command_dispatch = dispatch;
        self
    }

    /// This context with `web` tool settings applied (agent construction).
    pub fn with_web(mut self, web: crate::config::WebConfig) -> Self {
        self.web = Arc::new(web);
        self
    }

    /// This context with the checkpoint store attached (agent construction).
    pub fn with_checkpoints(mut self, store: Arc<crate::checkpoint::CheckpointStore>) -> Self {
        self.checkpoints = Some(store);
        self
    }

    /// This context with the session's image store attached (agent
    /// construction).
    pub fn with_images(mut self, store: Arc<crate::images::ImageStore>) -> Self {
        self.images = Some(store);
        self
    }

    /// This context with the agent's token counters attached (agent
    /// construction), so a tool that delegates to a model bills the parent.
    pub fn with_usage(mut self, usage: Arc<crate::usage::UsageTracker>) -> Self {
        self.usage = Some(usage);
        self
    }

    /// A copy of this context carrying the turn's event channel.
    pub fn with_events(&self, events: tokio::sync::mpsc::Sender<crate::agent::AgentEvent>) -> Self {
        Self {
            events: Some(events),
            ..self.clone()
        }
    }

    /// This context with the turn's cancel handle attached (agent
    /// construction), so a tool that can block for a long time observes Ctrl-C
    /// rather than being torn down under it.
    pub fn with_cancel(mut self, cancel: crate::agent::CancelHandle) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Declare whether a human can answer a command that prompts (see
    /// [`ConsoleAccess`]).
    ///
    /// Not "set by the surface's agent builder", which is what this said: the
    /// surfaces call [`crate::agent::Agent::set_console_access`] instead, and
    /// nothing calls this. Same standing as
    /// [`Self::with_command_dispatch`] above.
    pub fn with_console(mut self, console: ConsoleAccess) -> Self {
        self.console = console;
        self
    }
}

/// Whether a human is attached to this run and could answer a shell command
/// that asks a question on stdin.
///
/// This is a declaration by the surface, exactly like [`CommandDispatch`], and
/// for the same reason: only the surface knows whether there is a person in
/// front of it, and a tool that guessed would guess wrong in the two directions
/// that matter. Guessing *yes* in a headless run leaves a child blocked on a
/// pipe nobody will ever write to, until the timeout kills it — which is worse
/// than today's behaviour, not better. Guessing *no* in the TUI is the bug this
/// exists to fix.
///
/// It is deliberately **not** derived from `ctx.events.is_some()`. A headless
/// run has an event channel too; what it does not have is somebody reading it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ConsoleAccess {
    /// No human: a foreground command gets `/dev/null` on fd 0 and reads EOF
    /// immediately, which is how every command in this codebase has always
    /// run. Headless, the gateway, ACP, fleet runs, the browser GUI, and every
    /// subagent under any of them.
    #[default]
    None,
    /// A surface will claim the [`ConsoleGate`](crate::agent::ConsoleGate) on
    /// [`AgentEvent::ConsoleOpened`](crate::agent::AgentEvent::ConsoleOpened)
    /// and relay what the user types. The interactive TUI.
    Interactive,
}

/// Result of a tool execution, fed back to the model as a `role: tool`
/// message.
///
/// Serializable because it rides
/// [`AgentEvent::ToolFinished`](crate::agent::AgentEvent::ToolFinished), and
/// that event has to survive being recorded and read back.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolOutput {
    /// Text returned to the model (stdout, file contents, diff, ...).
    pub content: String,
    /// True when the tool ran but reported failure (non-zero exit, missing
    /// file, ...). Distinct from [`ToolError`], which means the call itself
    /// could not be carried out.
    pub is_error: bool,
    /// Images the tool produced (a generated image, a screenshot, a rendered
    /// chart). Build them with [`Image::from_bytes`](crate::llm::Image::from_bytes),
    /// which sniffs the media type and enforces the size cap.
    ///
    /// The agent loop takes them from here: it writes them to the session's
    /// image directory, announces them to the surfaces
    /// ([`AgentEvent::Images`](crate::agent::AgentEvent::Images)), and feeds
    /// them back to the model on a following user message — a `tool`-role
    /// message cannot carry image blocks on OpenAI, but a user message can
    /// everywhere (see [`ChatMessage::user_with_images`]).
    pub images: Vec<Image>,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            images: Vec::new(),
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            images: Vec::new(),
        }
    }

    /// Successful output carrying one or more images alongside its text. The
    /// text is what the model reads; the images are what it sees.
    pub fn ok_with_images(content: impl Into<String>, images: Vec<Image>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            images,
        }
    }
}

/// Failures in dispatching or running a tool call.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    #[error("invalid arguments for '{tool}': {message}")]
    InvalidArgs { tool: String, message: String },
    #[error("tool '{tool}' timed out after {seconds}s")]
    Timeout { tool: String, seconds: u64 },
    #[error("tool '{tool}' failed")]
    Execution {
        tool: String,
        #[source]
        source: anyhow::Error,
    },
}

/// Byte cap applied to tool output returned to the model. Keeps a single
/// tool result from flooding the context window.
///
/// The default, and the ceiling for the tools whose output genuinely is the
/// answer: a command's stdout, a file the model asked to read, a fetched
/// page, a manual section. 30 KB is roughly 7.5k tokens at the usual four
/// chars per token, which is a real bite out of a window and is why the tools
/// below take less.
pub(crate) const MAX_OUTPUT_BYTES: usize = 30_000;

/// Cap for `git_diff`.
///
/// A diff is the one summary output that is still worth several thousand
/// tokens, because the model is usually about to act on every hunk in it. 16
/// KB is about 400 lines of unified diff, past which the useful move is
/// `git_diff` on a path rather than more bytes.
pub(crate) const MAX_DIFF_BYTES: usize = 16_000;

/// Cap for `search_files`.
///
/// Search results are a map, not the territory: the model reads them to
/// decide what to open next. 12 KB is a few hundred matching lines, which is
/// already past the point where a narrower pattern beats a longer result, and
/// it is what a repeated grep would otherwise leave riding along on every
/// subsequent step.
pub(crate) const MAX_SEARCH_BYTES: usize = 12_000;

/// Cap for listings: `list_files`, `git_status`.
///
/// Paths, one per line. 8 KB holds several hundred of them, comfortably more
/// than the entry caps these tools already apply, and a working tree with more
/// changed files than that is not one the model should be reading in full.
pub(crate) const MAX_LISTING_BYTES: usize = 8_000;

/// Cap for a tool's error text (a failed git invocation, a search that could
/// not run).
///
/// Stderr this long has stopped being a message and started being a dump, and
/// [`truncate_output`] keeps the head and the tail, which is where the cause
/// and the summary line live.
pub(crate) const MAX_ERROR_BYTES: usize = 4_000;

/// Resolve a model-supplied path against the project root, expanding a
/// leading `~`. Absolute paths are used as-is.
pub(crate) fn resolve_path(ctx: &ToolContext, path: &str) -> PathBuf {
    let expanded = shellexpand::tilde(path);
    let candidate = std::path::Path::new(expanded.as_ref());
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        ctx.cwd.join(candidate)
    }
}

/// Deserialize tool arguments, mapping shape mismatches to
/// [`ToolError::InvalidArgs`]. `null` is treated as an empty object so models
/// may omit arguments for zero-parameter tools.
pub(crate) fn parse_args<T: serde::de::DeserializeOwned>(
    tool: &str,
    args: serde_json::Value,
) -> Result<T, ToolError> {
    let args = if args.is_null() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        args
    };
    serde_json::from_value(args).map_err(|err| ToolError::InvalidArgs {
        tool: tool.to_string(),
        message: err.to_string(),
    })
}

/// Bytes reserved for the truncation marker inside the `max_bytes` budget.
const TRUNCATION_MARKER_RESERVE: usize = 192;

/// Elision marker between head and tail when the omitted bytes were spilled to
/// a file. Short because the notice below the preview carries the counts and
/// the path; this only has to mark where the cut is.
const SPILL_ELISION: &str = "\n... [output truncated] ...\n";

/// Name suggested for a spill file. Generic because `truncate_output` does not
/// know which tool it is truncating for: the forty call sites pass a budget and
/// nothing else, and changing that to thread a label through every tool in the
/// tree buys a nicer `ls` and nothing the model can see.
const SPILL_FILE_NAME: &str = "tool-output.txt";

/// Truncate `text` to at most `max_bytes` (cutting on char boundaries),
/// keeping the head and a larger tail (build and test failures land at the end
/// of output) around a marker that says how much was omitted.
///
/// When a session has installed a spill sink ([`spill`]), the omitted bytes are
/// not lost: the full text goes to a private file and the marker becomes a path
/// the model can `read_file` or `search_files`. Without a sink, and whenever
/// spilling fails, this is the lossy truncation it has always been.
pub(crate) fn truncate_output(text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    if let Some(framed) = spill_and_frame(&text, max_bytes) {
        return framed;
    }
    truncate_output_without_spill(text, max_bytes)
}

/// [`truncate_output`] with the spill path taken out: oversized text loses its
/// middle and that is the end of it.
///
/// For `read_file`, which cannot use a spill file. Its answer to "this output
/// is too long" is already `start_line`/`end_line` on a file that exists, so
/// spilling would copy a file the model can already address in order to hand
/// back an instruction to read a file, and a model that followed it literally
/// would spend a call to arrive where it started. Also the fallback for every
/// other caller when no sink is installed, which is what unit tests and
/// short-lived subprocesses run with.
pub(crate) fn truncate_output_without_spill(mut text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    // Budgets too small for head+tail framing fall back to a plain head cut.
    if max_bytes <= TRUNCATION_MARKER_RESERVE {
        let mut cut = max_bytes;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n... [output truncated]");
        return text;
    }
    let (head_end, tail_start) = frame(&text, max_bytes - TRUNCATION_MARKER_RESERVE);
    let omitted = tail_start - head_end;
    format!(
        "{}\n... [output truncated] {omitted} bytes omitted from the middle; rerun a narrower \
         command for the full output, or task_output for a background task ...\n{}",
        &text[..head_end],
        &text[tail_start..]
    )
}

/// Write `text` to the installed spill sink and build the model-facing
/// preview around a pointer to it, or `None` when there is no sink, the spill
/// failed, or the budget cannot hold the notice.
///
/// Every `None` here is a fall-through to plain truncation, never an error: a
/// tool call that already produced too much output must not also fail because
/// the scratch directory was read-only.
fn spill_and_frame(text: &str, max_bytes: usize) -> Option<String> {
    // A budget this small has no head+tail framing to hang a path off, and the
    // notice alone would not fit. Leave it on the plain head cut.
    if max_bytes <= TRUNCATION_MARKER_RESERVE {
        return None;
    }
    let sink = spill::installed()?;
    let path = match sink.spill(text, SPILL_FILE_NAME) {
        Ok(path) => path,
        Err(err) => {
            tracing::debug!(
                "could not spill {} bytes of tool output: {err:#}",
                text.len()
            );
            return None;
        }
    };
    let location = path.display().to_string();

    // Size the notice at its widest before deciding how much preview fits. The
    // omitted count is not known yet and depends on this reserve, but it can
    // never have more digits than the length of the whole text, so measuring
    // with that overstates the notice by nothing worse than a few bytes and can
    // never understate it. Understating is the one failure that matters: it
    // would put a result over the cap the caller asked for.
    let reserve = SPILL_ELISION.len() + "\n\n".len() + spill_notice(text.len(), &location).len();
    if max_bytes <= reserve {
        // A long spill path against a small budget (an error message, say).
        // Nothing would be left for the preview, so drop the file rather than
        // leave one nothing points at.
        let _ = std::fs::remove_file(&path);
        return None;
    }

    let (head_end, tail_start) = frame(text, max_bytes - reserve);
    Some(format!(
        "{}{SPILL_ELISION}{}\n\n{}",
        &text[..head_end],
        &text[tail_start..],
        spill_notice(tail_start - head_end, &location)
    ))
}

/// What the model reads in place of the bytes that were cut: how many, where
/// they are, and which tools get them back.
///
/// Naming the tools matters more than it looks. The old marker said "rerun a
/// narrower command", and a model that follows that instruction redoes the work
/// that produced the output. This says the work is already on disk and the
/// recovery is a read, so the cheaper move is also the obvious one.
///
/// It says *result* rather than *output* on purpose, and the difference is not
/// pedantry. The file holds everything the tool handed back, which is not
/// always everything the underlying command produced: `execute` bounds its own
/// capture at [`shell::CAPTURE_HEAD_BYTES`] + [`shell::CAPTURE_TAIL_BYTES`]
/// while the child is still running, so a command that outruns 2 MiB reaches
/// this function already missing its middle. Saying "full output" there sends
/// the model looking for lines that were never captured — observed costing a
/// run half a minute of reconciling a line count against the script that
/// printed it. The bytes the capture dropped are marked in the middle of the
/// file by [`shell::CappedBuffer::into_string`], so the text stays honest to
/// anyone who reads it; this notice just no longer promises otherwise.
fn spill_notice(omitted: usize, location: &str) -> String {
    format!(
        "(Omitted {omitted} bytes here. The tool's full result is at {location}. Use read_file on \
         that path with start_line/end_line, or search_files with that path to search within it.)"
    )
}

/// Split `text` into a head and a tail together no longer than `budget`,
/// cutting on char boundaries and weighting the tail, where a failing build
/// puts its summary.
///
/// Boundary adjustment only ever shrinks the head and grows the tail's start,
/// so the two pieces stay inside `budget` and cannot overlap.
fn frame(text: &str, budget: usize) -> (usize, usize) {
    let mut head_end = budget / 4;
    while head_end > 0 && !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = text.len() - (budget - budget / 4);
    while tail_start < text.len() && !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    (head_end, tail_start)
}

/// A callable capability exposed to the model.
///
/// Contract:
/// - `name` is unique within the registry (MCP tools are namespaced
///   `server__tool` on collision).
/// - `parameters` returns a JSON Schema object; `execute` receives arguments
///   already validated against nothing — implementations must deserialize
///   defensively and return [`ToolError::InvalidArgs`] on shape mismatch.
/// - `access` classifies side effects conservatively: anything not provably
///   read-only or a path-addressed edit stays [`ToolAccess::Execute`].
#[async_trait]
pub trait Tool: Send + Sync {
    /// Unique tool name as advertised to the model (snake_case).
    fn name(&self) -> &str;

    /// One-paragraph description shown to the model.
    fn description(&self) -> &str;

    /// JSON Schema describing the arguments object.
    fn parameters(&self) -> serde_json::Value;

    /// How this tool touches the world. Drives the plan-mode read-only gate
    /// and checkpoint snapshots — never prompting.
    fn access(&self) -> ToolAccess {
        ToolAccess::Execute
    }

    /// Origin of this tool.
    fn kind(&self) -> ToolKind {
        ToolKind::Native
    }

    /// Wire-format spec sent to the active provider in the request `tools` array.
    fn spec(&self) -> ToolSpec {
        ToolSpec::function(self.name(), self.description(), self.parameters())
    }

    /// Run the tool with `args` (a JSON object) in `ctx`.
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError>;
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::spill::{SpillSink, hold_sink};
    use super::*;

    /// The one file in a sink directory, and a failure naming what was there
    /// instead. Every spilling test writes exactly one.
    fn only_spill_file(dir: &Path) -> PathBuf {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap_or_else(|err| panic!("reading {}: {err}", dir.display()))
            .map(|entry| entry.expect("a dir entry").path())
            .collect();
        assert_eq!(entries.len(), 1, "expected one spill file: {entries:?}");
        entries.pop().expect("the one entry")
    }

    /// One cap for every tool meant one result could inject 30 KB — about
    /// 7.5k tokens — and then ride along on every step after it. A budget is
    /// per tool because the tools differ in what their output *is*: a
    /// command's stdout is the answer, a directory listing is a signpost.
    #[test]
    fn a_signpost_tool_gets_less_of_the_window_than_the_answer_tools() {
        // The marker text below is the no-sink one, so hold the slot empty.
        let _hold = hold_sink(None);
        for budget in [MAX_DIFF_BYTES, MAX_SEARCH_BYTES, MAX_LISTING_BYTES] {
            assert!(
                budget < MAX_OUTPUT_BYTES,
                "{budget} must be under the default"
            );
            assert!(
                budget > TRUNCATION_MARKER_RESERVE * 4,
                "{budget} still has room for head+tail framing around the marker"
            );
        }
        const { assert!(MAX_ERROR_BYTES < MAX_LISTING_BYTES) };
        // Each one still truncates to its own ceiling, framing intact.
        for budget in [
            MAX_OUTPUT_BYTES,
            MAX_DIFF_BYTES,
            MAX_SEARCH_BYTES,
            MAX_LISTING_BYTES,
            MAX_ERROR_BYTES,
        ] {
            let out = truncate_output("x".repeat(MAX_OUTPUT_BYTES * 2), budget);
            assert!(out.len() <= budget, "{budget}: {} bytes", out.len());
            assert!(out.contains("bytes omitted"), "{budget}: {out}");
        }
    }

    #[test]
    fn truncate_leaves_short_text_alone() {
        assert_eq!(truncate_output("short".to_string(), 1_000), "short");
    }

    #[test]
    fn truncate_keeps_head_and_tail_and_counts_omitted_bytes() {
        let _hold = hold_sink(None);
        let text = format!("HEAD{}TAIL", "x".repeat(10_000));
        let out = truncate_output(text, 1_000);
        assert!(
            out.len() <= 1_000,
            "stays within budget: {} bytes",
            out.len()
        );
        assert!(out.starts_with("HEAD"), "head preserved");
        assert!(out.ends_with("TAIL"), "tail preserved");
        assert!(out.contains("[output truncated]"));
        assert!(out.contains("bytes omitted"), "{out}");
    }

    #[test]
    fn truncate_tail_is_larger_than_head() {
        // Counts letters, and a spill notice carries its own; keep it out.
        let _hold = hold_sink(None);
        let text = "h".repeat(500) + &"t".repeat(10_000);
        let out = truncate_output(text, 1_000);
        let heads = out.chars().filter(|&c| c == 'h').count();
        let tails = out.chars().filter(|&c| c == 't').count();
        assert!(tails > heads, "tail-weighted: {heads} head vs {tails} tail");
    }

    #[test]
    fn truncate_cuts_on_char_boundaries() {
        let text = "é".repeat(20_000);
        let out = truncate_output(text, 1_001);
        assert!(out.len() <= 1_001);
        assert!(out.contains("[output truncated]"));
    }

    #[test]
    fn truncate_tiny_budget_falls_back_to_head_cut() {
        let text = "x".repeat(500);
        let out = truncate_output(text, 100);
        assert!(out.starts_with("xxx"));
        assert!(out.ends_with("[output truncated]"));
    }

    /// The whole point: the bytes that leave the result are still somewhere the
    /// model can reach, byte for byte, and it is told where.
    #[test]
    fn a_spilled_result_names_a_file_holding_the_complete_output() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sink = SpillSink::in_dir(tmp.path().join("session"));
        let dir = sink.dir().to_path_buf();
        let _hold = hold_sink(Some(sink));

        let text = format!("HEAD{}TAIL", "x".repeat(60_000));
        let out = truncate_output(text.clone(), MAX_OUTPUT_BYTES);

        assert!(out.starts_with("HEAD"), "head kept: {}", &out[..16]);
        assert!(out.contains("TAIL"), "tail kept");
        assert!(
            out.contains("full result is at "),
            "points somewhere: {out:.400}"
        );
        let spilled = only_spill_file(&dir);
        assert!(
            out.contains(&spilled.display().to_string()),
            "names the file it wrote"
        );
        assert_eq!(
            std::fs::read_to_string(&spilled).expect("read the spill file"),
            text,
            "nothing was lost, including the middle"
        );
        assert!(
            !out.contains("rerun a narrower"),
            "the model is sent to the file, not back to the work: {out:.400}"
        );
    }

    /// A result over its cap is the bug the cap exists to prevent, and the
    /// notice is part of the result: a path is as long as the box's temp dir
    /// makes it, so the preview has to shrink by exactly that much.
    #[test]
    fn a_spilled_result_stays_inside_every_budget() {
        for budget in [
            MAX_OUTPUT_BYTES,
            MAX_DIFF_BYTES,
            MAX_SEARCH_BYTES,
            MAX_LISTING_BYTES,
            MAX_ERROR_BYTES,
            TRUNCATION_MARKER_RESERVE + 1,
        ] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let sink = SpillSink::in_dir(tmp.path().join("session"));
            let _hold = hold_sink(Some(sink));

            let out = truncate_output("é".repeat(MAX_OUTPUT_BYTES), budget);
            assert!(
                out.len() <= budget,
                "{budget}: result is {} bytes",
                out.len()
            );
        }
    }

    /// Two spills of the same text from the same session are two files. A name
    /// derived from the content, or a fixed one, would have the second call
    /// either overwrite the first or fail against `create_new`.
    #[test]
    fn two_spills_of_identical_output_do_not_collide() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sink = SpillSink::in_dir(tmp.path().join("session"));
        let dir = sink.dir().to_path_buf();
        let _hold = hold_sink(Some(sink));

        let text = "y".repeat(50_000);
        let first = truncate_output(text.clone(), MAX_OUTPUT_BYTES);
        let second = truncate_output(text.clone(), MAX_OUTPUT_BYTES);

        let files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .expect("read the sink dir")
            .map(|entry| entry.expect("a dir entry").path())
            .collect();
        assert_eq!(files.len(), 2, "two calls, two files: {files:?}");
        assert_ne!(first, second, "and two different notices");
        for file in files {
            assert_eq!(std::fs::read_to_string(&file).expect("read"), text);
        }
    }

    /// With no sink the function is the one it has always been, down to the
    /// bytes. Tests, `wizard --print` in a pipeline and any short-lived
    /// subprocess run this way, and a spill directory they never clean up
    /// would be litter rather than a feature.
    #[test]
    fn without_a_sink_nothing_changes() {
        let _hold = hold_sink(None);
        let text = format!("HEAD{}TAIL", "x".repeat(10_000));

        for budget in [MAX_OUTPUT_BYTES, MAX_ERROR_BYTES, 1_000, 100] {
            assert_eq!(
                truncate_output(text.clone(), budget),
                truncate_output_without_spill(text.clone(), budget),
                "budget {budget} must be untouched by the spill path"
            );
        }
        assert!(truncate_output(text, 1_000).contains("rerun a narrower"));
    }

    /// A path long enough that the notice cannot fit leaves the result as it
    /// was rather than blowing the cap, and takes the orphan file with it: a
    /// spill nothing points at is a leak, not a fallback.
    #[test]
    fn a_notice_that_cannot_fit_falls_back_and_cleans_up() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let deep = tmp.path().join("d".repeat(120)).join("e".repeat(120));
        let sink = SpillSink::in_dir(&deep);
        let _hold = hold_sink(Some(sink));

        let budget = TRUNCATION_MARKER_RESERVE + 8;
        let out = truncate_output("z".repeat(20_000), budget);

        assert!(out.len() <= budget, "still capped: {} bytes", out.len());
        assert!(
            out.contains("rerun a narrower"),
            "fell back to plain truncation: {out}"
        );
        assert_eq!(
            std::fs::read_dir(&deep)
                .map(|entries| entries.count())
                .unwrap_or(0),
            0,
            "no spill file left behind for a notice that was never emitted"
        );
    }

    /// `read_file` is the one tool a spill cannot help. Its output is already a
    /// file the model can address with `start_line`/`end_line`, so spilling
    /// would copy that file in order to tell the model to read a file.
    #[tokio::test]
    async fn read_file_never_spills() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sink = SpillSink::in_dir(tmp.path().join("session"));
        let dir = sink.dir().to_path_buf();
        let _hold = hold_sink(Some(sink));

        let big = tmp.path().join("big.txt");
        std::fs::write(&big, "line of text\n".repeat(4_000)).expect("write the big file");
        let ctx = ToolContext::new(tmp.path());
        let out = file::ReadFileTool
            .execute(serde_json::json!({ "path": "big.txt" }), &ctx)
            .await
            .expect("read_file runs");

        assert!(!out.is_error);
        assert!(
            out.content.len() <= MAX_OUTPUT_BYTES,
            "still capped: {} bytes",
            out.content.len()
        );
        assert!(
            !out.content.contains("full result is at"),
            "no spill pointer in a read result"
        );
        assert!(
            !dir.exists() || std::fs::read_dir(&dir).map(|e| e.count()).unwrap_or(0) == 0,
            "and nothing written to the sink"
        );
    }
}
