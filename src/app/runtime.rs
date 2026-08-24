//! Genie-mode entry point: the terminal event loop that drives [`App`],
//! starts agent turns, and drains queued messages and agent commands.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::FutureExt;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

use crate::agent::{
    Agent, AgentEvent, CancelHandle, DoneReason, FinishedNotification, ForkContext,
    SideQuestionContext,
};
use crate::commands::SlashCommand;
use crate::config::{Config, StepBudget};
use crate::event::{Event, EventLoop};
use crate::llm::provider::LlmProvider;
use crate::mcp::{McpConfig, McpManager};
use crate::session_registry;
use crate::skills::Skill;

use crate::cli::Cli;

use std::io::IsTerminal;

use crate::image_view::ImageCache;

use super::command::CommandContext;
use super::recover::{
    COMPACTION_DEADLINE, DrawFaults, RebuildRecovery, TerminalWatchdog, rebuild_recovery,
    spawn_answering, turn_failure, within,
};
use super::session::{
    SessionTarget, build_agent, is_local_kind, load_skill_roots, restore_ultra,
    spawn_session_rebuild, startup_client,
};
use super::term::{
    TerminalGuard, copy_to_clipboard, edit_config_file, edit_prompt_in_editor, is_terminal_armed,
    restore_terminal_best_effort, setup_terminal,
};
use super::{AgentRebuild, App, AppAction, INTERRUPT_GRACE};

