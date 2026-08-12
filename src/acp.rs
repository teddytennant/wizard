//! ACP (Agent Client Protocol) server: lets editors that speak ACP — Zed,
//! Neovim (CodeCompanion/avante), Emacs — embed Wizard as their agent over
//! stdio. `wizard acp` runs the server; the editor drives it with JSON-RPC.
//!
//! This is the inverse of the TUI and the window: the same agent core
//! ([`crate::agent`]), but the surface is an editor on the other end of a pipe
//! instead of a terminal. Each ACP `session/new` builds a headless agent for
//! the requested cwd; each `session/prompt` runs one turn and streams the
//! agent's events back as `session/update` notifications; `session/cancel`
//! interrupts it.
//!
//! The `agent-client-protocol` crate's futures are `!Send`, so the server runs
//! on a single-threaded `LocalSet` with `spawn_local` (see [`run`]) — the
//! agent's own turns still use the multi-thread runtime underneath. Wizard runs
//! tools without a per-action approval gate, so the server never needs to call
//! the client back for permission; it advertises no client-side capabilities
//! and does its own file and shell I/O.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use agent_client_protocol::{self as acp, Client as _};
use anyhow::{Context, Result};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

use crate::agent::session::Session;
use crate::agent::{self, Agent, AgentEvent, CancelHandle, DoneReason, PlanVerdict};
use crate::config::Config;
use crate::mcp::McpManager;
use crate::tools::CommandDispatch;

/// Cap on a tool result's text streamed to the editor, so a huge file read or
/// command output does not flood the transcript.
const TOOL_OUTPUT_CAP: usize = 8_192;

/// Serve the ACP protocol over stdio until the client closes the pipe.
pub async fn run(config: Config) -> Result<()> {
    // Connect MCP once and share it across every session (a per-session connect
    // would spawn a duplicate of every server).
    let mcp = Rc::new(agent::connect_mcp().await);

    // The crate's connection futures are !Send: run them on a LocalSet.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let (update_tx, mut update_rx) = mpsc::unbounded_channel();
            let server = WizardAcp::new(update_tx, config, mcp);

            let outgoing = tokio::io::stdout().compat_write();
            let incoming = tokio::io::stdin().compat();
            let (conn, handle_io) =
                acp::AgentSideConnection::new(server, outgoing, incoming, |fut| {
                    tokio::task::spawn_local(fut);
                });

            // Pump the server's session updates out to the client, acking each
            // so a `prompt` handler can keep its updates ordered before its
            // response (mirrors the crate's own agent example).
            tokio::task::spawn_local(async move {
                while let Some((notification, ack)) = update_rx.recv().await {
                    if let Err(err) = conn.session_notification(notification).await {
                        tracing::warn!("acp: session update failed: {err}");
                        break;
                    }
                    let _ = ack.send(());
                }
            });

            handle_io.await.context("acp stdio loop")
        })
        .await
}

/// A live ACP session: the built agent (behind a cell so a turn can borrow it
/// mutably without touching the sessions map) and a cancel handle the
/// `session/cancel` notification fires without disturbing a running turn.
struct SessionEntry {
    agent: Rc<RefCell<Agent>>,
    cancel: CancelHandle,
}

/// The ACP agent. Single-threaded (`!Send`), so plain `Rc`/`RefCell`/`Cell`.
struct WizardAcp {
    updates: mpsc::UnboundedSender<(acp::SessionNotification, oneshot::Sender<()>)>,
    config: Config,
    mcp: Rc<McpManager>,
    sessions: Rc<RefCell<HashMap<String, SessionEntry>>>,
    /// Monotonic across the whole connection so every tool call gets a unique
    /// ACP `toolCallId`.
    next_call_id: Rc<Cell<u64>>,
}

