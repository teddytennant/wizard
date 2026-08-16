//! The GUI (`wizard gui`): an iced window over the agent core.
//!
//! One process, one binary, no webview and no loopback HTTP. The browser GUI
//! this replaced was two programs talking JSON over a socket to themselves: a
//! turn's tokens were serialized into WebSocket frames, parsed back by
//! JavaScript, and re-rendered by a second markdown implementation that had
//! neither syntax highlighting nor math. This window folds the same
//! [`AgentEvent`]s into the same [`TranscriptModel`] every other surface uses,
//! and draws it. That page, its assets and its server are deleted.
//!
//! # What is here
//!
//! A window, a live selectable transcript, a composer, and the surfaces the
//! browser GUI had: the settings sheet and onboarding, the git rail, the diff
//! and image panes, the session picker, the subagent rail, the context meter,
//! the command palette, and the plan and interview gates. Plus one the browser
//! never had: a console that can answer a shell command that prompts.
//!
//! [`graph`], the explorer over the mesh, is built and tested but is **not
//! reachable from the window**: it has no `Screen`, no `Message` and no button.
//! It was too unfinished to ship in 2.0 and is deferred to a later release, so
//! it is wired out rather than deleted — see the module's own docs for what
//! putting it back involves.
//!
//! What is deliberately absent, and why, is in `docs/native-gui.md`. The short
//! version: no file-picker button (`rfd` links GTK, which is the dependency
//! this feature flag exists to avoid) and no upload path (there is no boundary
//! to upload across). Remote access is absent too, and is no longer this
//! surface's problem: a headless box is reached by running the TUI over SSH, by
//! `wizard -p`, by `wizard acp` from an editor, or through the Telegram
//! gateway.
//!
//! # Why it is behind a feature
//!
//! `native` is off by default and `cargo build` must keep linking no iced at
//! all. A terminal user, a `wizard acp` editor session and a CI container all
//! run this binary and none of them opens a window; making them link winit, a
//! font stack and a rasterizer for a surface they never start would be several
//! hundred crates of build time for nothing. It also keeps the UI layer out of
//! the coverage ratchet, which `contrib/check-coverage.sh` measures with default
//! features — a widget tree that can only be exercised with a compositor would
//! otherwise drag the floor down for the whole codebase.
//!
//! # The pieces
//!
//! | module | what it owns |
//! |---|---|
//! | [`select`] | cross-block text selection: the thing stock iced cannot do |
//! | [`theme`] | [`crate::theme`]'s tokens as window colours |
//! | [`event`] | the tokio↔iced bridge, and the executor |
//! | [`widget`] | the transcript's blocks, its markdown, and the composer |
//! | [`sidebar`] · [`rail`] · [`pane`] | the chat list, the right rail, the open pane |
//! | [`settings`] · [`command`] · [`console`] | the sheet, the `/` menu, a command's stdin |
//! | [`graph`] | the explorer over [`crate::graph`] and [`crate::mesh`] — deferred, not reachable |
//!
//! # It links `TaskManager`, not `Agent`
//!
//! [`crate::gui::tasks::TaskManager`] already does multi-session ownership,
//! keep-warm eviction, registry heartbeats, gate holding and slash-command
//! execution — it was the browser GUI's agent half, and it was never
//! web-specific, which is why deleting the HTTP layer above it left it standing.
//! Writing a second session manager for this window would be writing a second
//! answer to "what happens when two chats are open and one is evicted", and two
//! answers to that is how the transcript model ended up needing to exist. So
//! this window is a *client* of that manager, watching it through
//! [`TaskShared::tap`](crate::gui::tasks::TaskShared::tap).
//!
//! # Gates: what this window answers, and what it declines
//!
//! Three kinds of request can pause a turn, and all three are answered here.
//!
//! - **Plan review** ([`AgentEvent::PlanReady`]) shows the plan and grows an
//!   approve/reject pair on the composer. Rejection is two-stage: the first
//!   press reveals a feedback field, because a rejection with no reason gives
//!   the next attempt nothing to go on.
//! - **Interviews** ([`AgentEvent::Interview`]) render as a form.
//! - **Consoles** ([`AgentEvent::ConsoleOpened`]) bind the composer to the
//!   child's stdin, so a command that prompts can be answered. See
//!   [`console`] and `docs/interactive-commands.md`.
//!
//! The split on `claim()` is the part worth keeping straight. Plan and
//! interview tickets are claimed **inside** `TaskShared` and answered through
//! `resolve_plan` / `resolve_interview`; claiming either here would take the
//! reply channel away from that bookkeeping and park the turn with nothing able
//! to say why. The console ticket is the opposite: `TaskShared` does not claim
//! it, so if this window did not, nobody would, and a prompting command would
//! wait out its budget against a reader that never arrives.

pub mod command;
pub mod console;
pub mod event;
pub mod font;
pub mod graph;
pub mod pane;
pub mod rail;
pub mod select;
pub mod settings;
pub mod sidebar;
pub mod subagent;
pub mod theme;
pub mod widget;

#[cfg(test)]
pub(crate) mod probe;
#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::agent::{AgentEvent, DoneReason, PlanVerdict};
use crate::config::Config;
use crate::gui::git::{FileDiff, GitStatus};
use crate::gui::oauth::SignIn;
use crate::gui::settings::ConfigStore;
use crate::gui::tasks::{CommandRequest, TaskManager, TaskShared, TurnRequest};
use crate::theme::Token;
use crate::transcript::TranscriptModel;

use command::Action;
use console::Console;
use pane::Pane;
use select::{Block, Selectable};
use theme::Palette;
use widget::chrome;
use widget::markdown::MONO;

/// Which screen the window is on.
///
/// **This is where a second screen plugs in.** Add a variant, an arm in
/// [`view`] and one in [`update`], and a way to reach it (the sidebar's footer
/// is the obvious place); everything else — the palette, the subscriptions, the
/// task feed — is screen-independent and keeps running behind it. The chat is
/// never torn down when another screen is on top, so a turn started before the
/// switch is still streaming when you come back.
pub enum Screen {
    Chat,
    /// The settings sheet, which is also onboarding. See
    /// [`crate::native::settings`].
    Settings,
}

/// What the window can be told.
///
/// [`AgentEvent`] is carried unwrapped, which is the whole reward for the F3
/// work that made it `Clone`: iced's only bound on a message is `Clone + Send`,
/// the agent's report already satisfies it, and a wrapper would be a second enum
/// to keep in step with the first for no gain.
#[derive(Debug, Clone)]
pub enum Message {
    DraftChanged(String),
    /// Send the draft — or, when the `/` menu is open, complete it.
    Send,
    Stop,
    /// One event from the turn in flight.
    Agent(AgentEvent),
    // --- gates ---
    ApprovePlan,
    /// First press reveals the feedback field; second sends it.
    RejectPlan,
    PlanFeedback(String),
    InterviewAnswer(usize, String),
    SendAnswers,
    SkipInterview,
    /// Close the child's stdin: a terminal's Ctrl-D.
    ConsoleEof,
    // --- panels ---
    Sidebar(sidebar::Message),
    Rail(rail::Message),
    /// The window changed size. Only the width is kept, and only to decide
    /// whether there is room for the rail.
    Resized(f32),
    Settings(settings::Message),
    ClosePane,
    /// Back out of whatever is on top.
    Escape,
    /// A file was dropped on the window.
    Attach(PathBuf),
    Unattach(usize),
    // --- the `/` menu ---
    MenuStep(i32),
    MenuPick(usize),
    /// A window command finished; apply what it asked for.
    Commanded(Vec<Action>),
    // --- things that landed ---
    Opened(Box<Result<Opened, String>>),
    /// A Claude Code transcript was converted into a Wizard session. See
    /// [`App::open_claude`].
    Imported(Box<Result<crate::claude_resume::Imported, String>>),
    Diffed(String, Box<Result<FileDiff, String>>),
    Git(Box<Option<GitStatus>>),
    /// The branch list behind the rail's branch chip.
    Branches(Vec<String>),
    /// A checkout finished. `Err` carries git's own refusal — an uncommitted
    /// change the switch would overwrite is the common one, and it is the
    /// user's to resolve rather than ours to force through.
    CheckedOut(Box<Result<String, String>>),
    /// The timer: re-read the chat list, refresh a working chat's diffstat,
    /// and poll a sign-in that is in the other window. Everything that is a
    /// function of wall-clock time rather than of an event.
    Tick,
}