/// Genie-mode entry point: set up the terminal (raw mode + alternate
/// screen), build the agent stack (LLM provider, registry with scripted +
/// MCP tools, skills, session), pre-fill `cli.prompt` if given, and drive
/// the [`EventLoop`](crate::event::EventLoop) until quit. Restores the
/// terminal on exit and on panic. Returns the process exit code: 0 from the
/// TUI itself; the headless fallback propagates its outcome code.
pub async fn run_tui(mut config: Config, cli: Cli) -> Result<i32> {
    // No usable terminal: run headless when a task was given, otherwise we
    // cannot do anything sensible.
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        if cli.prompt.is_some() {
            return crate::headless::run(config, cli).await;
        }
        anyhow::bail!("wizard needs a terminal for the TUI; pass -p \"task\" to run headless");
    }

    let mut client = startup_client(&mut config).await?;
    // A cloud provider's health probe was skipped at startup (it would block the
    // first paint); run_tui runs it in the background below. Local providers
    // already proved themselves in startup_client (and loaded the model).
    let active_is_cloud = !is_local_kind(&config.active().kind);

    let project_root = std::env::current_dir().context("resolving project root")?;

    // Settle the per-project trust question here, and only here.
    //
    // This is the one moment of the TUI's life when the terminal is genuinely
    // ours: the `is_terminal` checks above have passed, `setup_terminal` has
    // not run (no raw mode, no alternate screen), and `EventLoop`, whose task
    // owns crossterm's `EventStream` and drains stdin, is not started until
    // after it. So a blocking `read_line` is safe *here* and catastrophic
    // three hundred lines further down, which is why `trust` takes the
    // capability as a declaration from the caller rather than probing for a
    // tty (see `crate::trust::Console`).
    //
    // Everything downstream keeps declaring nothing: `build_agent` below and
    // every mid-session rebuild (`/model`, a provider switch, `/fusion`, crash
    // recovery) go through `hooks::load`, which refuses rather than asks. They
    // find the answer this call recorded and ask nothing.
    //
    // The refusal is *not* printed. The alternate screen wipes the terminal a
    // moment later, so anything written here is invisible; it goes into the
    // transcript as a notice instead, once `App` exists (below). Routing the
    // question itself through the TUI's own modal path (the `PlanReady` /
    // `Interview` shape) would mean the rebuild tasks blocking on a `oneshot`
    // back through the event loop, and a rebuild is not a turn: nothing owns
    // its answer, `/model` would hang behind a modal, and it would re-ask on
    // every branch switch. One question, before the TUI, is the smaller design.
    let trust_refusal = crate::trust::preflight(&project_root);

    let mut skills = load_skill_roots();

    let mcp_path = Config::mcp_config_path()?;
    // Start with no MCP servers connected. Connecting them means spawning stdio
    // servers and running the `initialize` handshake (e.g. `npx @playwright/mcp`,
    // ~2s) — far too slow to block the first paint. The connect runs on a
    // background task once the TUI is up (see below); its tools merge into the
    // registry via `Event::McpConnected`. Built-in tools work immediately.
    // Shared with background rebuild tasks (model switch, crash recovery).
    let manager = Arc::new(Mutex::new(McpManager::empty()));

    let mut agent_slot: Option<Agent> = Some(
        build_agent(
            &client,
            &config,
            &skills,
            &project_root,
            &*manager.lock().await,
            if cli.resume {
                SessionTarget::Latest
            } else {
                SessionTarget::Fresh
            },
        )
        .await?,
    );
    // Seed so `/btw` and `/fork` work before any turn has run (system prompt
    // alone is enough context for a factual aside or a first side quest).
    let mut side_question_snapshot: Option<SideQuestionContext> = agent_slot
        .as_ref()
        .map(|agent| agent.side_question_context());
    let mut fork_snapshot: Option<ForkContext> =
        agent_slot.as_ref().map(|agent| agent.fork_context());
    // When no turn is in flight, fork progress still needs a live channel so
    // the subagent rail can open panes. The collector below forwards into the
    // main event loop; recreated whenever the previous one ends.
    let mut idle_fork_tx: Option<mpsc::Sender<AgentEvent>> = None;
    // `--plan` / `plan_first = true`: the session starts in plan mode (the
    // App mirror is set from the same config in App::new below).
    if config.plan_first
        && let Some(agent) = agent_slot.as_mut()
    {
        agent.set_plan_mode(true);
    }
    // `--omakase` / `omakase = true`: chef's choice. The badge in the status
    // line is lit from `config.omakase` alone (see `App::new`), so without
    // this the TUI claimed omakase while the agent ran plain plan mode — no
    // `OMAKASE_PROMPT`, `interview` still asking, `exit_plan` still opening the
    // human review modal. `set_omakase(true)` turns plan mode on by itself,
    // which is what makes `omakase = true` in config.toml — with no `--plan`
    // and no `plan_first` — land the agent in the mode the badge advertises.
    if config.omakase
        && let Some(agent) = agent_slot.as_mut()
    {
        agent.set_omakase(true);
    }
    let mut agent_task: Option<JoinHandle<Agent>> = None;
    // The running turn's cooperative cancel handle, cloned off the agent before
    // it moved into `agent_task` — which is the only moment it can be had, and
    // the reason it is kept here rather than reached for on Ctrl-C.
    let mut agent_cancel: Option<CancelHandle> = None;
    // When the cooperative interrupt gives up and the task is aborted instead.
    // `Some` only between a Ctrl-C and the turn actually stopping.
    let mut interrupt_at: Option<Instant> = None;

    // Genie-mode max_steps as configured, used when switching back from
    // sovereign in-session.
    let genie_max_steps = config.max_steps;

    // Identity for this session's dashboard heartbeat.
    let session_id = agent_slot
        .as_ref()
        .map(|agent| agent.session().id.clone())
        .unwrap_or_default();
    let session_name = cli
        .prompt
        .as_deref()
        .and_then(|prompt| prompt.lines().next())
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .map(|line| line.chars().take(48).collect::<String>())
        .unwrap_or_else(|| {
            // No prompt: name the session after its working directory.
            project_root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "session".to_string())
        });

    let mut app = App::new(config);
    app.project_root = project_root.clone();
    app.custom_commands = crate::commands::load(&project_root);
    app.session_id = session_id.clone();
    app.session_name = session_name;
    // The rail kills background subagents through this, so it must be reachable
    // while a turn holds the agent — hence a cloned Arc, not the agent itself.
    app.subagents = agent_slot.as_ref().map(|agent| agent.subagent_registry());
    app.tasks = agent_slot.as_ref().map(|agent| agent.task_registry());
    app.session_started_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Register this session so other sessions' /dashboard can see it.
    crate::session_registry::write(&app.session_record());

    // Join the mesh, if `[mesh] listen` says a peer could watch this node.
    //
    // After the session id, because every event the tee publishes is stamped
    // with it and a watcher demuxes on it. A failure is a notice rather than a
    // fatal: the port being taken (another Wizard is already listening, most
    // likely) is a reason not to be watchable, not a reason not to work.
    match super::MeshTee::join(&app.config, &app.session_id).await {
        Ok(Some(tee)) => {
            let at = tee
                .listening_at()
                .map_or_else(|_| "?".to_string(), |at| at.to_string());
            // Said out loud, always. A socket this process opened because a
            // config file asked it to is exactly the thing a user should not
            // have to go and check for.
            app.notice(format!(
                "mesh: listening on {at} as {} — trusted peers may watch this session",
                tee.address()
            ));
            app.mesh = Some(tee);
        }
        Ok(None) => {}
        Err(err) => app.notice(format!(
            "mesh: not listening — {err:#}\n\
             this session runs normally; no peer can watch it"
        )),
    }
    // `wizard agents` opens straight into the dashboard.
    if matches!(cli.command, Some(crate::cli::Command::Agents)) {
        app.show_dashboard = true;
        app.refresh_sessions();
        app.refresh_peek();
    }
    if let Some(prompt) = cli.prompt.clone() {
        app.set_input(prompt);
    }
    // No startup notice: the welcome screen already shows the model, mode,
    // and help pointers until the first message arrives.

    // ...except this one. The project ships hooks that are not going to run,
    // and the user has to be told where they went: silently dropping them is
    // what makes "my hooks stopped firing" unanswerable. It reads as
    // conversation, so it survives the first draw and scrolls with history.
    if let Some(why) = trust_refusal {
        app.notice(format!("wizard: {why}"));
    }

    // session_start hooks fire before the first draw; their activity (and
    // any failures) lands in the transcript as notices.
    {
        let (hook_tx, mut hook_rx) = mpsc::channel::<AgentEvent>(256);
        if let Some(agent) = agent_slot.as_mut() {
            agent.fire_session_start(&hook_tx).await;
        }
        drop(hook_tx);
        while let Some(event) = hook_rx.recv().await {
            app.handle_agent_event(event);
        }
    }

    // Ask the terminal how it can draw an image, while stdio is still the plain
    // terminal: the query writes escape sequences and reads the reply, which the
    // alternate screen and our own raw mode would both get in the way of. A
    // terminal that says nothing gets half-blocks, which every terminal can draw.
    *app.images.borrow_mut() = ImageCache::detect();
    tracing::debug!("terminal images: {:?}", app.images.borrow());

    // Terminal setup (raw mode, alternate screen, keyboard-enhancement probe)
    // must finish *before* EventLoop starts. EventLoop spawns a task that
    // immediately owns crossterm's EventStream; if that stream is already
    // draining stdin, `supports_keyboard_enhancement` can miss its CSI reply
    // and loop forever on poll errors — blank alternate screen, only Ctrl-C
    // gets you out. First paint also wants the terminal fully configured.
    let mut terminal = setup_terminal()?;
    let _guard = TerminalGuard;
    let mut events = EventLoop::new(Duration::from_millis(100));

    // Probe the cloud provider's health off the draw path so the network
    // round-trip doesn't delay launch. Only a failure is surfaced; success is
    // silent. The failure goes to `Event::ProviderHealthFailed` (not a plain
    // notice) so the main loop can show it where it's visible pre-conversation
    // — otherwise the welcome screen hides it until the first message fails.
    if active_is_cloud {
        let probe = client.clone();
        let notify = events.sender();
        tokio::spawn(async move {
            if let Err(err) = probe.health().await {
                let _ = notify
                    .send(Event::ProviderHealthFailed(format!("{err:#}")))
                    .await;
            }
        });
    }

    // Connect MCP servers off the draw path so a slow stdio server (npx, etc.)
    // can't delay launch. When the connect finishes, the main loop rebuilds the
    // registry from the now-populated manager (`Event::McpConnected`). The
    // indicator goes up unconditionally and comes down on every exit path
    // (no-servers early return, success, failure) so a message sent before the
    // tools arrive isn't a silent surprise.
    app.mcp_connecting = true;
    {
        let manager = Arc::clone(&manager);
        let mcp_path = mcp_path.clone();
        // A panic while connecting (a malformed `initialize` reply, a server
        // that closes its pipe mid-handshake) would otherwise leave the
        // "connecting MCP…" indicator up for the rest of the session.
        spawn_answering(
            events.sender(),
            Event::McpConnected {
                connected: 0,
                configured: 0,
            },
            async move {
                let mcp_config = match McpConfig::load(&mcp_path) {
                    Ok(config) => config,
                    Err(err) => {
                        tracing::warn!("loading {}: {err:#}", mcp_path.display());
                        // Tell the loop to clear the indicator (no servers will
                        // connect): nothing configured, nothing missing.
                        return Some(Event::McpConnected {
                            connected: 0,
                            configured: 0,
                        });
                    }
                };
                if mcp_config.servers.is_empty() {
                    // Nothing to connect: keep the empty manager and skip the
                    // registry rebuild entirely (the agent already has every
                    // tool).
                    return Some(Event::McpConnected {
                        connected: 0,
                        configured: 0,
                    });
                }
                let configured = mcp_config.servers.len();
                {
                    let mut manager = manager.lock().await;
                    if let Err(err) = manager.reload(&mcp_config).await {
                        tracing::warn!("connecting MCP servers: {err:#}");
                    }
                }
                let connected = manager.lock().await.connection_count();
                Some(Event::McpConnected {
                    connected,
                    configured,
                })
            },
        );
    }

    let mut last_heartbeat = Instant::now();
    // See `recover::DrawFaults` and `recover::TerminalWatchdog`: the loop
    // rides out a bad frame instead of exiting, and takes the terminal back if
    // something restored it while the TUI was still using it.
    let mut draw_faults = DrawFaults::new();
    let mut watchdog = TerminalWatchdog::new();
    // Consecutive background rebuilds that came back empty-handed. Reset on
    // the first one that produces an agent.
    let mut failed_rebuilds: u32 = 0;

    loop {
        // A panic on a spawned task runs the process-wide hook in `main`,
        // which restores the terminal — correctly, because it cannot know the
        // process is going to survive. It does, tokio having caught the panic,
        // and the TUI is now painting into the user's scrollback with raw mode
        // off. Take the terminal back before drawing anything into it.
        if watchdog.should_rearm(is_terminal_armed(), Instant::now()) {
            match setup_terminal() {
                Ok(fresh) => {
                    terminal = fresh;
                    let _ = terminal.clear();
                    app.notice(
                        "the terminal was reset by a background failure — display restored \
                         (see the session log for the panic)",
                    );
                }
                Err(err) => tracing::warn!("could not re-arm the terminal: {err:#}"),
            }
        }

        if let Err(err) = terminal.draw(|frame| crate::ui::draw(frame, &app)) {
            if draw_faults.failed() {
                return Err(
                    anyhow::Error::new(err).context("the terminal stopped accepting frames")
                );
            }
            // One failed write is routinely an `EINTR` or a full pty buffer,
            // not a dead terminal. Say so once, then keep drawing.
            if draw_faults.consecutive() == 1 {
                tracing::warn!("frame not drawn: {err}");
            }
        } else {
            draw_faults.succeeded();
        }

        // Refresh this session's heartbeat so other dashboards see it live.
        if last_heartbeat.elapsed() >= Duration::from_secs(3) {
            session_registry::write(&app.session_record());
            last_heartbeat = Instant::now();
        }

        let Some(event) = events.next().await else {
            break;
        };

        // A background rebuild finished: restore the agent into the slot.
        if let Event::AgentRebuilt(rebuild) = event {
            let rebuild = *rebuild;
            app.rebuilding = None;
            let was_compacting = app.compacting;
            app.compacting = false;
            if let Some(model) = rebuild.model {
                app.config.model = model.clone();
                app.status.model = model;
            }
            if let Some(mut agent) = rebuild.agent {
                // A rebuilt agent starts with plan mode off; restore the
                // session's setting.
                if app.plan_mode {
                    agent.set_plan_mode(true);
                }
                restore_ultra(&app, &mut agent);
                // A rebuild brings a fresh tool context, so the old registry
                // handle is dead — re-point the rail at the live one.
                app.subagents = Some(agent.subagent_registry());
                // After /compact the history shrank: refresh the context
                // meter to the post-compact estimate (last_prompt was cleared
                // so context_tokens() falls back to a char/4 estimate of the
                // remaining history) instead of leaving the pre-compact size.
                if was_compacting {
                    app.status.context_tokens = agent.context_tokens();
                }
                // Fresh agent owns the conversation again; any mid-turn
                // snapshot is stale.
                side_question_snapshot = Some(agent.side_question_context());
                fork_snapshot = Some(agent.fork_context());
                agent_slot = Some(agent);
            }
            app.notice(rebuild.notice);
            // An empty agent slot is invisible: the composer still accepts
            // text, `drain_message_queue` still declines to start a turn, and
            // the session is over without ever saying so. Try again from the
            // session file, and when that stops working say plainly what the
            // state is instead of retrying into the void.
            match rebuild_recovery(agent_slot.is_some(), failed_rebuilds + 1) {
                RebuildRecovery::Idle => failed_rebuilds = 0,
                RebuildRecovery::Retry => {
                    failed_rebuilds += 1;
                    spawn_session_rebuild(
                        &mut app,
                        &client,
                        &skills,
                        &project_root,
                        &manager,
                        events.sender(),
                        "agent restored",
                    );
                }
                RebuildRecovery::GiveUp => {
                    failed_rebuilds += 1;
                    app.notice(
                        "error: the agent could not be rebuilt — this session cannot run \
                         another turn; /quit and relaunch",
                    );
                }
            }
            // A `/model` in the queue triggered this rebuild and deferred the
            // rest of the queued commands; drain them now the agent is back.
            drain_agent_commands(
                &mut app,
                &mut client,
                &mut agent_slot,
                &manager,
                &mut skills,
                &project_root,
                &mcp_path,
                genie_max_steps,
                &events,
            )
            .await;
            // Then any user prompts that were typed mid-turn.
            drain_message_queue(
                &mut app,
                &mut agent_slot,
                &mut agent_task,
                &mut agent_cancel,
                &mut interrupt_at,
                &mut side_question_snapshot,
                &mut fork_snapshot,
                &mut idle_fork_tx,
                &events,
            );
            continue;
        }

        if let Event::BtwFinished = event {
            app.btw_inflight = false;
            continue;
        }

        // The background MCP connect finished: merge the servers' tools into the
        // live agent's registry. If a turn is running the agent is out of its
        // slot, so defer the merge until the turn returns it.
        if let Event::McpConnected {
            connected,
            configured,
        } = event
        {
            app.mcp_connecting = false;
            // Some configured servers came up but not all: surface the shortfall
            // as an `error:`-prefixed notice (bold/white, counts as conversation)
            // — the actionable counterpart to the now-silent success path.
            if connected < configured {
                app.notice(format!(
                    "error: {} of {configured} MCP servers failed to connect (see logs)",
                    configured - connected
                ));
            }
            if connected > 0 {
                if agent_slot.is_some() {
                    CommandContext {
                        app: &mut app,
                        client: &mut client,
                        agent_slot: &mut agent_slot,
                        manager: &manager,
                        skills: &mut skills,
                        project_root: &project_root,
                        mcp_path: &mcp_path,
                        genie_max_steps,
                        events: &events,
                    }
                    .merge_mcp_registry()
                    .await;
                } else {
                    app.mcp_merge_pending = true;
                }
            }
            continue;
        }

        // The deferred cloud-provider health probe failed: store the error so
        // it shows at launch (home screen + status bar) rather than only on the
        // first message.
        if let Event::ProviderHealthFailed(err) = event {
            app.provider_health_error = Some(err);
            continue;
        }

        // A background sign-in succeeded: add and switch to the provider. Owned
        // here because it mutates config and the agent slot.
        if let Event::ProviderActivated(cfg) = event {
            CommandContext {
                app: &mut app,
                client: &mut client,
                agent_slot: &mut agent_slot,
                manager: &manager,
                skills: &mut skills,
                project_root: &project_root,
                mcp_path: &mcp_path,
                genie_max_steps,
                events: &events,
            }
            .add_provider_config(
                *cfg,
                "signed in to xAI — provider added and active".to_string(),
            )
            .await;
            continue;
        }

        let turn_done = matches!(&event, Event::Agent(AgentEvent::Done { .. }));

        // An event handler that fails is a failure to react to one keystroke.
        // Propagating it here would unwind out of `run_tui` and end the
        // process, taking the conversation with it — for a keypress the user
        // could simply have pressed again. It goes in the transcript instead.
        let action = match app.handle_event(event) {
            Ok(action) => action,
            Err(err) => {
                app.notice(format!("error: could not handle that input: {err:#}"));
                None
            }
        };
        if let Some(action) = action {
            match action {
                AppAction::Submit(prepared) => {
                    if !start_agent_turn(
                        &mut app,
                        &mut agent_slot,
                        &mut agent_task,
                        &mut agent_cancel,
                        &mut interrupt_at,
                        &mut side_question_snapshot,
                        &mut fork_snapshot,
                        &mut idle_fork_tx,
                        &events,
                        prepared,
                    ) {
                        app.notice("the agent is busy — wait for the current turn to finish");
                    }
                }
                AppAction::Command(command) => {
                    CommandContext {
                        app: &mut app,
                        client: &mut client,
                        agent_slot: &mut agent_slot,
                        manager: &manager,
                        skills: &mut skills,
                        project_root: &project_root,
                        mcp_path: &mcp_path,
                        genie_max_steps,
                        events: &events,
                    }
                    .run(command)
                    .await;
                }
                AppAction::Interrupt => {
                    // Ask the turn to stop before killing it. The agent checks
                    // the cancel flag between stream chunks and between tool
                    // calls, and `/ultra`'s pre-phase selects on it — so on the
                    // cooperative path every subagent closes its own pane out,
                    // the partial answer stays on screen, and the agent comes
                    // back through the ordinary Done path with no rebuild at
                    // all. The flag cannot shorten a tool call that is already
                    // running, though (a 5-minute `cargo build` is 5 minutes),
                    // so the abort below takes over after `INTERRUPT_GRACE`.
                    if agent_task.is_some() {
                        match (&agent_cancel, interrupt_at) {
                            // Asked already and it has not stopped: the user is
                            // pressing again, so stop waiting for it.
                            (_, Some(_)) => interrupt_at = Some(Instant::now()),
                            (Some(cancel), None) => {
                                cancel.cancel();
                                interrupt_at = Some(Instant::now() + INTERRUPT_GRACE);
                            }
                            // A turn with no handle cannot be asked, only
                            // killed. (It does not happen — the handle is cloned
                            // wherever the task is spawned — but a Ctrl-C that
                            // did nothing at all would be the worse failure.)
                            (None, None) => interrupt_at = Some(Instant::now()),
                        }
                    }
                }
                AppAction::CopySelection => {
                    // The drag finished: re-render and read the cells under the
                    // selection from the fresh frame. (After a completed
                    // `Terminal::draw` the swapped-in current buffer is reset,
                    // so reading `current_buffer_mut` here would find only
                    // blanks — clearing the selection the moment the button is
                    // released.) The highlight stays on screen until the next
                    // keystroke / click / scroll.
                    if let Some(selection) = app.selection {
                        let mut text = String::new();
                        // A failed frame here costs one copy, not the session:
                        // the next loop iteration redraws, and the user can
                        // drag again. The main draw above is what decides
                        // whether the terminal is actually gone.
                        let drawn = terminal.draw(|frame| {
                            crate::ui::draw(frame, &app);
                            text = crate::ui::selection_text(frame.buffer_mut(), &selection);
                        });
                        if let Err(err) = drawn {
                            app.notice(format!("could not read the selection: {err}"));
                            app.selection = None;
                        } else if text.is_empty() {
                            app.selection = None;
                        } else {
                            match copy_to_clipboard(&text) {
                                // A copy that could not take every route it
                                // wanted still says so: the one thing worse
                                // than a failed copy is a successful-looking
                                // one that pastes nothing.
                                Ok(Some(notice)) => app.notice(notice),
                                Ok(None) => {}
                                Err(err) => {
                                    app.notice(format!("could not copy selection: {err:#}"));
                                }
                            }
                        }
                        // An ordinary copy is silent: the persistent highlight
                        // is the feedback, and an unchanged transcript keeps
                        // the highlight aligned with the selected rows.
                    }
                }
            }
        }

        // The `/settings` "Open config file" row asks the main loop (the
        // terminal owner) to suspend the TUI and run an external editor.
        if app.pending_edit_config {
            app.pending_edit_config = false;
            edit_config_file(&mut app, &mut terminal);
        }

        // Ctrl-G: same suspend/restore dance, on the composer draft.
        if app.pending_edit_prompt {
            app.pending_edit_prompt = false;
            edit_prompt_in_editor(&mut app, &mut terminal);
        }

        // `/compact`: take the agent and summarize history off the event loop
        // so the TUI keeps animating the progress bar. The agent returns via
        // Event::AgentRebuilt, the same path as crash recovery.
        if app.pending_compact {
            app.pending_compact = false;
            match agent_slot.take() {
                Some(mut agent) => {
                    app.compacting = true;
                    // The agent has left its slot, so a panic in the summariser
                    // would strand it: no `AgentRebuilt`, no agent, `compacting`
                    // lit forever, and every message from then on silently
                    // queued. The fallback hands the loop an empty rebuild,
                    // which its recovery path turns back into a working agent.
                    spawn_answering(
                        events.sender(),
                        Event::AgentRebuilt(Box::new(AgentRebuild {
                            agent: None,
                            model: None,
                            notice: "compacting crashed — restarting the agent".to_string(),
                        })),
                        async move {
                            // Bounded for the same reason the rebuild is: a
                            // summarisation call that never returns holds the
                            // agent hostage, and there is no key that gets it
                            // back.
                            let rebuild = match within(
                                "compacting the conversation",
                                COMPACTION_DEADLINE,
                                agent.compact_now(),
                            )
                            .await
                            {
                                Ok(outcome) => AgentRebuild {
                                    agent: Some(agent),
                                    model: None,
                                    notice: outcome.describe(),
                                },
                                // The agent is dropped with the timed-out
                                // future; the loop rebuilds it from the
                                // session, which is where the history it was
                                // compacting lives anyway.
                                Err(timed_out) => AgentRebuild {
                                    agent: None,
                                    model: None,
                                    notice: timed_out,
                                },
                            };
                            Some(Event::AgentRebuilt(Box::new(rebuild)))
                        },
                    );
                }
                None => app.notice("the agent is busy — try again in a moment"),
            }
        }

        // `/btw`: fork a one-shot, tool-less completion against a snapshot of
        // the conversation. Works while a turn holds the agent — that is the
        // point — by reading the mid-turn snapshot kept below, falling back to
        // the live agent when idle.
        if let Some(question) = app.pending_btw.take() {
            let ctx = agent_slot
                .as_ref()
                .map(|agent| agent.side_question_context())
                .or_else(|| side_question_snapshot.clone());
            match ctx {
                Some(ctx) => {
                    app.btw_inflight = true;
                    let notify = events.sender();
                    // `btw_inflight` gates the next `/btw`; a panic that never
                    // sent `BtwFinished` disabled the command for the rest of
                    // the session with nothing on screen to explain it.
                    spawn_answering(notify.clone(), Event::BtwFinished, async move {
                        let notice = match ctx.ask(&question).await {
                            Ok(answer) => format!("/btw {question}\n{answer}"),
                            Err(err) => format!("/btw failed: {err:#}"),
                        };
                        let _ = notify.send(Event::Notice(notice)).await;
                        Some(Event::BtwFinished)
                    });
                }
                None => {
                    // No agent in the slot and no mid-turn snapshot — still
                    // try a bare context from the idle agent once it's back,
                    // but right now there's nothing to ask against.
                    app.notice(
                        "no conversation context for /btw yet — wait for the agent to finish rebuilding",
                    );
                }
            }
        }

        // `/fork`: detach a side quest that inherits the full conversation.
        // Works mid-turn via the snapshot captured when the turn started.
        // Progress streams on the turn's event channel while a turn is running,
        // or on a dedicated idle collector otherwise.
        if let Some(task) = app.pending_fork.take() {
            let ctx = agent_slot
                .as_ref()
                .map(|agent| agent.fork_context())
                .or_else(|| fork_snapshot.clone());
            match ctx {
                Some(ctx) => {
                    // Prefer the live turn's forwarder when one is up; otherwise
                    // ensure an idle collector so panes still open between turns.
                    let progress = if agent_task.is_some() {
                        // A turn is running: its forwarder is already pumping
                        // AgentEvents into the loop. We don't hold that sender
                        // here, so open a sibling collector on the same path.
                        ensure_idle_fork_tx(&mut idle_fork_tx, &events)
                    } else {
                        ensure_idle_fork_tx(&mut idle_fork_tx, &events)
                    };
                    let notify = events.sender();
                    tokio::spawn(async move {
                        match ctx.spawn(&task, Some(progress)).await {
                            Ok(id) => {
                                let _ = notify
                                    .send(Event::Notice(format!("fork #{id} started: {task}")))
                                    .await;
                            }
                            Err(err) => {
                                let _ = notify
                                    .send(Event::Notice(format!("/fork failed: {err:#}")))
                                    .await;
                            }
                        }
                    });
                }
                None => {
                    app.notice(
                        "no conversation context for /fork yet — wait for the agent to finish rebuilding",
                    );
                }
            }
        }

        // Between turns, drain any background tasks/subagents (including forks)
        // that finished while the agent was idle, so their reports land in
        // history without waiting for the next user message.
        if agent_task.is_none()
            && let Some(agent) = agent_slot.as_mut()
        {
            for notification in agent.drain_finished_notifications() {
                match notification {
                    FinishedNotification::Task(task) => {
                        app.handle_agent_event(AgentEvent::TaskFinished {
                            id: task.id,
                            command: task.command,
                            status: task.status,
                        });
                    }
                    FinishedNotification::Subagent(task) => {
                        app.handle_agent_event(AgentEvent::SubagentFinished {
                            id: task.id,
                            name: task.name,
                            task: task.task,
                            completed: task.completed,
                            output: task.output,
                        });
                    }
                }
            }
            // Keep the mid-turn snapshots current after any history injection.
            fork_snapshot = Some(agent.fork_context());
            side_question_snapshot = Some(agent.side_question_context());
        }

        if turn_done && let Some(handle) = agent_task.take() {
            // Whatever ended it — a finished turn, or a cooperative interrupt
            // that landed — this turn is no longer interruptible.
            agent_cancel = None;
            interrupt_at = None;
            match handle.await {
                Ok(agent) => {
                    // Latest history is back in the slot; refresh the
                    // mid-turn snapshots so a follow-up `/btw` or `/fork` sees it.
                    side_question_snapshot = Some(agent.side_question_context());
                    fork_snapshot = Some(agent.fork_context());
                    agent_slot = Some(agent);
                    // The provider just served a turn, so any earlier health
                    // warning was transient — drop it so it self-heals.
                    app.provider_health_error = None;
                    // MCP finished connecting mid-turn: merge its tools now that
                    // the agent is back in its slot.
                    if app.mcp_merge_pending {
                        app.mcp_merge_pending = false;
                        CommandContext {
                            app: &mut app,
                            client: &mut client,
                            agent_slot: &mut agent_slot,
                            manager: &manager,
                            skills: &mut skills,
                            project_root: &project_root,
                            mcp_path: &mcp_path,
                            genie_max_steps,
                            events: &events,
                        }
                        .merge_mcp_registry()
                        .await;
                    }
                }
                Err(err) => {
                    // The turn task was aborted, or died somewhere the
                    // `catch_unwind` in `start_agent_turn` could not reach, and
                    // took the agent with it — and with it every subagent loop
                    // that would have closed its own pane. The queued messages
                    // are left alone: they are the user's, they were typed
                    // before any of this, and the rebuild below will run them.
                    app.notice(format!("agent task crashed: {err}"));
                    app.end_turn_abruptly("the turn crashed");
                    spawn_session_rebuild(
                        &mut app,
                        &client,
                        &skills,
                        &project_root,
                        &manager,
                        events.sender(),
                        "agent restarted from the last session",
                    );
                }
            }
        }

        // The interrupt the user asked for did not land: the turn is parked
        // somewhere the cancel flag is not checked (inside a long tool call, in
        // practice). Kill the task. That costs the agent — it moved into the
        // task — so rebuild from the session, the same path as crash recovery.
        //
        // Checked *after* the join above, so a turn that did stop in time is
        // already gone from `agent_task` and nothing here fires.
        if let Some(deadline) = interrupt_at
            && Instant::now() >= deadline
            && let Some(handle) = agent_task.take()
        {
            interrupt_at = None;
            agent_cancel = None;
            handle.abort();
            // Aborting drops every subagent loop the turn had in flight
            // mid-poll and whatever `execute` was parked on, so none of them
            // will ever close its own pane or announce that its console
            // closed. `end_turn_abruptly` is what takes all of that back.
            app.end_turn_abruptly("interrupted");
            // Queued prompts belong to the interrupted conversation flow —
            // drop them so the rebuild doesn't auto-start a turn the user may
            // no longer want.
            app.message_queue.clear();
            app.notice("interrupted");
            spawn_session_rebuild(
                &mut app,
                &client,
                &skills,
                &project_root,
                &manager,
                events.sender(),
                "ready",
            );
        }

        // Dispatch any slash commands the agent queued via `run_command` during
        // the turn, now that it's back in its slot and can be reconfigured. A
        // crashed turn leaves the slot empty (a rebuild is in flight); the queue
        // then waits for that rebuild, or the next completed turn.
        drain_agent_commands(
            &mut app,
            &mut client,
            &mut agent_slot,
            &manager,
            &mut skills,
            &project_root,
            &mcp_path,
            genie_max_steps,
            &events,
        )
        .await;

        // After the agent is idle again, start the next user prompt that was
        // typed mid-turn. Only one per event-loop iteration: the turn's Done
        // will come back around and drain the rest. Agent commands above may
        // themselves rebuild the agent (`/model`), so re-check the slot.
        drain_message_queue(
            &mut app,
            &mut agent_slot,
            &mut agent_task,
            &mut agent_cancel,
            &mut interrupt_at,
            &mut side_question_snapshot,
            &mut fork_snapshot,
            &mut idle_fork_tx,
            &events,
        );

        if app.should_quit {
            break;
        }
    }

    // session_end hooks: best-effort — skipped when quitting mid-turn took
    // the agent — with no event surfacing (the TUI is going away).
    if let Some(agent) = agent_slot.as_ref() {
        agent.fire_session_end(None).await;
    }

    // Tell the peers watching that the session ended and close the socket,
    // rather than leaving every watcher's stream to expire at an idle timeout.
    if let Some(tee) = app.mesh.take() {
        tee.leave().await;
    }

    // Drop this session's heartbeat so it leaves the dashboard immediately.
    session_registry::remove(&app.session_id);

    drop(_guard);
    restore_terminal_best_effort();
    Ok(0)
}

