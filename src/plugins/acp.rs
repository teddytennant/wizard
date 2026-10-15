//! ACP (Agent Client Protocol) server: lets editors that speak ACP — Zed,
//! Neovim (CodeCompanion/avante), Emacs — embed Wizard as their agent over
//! stdio. `wizard acp` runs the server; the editor drives it with JSON-RPC.
//!
//! This is the inverse of the TUI and the window: the same agent core
//! ([`crate::agent`]), but the surface is an editor on the other end of a pipe
//! instead of a terminal. Each ACP `session/new` builds a headless agent for
//! the requested cwd; each `session/prompt` runs one turn and streams the
//! agent's events back as `session/update` notifications; `session/cancel`
//! interrupts it. `session/list` pages through the saved sessions for a
//! project, and `session/load` reopens one from `~/.wizard/sessions` by id,
//! replays its transcript as updates, and continues it — so an editor's thread
//! history reaches every Wizard conversation, from any surface, and a client
//! that restarted this process picks the conversation up where it left off.
//! Every session advertises `model`, `thought_level`, and `wizard_mode` config
//! options, and `session/set_config_option` changes them for that session
//! alone; see [`models`]. It also advertises the slash commands this surface
//! runs, and a prompt that is one is dispatched rather than sent to the model;
//! see [`command`].
//!
//! ACP 2.0's request handlers run inside the connection's dispatch loop, so a
//! long `session/prompt` is spawned off the loop (see [`ConnectionTo::spawn`])
//! so `session/cancel` can still be delivered mid-turn. Wizard runs tools
//! without a per-action approval gate; the one thing it asks about is a
//! finished plan (`exit_plan` in plan mode), which goes to the client as
//! `session/request_permission`. It advertises no client-side capabilities and
//! does its own file and shell I/O.
//!
//! # As a plugin
//!
//! Behind `--features acp`, on by default, which is also what gates the
//! `agent-client-protocol` dependency: a build without this feature does not
//! link the protocol crate at all. Core parses `wizard acp` and looks the
//! *body* up by name; see [`AcpPlugin`] at the bottom of this file and
//! [`crate::entrypoint`] for why a subcommand's body is a service rather than
//! a slash command.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use agent_client_protocol::schema::v1::{
    AgentCapabilities, AuthenticateRequest, AuthenticateResponse, AvailableCommandsUpdate,
    CancelNotification, ConfigOptionUpdate, ContentBlock, ContentChunk, Implementation,
    InitializeRequest, InitializeResponse, ListSessionsRequest, ListSessionsResponse,
    LoadSessionRequest, LoadSessionResponse, Meta, NewSessionRequest, NewSessionResponse,
    PermissionOption, PermissionOptionKind, PromptRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, SessionCapabilities, SessionConfigOption,
    SessionId, SessionInfo, SessionListCapabilities, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StopReason, ToolCall,
    ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use agent_client_protocol::{
    self as acp, Agent as AcpRole, Client, ConnectionTo, Responder, Stdio,
};
use anyhow::{Context, Result};
use serde_json::Value;
use tokio::sync::{Mutex, mpsc, watch};

use crate::agent::session::{self, Session};
use crate::agent::{self, Agent, AgentEvent, CancelHandle, DoneReason, PlanGate, PlanVerdict};
use crate::config::Config;
use crate::entrypoint::{self, Entrypoint};
use crate::kernel::{Capability, Ctx, Plugin, PluginManifest, Service};
use crate::llm::{self, ChatMessage, Role};
use crate::mcp::McpManager;
use crate::tools::CommandDispatch;

mod auth;
mod command;
mod models;

use models::{Catalog, Selection};

/// Cap on a tool result's text streamed to the editor, so a huge file read or
/// command output does not flood the transcript.
const TOOL_OUTPUT_CAP: usize = 8_192;

/// How long a `session/new` waits for a first-ever model listing before
/// answering with the configured models alone.
const CATALOG_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Sessions per `session/list` page.
const SESSION_LIST_PAGE: usize = 50;

/// Cap on a listed session's title (its first prompt), in characters.
const SESSION_TITLE_CHARS: usize = 100;