/// A chat that was switched to: which chat, and its conversation off disk.
///
/// The conversation travels as the session file's own *entries* rather than as
/// a folded [`TranscriptModel`], because iced requires a message to be `Clone`
/// and the model is not: it carries per-turn streaming state that has no
/// meaning outside the fold. Folding happens in [`App::adopt`], on the draw
/// thread, where the model it produces is the one the window keeps — so there
/// is one fold rather than one fold and a copy.
#[derive(Debug, Clone)]
pub struct Opened {
    id: String,
    cwd: PathBuf,
    model: String,
    entries: Vec<crate::agent::session::SessionEntry>,
}

/// A plan awaiting a verdict.
///
/// The *text* only. The reply channel is parked in `TaskShared` and answered
/// through [`TaskShared::resolve_plan`]; a widget holding it would take it out
/// of the bookkeeping that a disconnect and a turn's end both depend on.
struct PlanReview {
    plan: String,
    feedback: String,
    /// The first Reject press reveals the field rather than sending; the second
    /// sends. Rejecting with no reason is the common mistake and it costs the
    /// agent the whole next turn.
    rejecting: bool,
}

/// An interview awaiting answers.
struct Interview {
    questions: Vec<String>,
    answers: Vec<String>,
}

/// Below this window width the rail is dropped, whatever it holds.
///
/// The sidebar is 240 and [`rail::WIDTH`] is 300, so all three columns cost
/// 540 px of chrome before a word of the conversation is drawn. At 700 the
/// chat pane was 160 px wide and the composer's send button — which is also
/// the stop control — had fallen off the end of it; below 540 the pane was
/// gone entirely and the window was a sidebar beside black. 900 leaves the
/// conversation 360, which is the narrowest that reads.
const MIN_WIDTH_FOR_RAIL: f32 = 900.0;

/// The window's state.
pub struct App {
    manager: Arc<TaskManager>,
    store: Arc<ConfigStore>,
    task: Arc<TaskShared>,
    /// Where a new chat opens: the launch directory, or whatever the sidebar's
    /// footer was last pointed at. A chat's own directory is fixed when its
    /// session is created and is never retroactively moved, which is why this
    /// is the window's and not the task's.
    cwd: PathBuf,
    /// The conversation, folded from the same events every other surface folds.
    transcript: TranscriptModel,
    /// The transcript revision `blocks` was built from, so a redraw that
    /// changed nothing rebuilds nothing.
    drawn: u64,
    /// The transcript as text runs. Owned by the state rather than built in
    /// `view`, because the selection widget borrows them and because rebuilding
    /// them on every mouse move would be a walk over the whole conversation per
    /// frame.
    blocks: Vec<Block>,
    /// The blocks of the subagent run whose pane is open, built the same way.
    run_blocks: Vec<Block>,
    palette: Palette,
    /// The window's width, from the last `Resized`. Seeded with iced's default
    /// so the first frame — drawn before any resize event arrives — decides
    /// the same way the second one will.
    width: f32,
    draft: String,
    working: bool,
    model: String,
    screen: Screen,
    plan: Option<PlanReview>,
    interview: Option<Interview>,
    /// The command whose stdin the composer is bound to. See
    /// [`crate::native::console`].
    console: Option<Console>,
    sidebar: sidebar::Sidebar,
    rail: rail::Rail,
    settings: settings::Sheet,
    menu: command::Menu,
    pane: Pane,
    /// Files dropped on the window, to go up with the next message.
    attachments: Vec<PathBuf>,
    /// A line to put in the *next* conversation this window adopts.
    ///
    /// Exists for exactly one caller: importing a Claude Code session has
    /// something to say about what just happened ("this many messages came
    /// across, the file was not modified"), and the only place to say it is the
    /// transcript of the chat the import produced — which does not exist until
    /// [`App::adopt`] runs, and which replaces whatever the window was showing.
    /// Held here rather than pushed early, because pushing it early would put
    /// it in the conversation being left behind.
    pending_notice: Option<String>,
    /// Bumped to make iced rebuild the event subscription.
    generation: u64,
}

impl App {
    /// Fold an event into the conversation and into the window's own state.
    fn absorb(&mut self, event: AgentEvent) {
        self.transcript.apply(&event);
        self.rail.apply(&event);
        match &event {
            AgentEvent::PlanReady { plan, .. } => {
                self.plan = Some(PlanReview {
                    plan: plan.clone(),
                    feedback: String::new(),
                    rejecting: false,
                })
            }
            AgentEvent::Interview { questions, .. } => {
                self.interview = Some(Interview {
                    answers: vec![String::new(); questions.len()],
                    questions: questions.iter().map(|q| q.question.clone()).collect(),
                });
            }
            // The one gate this window claims. See `src/native/console.rs`.
            AgentEvent::ConsoleOpened { command, gate } => {
                match Console::claim(*gate, command.clone()) {
                    Some(console) => self.console = Some(console),
                    // Already claimed, or the command ended in the gap. Said
                    // out loud, because a prompt with no way to answer it is
                    // about to time out and the user deserves to know why.
                    None => self.transcript.notice(format!(
                        "'{command}' is waiting for input, but its console could not be claimed"
                    )),
                }
            }
            AgentEvent::ConsoleClosed { gate }
                if self.console.as_ref().is_some_and(|open| open.is(*gate)) =>
            {
                self.console = None;
            }
            AgentEvent::Done { reason } => {
                self.working = false;
                self.plan = None;
                self.interview = None;
                self.console = None;
                if !matches!(reason, DoneReason::Completed) {
                    self.transcript
                        .notice(format!("turn ended: {}", describe(*reason)));
                }
            }
            _ => {}
        }
    }

    /// Rebuild the text runs if the conversation moved.
    ///
    /// A whole rebuild, not an incremental splice, and that is safe *because*
    /// the expensive half is elsewhere: this walk produces owned data and costs
    /// microseconds, while shaping — the part that costs milliseconds — is
    /// cached in [`select::cache`] under a key derived from a block's content.
    /// Which is also the answer to [`Change::Inserted`](crate::transcript::Change):
    /// the replay path splices a tool's images in mid-vector and shifts every row
    /// below, so a cache keyed by row index would hand the shifted rows the wrong
    /// paragraphs. Keyed by content, an insert costs exactly one reshape and the
    /// indices are free to move.
    fn refresh(&mut self) {
        if self.transcript.revision() != self.drawn {
            self.drawn = self.transcript.revision();
            self.blocks = widget::transcript::blocks(&self.transcript, &self.palette);
        }
        // The open run's pane, from its own model. Rebuilt unconditionally
        // because a run streams while its pane is open and there is exactly one
        // of them.
        self.run_blocks = match &self.pane {
            Pane::Run(id) => self
                .rail
                .subagents
                .run(*id)
                .map(|run| widget::transcript::blocks(&run.transcript, &self.palette))
                .unwrap_or_default(),
            _ => Vec::new(),
        };
    }

    /// What the composer's Enter does.
    ///
    /// With the menu open it completes rather than sending, which is the whole
    /// reason the menu is autocomplete on the composer rather than an overlay:
    /// the key you already pressed does the right thing.
    fn submit(&mut self) -> iced::Task<Message> {
        if !self.menu.entries.is_empty() {
            return self.complete(self.menu.cursor);
        }
        self.send()
    }