/// Start one agent turn from a preprocessed prompt: take the agent out of its
/// slot, mark the UI busy, and spawn the turn task. Returns `true` when the
/// turn was started. On a missing agent the prepared prompt is re-queued so a
/// later idle cycle can retry.
#[allow(clippy::too_many_arguments)]
fn start_agent_turn(
    app: &mut App,
    agent_slot: &mut Option<Agent>,
    agent_task: &mut Option<JoinHandle<Agent>>,
    agent_cancel: &mut Option<CancelHandle>,
    interrupt_at: &mut Option<Instant>,
    side_question_snapshot: &mut Option<SideQuestionContext>,
    fork_snapshot: &mut Option<ForkContext>,
    idle_fork_tx: &mut Option<mpsc::Sender<AgentEvent>>,
    events: &EventLoop,
    prepared: crate::commands::Preprocessed,
) -> bool {
    let Some(mut agent) = agent_slot.take() else {
        app.message_queue.push_front(prepared);
        return false;
    };
    // Capture conversation context *before* the agent leaves its slot so a
    // mid-turn `/btw` or `/fork` still has something to ground against.
    *side_question_snapshot = Some(agent.side_question_context());
    *fork_snapshot = Some(agent.fork_context());
    // A turn brings its own event forwarder; drop any idle collector so we
    // don't keep two pumps alive for the same surface.
    *idle_fork_tx = None;
    app.status.busy = true;
    app.status.step = 0;
    app.transcript.commit();
    app.turn_started = Some(Instant::now());
    app.roll_spinner_verb();

    let (agent_tx, mut agent_rx) = mpsc::channel::<AgentEvent>(256);
    // Keep a clone so mid-turn `/fork` can stream into the same forwarder
    // the turn uses (panes open while the main turn is still running).
    let turn_events = agent_tx.clone();
    let forward = events.sender();
    tokio::spawn(async move {
        while let Some(agent_event) = agent_rx.recv().await {
            if forward.send(Event::Agent(agent_event)).await.is_err() {
                break;
            }
        }
    });
    // Stash the turn's event sender as the idle_fork_tx so a mid-turn /fork
    // finds it. (When the turn ends, start_agent_turn's next call or the idle
    // drain path replaces it.)
    *idle_fork_tx = Some(turn_events);

    // Cloned before the agent moves into the task: Ctrl-C stops the turn
    // through this, and only falls back to aborting the task (which loses
    // the agent) when the turn does not take the hint.
    *agent_cancel = Some(agent.cancel_handle());
    *interrupt_at = None;

    let prompt = prepared.text;
    let images = prepared.images;
    *agent_task = Some(tokio::spawn(async move {
        let fallback = agent_tx.clone();
        // The turn runs inside `catch_unwind` because the alternative is the
        // session ending on its feet. `Done` is what clears `App::status.busy`,
        // returns the agent to its slot, and drains the queued messages; a
        // turn task that unwinds sends no `Done`, so the spinner spins
        // forever, every subsequent Enter queues a message that will never be
        // sent, and the only way out is Ctrl-C twice. Nothing on screen says
        // any of that happened. Catching it here turns the same crash into an
        // error line, a stopped turn, and a working prompt.
        //
        // The agent is handed back afterwards rather than rebuilt. It is an
        // owned value whose future was dropped mid-poll, so its conversation
        // may be a message short of tidy — but the history it holds is the
        // session's, the tool registry and MCP connections behind it are live,
        // and throwing all of that away to re-read the session file is a
        // strictly worse answer to "the renderer for one tool result had a
        // bug".
        let outcome =
            std::panic::AssertUnwindSafe(agent.run_turn_with_images(&prompt, images, agent_tx))
                .catch_unwind()
                .await;
        // run_turn normally ends with Done itself; on a hard error — or a
        // panic — make sure the UI unblocks.
        if let Some(message) = turn_failure(outcome) {
            let _ = fallback.send(AgentEvent::Error(message)).await;
            let _ = fallback
                .send(AgentEvent::Done {
                    reason: DoneReason::Stopped,
                })
                .await;
        }
        agent
    }));
    true
}