impl WizardAcp {
    fn new(
        updates: mpsc::UnboundedSender<(acp::SessionNotification, oneshot::Sender<()>)>,
        config: Config,
        mcp: Rc<McpManager>,
    ) -> Self {
        Self {
            updates,
            config,
            mcp,
            sessions: Rc::new(RefCell::new(HashMap::new())),
            next_call_id: Rc::new(Cell::new(0)),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Agent for WizardAcp {
    async fn initialize(
        &self,
        args: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, acp::Error> {
        // Echo the client's protocol version (both crate lines negotiate V1);
        // advertise Wizard with default capabilities — text prompts, no auth,
        // no client-side fs/terminal needed (Wizard does its own I/O).
        Ok(
            acp::InitializeResponse::new(args.protocol_version).agent_info(
                acp::Implementation::new("wizard", env!("CARGO_PKG_VERSION")).title("Wizard"),
            ),
        )
    }

    async fn authenticate(
        &self,
        _args: acp::AuthenticateRequest,
    ) -> Result<acp::AuthenticateResponse, acp::Error> {
        // Wizard authenticates to its own providers via ~/.wizard config; the
        // editor never authenticates it.
        Ok(acp::AuthenticateResponse::default())
    }

    async fn new_session(
        &self,
        args: acp::NewSessionRequest,
    ) -> Result<acp::NewSessionResponse, acp::Error> {
        let cwd = args.cwd;
        let sessions_dir = Config::sessions_dir().map_err(internal)?;
        let session = Session::create_in(&sessions_dir, &cwd).map_err(internal)?;
        let session_id = session.id.clone();

        let mut agent =
            agent::build_headless_agent_for_session(&self.config, &cwd, session, Some(&self.mcp))
                .await
                .map_err(internal)?;
        // No Wizard slash commands over ACP: `run_command` refuses cleanly to
        // the model rather than silently dropping work.
        agent.set_command_dispatch(CommandDispatch::None);
        let cancel = agent.cancel_handle();

        self.sessions.borrow_mut().insert(
            session_id.clone(),
            SessionEntry {
                agent: Rc::new(RefCell::new(agent)),
                cancel,
            },
        );
        Ok(acp::NewSessionResponse::new(session_id))
    }

    // The `RefMut` is held across the turn's awaits on purpose: everything runs
    // on one thread in a LocalSet, and a still-borrowed cell is how a second
    // `prompt` on the same session gets rejected (see `try_borrow_mut` below).
    #[allow(clippy::await_holding_refcell_ref)]
    async fn prompt(&self, args: acp::PromptRequest) -> Result<acp::PromptResponse, acp::Error> {
        let session_id = args.session_id.clone();
        let text = prompt_text(&args.prompt);

        // Short borrow of the sessions map: clone out the agent cell and cancel
        // handle, then release it so `cancel` can run during the turn.
        let (agent_cell, cancel) = {
            let sessions = self.sessions.borrow();
            let entry = sessions
                .get(session_id.0.as_ref())
                .ok_or_else(acp::Error::invalid_params)?;
            (Rc::clone(&entry.agent), entry.cancel.clone())
        };
        // One turn at a time per session: a still-running turn keeps the borrow.
        let mut agent = agent_cell
            .try_borrow_mut()
            .map_err(|_| acp::Error::internal_error())?;

        let mut translator = Translator {
            session_id: session_id.clone(),
            updates: self.updates.clone(),
            next_call_id: Rc::clone(&self.next_call_id),
            open_calls: HashMap::new(),
        };
        let (events_tx, mut events_rx) = mpsc::channel::<AgentEvent>(256);
        let collector = async {
            while let Some(event) = events_rx.recv().await {
                translator.handle(event).await;
            }
        };
        // Stream while the turn runs — the bounded channel back-pressures, so
        // draining concurrently is required, not optional.
        let (result, ()) = tokio::join!(agent.run_turn(&text, events_tx), collector);
        let reason = result.map_err(internal)?;

        Ok(acp::PromptResponse::new(stop_reason(
            reason,
            cancel.is_cancelled(),
        )))
    }

    async fn cancel(&self, args: acp::CancelNotification) -> Result<(), acp::Error> {
        // Fires the cancel handle without touching the agent cell the running
        // turn holds — cooperative, stops at the next stream/tool boundary.
        if let Some(entry) = self.sessions.borrow().get(args.session_id.0.as_ref()) {
            entry.cancel.cancel();
        }
        Ok(())
    }
}

/// Map a `DoneReason` (plus whether the user cancelled) to the ACP stop
/// reason.
fn stop_reason(reason: DoneReason, cancelled: bool) -> acp::StopReason {
    match reason {
        DoneReason::Completed => acp::StopReason::EndTurn,
        DoneReason::MaxSteps => acp::StopReason::MaxTurnRequests,
        // `Stopped` is a clean cancel or a mid-turn provider failure that was
        // already surfaced as an error message; the cancel handle disambiguates.
        DoneReason::Stopped if cancelled => acp::StopReason::Cancelled,
        DoneReason::Stopped | DoneReason::TimeLimit | DoneReason::CircuitBreaker => {
            acp::StopReason::EndTurn
        }
    }
}

/// Concatenate the text of a prompt's content blocks. Images/audio are not
/// advertised in `PromptCapabilities`, so a well-behaved client won't send
/// them; resource links are named inline.
fn prompt_text(blocks: &[acp::ContentBlock]) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            acp::ContentBlock::Text(text) => parts.push(text.text.clone()),
            acp::ContentBlock::ResourceLink(link) => {
                parts.push(format!("[resource: {} ({})]", link.name, link.uri));
            }
            _ => {}
        }
    }
    parts.join("\n")
}

/// Translates one turn's [`AgentEvent`] stream into ACP `session/update`
/// notifications, synthesizing stable tool-call ids.
struct Translator {
    session_id: acp::SessionId,
    updates: mpsc::UnboundedSender<(acp::SessionNotification, oneshot::Sender<()>)>,
    next_call_id: Rc<Cell<u64>>,
    /// Per tool name: a FIFO of (call id, args) from starts awaiting their
    /// finishes — the event stream carries no id tying the two together.
    open_calls: HashMap<String, VecDeque<(String, Value)>>,
}

impl Translator {
    async fn send(&self, update: acp::SessionUpdate) {
        let (ack, done) = oneshot::channel();
        if self
            .updates
            .send((
                acp::SessionNotification::new(self.session_id.clone(), update),
                ack,
            ))
            .is_ok()
        {
            // Wait for the pump to flush this update, so updates stay ordered
            // ahead of the eventual prompt response.
            let _ = done.await;
        }
    }