    /// Take the menu's `index`th row into the composer, and run it when it
    /// takes no arguments.
    fn complete(&mut self, index: usize) -> iced::Task<Message> {
        let Some(entry) = self.menu.entries.get(index) else {
            return iced::Task::none();
        };
        if entry.unavailable || !entry.args.is_empty() {
            // Completed, not run: an unavailable command still gets to explain
            // itself when the user presses Enter again, and one that takes
            // arguments is waiting for them.
            self.draft = format!("/{} ", entry.name);
            self.menu.sync(&self.draft);
            return iced::Task::none();
        }
        self.draft = format!("/{}", entry.name);
        self.send()
    }

    /// Send the draft: as a line into a running command, as a slash command, or
    /// as a message to the agent.
    fn send(&mut self) -> iced::Task<Message> {
        let text = self.draft.trim().to_string();

        // A bound console takes the line first, and takes it *whole*: a blank
        // line is a real answer to a prompt with a default, so the emptiness
        // check below must not swallow it.
        if let Some(console) = &self.console {
            let line = self.draft.clone();
            self.draft.clear();
            if console.line(&line) {
                self.transcript.console_echo(&line);
            } else {
                self.transcript
                    .notice("the command is no longer reading input".to_string());
                self.console = None;
            }
            return iced::Task::none();
        }

        if text.is_empty() && self.attachments.is_empty() {
            return iced::Task::none();
        }

        match command::route(&text, &self.menu.custom) {
            command::Route::Refused(why) => {
                self.draft.clear();
                self.menu.sync("");
                self.transcript.notice(why);
                iced::Task::none()
            }
            command::Route::Agent { name, args } => {
                self.draft.clear();
                self.menu.sync("");
                self.transcript
                    .notice(format!("/{name} {args}").trim().to_string());
                if let Err(why) = self
                    .manager
                    .submit_command(&self.task.id, CommandRequest { name, args })
                {
                    self.transcript.notice(why);
                }
                iced::Task::none()
            }
            command::Route::Window(parsed) => {
                self.draft.clear();
                self.menu.sync("");
                self.transcript.notice(text.clone());
                let root = self.task.cwd.clone();
                let snapshot = self.snapshot();
                let plan = self.plan_state();
                iced::Task::perform(
                    command::run(parsed, root, snapshot, plan),
                    Message::Commanded,
                )
            }
            command::Route::Message(text) => {
                self.draft.clear();
                self.menu.sync("");
                let (images, files) = split_attachments(std::mem::take(&mut self.attachments));
                let refs: Vec<crate::images::ImageRef> = images
                    .iter()
                    .map(|path| crate::images::ImageRef {
                        path: path.clone(),
                        mime: mime_of(path).to_string(),
                        bytes: std::fs::metadata(path)
                            .map(|meta| meta.len() as usize)
                            .unwrap_or(0),
                    })
                    .collect();
                self.transcript.user(text.clone(), refs);
                match self.manager.submit_turn(
                    &self.task.id,
                    TurnRequest {
                        text,
                        model: None,
                        images,
                        files,
                    },
                ) {
                    Ok(()) => self.working = true,
                    Err(why) => self.transcript.notice(why),
                }
                iced::Task::none()
            }
        }
    }

    /// What `/status` and `/cost` report about this chat.
    ///
    /// Assembled from what the *window* can see. The fields it cannot — the
    /// step budget, the session id while a turn holds the agent — are `None`
    /// rather than guessed, which is what the type is for.
    fn snapshot(&self) -> crate::commands::surface::SessionSnapshot {
        let config = self.store.current();
        let provider = config.active();
        crate::commands::surface::SessionSnapshot {
            model: self.model.clone(),
            provider_name: provider.name.clone(),
            provider_kind: provider.kind,
            provider_base_url: provider.base_url.clone(),
            mode: config.mode,
            effort: config.reasoning_effort,
            max_steps: Some(config.max_steps),
            session: Some(self.task.id.clone()),
            prompt_tokens: self.rail.meter.prompt,
            completion_tokens: self.rail.meter.completion,
            // The window's meter mirrors the two flat totals off the event
            // stream and carries no cache split, so `/cost` here prices as
            // all-fresh input and overstates a cached session. Reported as
            // `None` rather than as `(0, 0)`, which would be the same
            // arithmetic asserted as a fact.
            cache_tokens: None,
            context_tokens: self.rail.meter.context,
            background_tasks: None,
            todos: (
                self.rail.todos.iter().filter(|todo| todo.done).count(),
                self.rail.todos.len(),
            ),
            plan: self.plan_state(),
            ultra: None,
            usd_per_mtok_in: provider.usd_per_mtok_in,
            usd_per_mtok_out: provider.usd_per_mtok_out,
        }
    }

    fn plan_state(&self) -> crate::commands::surface::PlanState {
        let config = self.store.current();
        crate::commands::surface::PlanState {
            plan: config.plan_first,
            omakase: config.omakase,
        }
    }

    /// Apply what a window command asked for.
    fn apply(&mut self, actions: Vec<Action>) -> iced::Task<Message> {
        let mut task = iced::Task::none();
        for action in actions {
            match action {
                Action::Notice(text) | Action::Error(text) => self.transcript.notice(text),
                Action::OpenSettings { picker } => {
                    if picker {
                        self.settings.open_picker();
                    }
                    self.screen = Screen::Settings;
                }
                Action::ShowDiff(path) => {
                    let wanted = path.or_else(|| {
                        self.rail
                            .git
                            .as_ref()
                            .and_then(|git| git.files.first())
                            .map(|file| file.path.clone())
                    });
                    match wanted {
                        Some(path) => task = self.open_diff(path),
                        None => self
                            .transcript
                            .notice("nothing has changed in the working tree".to_string()),
                    }
                }
                Action::TogglePanel(panel) => match panel {
                    crate::commands::surface::Panel::Todos => {
                        self.rail.todos_hidden = !self.rail.todos_hidden
                    }
                    // The chat list *is* the dashboard here: every live session
                    // on the machine is already in it, with its state on its
                    // row, so there is nothing to open.
                    crate::commands::surface::Panel::Dashboard => {
                        let live = self
                            .sidebar
                            .workspaces
                            .iter()
                            .flat_map(|w| &w.chats)
                            .filter(|chat| chat.state.is_some_and(|state| !state.is_terminal()));
                        let count = live.count();
                        self.transcript
                            .notice(format!("{count} live session(s) — in the chat list"));
                    }
                    crate::commands::surface::Panel::Diff => {}
                },
                Action::DismissRail(filter) => {
                    let before = self.rail.subagents.runs.len();
                    match filter.as_deref() {
                        None => {
                            let ids: Vec<u64> = self
                                .rail
                                .subagents
                                .runs
                                .iter()
                                .filter(|run| {
                                    !matches!(run.status, crate::native::subagent::Status::Running)
                                })
                                .map(|run| run.id)
                                .collect();
                            for id in ids {
                                self.rail.subagents.dismiss(id);
                            }
                        }
                        Some(want) => {
                            let ids: Vec<u64> = self
                                .rail
                                .subagents
                                .runs
                                .iter()
                                .filter(|run| {
                                    !matches!(run.status, crate::native::subagent::Status::Running)
                                        && (run.name.eq_ignore_ascii_case(want)
                                            || run.id.to_string() == want)
                                })
                                .map(|run| run.id)
                                .collect();
                            for id in ids {
                                self.rail.subagents.dismiss(id);
                            }
                        }
                    }
                    if matches!(self.pane, Pane::Run(id) if !self.rail.subagents.runs.iter().any(|run| run.id == id))
                    {
                        self.pane = Pane::Chat;
                    }
                    let gone = before.saturating_sub(self.rail.subagents.runs.len());
                    self.transcript.notice(format!("dismissed {gone}"));
                }
                Action::NewChat => task = self.new_chat(),
                Action::Resume(Some(id)) => task = self.open_chat(id),
                Action::Resume(None) => self
                    .transcript
                    .notice("pick a chat from the list on the left".to_string()),
                // Unfolded rather than toggled: `/resume-claude` typed at an
                // already-open section must not fold it shut, which is what a
                // toggle would do and would read as the command failing.
                Action::RevealClaude => {
                    let cwd = self.cwd.display().to_string();
                    if !self.sidebar.claude_here() {
                        self.transcript
                            .notice(format!("Claude Code has no sessions recorded for {cwd}"));
                    } else if let Some(wanted) = self.sidebar.reveal_claude(&cwd) {
                        task = self.read_claude(wanted);
                    }
                }
            }
        }
        task
    }