/// Ensure a collector is pumping `AgentEvent`s into the main event loop while
/// no turn is running (or as a sibling of a running turn's forwarder). Returns
/// a sender the fork can stream into.
fn ensure_idle_fork_tx(
    idle_fork_tx: &mut Option<mpsc::Sender<AgentEvent>>,
    events: &EventLoop,
) -> mpsc::Sender<AgentEvent> {
    if let Some(tx) = idle_fork_tx.as_ref()
        && !tx.is_closed()
    {
        return tx.clone();
    }
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    let forward = events.sender();
    tokio::spawn(async move {
        while let Some(agent_event) = rx.recv().await {
            if forward.send(Event::Agent(agent_event)).await.is_err() {
                break;
            }
        }
    });
    *idle_fork_tx = Some(tx.clone());
    tx
}

/// If the agent is idle and a user prompt is waiting, start it. One message
/// per call so the turn's Done comes back around and drains the rest.
#[allow(clippy::too_many_arguments)]
fn drain_message_queue(
    app: &mut App,
    agent_slot: &mut Option<Agent>,
    agent_task: &mut Option<JoinHandle<Agent>>,
    agent_cancel: &mut Option<CancelHandle>,
    interrupt_at: &mut Option<Instant>,
    side_question_snapshot: &mut Option<SideQuestionContext>,
    fork_snapshot: &mut Option<ForkContext>,
    idle_fork_tx: &mut Option<mpsc::Sender<AgentEvent>>,
    events: &EventLoop,
) {
    if app.status.busy || app.rebuilding.is_some() || agent_slot.is_none() {
        return;
    }
    let Some(prepared) = app.pop_queued_message() else {
        return;
    };
    let remaining = app.message_queue.len();
    if start_agent_turn(
        app,
        agent_slot,
        agent_task,
        agent_cancel,
        interrupt_at,
        side_question_snapshot,
        fork_snapshot,
        idle_fork_tx,
        events,
        prepared,
    ) && remaining > 0
    {
        app.notice(format!(
            "sending queued message ({remaining} still waiting)"
        ));
    }
}