    fn alloc_call_id(&self) -> String {
        let id = self.next_call_id.get();
        self.next_call_id.set(id + 1);
        format!("call-{id}")
    }

    async fn handle(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::TextDelta(text) => {
                self.send(acp::SessionUpdate::AgentMessageChunk(
                    acp::ContentChunk::new(acp::ContentBlock::from(text)),
                ))
                .await;
            }
            AgentEvent::ThinkingDelta(text) => {
                self.send(acp::SessionUpdate::AgentThoughtChunk(
                    acp::ContentChunk::new(acp::ContentBlock::from(text)),
                ))
                .await;
            }
            AgentEvent::ToolStarted { name, args } => {
                let id = self.alloc_call_id();
                self.open_calls
                    .entry(name.clone())
                    .or_default()
                    .push_back((id.clone(), args.clone()));
                self.send(acp::SessionUpdate::ToolCall(
                    acp::ToolCall::new(acp::ToolCallId::new(id), tool_title(&name, &args))
                        .kind(tool_kind(&name))
                        .status(acp::ToolCallStatus::InProgress)
                        .raw_input(args),
                ))
                .await;
            }
            AgentEvent::ToolFinished { name, output } => {
                // Pair to the matching start (FIFO per name); mint a fresh id if
                // somehow unpaired.
                let id = self
                    .open_calls
                    .get_mut(&name)
                    .and_then(|calls| calls.pop_front())
                    .map(|(id, _args)| id)
                    .unwrap_or_else(|| self.alloc_call_id());
                let status = if output.is_error {
                    acp::ToolCallStatus::Failed
                } else {
                    acp::ToolCallStatus::Completed
                };
                let text = truncate(&output.content, TOOL_OUTPUT_CAP);
                let fields = acp::ToolCallUpdateFields::new()
                    .status(status)
                    .content(vec![acp::ToolCallContent::from(text)]);
                self.send(acp::SessionUpdate::ToolCallUpdate(
                    acp::ToolCallUpdate::new(acp::ToolCallId::new(id), fields),
                ))
                .await;
            }
            AgentEvent::Error(message) | AgentEvent::Notice(message) => {
                self.send(acp::SessionUpdate::AgentMessageChunk(
                    acp::ContentChunk::new(acp::ContentBlock::from(format!("[wizard] {message}"))),
                ))
                .await;
            }
            // A dead stream is re-generated from scratch, and the deltas above
            // have already left the building: an ACP client paints each chunk
            // as it arrives, so there is no partial buffer here to discard.
            // Saying so is the only honest move left: staying quiet welds the
            // abandoned attempt onto the front of its replacement, and the
            // editor shows the answer twice.
            AgentEvent::StreamRetrying => {
                self.send(acp::SessionUpdate::AgentMessageChunk(
                    acp::ContentChunk::new(acp::ContentBlock::from(
                        "\n[wizard] the response stream dropped; it restarts below\n".to_string(),
                    )),
                ))
                .await;
            }
            // Wizard runs in the default non-plan mode over ACP, so these should
            // not fire — but the exit_plan/interview tools are always
            // registered, and an unanswered gate parks the turn inside the tool.
            // Auto-approve a plan and decline an interview so a turn can never
            // wedge.
            AgentEvent::PlanReady { gate, .. } => {
                gate.answer(PlanVerdict::approve());
            }
            AgentEvent::Interview { gate, .. } => {
                gate.decline();
            }
            // v1 does not surface usage, context, images, background tasks, or
            // subagent runs to the editor; the core text/thinking/tool stream is
            // what an ACP client renders. Spelled out rather than caught by a
            // wildcard: a new event must break this match, so somebody decides
            // what an editor does with it instead of it vanishing here.
            AgentEvent::Images { .. }
            | AgentEvent::StepCompleted { .. }
            | AgentEvent::HookFired { .. }
            | AgentEvent::OmakaseProceeding { .. }
            | AgentEvent::Usage { .. }
            | AgentEvent::ContextSize { .. }
            | AgentEvent::UltraGuidance { .. }
            | AgentEvent::TodoUpdated(_)
            | AgentEvent::TaskStarted { .. }
            | AgentEvent::TaskFinished { .. }
            | AgentEvent::SubagentStarted { .. }
            | AgentEvent::SubagentFinished { .. }
            | AgentEvent::SubagentRunStarted { .. }
            | AgentEvent::SubagentRunText { .. }
            | AgentEvent::SubagentRunToolStarted { .. }
            | AgentEvent::SubagentRunToolFinished { .. }
            | AgentEvent::SubagentRunImages { .. }
            | AgentEvent::SubagentRunStep { .. }
            | AgentEvent::SubagentRunDone { .. }
            | AgentEvent::CommandRequested(_)
            | AgentEvent::Done { .. } => {}
            // A shell command's console. ACP drives an editor, not a
            // terminal with a person typing into it, so this run's tool context
            // never declares `ConsoleAccess::Interactive` and no command ever
            // opens one. Named rather than wildcarded so that the day ACP grows
            // a place to type, somebody has to decide here.
            AgentEvent::ConsoleOpened { .. }
            | AgentEvent::ConsoleWaiting { .. }
            | AgentEvent::ConsoleOutput { .. }
            | AgentEvent::ConsoleClosed { .. } => {}
        }
    }
}