    /// Open a fresh chat in [`App::cwd`] — the directory the window was
    /// launched from, until the sidebar's footer is used to name another one.
    fn new_chat(&mut self) -> iced::Task<Message> {
        match self.manager.create_task(&self.cwd, None, None) {
            Ok(id) => self.open_chat(id),
            Err(err) => {
                self.transcript
                    .notice(format!("could not open a chat: {err:#}"));
                iced::Task::none()
            }
        }
    }

    /// Switch to chat `id`: reopen its session, reseed the conversation from
    /// disk, and rebuild the event subscription.
    ///
    /// The reseed is why this is asynchronous. The tap ([`TaskShared::tap`])
    /// carries no backlog — a window holds its whole conversation in a
    /// [`TranscriptModel`] rather than having it streamed to it, so there is
    /// nothing to replay. Switching is the one moment that stops being true, so
    /// the session file is read and folded.
    fn open_chat(&self, id: String) -> iced::Task<Message> {
        let manager = Arc::clone(&self.manager);
        let fallback = self.store.current().active().model;
        iced::Task::perform(
            async move {
                let sessions = Config::sessions_dir().map_err(|err| format!("{err:#}"))?;
                let session = crate::agent::session::Session::open_by_id(&sessions, &id)
                    .map_err(|err| format!("{err:#}"))?
                    .ok_or_else(|| format!("no chat '{id}'"))?;
                let entries = session.entries().map_err(|err| format!("{err:#}"))?;
                let cwd = session
                    .cwd()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."));
                // Spawns the worker when the chat is not live, which is what
                // makes an old session typeable rather than only readable.
                manager.ensure(&id).map_err(|err| format!("{err:#}"))?;
                let model = manager.model_of(&id).unwrap_or(fallback);
                Ok(Opened {
                    id,
                    cwd,
                    model,
                    entries,
                })
            },
            |opened| Message::Opened(Box::new(opened)),
        )
    }

    /// Import a Claude Code transcript and open the Wizard session it produces.
    ///
    /// Two steps rather than one, and the seam is where it is on purpose. This
    /// half does the conversion — [`crate::claude_resume::import`], the same
    /// function `wizard resume --claude` calls, walking the parent chain back
    /// from `leaf` because the transcript is a DAG and a top-to-bottom read
    /// would interleave branches that were never one conversation. The other
    /// half is [`App::open_chat`], unchanged: what the import leaves behind is
    /// an ordinary Wizard session, and opening it is an ordinary open.
    ///
    /// `~/.claude` is read and not written. That is guaranteed a layer down,
    /// where [`crate::claude_session`] cannot name a write API, and it is said
    /// out loud in the notice because it is the thing a user would otherwise
    /// have to take on trust.
    fn open_claude(&self, source: PathBuf, leaf: Option<String>) -> iced::Task<Message> {
        let root = self.cwd.clone();
        iced::Task::perform(
            async move {
                // Blocking, and not trivially so: the transcript can be tens of
                // megabytes. Off the draw thread, onto a blocking pool.
                tokio::task::spawn_blocking(move || {
                    crate::claude_resume::import(&source, leaf.as_deref(), &root)
                        .map_err(|err| format!("{err:#}"))
                })
                .await
                .unwrap_or_else(|err| Err(format!("the import did not finish: {err}")))
            },
            |imported| Message::Imported(Box::new(imported)),
        )
    }

    /// Bind the window to the chat that was just opened.
    fn adopt(&mut self, opened: Opened) -> iced::Task<Message> {
        let Some(task) = self.manager.get(&opened.id) else {
            self.transcript
                .notice(format!("chat '{}' is not live", opened.id));
            return iced::Task::none();
        };
        self.task = task;
        self.transcript = TranscriptModel::seed(&opened.entries);
        // Whatever the act that produced this chat had to say about itself,
        // said in the chat it produced. See [`App::pending_notice`].
        if let Some(notice) = self.pending_notice.take() {
            self.transcript.notice(notice);
        }
        self.model = opened.model;
        self.sidebar.selected = opened.id;
        self.drawn = u64::MAX;
        self.working = self.task.state() == crate::gui::tasks::TaskState::Working;
        // Per-chat state, all of it. A subagent rail or a context reading
        // carried across a switch would be another chat's facts under this
        // chat's name.
        self.rail.subagents = subagent::Rail::default();
        self.rail.meter = rail::Meter::default();
        self.rail.todos.clear();
        self.plan = None;
        self.interview = None;
        self.console = None;
        self.attachments.clear();
        self.pane = Pane::Chat;
        self.menu.custom = crate::commands::load(&opened.cwd);
        self.menu.sync(&self.draft);
        // The one line that makes the switch real: `event::Feed` hashes as
        // (task id, generation), so iced tears the old tap down and stands a
        // new one up.
        self.generation += 1;
        self.refresh_git()
    }

    fn open_diff(&mut self, path: String) -> iced::Task<Message> {
        self.pane = Pane::Diff {
            path: path.clone(),
            diff: None,
        };
        let root = self.task.cwd.clone();
        iced::Task::perform(
            async move {
                let diff = crate::gui::git::diff(&root, &path)
                    .await
                    .map_err(|err| format!("{err:#}"));
                (path, diff)
            },
            |(path, diff)| Message::Diffed(path, Box::new(diff)),
        )
    }

    fn refresh_git(&self) -> iced::Task<Message> {
        let root = self.task.cwd.clone();
        iced::Task::perform(
            async move { crate::gui::git::status(&root).await.ok() },
            |status| Message::Git(Box::new(status)),
        )
    }

    /// List the chat's local branches, for the rail's branch chip.
    fn refresh_branches(&self) -> iced::Task<Message> {
        let root = self.task.cwd.clone();
        iced::Task::perform(
            async move {
                crate::gui::git::branches(&root)
                    .await
                    .map(|found| found.branches)
                    .unwrap_or_default()
            },
            Message::Branches,
        )
    }

    /// Check `branch` out in the chat's own directory.
    ///
    /// Deliberately not a force or a stash: git's refusal to overwrite an
    /// uncommitted change is the safety property, so it is reported verbatim.
    fn checkout(&self, branch: String) -> iced::Task<Message> {
        let root = self.task.cwd.clone();
        iced::Task::perform(
            async move {
                crate::gui::git::checkout(&root, &branch, false)
                    .await
                    .map_err(|err| format!("{err:#}"))
            },
            |result| Message::CheckedOut(Box::new(result)),
        )
    }

    fn refresh_sidebar(&self) -> iced::Task<Message> {
        let manager = Arc::clone(&self.manager);
        let cwd = self.cwd.display().to_string();
        iced::Task::perform(
            async move { sidebar::Sidebar::read(&manager.registry_states(), &cwd) },
            |listing| Message::Sidebar(sidebar::Message::Loaded(listing)),
        )
    }

    /// Read Claude Code's sessions for `cwd`, off the draw thread.
    ///
    /// Deliberately not on the refresh timer: this parses every transcript in
    /// the project, which is tens of megabytes for a repository that has been
    /// worked in for months. It runs when the sidebar's Claude section is
    /// opened, and the cheap probe that decides whether that section exists at
    /// all rides the timer instead.
    fn read_claude(&self, cwd: String) -> iced::Task<Message> {
        iced::Task::perform(
            async move {
                let rows = tokio::task::spawn_blocking({
                    let cwd = cwd.clone();
                    move || sidebar::Sidebar::read_claude(&cwd)
                })
                .await
                .unwrap_or_default();
                (cwd, rows)
            },
            |(cwd, rows)| Message::Sidebar(sidebar::Message::ClaudeLoaded(cwd, rows)),
        )
    }
}