/// Dispatch the slash commands the agent queued via `run_command`, in order,
/// now that the turn has ended and the agent is back in its slot. A command
/// that starts a background rebuild (e.g. `/model`) empties the slot; draining
/// stops there and leaves the rest queued, so the `AgentRebuilt` handler drains
/// them once the agent returns. Called both after a turn completes and after a
/// rebuild restores the slot, so no queued command is silently dropped.
#[allow(clippy::too_many_arguments)]
async fn drain_agent_commands(
    app: &mut App,
    client: &mut Arc<dyn LlmProvider>,
    agent_slot: &mut Option<Agent>,
    manager: &Arc<Mutex<McpManager>>,
    skills: &mut Vec<Skill>,
    project_root: &Path,
    mcp_path: &Path,
    genie_max_steps: StepBudget,
    events: &EventLoop,
) {
    while agent_slot.is_some() && !app.pending_agent_commands.is_empty() {
        let line = app.pending_agent_commands.remove(0);
        let Some(Ok(command)) = SlashCommand::parse(&line) else {
            continue;
        };
        CommandContext {
            app: &mut *app,
            client: &mut *client,
            agent_slot: &mut *agent_slot,
            manager,
            skills: &mut *skills,
            project_root,
            mcp_path,
            genie_max_steps,
            events,
        }
        .run(command)
        .await;
    }
}