/// Serve the ACP protocol over stdio until the client closes the pipe.
pub async fn run(config: Config) -> Result<()> {
    // The model catalog is read from disk now and refreshed off the request
    // path when a provider was never listed or its list has gone stale.
    let catalog = Catalog::load();
    let refresh_due = catalog.needs_refresh(&config, models::now_secs());
    let (catalog_done, catalog_ready) = watch::channel(!refresh_due);

    // Connect MCP once and share it across every session (a per-session connect
    // would spawn a duplicate of every server).
    let state = Arc::new(State {
        config: std::sync::RwLock::new(Arc::new(config)),
        sign_in: std::sync::Mutex::new(None),
        mcp: Arc::new(agent::connect_mcp().await),
        sessions: Mutex::new(HashMap::new()),
        next_call_id: Arc::new(AtomicU64::new(0)),
        catalog: std::sync::Mutex::new(catalog),
        catalog_ready,
    });
    if refresh_due {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let current = state.catalog().clone();
            let next = current.refresh(&state.config()).await;
            if let Err(err) = next.save() {
                tracing::debug!("acp: could not cache the model catalog: {err:#}");
            }
            *state.catalog.lock().unwrap_or_else(|e| e.into_inner()) = next;
            let _ = catalog_done.send(true);
        });
    }

    let new_session_state = Arc::clone(&state);
    let load_session_state = Arc::clone(&state);
    let config_state = Arc::clone(&state);
    let prompt_state = Arc::clone(&state);
    let cancel_state = Arc::clone(&state);
    let exit_state = Arc::clone(&state);
    let auth_state = Arc::clone(&state);

    let serve = AcpRole
        .builder()
        .name("wizard")
        .on_receive_request(
            async move |args: InitializeRequest, responder: Responder<InitializeResponse>, _cx| {
                // Echo the client's protocol version; advertise Wizard with
                // text prompts, the sign-ins `auth` can run, no client-side
                // fs/terminal needed (Wizard does its own I/O), `session/load`,
                // and `session/list`.
                let capabilities = AgentCapabilities::new()
                    .load_session(true)
                    .session_capabilities(
                        SessionCapabilities::new().list(SessionListCapabilities::new()),
                    );
                responder.respond(
                    InitializeResponse::new(args.protocol_version)
                        .agent_capabilities(capabilities)
                        .auth_methods(auth::methods())
                        .agent_info(
                            Implementation::new("wizard", env!("CARGO_PKG_VERSION"))
                                .title("Wizard"),
                        ),
                )
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |args: AuthenticateRequest,
                        responder: Responder<AuthenticateResponse>,
                        cx: ConnectionTo<Client>| {
                // An OAuth sign-in waits minutes for the browser, so it runs
                // off the dispatch loop like a prompt does.
                let state = Arc::clone(&auth_state);
                let connection = cx.clone();
                cx.spawn(async move {
                    let result = sign_in(&state, &args, &connection).await;
                    match result {
                        Ok(()) => responder.respond(AuthenticateResponse::default()),
                        Err(err) => responder.respond_with_error(err),
                    }
                })?;
                Ok(())
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |args: NewSessionRequest,
                        responder: Responder<NewSessionResponse>,
                        cx: ConnectionTo<Client>| {
                match open_session(&new_session_state, args).await {
                    Ok(response) => {
                        let id = response.session_id.clone();
                        responder.respond(response)?;
                        advertise_commands(&cx, id)
                    }
                    Err(err) => responder.respond_with_error(err),
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |args: ListSessionsRequest,
                        responder: Responder<ListSessionsResponse>,
                        _cx| {
                // Summarizing reads every session file; keep that off the
                // dispatch loop so a turn's `session/cancel` still lands.
                let listed = tokio::task::spawn_blocking(move || list_sessions(&args)).await;
                match listed {
                    Ok(Ok(response)) => responder.respond(response),
                    Ok(Err(err)) => responder.respond_with_error(err),
                    Err(err) => responder.respond_with_error(internal(err)),
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |args: LoadSessionRequest,
                        responder: Responder<LoadSessionResponse>,
                        cx: ConnectionTo<Client>| {
                let id = args.session_id.clone();
                match load_session(&load_session_state, args, &cx).await {
                    Ok(response) => {
                        responder.respond(response)?;
                        advertise_commands(&cx, id)
                    }
                    Err(err) => responder.respond_with_error(err),
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |args: SetSessionConfigOptionRequest,
                        responder: Responder<SetSessionConfigOptionResponse>,
                        _cx| {
                match set_config_option(&config_state, args).await {
                    Ok(response) => responder.respond(response),
                    Err(err) => responder.respond_with_error(err),
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
        .connect_to(Stdio::new());

    // A client ends the server by closing stdin or, as Zeron and most process
    // supervisors do, with SIGTERM. Either way the sessions nothing was said
    // in are removed on the way out, so a client that opens a session only to
    // read its config options leaves nothing in ~/.wizard/sessions.
    let outcome = tokio::select! {
        result = serve => result.context("acp stdio loop"),
        () = terminated() => Ok(()),
    };
    discard_unused_sessions(&exit_state).await;
    outcome
}

/// Resolves on SIGTERM or SIGHUP; never, where there are no such signals or
/// they cannot be watched (the default disposition still ends the process).
async fn terminated() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let (Ok(mut term), Ok(mut hup)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
        ) {
            tokio::select! {
                _ = term.recv() => {}
                _ = hup.recv() => {}
            }
            return;
        }
    }
    std::future::pending::<()>().await
}

/// Delete the session files this connection created and never prompted.
async fn discard_unused_sessions(state: &State) {
    let sessions = state.sessions.lock().await;
    for entry in sessions.values() {
        if let Some(path) = &entry.unused_file
            && session_file_unused(path)
        {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// `authenticate`: run the sign-in, then serve every later session from the
/// config it saved. The model catalog is refreshed for the new provider,
/// bounded like a first `session/new`, so the picker lists its models.
async fn sign_in(
    state: &State,
    args: &AuthenticateRequest,
    connection: &ConnectionTo<Client>,
) -> Result<(), acp::Error> {
    let (canceller, cancel) = llm::oauth_callback::cancellation();
    let previous = state
        .sign_in
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(canceller);
    if let Some(previous) = previous {
        previous.cancel();
    }
    let config = auth::authenticate(args, connection, &state.config(), cancel).await?;
    let current = state.catalog().clone();
    if let Ok(next) = tokio::time::timeout(CATALOG_WAIT, current.refresh(&config)).await {
        if let Err(err) = next.save() {
            tracing::debug!("acp: could not cache the model catalog: {err:#}");
        }
        *state.catalog() = next;
    }
    *state.config.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(config);
    Ok(())
}

/// A session file that holds nothing but its header line.
fn session_file_unused(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .is_ok_and(|text| text.lines().filter(|line| !line.trim().is_empty()).count() <= 1)
}

/// Shared connection state. `Send + Sync` so ACP 2.0's dispatch handlers (which
/// require `Send`) can share it across request/notification callbacks.
struct State {
    /// Replaced whole when `authenticate` adds a provider; a session keeps
    /// the config it was built from.
    config: std::sync::RwLock<Arc<Config>>,
    /// Cancels the OAuth sign-in in flight, if any, so a newer one can bind
    /// the callback port.
    sign_in: std::sync::Mutex<Option<llm::oauth_callback::Canceller>>,
    mcp: Arc<McpManager>,
    sessions: Mutex<HashMap<String, SessionEntry>>,
    /// Monotonic across the whole connection so every tool call gets a unique
    /// ACP `toolCallId`.
    next_call_id: Arc<AtomicU64>,
    /// Models each provider listed, cached across runs; see [`models`].
    catalog: std::sync::Mutex<Catalog>,
    /// Flips to true once a startup refresh of `catalog` is done (or was not
    /// needed).
    catalog_ready: watch::Receiver<bool>,
}

impl State {
    fn config(&self) -> Arc<Config> {
        Arc::clone(&self.config.read().unwrap_or_else(|e| e.into_inner()))
    }

    fn catalog(&self) -> std::sync::MutexGuard<'_, Catalog> {
        self.catalog.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The config options for a session with `selection`. The first session
    /// of a fresh install waits briefly for the providers' model lists, so a
    /// client that asks once (to fill a picker) sees them.
    async fn config_options(&self, selection: &Selection) -> Vec<SessionConfigOption> {
        let config = self.config();
        if self.catalog().is_missing_any(&config) {
            let mut ready = self.catalog_ready.clone();
            let _ = tokio::time::timeout(CATALOG_WAIT, ready.wait_for(|done| *done)).await;
        }
        let catalog = self.catalog().clone();
        models::config_options(&config, &catalog, selection)
    }
}

/// A live ACP session: the built agent (behind a mutex so a turn can borrow it
/// mutably without touching the sessions map) and a cancel handle the
/// `session/cancel` notification fires without disturbing a running turn.
struct SessionEntry {
    agent: Arc<Mutex<Agent>>,
    cancel: CancelHandle,
    /// Working directory the session runs in, for a rebuild on a model switch.
    cwd: PathBuf,
    /// The session file, for the same rebuild.
    session: Session,
    /// The model, effort, and mode this session runs with.
    selection: Selection,
    /// Set while a session this connection created has never been prompted:
    /// its file is removed when the connection ends.
    unused_file: Option<PathBuf>,
    /// Turns run through the fusion panel (`/fusion`). A rebuild onto another
    /// model leaves it.
    fusion: bool,
}

/// Tell the client which slash commands a session runs. Sent after the
/// `session/new` or `session/load` answer, so the client knows the session id
/// it belongs to.
fn advertise_commands(connection: &ConnectionTo<Client>, id: SessionId) -> Result<(), acp::Error> {
    let update = AvailableCommandsUpdate::new(command::available_commands());
    connection.send_notification(SessionNotification::new(
        id,
        SessionUpdate::AvailableCommandsUpdate(update),
    ))
}

async fn open_session(
    state: &State,
    args: NewSessionRequest,
) -> Result<NewSessionResponse, acp::Error> {
    let cwd = args.cwd;
    let sessions_dir = Config::sessions_dir().map_err(internal)?;
    let session = Session::create_in(&sessions_dir, &cwd).map_err(internal)?;
    let session_id = session.id.clone();
    let unused_file = session.path().to_path_buf();
    let selection = starting_selection(&state.config());

    let built = build_session_agent(state, &selection, &cwd, session.clone(), None).await;
    let (agent, cancel) = match built {
        Ok(built) => built,
        Err(err) => {
            // Nothing will ever be said in it.
            let _ = std::fs::remove_file(&unused_file);
            return Err(err);
        }
    };

    let options = state.config_options(&selection).await;
    state.sessions.lock().await.insert(
        session_id.clone(),
        SessionEntry {
            agent,
            cancel,
            cwd,
            session,
            selection,
            unused_file: Some(unused_file),
            fusion: false,
        },
    );
    Ok(NewSessionResponse::new(session_id).config_options(options))
}

/// What a new or loaded session runs on: the config's defaults, unless `/ui`
/// started this look from another one, which hands over its model and mode.
fn starting_selection(config: &Config) -> Selection {
    match crate::skin::handoff::Handoff::from_env() {
        Some(handoff) => {
            let mut config = config.clone();
            handoff.apply(&mut config);
            Selection::from_config(&config)
        }
        None => Selection::from_config(config),
    }
}

/// Build the agent a session runs, for its selection: the config with the
/// selection applied, over the session's file (so a rebuild keeps history).
async fn build_session_agent(
    state: &State,
    selection: &Selection,
    cwd: &Path,
    session: Session,
    replacing: Option<&Agent>,
) -> Result<(Arc<Mutex<Agent>>, CancelHandle), acp::Error> {
    let mut config = selection.apply(&state.config());
    // Set up after this server started? `wizard computer setup` writes the
    // file from another process; a new session picks that up.
    if let Some(computer) = Config::computer_on_disk() {
        config.computer = computer;
    }
    let mut agent =
        agent::build_headless_agent_for_session(&config, cwd, session, Some(&state.mcp))
            .await
            .map_err(internal)?;
    // The user's `/command` prompts are dispatched by `run_slash_command`; the
    // model's `run_command` tool stays off, so it refuses cleanly rather than
    // queueing a command nothing on this surface drains.
    agent.set_command_dispatch(CommandDispatch::None);
    if let Some(old) = replacing {
        agent.usage().carry_session(old.usage());
        // plan mode and omakase live in the agent, not the selection: a model switch must not
        // drop them while the client still shows them
        agent.set_plan_mode(old.plan_mode());
        agent.set_omakase(old.omakase());
    }
    let cancel = agent.cancel_handle();
    Ok((Arc::new(Mutex::new(agent)), cancel))
}

/// `session/set_config_option`: switch this session's model, reasoning
/// effort, or mode. Effort and mode apply in place; a model switch rebuilds
/// the session's agent over its file, so the conversation carries over. The
/// user's config is never written — the choice lasts as long as the session
/// is open here, and a client re-applies it after `session/load`.
async fn set_config_option(
    state: &State,
    args: SetSessionConfigOptionRequest,
) -> Result<SetSessionConfigOptionResponse, acp::Error> {
    let id = args.session_id.0.to_string();
    let value = args
        .value
        .as_value_id()
        .map(|value| value.0.to_string())
        .ok_or_else(acp::Error::invalid_params)?;
    let (agent_cell, mut selection, cwd, session) = {
        let sessions = state.sessions.lock().await;
        let entry = sessions.get(&id).ok_or_else(acp::Error::invalid_params)?;
        (
            Arc::clone(&entry.agent),
            entry.selection.clone(),
            entry.cwd.clone(),
            entry.session.clone(),
        )
    };
    // A running turn holds the agent; switching under it would leave two
    // agents writing one session file.
    let mut agent = agent_cell.try_lock().map_err(|_| {
        acp::Error::invalid_request().data("a turn is running; set options between turns")
    })?;

    let mut rebuilt = None;
    match args.config_id.0.as_ref() {
        models::MODEL_OPTION => {
            let (provider, model) =
                models::parse_model_id(&value, &state.config()).ok_or_else(|| {
                    acp::Error::invalid_params()
                        .data(format!("{value} names no configured provider"))
                })?;
            if provider != selection.provider || model != selection.model {
                selection.provider = provider;
                selection.model = model;
                rebuilt = Some(
                    build_session_agent(state, &selection, &cwd, session, Some(&agent)).await?,
                );
            }
        }
        models::EFFORT_OPTION => {
            let effort = models::parse_effort(&value).ok_or_else(acp::Error::invalid_params)?;
            selection.effort = effort;
            agent.set_reasoning_effort(effort);
        }
        models::MODE_OPTION => {
            let mode = models::parse_mode(&value).ok_or_else(acp::Error::invalid_params)?;
            selection.mode = mode;
            agent.set_mode(mode);
        }
        models::SEARCH_OPTION => {
            let backend = models::parse_search_backend(&value).ok_or_else(|| {
                acp::Error::invalid_params().data(format!("unknown search backend {value}"))
            })?;
            agent.set_search_backend(&backend);
            selection.search_backend = backend;
        }
        _ => return Err(acp::Error::invalid_params()),
    }
    drop(agent);

    {
        let mut sessions = state.sessions.lock().await;
        let entry = sessions
            .get_mut(&id)
            .ok_or_else(acp::Error::invalid_params)?;
        if let Some((agent, cancel)) = rebuilt {
            entry.agent = agent;
            entry.cancel = cancel;
            entry.fusion = false;
        }
        entry.selection = selection.clone();
    }
    Ok(SetSessionConfigOptionResponse::new(
        state.config_options(&selection).await,
    ))
}

/// Reopen a persisted session by id: rebuild its agent over the saved history,
/// replay the transcript to the client, then answer. Every update goes out
/// before the response, which is the order the protocol asks for.
async fn load_session(
    state: &State,
    args: LoadSessionRequest,
    connection: &ConnectionTo<Client>,
) -> Result<LoadSessionResponse, acp::Error> {
    let id = args.session_id.0.to_string();
    // The id names a file under ~/.wizard/sessions, so anything that could
    // step outside that directory is refused rather than resolved.
    if !is_session_id(&id) {
        return Err(acp::Error::invalid_params());
    }
    let sessions_dir = Config::sessions_dir().map_err(internal)?;
    let session = Session::open_by_id(&sessions_dir, &id)
        .map_err(internal)?
        .ok_or_else(|| acp::Error::resource_not_found(Some(id.clone())))?;
    let history = session.load_history().map_err(internal)?;

    // Loading a session that is already live on this connection rebuilds it;
    // a turn still holding the old agent keeps it until the turn ends. It
    // reopens on the config's defaults, or on what the look before this one
    // ran: a model picked over ACP lasts as long as the connection, and the
    // client sets it again.
    let selection = starting_selection(&state.config());
    let (agent, cancel) =
        build_session_agent(state, &selection, &args.cwd, session.clone(), None).await?;
    let options = state.config_options(&selection).await;
    state.sessions.lock().await.insert(
        id,
        SessionEntry {
            agent,
            cancel,
            cwd: args.cwd.clone(),
            session,
            selection,
            unused_file: None,
            fusion: false,
        },
    );

    // `_meta.isReplay` lets a client that already holds the transcript (T3
    // Code, Zeron) drop these instead of painting the history twice.
    let mut replay = Meta::new();
    replay.insert("isReplay".to_string(), Value::Bool(true));
    for update in history_updates(&history) {
        connection
            .send_notification(
                SessionNotification::new(args.session_id.clone(), update).meta(replay.clone()),
            )
            .map_err(internal)?;
    }
    Ok(LoadSessionResponse::new().config_options(options))
}

/// One page of saved sessions, newest first, for `session/list`.
fn list_sessions(args: &ListSessionsRequest) -> Result<ListSessionsResponse, acp::Error> {
    let dir = Config::sessions_dir().map_err(internal)?;
    let listed = session::summaries(&dir).into_iter().filter_map(|summary| {
        let cwd = summary.cwd?;
        let updated_at = std::fs::metadata(dir.join(format!("{}.jsonl", summary.id)))
            .and_then(|meta| meta.modified())
            .ok()
            .map(|modified| chrono::DateTime::<chrono::Utc>::from(modified).to_rfc3339());
        Some(ListedSession {
            id: summary.id,
            cwd,
            title: summary.summary,
            updated_at,
        })
    });
    let (sessions, next_cursor) =
        session_list_page(listed, args.cwd.as_deref(), args.cursor.as_deref());
    Ok(ListSessionsResponse::new(sessions).next_cursor(next_cursor))
}

/// A saved session as `session/list` reports it.
struct ListedSession {
    id: String,
    cwd: String,
    title: String,
    updated_at: Option<String>,
}

/// Select one page from sessions sorted newest id first (ids are timestamps).
/// The cursor is the last id of the previous page, so a session written
/// between two requests shifts nothing already paged past. Sessions from
/// other working directories are left out when the client names one.
fn session_list_page(
    sessions: impl IntoIterator<Item = ListedSession>,
    cwd: Option<&Path>,
    cursor: Option<&str>,
) -> (Vec<SessionInfo>, Option<String>) {
    let mut matching = sessions
        .into_iter()
        .filter(|session| cwd.is_none_or(|cwd| Path::new(&session.cwd) == cwd))
        .filter(|session| cursor.is_none_or(|cursor| session.id.as_str() < cursor));
    let page: Vec<ListedSession> = matching.by_ref().take(SESSION_LIST_PAGE).collect();
    let next_cursor = match (matching.next(), page.last()) {
        (Some(_), Some(last)) => Some(last.id.clone()),
        _ => None,
    };
    let infos = page
        .into_iter()
        .map(|session| {
            SessionInfo::new(session.id, session.cwd)
                .title(truncate_title(&session.title))
                .updated_at(session.updated_at)
        })
        .collect();
    (infos, next_cursor)
}

fn truncate_title(title: &str) -> String {
    let title = title.trim();
    match title.char_indices().nth(SESSION_TITLE_CHARS) {
        Some((end, _)) => format!("{}…", &title[..end]),
        None => title.to_string(),
    }
}

/// Whether `id` can only name a file directly inside the sessions directory.
fn is_session_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The updates that replay a saved transcript: what the user said, what the
/// model said and thought, and each tool call with its result. System notes
/// are the agent's own bookkeeping and stay out of the client's transcript.
fn history_updates(messages: &[ChatMessage]) -> Vec<SessionUpdate> {
    // Saved calls carry the provider's ids; a prefix keeps them clear of the
    // `call-N` ids this connection mints for live turns.
    let call_id = |id: &str| ToolCallId::new(format!("history-{id}"));
    let mut updates = Vec::new();
    for message in messages {
        for block in &message.content {
            let update = match (message.role, block) {
                (Role::User, llm::ContentBlock::Text(text)) => SessionUpdate::UserMessageChunk(
                    ContentChunk::new(ContentBlock::from(text.text.clone())),
                ),
                (Role::Assistant, llm::ContentBlock::Text(text)) => {
                    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from(
                        text.text.clone(),
                    )))
                }
                (Role::Assistant, llm::ContentBlock::Thinking(thinking))
                    if !thinking.thinking.is_empty() =>
                {
                    SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::from(
                        thinking.thinking.clone(),
                    )))
                }
                (Role::Assistant, llm::ContentBlock::ToolUse(call)) => {
                    let name = &call.function.name;
                    let args = &call.function.arguments;
                    SessionUpdate::ToolCall(
                        ToolCall::new(call_id(&call.id), tool_title(name, args))
                            .kind(tool_kind(name))
                            .status(ToolCallStatus::Completed)
                            .raw_input(args.clone()),
                    )
                }
                (Role::Tool, llm::ContentBlock::ToolResult(result)) => {
                    // the stored flag; a record from before it existed falls back on the
                    // prefix the turn gives a failed call's text
                    let failed = result
                        .is_error
                        .unwrap_or_else(|| result.content.starts_with("Error: "));
                    let status = if failed {
                        ToolCallStatus::Failed
                    } else {
                        ToolCallStatus::Completed
                    };
                    let fields = ToolCallUpdateFields::new().status(status).content(vec![
                        ToolCallContent::from(truncate(&result.content, TOOL_OUTPUT_CAP)),
                    ]);
                    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                        call_id(&result.tool_use_id),
                        fields,
                    ))
                }
                _ => continue,
            };
            updates.push(update);
        }
    }
    updates
}

async fn run_prompt(
    state: &State,
    args: PromptRequest,
    connection: &ConnectionTo<Client>,
    responder: Responder<PromptResponse>,
) -> Result<(), acp::Error> {
    let session_id = args.session_id.clone();
    let text = prompt_text(&args.prompt);
    if let Some(line) = command::command_line(&text) {
        return run_slash_command(state, &session_id, line, connection, responder).await;
    }

    // Short borrow of the sessions map: clone out the agent cell and cancel
    // handle, then release it so `cancel` can run during the turn.
    let (agent_cell, cancel) = {
        let mut sessions = state.sessions.lock().await;
        let entry = sessions
            .get_mut(session_id.0.as_ref())
            .ok_or_else(acp::Error::invalid_params)?;
        // Something is being said in it now: the file stays.
        entry.unused_file = None;
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
        ask: true,
    };

    // Pump session updates out while the turn runs, then respond. Closing the
    // translator (and its channel) ends the pump so the PromptResponse stays
    // ordered after every update.
    let pump = async {
        while let Some(out) = update_rx.recv().await {
            match out {
                Outgoing::Update(notification) => {
                    if let Err(err) = connection.send_notification(notification) {
                        tracing::warn!("acp: session update failed: {err}");
                        break;
                    }
                }
                // Sent from here so it lands after the tool call it is about; the
                // answer is awaited off the dispatch loop.
                Outgoing::Ask(request, gate) => {
                    let sent = connection.send_request(request);
                    let cancel = cancel.clone();
                    // a client that never answers is let go of when the turn is cancelled
                    let spawned = connection.spawn(async move {
                        let verdict = tokio::select! {
                            answer = sent.block_task() => plan_verdict(answer),
                            () = cancel.cancelled() => PlanVerdict::reject("the review was cancelled"),
                        };
                        let _ = gate.answer(verdict);
                        Ok(())
                    });
                    if let Err(err) = spawned {
                        tracing::warn!("acp: plan review request failed: {err}");
                    }
                }
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

/// Run a `/command` prompt through the one dispatcher, against this session
/// only, and answer with what it said. No turn and no model call, except for
/// the commands whose whole point is one (`/btw`, `/compact`, `/evolve`).
///
/// A change to the model, effort, or mode is also sent as a
/// `config_option_update`, so the client's pickers show what the session now
/// runs.
async fn run_slash_command(
    state: &State,
    session_id: &SessionId,
    line: &str,
    connection: &ConnectionTo<Client>,
    responder: Responder<PromptResponse>,
) -> Result<(), acp::Error> {
    let id = session_id.0.to_string();
    let (agent_cell, selection, cwd, session, fusion) = {
        let sessions = state.sessions.lock().await;
        let entry = sessions.get(&id).ok_or_else(acp::Error::invalid_params)?;
        (
            Arc::clone(&entry.agent),
            entry.selection.clone(),
            entry.cwd.clone(),
            entry.session.clone(),
            entry.fusion,
        )
    };
    let mut agent = agent_cell.try_lock().map_err(|_| {
        acp::Error::invalid_request().data("a turn is running; send commands between turns")
    })?;

    let base = state.config();
    let mut providers_changed = false;
    let (mut answer, mut next, fusion, model_change) =
        match crate::commands::SlashCommand::parse(line) {
            Some(Ok(parsed)) => {
                let mut surface = command::AcpSurface {
                    agent: &mut agent,
                    base: &base,
                    config: selection.apply(&base),
                    selection: selection.clone(),
                    mcp: &state.mcp,
                    cwd: cwd.clone(),
                    fusion,
                    model_change: None,
                    base_change: None,
                    out: String::new(),
                };
                crate::commands::surface::dispatch(parsed, &mut surface).await;
                if let Some(config) = surface.base_change.take() {
                    *state.config.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(config);
                    providers_changed = true;
                }
                (
                    surface.out,
                    surface.selection,
                    surface.fusion,
                    surface.model_change,
                )
            }
            // The parser's own words: a bad argument, a usage line.
            Some(Err(message)) => (message, selection.clone(), fusion, None),
            None => (String::new(), selection.clone(), fusion, None),
        };

    // Still holding the old agent, so no turn starts on it mid-switch.
    let mut rebuilt = None;
    if let Some((provider, model)) = model_change {
        let mut switched = next.clone();
        switched.provider = provider;
        switched.model = model;
        let line = if switched == next {
            format!("already on {}", next.model_id())
        } else {
            match build_session_agent(state, &switched, &cwd, session, Some(&agent)).await {
                Ok(built) => {
                    let mut line = format!("switched to model {}", switched.model_id());
                    if fusion {
                        line.push_str(" (fusion off)");
                    }
                    rebuilt = Some(built);
                    next = switched;
                    line
                }
                Err(_) => format!("could not switch to {}", switched.model_id()),
            }
        };
        answer = match answer.is_empty() {
            true => line,
            false => format!("{answer}\n\n{line}"),
        };
    }
    let fusion = fusion && rebuilt.is_none();
    {
        let mut sessions = state.sessions.lock().await;
        if let Some(entry) = sessions.get_mut(&id) {
            if let Some((agent, cancel)) = rebuilt {
                entry.agent = agent;
                entry.cancel = cancel;
            }
            entry.selection = next.clone();
            entry.fusion = fusion;
        }
    }
    drop(agent);

    if !answer.is_empty() {
        connection.send_notification(SessionNotification::new(
            session_id.clone(),
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from(answer))),
        ))?;
    }
    if next != selection || providers_changed {
        let options = state.config_options(&next).await;
        connection.send_notification(SessionNotification::new(
            session_id.clone(),
            SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(options)),
        ))?;
    }
    responder.respond(PromptResponse::new(StopReason::EndTurn))
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

/// What a turn sends the client, in the order it happened.
enum Outgoing {
    Update(SessionNotification),
    /// A plan waiting for review, and the gate its verdict goes back through.
    Ask(RequestPermissionRequest, PlanGate),
}

const APPROVE_PLAN: &str = "approve";
const KEEP_PLANNING: &str = "keep-planning";

/// The client's answer to a plan review. A client that cannot ask (an error
/// back) gets what it had before the question existed: the plan goes ahead.
fn plan_verdict(
    answer: Result<agent_client_protocol::schema::v1::RequestPermissionResponse, acp::Error>,
) -> PlanVerdict {
    match answer {
        Ok(response) => match response.outcome {
            RequestPermissionOutcome::Selected(picked)
                if picked.option_id.0.as_ref() == APPROVE_PLAN =>
            {
                PlanVerdict::approve()
            }
            RequestPermissionOutcome::Selected(_) => {
                PlanVerdict::reject("the user wants to keep planning")
            }
            _ => PlanVerdict::reject("the review was cancelled"),
        },
        Err(_) => PlanVerdict::approve(),
    }
}

/// Translates one turn's [`AgentEvent`] stream into ACP `session/update`
/// notifications, synthesizing stable tool-call ids.
struct Translator {
    session_id: SessionId,
    updates: mpsc::UnboundedSender<Outgoing>,
    next_call_id: Arc<AtomicU64>,
    /// Per tool name: a FIFO of (call id, args) from starts awaiting their
    /// finishes — the event stream carries no id tying the two together.
    open_calls: HashMap<String, VecDeque<(String, Value)>>,
    /// A finished plan is put to the client; off, it is approved here.
    ask: bool,
}

impl Translator {
    fn send(&self, update: SessionUpdate) {
        let _ = self.updates.send(Outgoing::Update(SessionNotification::new(
            self.session_id.clone(),
            update,
        )));
    }

    /// Put a finished plan to the client as a permission request on the
    /// `exit_plan` call that produced it.
    fn ask_about_plan(&mut self, plan: String, gate: PlanGate) {
        let id = self
            .open_calls
            .get(crate::tools::plan::EXIT_PLAN_TOOL_NAME)
            .and_then(|calls| calls.back())
            .map(|(id, _)| id.clone())
            .unwrap_or_else(|| self.alloc_call_id());
        let call = ToolCallUpdate::new(
            ToolCallId::new(id),
            ToolCallUpdateFields::new()
                .title("Ready to code?".to_string())
                .kind(ToolKind::SwitchMode)
                .raw_input(serde_json::json!({ "plan": plan })),
        );
        let options = vec![
            PermissionOption::new(
                APPROVE_PLAN,
                "Yes, proceed",
                PermissionOptionKind::AllowOnce,
            ),
            PermissionOption::new(
                KEEP_PLANNING,
                "No, keep planning",
                PermissionOptionKind::RejectOnce,
            ),
        ];
        let request = RequestPermissionRequest::new(self.session_id.clone(), call, options);
        if let Err(mpsc::error::SendError(Outgoing::Ask(_, gate))) =
            self.updates.send(Outgoing::Ask(request, gate))
        {
            let _ = gate.answer(PlanVerdict::approve());
        }
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
                // exit_plan only succeeds by leaving plan mode, approved or by omakase. Say so
                // the way `/plan` does, or a client that tracks plan mode from those notices
                // keeps showing it while the agent writes.
                if name == crate::tools::plan::EXIT_PLAN_TOOL_NAME && !output.is_error {
                    self.send(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                        ContentBlock::from("[wizard] plan mode off".to_string()),
                    )));
                }
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
            // `/plan` turns plan mode on over ACP too, and a finished plan waits
            // for a verdict: the client is asked. The interview tool has no
            // ACP shape, so it is declined and the turn cannot wedge on it.
            AgentEvent::PlanReady { plan, gate } if self.ask => self.ask_about_plan(plan, gate),
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
        "write_file" | "edit_file" | "apply_patch" => ToolKind::Edit,
        "search_files" | "web_search" | "x_search" => ToolKind::Search,
        "execute" => ToolKind::Execute,
        "web_fetch" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

/// A one-line title for a tool call, preferring a path/command/query from its
/// arguments.
fn tool_title(name: &str, args: &Value) -> String {
    // `computer` has no path or command; its action and target are what a
    // reader of the thread wants to see.
    if name == crate::tools::computer::TOOL_NAME
        && let Some(action) = args.get("action").and_then(Value::as_str)
    {
        let xy = match (args.get("x"), args.get("y"), args.get("coordinate")) {
            (Some(x), Some(y), _) => Some((x.clone(), y.clone())),
            (_, _, Some(Value::Array(c))) if c.len() == 2 => Some((c[0].clone(), c[1].clone())),
            _ => None,
        };
        return match xy {
            Some((x, y)) => format!("{name}: {action} ({x}, {y})"),
            None => format!("{name}: {action}"),
        };
    }
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

/// Map any error into an ACP internal error. The whole cause chain rides in
/// `data`, which clients show beside the code: without it a signed-out
/// provider reaches the editor as a bare "Internal error", and the one line
/// that says to run `wizard --login` only lands in `~/.wizard/logs`.
fn internal<E: std::fmt::Display>(err: E) -> acp::Error {
    let detail = format!("{err:#}");
    tracing::warn!("acp: {detail}");
    acp::util::internal_error(detail)
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// The line `wizard --help` gives `acp`.
///
/// It used to be a doc comment on core's `clap` variant, which printed on
/// every build whether or not one had this plugin in it. Moved here verbatim,
/// minus the trailing stop `clap` strips off a doc comment on its way into
/// the same slot. What core kept is the shorter sentence for the build that
/// does not have this file at all.
const ABOUT: &str = "Run Wizard as an Agent Client Protocol (ACP) agent over stdio, so ACP \
                     editors (Zed, Neovim, Emacs) can embed it. Loads config but never \
                     onboards or opens a TUI — stdin/stdout carry the JSON-RPC protocol";

/// The ACP server, as a plugin.
///
/// The registration sits at the bottom of this file rather than in a
/// `plugin.rs` beside it. This file is one screen of scrolling and its
/// contract is the last thing in it, which is where a reader looks.
pub struct AcpPlugin {
    manifest: PluginManifest,
}

impl AcpPlugin {
    pub fn new() -> Self {
        Self {
            manifest: PluginManifest {
                name: "acp".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                description: "`wizard acp`: an Agent Client Protocol server over stdio, for \
                              editors that embed Wizard"
                    .to_string(),
                // Everything a headless agent can do, because that is what a
                // `session/new` builds — and this surface runs its tools with
                // no per-action approval gate, so the editor never sees a
                // permission prompt and the process does its own file and
                // shell I/O. `Ui` is the transcript: every agent event becomes
                // a `session/update` the editor paints.
                //
                // Declared, not enforced; see `native/plugin.rs`.
                capabilities: vec![
                    Capability::Filesystem,
                    Capability::Process,
                    Capability::Network,
                    Capability::Model,
                    Capability::Ui,
                    Capability::Agent,
                ],
                optional_deps: Vec::new(),
                // `docs/plugins.md` puts ACP in `server` by name: an editor on
                // a laptop driving a checkout on a headless box over stdio is
                // exactly the machine that has no window and wants this.
                profiles: vec!["server".to_string(), "default".to_string()],
            },
        }
    }
}

impl Default for AcpPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for AcpPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    /// One entrypoint and nothing else. There is no tool and no slash command
    /// to add: everything an ACP session runs is a built-in applied to that
    /// session's own agent, and the protocol is the surface rather than an
    /// extension of the ones that stay in.
    fn apply(&self, ctx: &mut Ctx) -> anyhow::Result<()> {
        ctx.provide(
            entrypoint::ACP,
            Service::native(Entrypoint::new(entrypoint::ACP, ABOUT, run)),
        );
        Ok(())
    }
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
        assert_eq!(
            tool_title(
                "computer",
                &json!({ "action": "left_click", "x": 5, "y": 6 })
            ),
            "computer: left_click (5, 6)"
        );
        assert_eq!(
            tool_title("computer", &json!({ "action": "screenshot" })),
            "computer: screenshot"
        );
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
            ask: true,
        };
        translator.handle(AgentEvent::StreamRetrying);
        let Ok(Outgoing::Update(notification)) = rx.try_recv() else {
            panic!("expected one update");
        };

        let SessionUpdate::AgentMessageChunk(chunk) = notification.update else {
            panic!("expected an assistant message chunk");
        };
        let ContentBlock::Text(text) = chunk.content else {
            panic!("expected text content");
        };
        assert!(text.text.contains("restarts below"), "{}", text.text);
    }

    /// A finished plan goes to the client as a permission request on the
    /// exit_plan call, after the call itself, and nothing answers it here.
    #[test]
    fn a_finished_plan_is_put_to_the_client() {
        let (updates, mut rx) = mpsc::unbounded_channel();
        let mut translator = Translator {
            session_id: SessionId::new("session-1"),
            updates,
            next_call_id: Arc::new(AtomicU64::new(1)),
            open_calls: HashMap::new(),
            ask: true,
        };
        translator.handle(AgentEvent::ToolStarted {
            name: crate::tools::plan::EXIT_PLAN_TOOL_NAME.to_string(),
            args: serde_json::json!({ "plan": "1. edit main.rs" }),
        });
        let (gate, mut verdict) = PlanGate::open();
        translator.handle(AgentEvent::PlanReady {
            plan: "1. edit main.rs".to_string(),
            gate,
        });
        let Ok(Outgoing::Update(started)) = rx.try_recv() else {
            panic!("the tool call goes first");
        };
        let SessionUpdate::ToolCall(call) = started.update else {
            panic!("expected the exit_plan call");
        };
        let Ok(Outgoing::Ask(request, gate)) = rx.try_recv() else {
            panic!("then the question");
        };
        assert_eq!(request.tool_call.tool_call_id, call.tool_call_id);
        let ids: Vec<_> = request
            .options
            .iter()
            .map(|o| o.option_id.0.to_string())
            .collect();
        assert_eq!(ids, [APPROVE_PLAN, KEEP_PLANNING]);
        assert!(verdict.try_recv().is_err(), "nobody answered yet");
        assert!(gate.answer(PlanVerdict::reject("no")));
        assert!(!verdict.try_recv().expect("the verdict").approved);
    }

    /// Leaving plan mode through exit_plan is announced like `/plan off`; a plan that was
    /// sent back leaves plan mode on and says nothing.
    #[test]
    fn an_approved_plan_says_plan_mode_is_off() {
        let (updates, mut rx) = mpsc::unbounded_channel();
        let mut translator = Translator {
            session_id: SessionId::new("session-1"),
            updates,
            next_call_id: Arc::new(AtomicU64::new(1)),
            open_calls: HashMap::new(),
            ask: true,
        };
        let mut texts = |output: crate::tools::ToolOutput| {
            translator.handle(AgentEvent::ToolStarted {
                name: crate::tools::plan::EXIT_PLAN_TOOL_NAME.to_string(),
                args: serde_json::json!({ "plan": "1. edit main.rs" }),
            });
            translator.handle(AgentEvent::ToolFinished {
                name: crate::tools::plan::EXIT_PLAN_TOOL_NAME.to_string(),
                output,
            });
            let mut texts = Vec::new();
            while let Ok(Outgoing::Update(n)) = rx.try_recv() {
                if let SessionUpdate::AgentMessageChunk(chunk) = n.update {
                    if let ContentBlock::Text(t) = chunk.content {
                        texts.push(t.text);
                    }
                }
            }
            texts
        };
        assert_eq!(
            texts(crate::tools::ToolOutput::ok(
                "Plan approved — proceed to execute it."
            )),
            ["[wizard] plan mode off"]
        );
        assert!(texts(crate::tools::ToolOutput::error("the plan was not accepted")).is_empty());
    }

    #[test]
    fn plan_review_answers_map_to_verdicts() {
        use agent_client_protocol::schema::v1::{
            RequestPermissionResponse, SelectedPermissionOutcome,
        };
        let pick = |id: &str| {
            Ok(RequestPermissionResponse::new(
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id.to_string())),
            ))
        };
        assert!(plan_verdict(pick(APPROVE_PLAN)).approved);
        assert!(!plan_verdict(pick(KEEP_PLANNING)).approved);
        assert!(
            !plan_verdict(Ok(RequestPermissionResponse::new(
                RequestPermissionOutcome::Cancelled
            )))
            .approved
        );
        assert!(
            plan_verdict(Err(acp::Error::method_not_found())).approved,
            "a client that cannot ask keeps the old behaviour"
        );
    }

    /// A loaded session replays in order: the user's words, the model's
    /// reasoning and reply, and each tool call closed by its result under the
    /// same id. System notes are the agent's bookkeeping, not transcript.
    #[test]
    fn history_updates_replay_the_transcript_in_order() {
        let mut call = llm::ToolCall::new("read_file", json!({ "path": "src/main.rs" }));
        call.id = "toolu_1".to_string();
        let mut thought = ChatMessage::assistant_turn("", Vec::new(), vec![call]);
        thought
            .content
            .insert(0, llm::ContentBlock::thinking("look first", None));
        let history = vec![
            ChatMessage::system("compacted context"),
            ChatMessage::user("what does main do?"),
            thought,
            ChatMessage::tool_result("toolu_1", "read_file", "fn main() {}"),
            ChatMessage::assistant("It is empty."),
        ];

        let updates = history_updates(&history);
        assert_eq!(updates.len(), 5, "{updates:?}");
        let text = |chunk: &ContentChunk| match &chunk.content {
            ContentBlock::Text(text) => text.text.clone(),
            other => panic!("expected text, got {other:?}"),
        };
        let SessionUpdate::UserMessageChunk(chunk) = &updates[0] else {
            panic!("expected the user message first: {:?}", updates[0]);
        };
        assert_eq!(text(chunk), "what does main do?");
        let SessionUpdate::AgentThoughtChunk(chunk) = &updates[1] else {
            panic!("expected the reasoning: {:?}", updates[1]);
        };
        assert_eq!(text(chunk), "look first");
        let SessionUpdate::ToolCall(call) = &updates[2] else {
            panic!("expected the tool call: {:?}", updates[2]);
        };
        assert_eq!(call.tool_call_id, ToolCallId::new("history-toolu_1"));
        assert_eq!(call.title, "read_file: src/main.rs");
        assert_eq!(call.kind, ToolKind::Read);
        let SessionUpdate::ToolCallUpdate(result) = &updates[3] else {
            panic!("expected the tool result: {:?}", updates[3]);
        };
        assert_eq!(result.tool_call_id, call.tool_call_id);
        assert_eq!(result.fields.status, Some(ToolCallStatus::Completed));
        let SessionUpdate::AgentMessageChunk(chunk) = &updates[4] else {
            panic!("expected the reply last: {:?}", updates[4]);
        };
        assert_eq!(text(chunk), "It is empty.");
    }

    /// A call that worked and printed `Error: ` replays as completed: the
    /// stored flag decides, not the text.
    #[test]
    fn history_updates_trust_the_flag_over_the_text() {
        let mut call = llm::ToolCall::new("execute", json!({ "command": "echo 'Error: x'" }));
        call.id = "toolu_3".to_string();
        let mut result = ChatMessage::tool_result("toolu_3", "execute", "Error: x");
        if let llm::ContentBlock::ToolResult(block) = &mut result.content[0] {
            block.is_error = Some(false);
        }
        let history = vec![
            ChatMessage::user("run it"),
            ChatMessage::assistant_turn("", Vec::new(), vec![call]),
            result,
        ];
        let updates = history_updates(&history);
        let SessionUpdate::ToolCallUpdate(update) = &updates[2] else {
            panic!("expected the tool result: {:?}", updates[2]);
        };
        assert_eq!(update.fields.status, Some(ToolCallStatus::Completed));
    }

    /// A call that failed replays as failed, not as one that worked.
    #[test]
    fn history_updates_keep_a_failed_call_failed() {
        let mut call = llm::ToolCall::new("edit_file", json!({ "path": "main.rs" }));
        call.id = "toolu_2".to_string();
        let history = vec![
            ChatMessage::user("edit it"),
            ChatMessage::assistant_turn("", Vec::new(), vec![call]),
            ChatMessage::tool_result("toolu_2", "edit_file", "Error: old_string not found"),
        ];
        let updates = history_updates(&history);
        let SessionUpdate::ToolCallUpdate(result) = &updates[2] else {
            panic!("expected the tool result: {:?}", updates[2]);
        };
        assert_eq!(result.fields.status, Some(ToolCallStatus::Failed));
    }

    fn listed(id: &str, cwd: &str, title: &str) -> ListedSession {
        ListedSession {
            id: id.to_string(),
            cwd: cwd.to_string(),
            title: title.to_string(),
            updated_at: None,
        }
    }

    /// Editors page through history by cursor, scoped to the open project.
    #[test]
    fn session_list_pages_newest_first_within_the_requested_cwd() {
        let sessions: Vec<ListedSession> = (0..SESSION_LIST_PAGE + 3)
            .rev()
            .map(|n| listed(&format!("2026-09-22T10-{n:02}-00"), "/repo", "prompt"))
            .chain([listed("2026-09-22T09-00-00", "/elsewhere", "other project")])
            .collect();
        let ids = |infos: &[SessionInfo]| {
            infos
                .iter()
                .map(|info| info.session_id.0.to_string())
                .collect::<Vec<_>>()
        };

        let (first, cursor) = session_list_page(
            sessions.iter().map(|s| listed(&s.id, &s.cwd, &s.title)),
            Some(Path::new("/repo")),
            None,
        );
        assert_eq!(first.len(), SESSION_LIST_PAGE);
        assert_eq!(
            ids(&first)[0],
            format!("2026-09-22T10-{:02}-00", SESSION_LIST_PAGE + 2)
        );
        let cursor = cursor.expect("a second page");

        let (second, next) = session_list_page(
            sessions.iter().map(|s| listed(&s.id, &s.cwd, &s.title)),
            Some(Path::new("/repo")),
            Some(&cursor),
        );
        assert_eq!(
            ids(&second),
            [
                "2026-09-22T10-02-00",
                "2026-09-22T10-01-00",
                "2026-09-22T10-00-00"
            ]
        );
        assert_eq!(next, None);

        // Without a cwd every project's sessions are listed.
        let (all, _) = session_list_page(
            sessions.iter().map(|s| listed(&s.id, &s.cwd, &s.title)),
            None,
            Some("2026-09-22T10-00-00"),
        );
        assert_eq!(ids(&all), ["2026-09-22T09-00-00"]);
    }

    #[test]
    fn session_titles_are_trimmed_and_capped() {
        assert_eq!(truncate_title("  fix the build \n"), "fix the build");
        let long = "é".repeat(SESSION_TITLE_CHARS + 5);
        let title = truncate_title(&long);
        assert_eq!(title.chars().count(), SESSION_TITLE_CHARS + 1);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn only_a_header_only_session_file_counts_as_unused() {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::create_in(dir.path(), dir.path()).unwrap();
        assert!(session_file_unused(session.path()));
        session.append(&ChatMessage::user("hello")).unwrap();
        assert!(!session_file_unused(session.path()));
        // A file that is gone or unreadable is never "unused": nothing to delete.
        assert!(!session_file_unused(&dir.path().join("missing.jsonl")));
    }

    #[test]
    fn session_ids_cannot_leave_the_sessions_directory() {
        for id in [
            "2026-09-22T13-08-00",
            "2026-09-22T13-08-00-002",
            "imported_a1.b2",
        ] {
            assert!(is_session_id(id), "{id}");
        }
        for id in [
            "",
            ".",
            "..",
            "../config",
            "a/b",
            "a\\b",
            ".hidden",
            "/etc/passwd",
        ] {
            assert!(!is_session_id(id), "{id}");
        }
    }

    #[test]
    fn truncate_marks_the_cut_and_respects_boundaries() {
        assert_eq!(truncate("short", 100), "short");
        let long = "a".repeat(9000);
        let cut = truncate(&long, TOOL_OUTPUT_CAP);
        assert!(cut.ends_with("… (truncated)"));
        assert!(cut.len() < long.len());
    }

    /// `apply` registers the one thing it claims to, under the name and the
    /// argument type core looks it up with. A kernel of its own rather than
    /// the process one, so this still means something in a binary where some
    /// other test already booted plugins.
    #[test]
    fn applying_the_plugin_registers_the_acp_entrypoint() {
        let kernel = crate::kernel::Kernel::new(crate::kernel::KernelOptions::default());
        kernel
            .load(std::sync::Arc::new(AcpPlugin::new()))
            .expect("the acp plugin loads");
        let found = kernel
            .services()
            .inject_as::<Entrypoint<Config>>(entrypoint::ACP)
            .expect("the server registered its entrypoint");
        assert_eq!(found.name(), entrypoint::ACP);
    }

    /// Unloading takes it back, so a reload does not leave two servers
    /// answering to one name.
    #[tokio::test]
    async fn unloading_the_plugin_withdraws_the_entrypoint() {
        let kernel = crate::kernel::Kernel::new(crate::kernel::KernelOptions::default());
        let id = kernel
            .load(std::sync::Arc::new(AcpPlugin::new()))
            .expect("the acp plugin loads");
        kernel.unload(&id).await.expect("it unloads");
        assert!(
            kernel
                .services()
                .inject_as::<Entrypoint<Config>>(entrypoint::ACP)
                .is_none(),
            "the entrypoint outlived the plugin that registered it"
        );
    }
}