/// A dropped image's media type, from its extension.
///
/// Only for the attachment chip in the transcript: the bytes themselves are
/// read by the agent from the path, so nothing downstream depends on this being
/// right. It is derived rather than sniffed for exactly that reason.
fn mime_of(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "application/octet-stream",
    }
}

/// Split dropped files into the vision path and the `@file` path.
fn split_attachments(paths: Vec<PathBuf>) -> (Vec<PathBuf>, Vec<PathBuf>) {
    paths
        .into_iter()
        .partition(|path| crate::commands::is_image_path(path))
}

/// One turn's ending, for a notice. Only the endings worth saying out loud:
/// a completed turn says nothing, because the reply is the report.
fn describe(reason: DoneReason) -> &'static str {
    match reason {
        DoneReason::Completed => "completed",
        DoneReason::Stopped => "stopped",
        DoneReason::MaxSteps => "hit the step budget",
        DoneReason::TimeLimit => "hit the time limit",
        DoneReason::CircuitBreaker => "tripped the circuit breaker",
    }
}

/// Apply one message.
pub fn update(app: &mut App, message: Message) -> iced::Task<Message> {
    let task = match message {
        Message::DraftChanged(text) => {
            app.draft = text;
            app.menu.sync(&app.draft);
            iced::Task::none()
        }
        Message::Send => app.submit(),
        Message::Stop => {
            app.task.cancel_turn();
            iced::Task::none()
        }
        Message::Agent(event) => {
            app.absorb(event);
            iced::Task::none()
        }
        Message::ApprovePlan => {
            if app.task.resolve_plan(PlanVerdict::approve()) {
                app.plan = None;
            }
            iced::Task::none()
        }
        Message::RejectPlan => {
            let Some(review) = &mut app.plan else {
                return iced::Task::none();
            };
            if !review.rejecting {
                review.rejecting = true;
                return iced::Task::none();
            }
            let feedback = match review.feedback.trim().is_empty() {
                true => "rejected from the native GUI".to_string(),
                false => review.feedback.trim().to_string(),
            };
            if app.task.resolve_plan(PlanVerdict::reject(feedback)) {
                app.plan = None;
            }
            iced::Task::none()
        }
        Message::PlanFeedback(text) => {
            if let Some(review) = &mut app.plan {
                review.feedback = text;
            }
            iced::Task::none()
        }
        Message::InterviewAnswer(index, text) => {
            if let Some(interview) = &mut app.interview
                && let Some(answer) = interview.answers.get_mut(index)
            {
                *answer = text;
            }
            iced::Task::none()
        }
        Message::SendAnswers => {
            if let Some(interview) = &app.interview
                && app.task.resolve_interview(Some(interview.answers.clone()))
            {
                app.interview = None;
            }
            iced::Task::none()
        }
        Message::SkipInterview => {
            if app.task.resolve_interview(None) {
                app.interview = None;
            }
            iced::Task::none()
        }
        Message::ConsoleEof => {
            if let Some(console) = &app.console {
                console.eof();
            }
            iced::Task::none()
        }
        Message::Sidebar(sidebar::Message::Loaded(listing)) => {
            app.sidebar.loaded(listing);
            iced::Task::none()
        }
        Message::Sidebar(sidebar::Message::Select(id)) => app.open_chat(id),
        Message::Sidebar(sidebar::Message::New) => app.new_chat(),
        Message::Sidebar(sidebar::Message::ToggleClaude) => {
            let cwd = app.cwd.display().to_string();
            match app.sidebar.toggle_claude(&cwd) {
                Some(wanted) => app.read_claude(wanted),
                None => iced::Task::none(),
            }
        }
        Message::Sidebar(sidebar::Message::ClaudeLoaded(cwd, rows)) => {
            app.sidebar.claude_loaded(cwd, rows);
            iced::Task::none()
        }
        Message::Sidebar(sidebar::Message::OpenClaude { source, leaf }) => {
            app.open_claude(source, leaf)
        }
        Message::Sidebar(sidebar::Message::ToggleWorkspaces) => {
            app.sidebar.toggle_workspaces();
            iced::Task::none()
        }
        Message::Sidebar(sidebar::Message::UseWorkspace(path)) => {
            // Where the *next* new chat opens. The chat on screen keeps its
            // own directory, which is fixed at session creation — nothing here
            // moves a session that already exists.
            app.cwd = PathBuf::from(path);
            app.sidebar.close_workspaces();
            // The Claude Code rows on screen are the *other* directory's
            // conversations. Dropped rather than relabelled.
            app.sidebar.forget_claude();
            iced::Task::none()
        }
        Message::Sidebar(sidebar::Message::OpenSettings) => {
            app.screen = Screen::Settings;
            iced::Task::none()
        }
        Message::Rail(rail::Message::ShowDiff(path)) => app.open_diff(path),
        Message::Rail(rail::Message::ShowRun(id)) => {
            app.rail.subagents.open(id);
            app.pane = Pane::Run(id);
            iced::Task::none()
        }
        Message::Rail(rail::Message::DismissRun(id)) => {
            app.rail.subagents.dismiss(id);
            if matches!(app.pane, Pane::Run(open) if open == id) {
                app.pane = Pane::Chat;
            }
            iced::Task::none()
        }
        Message::Rail(rail::Message::ToggleBranches) => {
            // Closing needs no read; opening always takes a fresh one, because
            // the list on screen would otherwise be as old as the last time
            // the chip was opened, and a branch created since then is exactly
            // the one being looked for.
            app.rail.update(&rail::Message::ToggleBranches);
            match app.rail.branches_open {
                true => app.refresh_branches(),
                false => iced::Task::none(),
            }
        }
        Message::Rail(rail::Message::Checkout(branch)) => app.checkout(branch),
        Message::Rail(other) => {
            app.rail.update(&other);
            iced::Task::none()
        }
        Message::Settings(inner) => {
            // Onboarding cannot be dismissed: there is nothing behind it to
            // dismiss *to* until a provider is configured, and a chat you
            // cannot send a message to is worse than a sheet you cannot close.
            let close = matches!(inner, settings::Message::Close) && !app.settings.first_run();
            let task = app.settings.update(inner).map(Message::Settings);
            if close {
                app.screen = Screen::Chat;
            }
            task
        }
        Message::ClosePane => {
            app.rail.subagents.close();
            app.pane = Pane::Chat;
            iced::Task::none()
        }
        Message::Escape => match app.screen {
            Screen::Settings => update(app, Message::Settings(settings::Message::Close)),
            _ => update(app, Message::ClosePane),
        },
        Message::Resized(width) => {
            app.width = width;
            iced::Task::none()
        }
        Message::Attach(path) => {
            app.attachments.push(path);
            iced::Task::none()
        }
        Message::Unattach(index) => {
            if index < app.attachments.len() {
                app.attachments.remove(index);
            }
            iced::Task::none()
        }
        Message::MenuStep(by) => {
            app.menu.step(by);
            iced::Task::none()
        }
        Message::MenuPick(index) => app.complete(index),
        Message::Commanded(actions) => app.apply(actions),
        Message::Opened(opened) => match *opened {
            Ok(opened) => app.adopt(opened),
            Err(why) => {
                app.transcript.notice(why);
                iced::Task::none()
            }
        },
        Message::Imported(imported) => match *imported {
            Ok(imported) => {
                // Parked, not pushed: the transcript it belongs in is the one
                // `adopt` is about to build. See [`App::pending_notice`].
                app.pending_notice = Some(imported.summary());
                app.open_chat(imported.id)
            }
            Err(why) => {
                app.transcript
                    .notice(format!("could not import that session: {why}"));
                iced::Task::none()
            }
        },
        Message::Diffed(path, diff) => {
            // A later click wins: the pane shows what was asked for last.
            if let Pane::Diff {
                path: open,
                diff: slot,
            } = &mut app.pane
                && *open == path
            {
                *slot = Some(diff);
            }
            iced::Task::none()
        }
        Message::Git(status) => {
            app.rail.git = *status;
            iced::Task::none()
        }
        Message::Branches(branches) => {
            app.rail.branches_loaded(branches);
            iced::Task::none()
        }
        Message::CheckedOut(result) => match *result {
            // The whole rail is a function of HEAD, so re-read it rather than
            // patching the branch name in: the changed-file list and the
            // diffstat moved too, and a diff pane open on a file the other
            // branch does not have is showing a file that is not there.
            Ok(branch) => {
                app.rail.branches_open = false;
                app.transcript.notice(format!("switched to {branch}"));
                // A diff pane open on a file the new branch does not have is
                // showing a file that is not there, so the slot goes back to
                // the conversation.
                if !app.pane.is_chat() {
                    app.rail.subagents.close();
                    app.pane = Pane::Chat;
                }
                app.refresh_git()
            }
            Err(err) => {
                app.transcript.notice(err);
                iced::Task::none()
            }
        },
        Message::Tick => {
            let mut tasks = vec![app.refresh_sidebar()];
            if app.working {
                tasks.push(app.refresh_git());
            }
            if app.settings.awaiting_sign_in {
                let poll = app.settings.poll_sign_in();
                tasks.push(iced::Task::done(Message::Settings(poll)));
            }
            iced::Task::batch(tasks)
        }
    };
    app.refresh();
    task
}