/// Classify a Wizard tool by name into an ACP tool kind (drives the editor's
/// tool-call iconography).
fn tool_kind(name: &str) -> acp::ToolKind {
    match name {
        "read_file" | "list_files" | "git_status" | "git_diff" => acp::ToolKind::Read,
        "write_file" | "edit_file" => acp::ToolKind::Edit,
        "search_files" | "web_search" | "x_search" => acp::ToolKind::Search,
        "execute" => acp::ToolKind::Execute,
        "web_fetch" => acp::ToolKind::Fetch,
        _ => acp::ToolKind::Other,
    }
}

/// A one-line title for a tool call, preferring a path/command/query from its
/// arguments.
fn tool_title(name: &str, args: &Value) -> String {
    let detail = args
        .get("path")
        .and_then(Value::as_str)
        .or_else(|| args.get("command").and_then(Value::as_str))
        .or_else(|| args.get("query").and_then(Value::as_str))
        .or_else(|| args.get("url").and_then(Value::as_str));
    match detail {
        Some(detail) => format!("{name}: {detail}"),
        None => name.to_string(),
    }
}

/// Truncate `text` to at most `cap` bytes on a char boundary, marking the cut.
fn truncate(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_string();
    }
    let mut end = cap;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… (truncated)", &text[..end])
}

