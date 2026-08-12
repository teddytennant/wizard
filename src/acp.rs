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
//! ACP 2.0's request handlers run inside the connection's dispatch loop, so a
//! long `session/prompt` is spawned off the loop (see [`ConnectionTo::spawn`])
//! so `session/cancel` can still be delivered mid-turn. Wizard runs tools
//! without a per-action approval gate, so the server never needs to call the
//! client back for permission; it advertises no client-side capabilities and
//! does its own file and shell I/O.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use agent_client_protocol::schema::v1::{
    AuthenticateRequest, AuthenticateResponse, CancelNotification, ContentBlock, ContentChunk,
    Implementation, InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse,
    PromptRequest, PromptResponse, SessionId, SessionNotification, SessionUpdate, StopReason,
    ToolCall, ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    ToolKind,
};
use agent_client_protocol::{
    self as acp, Agent as AcpRole, Client, ConnectionTo, Responder, Stdio,
};
use anyhow::{Context, Result};
use serde_json::Value;
use tokio::sync::{Mutex, mpsc};

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
    let state = Arc::new(State {
        config,
        mcp: Arc::new(agent::connect_mcp().await),
        sessions: Mutex::new(HashMap::new()),
        next_call_id: Arc::new(AtomicU64::new(0)),
    });

    let new_session_state = Arc::clone(&state);
    let prompt_state = Arc::clone(&state);
    let cancel_state = Arc::clone(&state);

    AcpRole
        .builder()
        .name("wizard")
        .on_receive_request(
            async move |args: InitializeRequest, responder: Responder<InitializeResponse>, _cx| {
                // Echo the client's protocol version; advertise Wizard with
                // default capabilities — text prompts, no auth, no client-side
                // fs/terminal needed (Wizard does its own I/O).
                responder.respond(InitializeResponse::new(args.protocol_version).agent_info(
                    Implementation::new("wizard", env!("CARGO_PKG_VERSION")).title("Wizard"),
                ))
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |_args: AuthenticateRequest,
                        responder: Responder<AuthenticateResponse>,
                        _cx| {
                // Wizard authenticates to its own providers via ~/.wizard
                // config; the editor never authenticates it.
                responder.respond(AuthenticateResponse::default())
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |args: NewSessionRequest, responder: Responder<NewSessionResponse>, _cx| {
                match open_session(&new_session_state, args).await {
                    Ok(response) => responder.respond(response),
                    Err(err) => responder.respond_with_error(internal(err)),
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |args: PromptRequest, responder: Responder<PromptResponse>, cx| {
                // Hold the dispatch loop only long enough to spawn: a turn can
                // run for minutes, and `session/cancel` must still be delivered.
                let state = Arc::clone(&prompt_state);
                let connection = cx.clone();
                cx.spawn(async move {
                    // Never return Err from a spawned task — that tears down the
                    // whole connection. Surface turn failures on the responder.
                    if let Err(err) = run_prompt(&state, args, &connection, responder).await {
                        tracing::warn!("acp: prompt task failed: {err}");
                    }
                    Ok(())
                })?;
                Ok(())
            },
            acp::on_receive_request!(),
        )
        .on_receive_notification(
            async move |args: CancelNotification, _cx: ConnectionTo<Client>| {
                // Fires the cancel handle without touching the agent lock the
                // running turn holds — cooperative, stops at the next
                // stream/tool boundary.
                if let Some(entry) = cancel_state
                    .sessions
                    .lock()
                    .await
                    .get(args.session_id.0.as_ref())
                {
                    entry.cancel.cancel();
                }
                Ok(())
            },
            acp::on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await
        .context("acp stdio loop")
}

/// Shared connection state. `Send + Sync` so ACP 2.0's dispatch handlers (which
/// require `Send`) can share it across request/notification callbacks.
struct State {
    config: Config,
    mcp: Arc<McpManager>,
    sessions: Mutex<HashMap<String, SessionEntry>>,
    /// Monotonic across the whole connection so every tool call gets a unique
    /// ACP `toolCallId`.
    next_call_id: Arc<AtomicU64>,
}

/// A live ACP session: the built agent (behind a mutex so a turn can borrow it
/// mutably without touching the sessions map) and a cancel handle the
/// `session/cancel` notification fires without disturbing a running turn.
struct SessionEntry {
    agent: Arc<Mutex<Agent>>,
    cancel: CancelHandle,
}

async fn open_session(
    state: &State,
    args: NewSessionRequest,
) -> Result<NewSessionResponse, acp::Error> {
    let cwd = args.cwd;
    let sessions_dir = Config::sessions_dir().map_err(internal)?;
    let session = Session::create_in(&sessions_dir, &cwd).map_err(internal)?;
    let session_id = session.id.clone();

    let mut agent =
        agent::build_headless_agent_for_session(&state.config, &cwd, session, Some(&state.mcp))
            .await
            .map_err(internal)?;
    // No Wizard slash commands over ACP: `run_command` refuses cleanly to
    // the model rather than silently dropping work.
    agent.set_command_dispatch(CommandDispatch::None);
    let cancel = agent.cancel_handle();

    state.sessions.lock().await.insert(
        session_id.clone(),
        SessionEntry {
            agent: Arc::new(Mutex::new(agent)),
            cancel,
        },
    );
    Ok(NewSessionResponse::new(session_id))
}

async fn run_prompt(
    state: &State,
    args: PromptRequest,
    connection: &ConnectionTo<Client>,
    responder: Responder<PromptResponse>,
) -> Result<(), acp::Error> {
    let session_id = args.session_id.clone();
    let text = prompt_text(&args.prompt);

    // Short borrow of the sessions map: clone out the agent cell and cancel
    // handle, then release it so `cancel` can run during the turn.
    let (agent_cell, cancel) = {
        let sessions = state.sessions.lock().await;
        let entry = sessions
            .get(session_id.0.as_ref())
            .ok_or_else(acp::Error::invalid_params)?;
        (Arc::clone(&entry.agent), entry.cancel.clone())
    };
    // One turn at a time per session: a still-running turn keeps the lock.
    let mut agent = agent_cell
        .try_lock()
        .map_err(|_| acp::Error::internal_error())?;

    let (update_tx, mut update_rx) = mpsc::unbounded_channel();
    let mut translator = Translator {
        session_id: session_id.clone(),
        updates: update_tx,
        next_call_id: Arc::clone(&state.next_call_id),
        open_calls: HashMap::new(),
    };

    // Pump session updates out while the turn runs, then respond. Closing the
    // translator (and its channel) ends the pump so the PromptResponse stays
    // ordered after every update.
    let pump = async {
        while let Some(notification) = update_rx.recv().await {
            if let Err(err) = connection.send_notification(notification) {
                tracing::warn!("acp: session update failed: {err}");
                break;
            }
        }
    };

    let turn = async {
        let (events_tx, mut events_rx) = mpsc::channel::<AgentEvent>(256);
        let collector = async {
            while let Some(event) = events_rx.recv().await {
                translator.handle(event);
            }
        };
        // Stream while the turn runs — the bounded channel back-pressures, so
        // draining concurrently is required, not optional.
        let (result, ()) = tokio::join!(agent.run_turn(&text, events_tx), collector);
        drop(translator);
        result
    };

    let (result, ()) = tokio::join!(turn, pump);
    let reason = result.map_err(internal)?;
    responder.respond(PromptResponse::new(stop_reason(
        reason,
        cancel.is_cancelled(),
    )))
}

/// Map a `DoneReason` (plus whether the user cancelled) to the ACP stop
/// reason.
fn stop_reason(reason: DoneReason, cancelled: bool) -> StopReason {
    match reason {
        DoneReason::Completed => StopReason::EndTurn,
        DoneReason::MaxSteps => StopReason::MaxTurnRequests,
        // `Stopped` is a clean cancel or a mid-turn provider failure that was
        // already surfaced as an error message; the cancel handle disambiguates.
        DoneReason::Stopped if cancelled => StopReason::Cancelled,
        DoneReason::Stopped | DoneReason::TimeLimit | DoneReason::CircuitBreaker => {
            StopReason::EndTurn
        }
    }
}

/// Concatenate the text of a prompt's content blocks. Images/audio are not
/// advertised in `PromptCapabilities`, so a well-behaved client won't send
/// them; resource links are named inline.
fn prompt_text(blocks: &[ContentBlock]) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text) => parts.push(text.text.clone()),
            ContentBlock::ResourceLink(link) => {
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
    session_id: SessionId,
    updates: mpsc::UnboundedSender<SessionNotification>,
    next_call_id: Arc<AtomicU64>,
    /// Per tool name: a FIFO of (call id, args) from starts awaiting their
    /// finishes — the event stream carries no id tying the two together.
    open_calls: HashMap<String, VecDeque<(String, Value)>>,
}

impl Translator {
    fn send(&self, update: SessionUpdate) {
        let _ = self
            .updates
            .send(SessionNotification::new(self.session_id.clone(), update));
    }

    fn alloc_call_id(&self) -> String {
        let id = self.next_call_id.fetch_add(1, Ordering::Relaxed);
        format!("call-{id}")
    }

    fn handle(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::TextDelta(text) => {
                self.send(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                    ContentBlock::from(text),
                )));
            }
            AgentEvent::ThinkingDelta(text) => {
                self.send(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
                    ContentBlock::from(text),
                )));
            }
            AgentEvent::ToolStarted { name, args } => {
                let id = self.alloc_call_id();
                self.open_calls
                    .entry(name.clone())
                    .or_default()
                    .push_back((id.clone(), args.clone()));
                self.send(SessionUpdate::ToolCall(
                    ToolCall::new(ToolCallId::new(id), tool_title(&name, &args))
                        .kind(tool_kind(&name))
                        .status(ToolCallStatus::InProgress)
                        .raw_input(args),
                ));
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
                    ToolCallStatus::Failed
                } else {
                    ToolCallStatus::Completed
                };
                let text = truncate(&output.content, TOOL_OUTPUT_CAP);
                let fields = ToolCallUpdateFields::new()
                    .status(status)
                    .content(vec![ToolCallContent::from(text)]);
                self.send(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    ToolCallId::new(id),
                    fields,
                )));
            }
            AgentEvent::Error(message) | AgentEvent::Notice(message) => {
                self.send(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                    ContentBlock::from(format!("[wizard] {message}")),
                )));
            }
            // A dead stream is re-generated from scratch, and the deltas above
            // have already left the building: an ACP client paints each chunk
            // as it arrives, so there is no partial buffer here to discard.
            // Saying so is the only honest move left: staying quiet welds the
            // abandoned attempt onto the front of its replacement, and the
            // editor shows the answer twice.
            AgentEvent::StreamRetrying => {
                self.send(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                    ContentBlock::from(
                        "\n[wizard] the response stream dropped; it restarts below\n".to_string(),
                    ),
                )));
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
fn tool_kind(name: &str) -> ToolKind {
    match name {
        "read_file" | "list_files" | "git_status" | "git_diff" => ToolKind::Read,
        "write_file" | "edit_file" => ToolKind::Edit,
        "search_files" | "web_search" | "x_search" => ToolKind::Search,
        "execute" => ToolKind::Execute,
        "web_fetch" => ToolKind::Fetch,
        _ => ToolKind::Other,
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
    use agent_client_protocol::schema::v1::ResourceLink;
    use serde_json::json;

    #[test]
    fn prompt_text_joins_text_and_names_resource_links() {
        let blocks = vec![
            ContentBlock::from("first line".to_string()),
            ContentBlock::ResourceLink(ResourceLink::new("main.rs", "file:///main.rs")),
            ContentBlock::from("second line".to_string()),
        ];
        assert_eq!(
            prompt_text(&blocks),
            "first line\n[resource: main.rs (file:///main.rs)]\nsecond line"
        );
    }

    #[test]
    fn tool_kind_classifies_the_native_tools() {
        assert_eq!(tool_kind("read_file"), ToolKind::Read);
        assert_eq!(tool_kind("edit_file"), ToolKind::Edit);
        assert_eq!(tool_kind("execute"), ToolKind::Execute);
        assert_eq!(tool_kind("web_fetch"), ToolKind::Fetch);
        assert_eq!(tool_kind("some_mcp_tool"), ToolKind::Other);
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
            StopReason::EndTurn
        );
        assert_eq!(
            stop_reason(DoneReason::Stopped, true),
            StopReason::Cancelled
        );
        assert_eq!(
            stop_reason(DoneReason::MaxSteps, false),
            StopReason::MaxTurnRequests
        );
    }

    /// The editor paints every delta the moment it arrives, so a retried
    /// stream cannot be un-rendered: the translator has to say the response
    /// restarts, or the abandoned attempt reads as the first half of the
    /// answer.
    #[test]
    fn stream_retrying_tells_the_editor_the_response_restarts() {
        let (updates, mut rx) = mpsc::unbounded_channel();
        let mut translator = Translator {
            session_id: SessionId::new("session-1"),
            updates,
            next_call_id: Arc::new(AtomicU64::new(1)),
            open_calls: HashMap::new(),
        };
        translator.handle(AgentEvent::StreamRetrying);
        let notification = rx.try_recv().expect("one update");

        let SessionUpdate::AgentMessageChunk(chunk) = notification.update else {
            panic!("expected an assistant message chunk");
        };
        let ContentBlock::Text(text) = chunk.content else {
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