/// Draw the window.
pub fn view(app: &App) -> iced::Element<'_, Message> {
    use iced::widget::{column, container, row};
    use iced::{Length, Padding};

    let centre: iced::Element<'_, Message> = match &app.pane {
        Pane::Chat => chat(app),
        Pane::Diff { path, diff } => {
            pane::diff(path, diff.as_deref(), Message::ClosePane, &app.palette)
        }
        Pane::Image(path) => pane::image_pane(path, Message::ClosePane, &app.palette),
        Pane::Run(id) => run_pane(app, *id),
    };

    let workspace = app
        .sidebar
        .workspace_of(&app.task.id)
        .map(str::to_string)
        .unwrap_or_else(|| {
            crate::session_registry::workspace_name(&app.task.cwd.display().to_string())
        });
    let branch = app
        .rail
        .git
        .as_ref()
        .map(|git| format!("⎇ {}", git.branch))
        .unwrap_or_default();
    let top = column![
        container(
            row![
                // The title is the elastic one and the chips are not. A chat
                // is named after its first prompt, so the title is arbitrarily
                // long, and as a `Shrink` element it grew the row past the
                // pane: the workspace chip crossed the hairline into the git
                // rail and printed over "GIT TOOLS", and the branch chip was
                // pushed out of the window entirely. Losing the branch you are
                // on is the expensive half of that.
                //
                // `Fill` gives the title the leftover width instead of taking
                // whatever it wants, `Wrapping::None` keeps it on the one line
                // this bar is tall, and the clip is what turns the overflow
                // into a truncation rather than into ink on the neighbour.
                container(
                    chrome::body(app.title_line_beside(&workspace), &app.palette)
                        .wrapping(iced::widget::text::Wrapping::None)
                )
                .width(Length::Fill)
                .clip(true),
                chrome::literal(workspace, &app.palette),
                chrome::literal(branch, &app.palette),
            ]
            .spacing(10)
            .align_y(iced::Alignment::Center)
        )
        .padding(Padding::new(8.0).left(14.0).right(14.0)),
        chrome::hairline(&app.palette),
    ];

    // The rail is a guest, not a fixture. It costs a fixed `rail::WIDTH` and is
    // dropped outright in the two cases where paying that is wrong:
    //
    //  - it has nothing to draw. A fresh chat has no diff, no meter, no
    //    subagents and no todos, so the panel was 300 px of empty background
    //    beside the conversation on every window, forever.
    //  - the window is too narrow to seat all three columns. Sidebar plus rail
    //    is 540 px of chrome; at 700 px the conversation got 160 px and its
    //    send button fell off the composer, and below 540 the chat pane
    //    vanished completely — sidebar, black, nothing else.
    //
    // `app.width` starts at the window's configured width and is corrected by
    // the first `Resized`, so the opening frame is not a guess.
    let show_rail = !app.rail.is_empty() && app.width >= MIN_WIDTH_FOR_RAIL;
    let mut body = row![
        app.sidebar
            .view(&app.cwd.display().to_string(), &app.palette)
            .map(Message::Sidebar),
        container(column![top, centre].spacing(0))
            .width(Length::Fill)
            .height(Length::Fill),
    ]
    // The window's three columns, laid out against the window. A `Row` is
    // `Shrink` by default, and a `Shrink` row resolves its `Fill` child — the
    // centre pane — against its own intrinsic width, so the pane sized itself
    // to its widest content instead of to the room between the two rails.
    .width(Length::Fill)
    .height(Length::Fill);
    if show_rail {
        body = body.push(app.rail.view(&app.palette).map(Message::Rail));
    }

    let screen: iced::Element<'_, Message> = match app.screen {
        Screen::Chat => body.into(),
        // The sheet is drawn *over* the window rather than instead of it, so a
        // turn started before Settings was opened is still visibly running
        // behind it.
        Screen::Settings => {
            // A scrim between the window and the sheet. The sheet is a card
            // with rounded corners floating over the chat, so without one the
            // composer showed through underneath it — the send button's white
            // pill sat in the sheet's bottom-right corner, bright, looking
            // like part of the settings — and the sidebar's buttons stayed
            // legible behind a dialog that had taken the keyboard.
            //
            // Not opaque: the point of a sheet rather than a screen is that
            // you can still see where you are.
            //
            // Visual only. A `container` with a background captures no mouse
            // events, so the sidebar behind this is still clickable — which is
            // what it was before, and making the sheet modal is a behaviour
            // change, not a paint fix.
            let scrim = container(iced::widget::space())
                .width(Length::Fill)
                .height(Length::Fill)
                .style(|_theme| container::Style {
                    background: Some(iced::Background::Color(iced::Color {
                        a: 0.55,
                        ..iced::Color::BLACK
                    })),
                    ..container::Style::default()
                });
            iced::widget::stack![
                body,
                scrim,
                app.settings.view(&app.palette).map(Message::Settings)
            ]
            .into()
        }
    };

    container(screen)
        .width(Length::Fill)
        .height(Length::Fill)
        .style({
            let canvas = app.palette.canvas;
            move |_theme| container::Style {
                background: Some(iced::Background::Color(canvas)),
                ..container::Style::default()
            }
        })
        .into()
}

impl App {
    /// The chat's own name, for the top bar.
    fn title_line(&self) -> String {
        self.sidebar
            .workspaces
            .iter()
            .flat_map(|group| &group.chats)
            .find(|chat| chat.id == self.task.id)
            .map(|chat| chat.title.clone())
            .unwrap_or_else(|| "New chat".to_string())
    }

    /// [`Self::title_line`], except it does not repeat the chip next to it.
    ///
    /// A chat has no title until its first turn names it, and until then it
    /// carries the workspace's name — which the top bar already shows, as a
    /// chip, eight pixels to the right. The bar read `scratchpad …
    /// scratchpad`, and the same duplication put two identical rows in the
    /// sidebar under a group header of the same name again.
    fn title_line_beside(&self, workspace: &str) -> String {
        let title = self.title_line();
        match title == workspace {
            true => "New chat".to_string(),
            false => title,
        }
    }
}