/// Map any error into an ACP internal error, logging the detail (the wire error
/// is intentionally opaque).
fn internal<E: std::fmt::Display>(err: E) -> acp::Error {
    tracing::warn!("acp: {err}");
    acp::Error::internal_error()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn prompt_text_joins_text_and_names_resource_links() {
        let blocks = vec![
            acp::ContentBlock::from("first line".to_string()),
            acp::ContentBlock::ResourceLink(acp::ResourceLink::new("main.rs", "file:///main.rs")),
            acp::ContentBlock::from("second line".to_string()),
        ];
        assert_eq!(
            prompt_text(&blocks),
            "first line\n[resource: main.rs (file:///main.rs)]\nsecond line"
        );
    }

    #[test]
    fn tool_kind_classifies_the_native_tools() {
        assert_eq!(tool_kind("read_file"), acp::ToolKind::Read);
        assert_eq!(tool_kind("edit_file"), acp::ToolKind::Edit);
        assert_eq!(tool_kind("execute"), acp::ToolKind::Execute);
        assert_eq!(tool_kind("web_fetch"), acp::ToolKind::Fetch);
        assert_eq!(tool_kind("some_mcp_tool"), acp::ToolKind::Other);
    }

    #[test]
    fn tool_title_prefers_a_detail_argument() {
        assert_eq!(
            tool_title("read_file", &json!({ "path": "src/main.rs" })),
            "read_file: src/main.rs"
        );
        assert_eq!(
            tool_title("execute", &json!({ "command": "cargo test" })),
            "execute: cargo test"
        );
        assert_eq!(tool_title("todo", &json!({})), "todo");
    }

    #[test]
    fn stop_reason_maps_done_reasons() {
        assert_eq!(
            stop_reason(DoneReason::Completed, false),
            acp::StopReason::EndTurn
        );
        assert_eq!(
            stop_reason(DoneReason::Stopped, true),
            acp::StopReason::Cancelled
        );
        assert_eq!(
            stop_reason(DoneReason::MaxSteps, false),
            acp::StopReason::MaxTurnRequests
        );
    }

    /// The editor paints every delta the moment it arrives, so a retried
    /// stream cannot be un-rendered: the translator has to say the response
    /// restarts, or the abandoned attempt reads as the first half of the
    /// answer.
    #[tokio::test]
    async fn stream_retrying_tells_the_editor_the_response_restarts() {
        let (updates, mut rx) = mpsc::unbounded_channel();
        let mut translator = Translator {
            session_id: acp::SessionId::new("session-1"),
            updates,
            next_call_id: Rc::new(Cell::new(1)),
            open_calls: HashMap::new(),
        };
        // `send` waits for the pump to acknowledge the update, so the drain has
        // to run alongside it: dropping the ack is what a flushed update looks
        // like from here.
        let (notification, ()) = tokio::join!(
            async {
                let (notification, ack) = rx.recv().await.expect("one update");
                drop(ack);
                notification
            },
            translator.handle(AgentEvent::StreamRetrying),
        );

        let acp::SessionUpdate::AgentMessageChunk(chunk) = notification.update else {
            panic!("expected an assistant message chunk");
        };
        let acp::ContentBlock::Text(text) = chunk.content else {
            panic!("expected text content");
        };
        assert!(text.text.contains("restarts below"), "{}", text.text);
    }

    #[test]
    fn truncate_marks_the_cut_and_respects_boundaries() {
        assert_eq!(truncate("short", 100), "short");
        let long = "a".repeat(9000);
        let cut = truncate(&long, TOOL_OUTPUT_CAP);
        assert!(cut.ends_with("… (truncated)"));
        assert!(cut.len() < long.len());
    }
}