/// The conversation, the gates, and the composer.
fn chat(app: &App) -> iced::Element<'_, Message> {
    use iced::widget::{button, column, container, row, scrollable, text, text_input};
    use iced::{Length, Padding};

    // An empty conversation is a large rectangle of background, and it was
    // exactly that: on a 1024-wide window a fresh chat is about 630x610 px of
    // nothing between the top bar and the composer, with no indication that
    // the window had finished loading or that the thing to do was type.
    //
    // Two lines, centred, muted. Not a tutorial and not a set of example
    // prompts that go stale — the composer's placeholder already says what to
    // type, so this says what this window is and gets out of the way.
    let transcript: iced::Element<'_, Message> = if app.blocks.is_empty() {
        container(
            column![
                chrome::body("Nothing here yet", &app.palette),
                // Centred on the text, not just on the block: `align_x` on
                // the column centres the two widgets against each other, and
                // the wrapped second line would still sit left inside its own
                // width.
                chrome::muted(
                    "Ask for a change and Wizard makes it — reading, editing and running \
                     what it needs in this directory.",
                    &app.palette,
                )
                .align_x(iced::alignment::Horizontal::Center),
            ]
            .spacing(6)
            .align_x(iced::Alignment::Center)
            .max_width(420),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .padding(Padding::new(18.0))
        .into()
    } else {
        scrollable(
            container(
                Selectable::new(&app.blocks)
                    .selection_color(app.palette.selection)
                    .text_color(app.palette.color(Token::Text))
                    .padding(4.0),
            )
            .width(Length::Fill)
            .padding(Padding::new(18.0)),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    };

    let mut body = column![transcript].spacing(8);

    // The plan gate, above the composer, so the decision sits where the turn
    // stopped.
    if let Some(review) = &app.plan {
        let mut card = column![
            chrome::label("plan — awaiting approval", &app.palette),
            text(review.plan.as_str())
                .size(MONO)
                .color(app.palette.color(Token::Text)),
        ]
        .spacing(8);
        if review.rejecting {
            card = card.push(
                text_input(
                    "What should change? (sent back to the agent)",
                    &review.feedback,
                )
                .on_input(Message::PlanFeedback)
                .on_submit(Message::RejectPlan)
                .size(MONO),
            );
        }
        card = card.push(
            row![
                chrome::primary("approve", Some(Message::ApprovePlan), &app.palette),
                chrome::action(
                    match review.rejecting {
                        true => "send rejection",
                        false => "reject",
                    },
                    Message::RejectPlan,
                    &app.palette
                ),
            ]
            .spacing(8),
        );
        body = body.push(card_frame(card, &app.palette));
    }

    if let Some(interview) = &app.interview {
        let mut card = column![chrome::label("the agent has questions", &app.palette)].spacing(8);
        for (index, question) in interview.questions.iter().enumerate() {
            card = card.push(
                column![
                    chrome::body(question.clone(), &app.palette),
                    text_input("Your answer (optional)", &interview.answers[index])
                        .on_input(move |text| Message::InterviewAnswer(index, text))
                        .size(MONO),
                ]
                .spacing(4),
            );
        }
        card = card.push(
            row![
                chrome::primary("send answers", Some(Message::SendAnswers), &app.palette),
                chrome::action("skip", Message::SkipInterview, &app.palette),
            ]
            .spacing(8),
        );
        body = body.push(card_frame(card, &app.palette));
    }

    // The `/` menu, anchored above the composer.
    if let Some(menu) = app.menu.view(Message::MenuPick, &app.palette) {
        body = body.push(menu);
    }

    if !app.attachments.is_empty() {
        let mut tray = row![].spacing(6);
        for (index, path) in app.attachments.iter().enumerate() {
            tray = tray.push(
                button(
                    row![
                        chrome::literal(
                            rail::elide_left(&path.display().to_string(), 28),
                            &app.palette
                        ),
                        text("✕")
                            .size(chrome::SMALL)
                            .color(app.palette.color(Token::Faint)),
                    ]
                    .spacing(6),
                )
                .on_press(Message::Unattach(index))
                .padding(Padding::new(4.0).left(8.0).right(8.0))
                .style({
                    let raised = app.palette.raised;
                    move |_theme, _status| button::Style {
                        background: Some(iced::Background::Color(raised)),
                        text_color: iced::Color::WHITE,
                        border: iced::Border::default().rounded(8.0),
                        ..button::Style::default()
                    }
                }),
            );
        }
        // Wrapped, because the number of chips is the number of files
        // somebody dropped and a row does not run out politely: past the
        // pane's width the extra chips are laid out beyond it, where the git
        // rail is drawn over them — so a file you attached is invisible and
        // its `✕` is unclickable, which means you cannot take it back off.
        body = body.push(tray.wrap());
    }

    // Console mode: the composer is bound to a running command's stdin, and it
    // has to *say* so — a line typed into the wrong end is either a message the
    // agent never asked for or an answer the command never gets.
    if let Some(console) = &app.console {
        body = body.push(
            row![
                chrome::label("answering", &app.palette),
                // The command is the variable-length part and the only part
                // that may be shortened. It is a whole command line — an
                // `apt-get install` with a dozen packages is ordinary — and as
                // a `Shrink` element next to a `Fill` space it grew the row
                // until `end input (Ctrl-D)` was laid out past the pane. That
                // button is the *only* way to close a command's stdin from
                // this window, on the one surface where the composer is not
                // talking to the agent, so pushing it out of sight strands the
                // command and the person answering it.
                container(
                    chrome::literal(console.command.clone(), &app.palette)
                        .wrapping(iced::widget::text::Wrapping::None)
                )
                .width(Length::Fill)
                .clip(true),
                chrome::action("end input (Ctrl-D)", Message::ConsoleEof, &app.palette),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        );
    }

    body = body.push(widget::composer::composer(
        &app.draft,
        &app.model,
        app.working,
        app.console.is_some(),
        &app.palette,
    ));

    container(body)
        .padding(Padding::new(12.0))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// One subagent run's pane, drawn by the transcript renderer the chat uses.
fn run_pane(app: &App, id: u64) -> iced::Element<'_, Message> {
    use iced::widget::{column, container, scrollable};
    use iced::{Length, Padding};

    let Some(run) = app.rail.subagents.run(id) else {
        return chat(app);
    };
    column![
        pane::header(
            chrome::body(run.name.clone(), &app.palette).into(),
            Some(
                chrome::muted(
                    format!(
                        "{} · {} step{}",
                        run.status.label(),
                        run.steps,
                        if run.steps == 1 { "" } else { "s" }
                    ),
                    &app.palette
                )
                .into()
            ),
            Message::ClosePane,
            &app.palette,
        ),
        container(chrome::muted(run.task.clone(), &app.palette))
            .padding(Padding::new(6.0).left(14.0)),
        scrollable(
            container(
                Selectable::new(&app.run_blocks)
                    .selection_color(app.palette.selection)
                    .text_color(app.palette.color(Token::Text))
                    .padding(4.0),
            )
            .width(Length::Fill)
            .padding(Padding::new(18.0)),
        )
        .width(Length::Fill)
        .height(Length::Fill),
    ]
    .into()
}

/// A gate card: a surface with a hairline, not a modal over the window. The
/// decision belongs where the turn stopped.
fn card_frame<'a>(
    content: impl Into<iced::Element<'a, Message>>,
    palette: &Palette,
) -> iced::Element<'a, Message> {
    use iced::widget::container;
    use iced::{Border, Length, Padding};
    container(content)
        .width(Length::Fill)
        .padding(Padding::new(14.0))
        .style({
            let surface = palette.surface;
            let hairline = palette.hairline;
            move |_theme| container::Style {
                background: Some(iced::Background::Color(surface)),
                border: Border {
                    color: hairline,
                    width: 1.0,
                    radius: 10.0.into(),
                },
                ..container::Style::default()
            }
        })
        .into()
}

/// The events of the task currently on screen, plus the window's own timers.
pub fn subscription(app: &App) -> iced::Subscription<Message> {
    let feed = event::events(event::Feed {
        task: Arc::clone(&app.task),
        generation: app.generation,
    })
    .map(Message::Agent);

    // One timer for everything that is a function of wall-clock time: the chat
    // list's ages, a working chat's diffstat, and a sign-in poll. One
    // subscription rather than three, because they are all "something outside
    // this window may have moved".
    let tick = iced::time::every(sidebar::REFRESH).map(|_| Message::Tick);

    // Files dropped on the window. This is the whole of the attachment path:
    // there is no upload, because there is no boundary to upload across, and a
    // dropped path is already a path the agent can open.
    let drops = iced::event::listen_with(|event, _status, _window| match event {
        iced::Event::Window(iced::window::Event::FileDropped(path)) => Some(Message::Attach(path)),
        iced::Event::Window(iced::window::Event::Resized(size)) => {
            Some(Message::Resized(size.width))
        }
        iced::Event::Keyboard(iced::keyboard::Event::KeyPressed { key, modifiers, .. }) => {
            match (key.as_ref(), modifiers.command()) {
                (iced::keyboard::Key::Named(iced::keyboard::key::Named::ArrowUp), _) => {
                    Some(Message::MenuStep(-1))
                }
                (iced::keyboard::Key::Named(iced::keyboard::key::Named::ArrowDown), _) => {
                    Some(Message::MenuStep(1))
                }
                // Escape backs out of whatever is on top: the sheet if one is
                // open, the pane otherwise. The sheet handles its own refusal
                // during onboarding, so this does not have to know about it.
                (iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape), _) => {
                    Some(Message::Escape)
                }
                (iced::keyboard::Key::Character("n"), true) => {
                    Some(Message::Sidebar(sidebar::Message::New))
                }
                // The keystroke the console banner has been promising. Its
                // button read "end input (Ctrl-D)" while nothing anywhere
                // bound Ctrl-D: `Message::ConsoleEof` had exactly two
                // mentions, the button and the arm that handles it. Somebody
                // answering a prompt would press the documented key, watch
                // nothing happen, and reasonably conclude the console was
                // broken rather than that the label was.
                //
                // Unconditional, like the rest of this match: `ConsoleEof` is
                // a no-op when no console is bound (see its arm in `update`),
                // and a binding that is live only in one state is a binding
                // that fails in the state nobody tested.
                (iced::keyboard::Key::Character("d"), true) => Some(Message::ConsoleEof),
                _ => None,
            }
        }
        _ => None,
    });

    iced::Subscription::batch([feed, tick, drops])
}

fn title(app: &App) -> String {
    format!("wizard — {}", app.task.cwd.display())
}

/// Why a window cannot open here, if it cannot.
///
/// Only asked on Unix-that-is-not-macOS, because that is the only place the
/// answer can be no: a Mac always has a window server, and so does Windows.
/// Exactly the two variables winit consults, in the order it consults them, so
/// this cannot say yes to something winit then refuses — the point is to
/// forecast winit's answer, not to have an opinion of its own.
fn no_display() -> Option<String> {
    if cfg!(not(all(unix, not(target_os = "macos")))) {
        return None;
    }
    let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
    if set("WAYLAND_DISPLAY") || set("WAYLAND_SOCKET") || set("DISPLAY") {
        return None;
    }
    Some(
        "the GUI needs a display, and neither WAYLAND_DISPLAY nor DISPLAY is set.\n\
         \n\
         Over SSH, forward X11 with `ssh -X`. On a headless machine there is no window to \
         open, and four ways to drive it without one: `wizard` (the TUI) over SSH, \
         `wizard -p '<prompt>'`, `wizard acp` from an ACP editor, or `wizard gateway` for \
         Telegram."
            .to_string(),
    )
}

/// Open the window on a fresh chat in `cwd`.
///
/// Everything asynchronous happens here, before the event loop takes the thread:
/// the MCP servers connect, the manager is built, and the first session is
/// created on disk. Nothing below this point awaits, because below this point is
/// winit's loop.
pub async fn run(config: Config) -> Result<()> {
    // Before anything else has a side effect. winit's failure here is an
    // `.expect()` inside iced, so a headless box would otherwise get a panic
    // with a backtrace — after this process had already connected the MCP
    // servers and written a session file for a chat nobody will ever see.
    // "You are on a server" is the single likeliest reason this command fails,
    // and it deserves a sentence rather than a stack trace.
    if let Some(why) = no_display() {
        anyhow::bail!(why);
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let model = config.active().model;
    let store = Arc::new(ConfigStore::new(config));
    // One manager's worth of MCP servers for the whole window, as the TUI keeps
    // one: connecting per task would leave four warm chats running four copies
    // of every configured server, each a real process.
    let mcp = Arc::new(tokio::sync::RwLock::new(crate::agent::connect_mcp().await));
    // `attended`, not `with_registry`: this window is in-process with the
    // commands it runs and has a person in front of it, so a command that
    // prompts announces itself instead of reading `/dev/null`. See
    // `src/native/console.rs`.
    let manager = Arc::new(TaskManager::attended(Arc::clone(&store), mcp));

    let id = manager
        .create_task(&cwd, None, None)
        .context("creating the first chat")?;
    let task = manager
        .get(&id)
        .context("the chat that was just created is not live")?;

    // The sign-in lives on the sheet, which is the only thing that starts one
    // and the only thing that polls it.
    let settings = settings::Sheet::new(Arc::clone(&store), Arc::new(SignIn::default()));
    // Nothing to send a message to yet, so the window opens on the sheet. The
    // chat is behind it and becomes reachable the moment a provider saves.
    let screen = match settings.first_run() {
        true => Screen::Settings,
        false => Screen::Chat,
    };

    let mut sidebar = sidebar::Sidebar::default();
    sidebar.selected = id;
    let app = App {
        menu: command::Menu {
            custom: crate::commands::load(&cwd),
            entries: Vec::new(),
            cursor: 0,
        },
        manager,
        store,
        task,
        cwd,
        transcript: TranscriptModel::new(),
        drawn: u64::MAX,
        blocks: Vec::new(),
        run_blocks: Vec::new(),
        palette: Palette::active(),
        // iced's default window is 1024x768 and nothing here asks for another,
        // so this matches what the first frame is actually drawn into.
        width: 1024.0,
        draft: String::new(),
        working: false,
        model,
        screen,
        plan: None,
        interview: None,
        console: None,
        sidebar,
        rail: rail::Rail::default(),
        settings,
        pane: Pane::Chat,
        attachments: Vec::new(),
        pending_notice: None,
        generation: 0,
    };

    // Held past the move into `App`, so the heartbeats can be dropped when the
    // window closes: a chat that stopped existing must not sit in `/dashboard`
    // claiming to be running until it ages out.
    let manager = Arc::clone(&app.manager);
    let outcome = launch(app);
    manager.shutdown();
    outcome.map_err(|error| anyhow::anyhow!("the native GUI could not start: {error}"))
}

/// Hand the thread to iced. Separated from [`run`] so that everything above is
/// testable without a compositor, and so the one place a display is required is
/// one line long.
fn launch(app: App) -> Result<(), iced::Error> {
    // iced wants a boot function it may call more than once (a `Fn`), because a
    // program can be restarted from a preset in its own test harness. This one
    // has a live `TaskManager` in it and cannot be rebuilt, so it is handed over
    // exactly once and a second call is a bug in this file rather than a state
    // worth inventing.
    let app = std::cell::RefCell::new(Some(app));
    let settings = font::settings();
    iced::application(
        move || {
            let mut app = app.borrow_mut();
            let app = app.take().expect("the native GUI boots exactly once");
            // The first list of chats, and the first diffstat.
            let boot = iced::Task::batch([app.refresh_sidebar(), app.refresh_git()]);
            (app, boot)
        },
        update,
        view,
    )
    .settings(settings)
    .title(title)
    .subscription(subscription)
    .theme(|app: &App| theme::iced_theme(&app.palette))
    .executor::<event::Ambient>()
    .run()
}
