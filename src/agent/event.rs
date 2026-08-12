//! What a turn reports, and the gates it blocks on.
//!
//! [`AgentEvent`] is the agent loop's report of what happened: text, tool
//! calls, images, usage, subagent activity, the end of the turn. It is plain
//! owned data, and that is a property the rest of Wizard now depends on rather
//! than a coincidence:
//!
//! - It is [`Clone`], so one turn's stream can feed several consumers at once
//!   (render it, record it, forward it) instead of exactly one.
//! - It is [`Serialize`]/[`Deserialize`], so a turn can be written to disk and
//!   read back, and so the same type can eventually cross a socket to a peer.
//! - It borrows nothing and owns no channel, so holding onto an event keeps
//!   nothing else alive and answers no live question.
//!
//! # Why gates are not events
//!
//! Two things the agent does are not reports at all: `exit_plan` asks for a
//! verdict and `interview` asks for answers, and the turn is parked inside the
//! tool until it gets one. Those used to carry a
//! [`oneshot::Sender`](tokio::sync::oneshot::Sender) inside the event, which is
//! what made the whole enum unclonable and unserializable: a reply channel
//! cannot be duplicated (which of the two copies owns the answer?) and cannot
//! be written to disk (what would a recorded sender even mean?).
//!
//! So the request stays on the stream, in order, as data, and the reply channel
//! waits at a desk. [`AgentEvent::PlanReady`] carries a [`PlanGate`] and
//! [`AgentEvent::Interview`] carries an [`InterviewGate`]: tickets, a number
//! each. [`PlanGate::claim`] hands over the reply channel, and it does so
//! exactly once, so a stream that is teed to three consumers still has exactly
//! one author of the answer, and a replayed recording answers nothing at all.
//!
//! Keeping the request on the same channel as the deltas that preceded it is
//! deliberate: a plan review that overtakes the text explaining it is a worse
//! bug than the one this split fixes, and a second channel cannot be ordered
//! against the first.
//!
//! # A third thing that waits: a command's console
//!
//! A shell command that prompts (`Do you want to continue? [Y/n]`) is the same
//! shape one more time, with one difference: the answer is not a single value
//! but a *conversation*, possibly several lines long, and it has to reach the
//! child process while it is still running rather than when it ends. So
//! [`ConsoleGate`] parks an [`mpsc::Sender`](tokio::sync::mpsc::Sender) at the
//! same desk instead of a `oneshot`, and [`AgentEvent::ConsoleOpened`] carries
//! the ticket. Everything else is identical, including the part that matters
//! most: [`ConsoleGate::claim`] succeeds exactly once, so a stream that is teed
//! to a recorder and a peer still has exactly one writer into that child's
//! stdin, and it is the surface with a human in front of it.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use super::{DoneReason, ImageSource, InterviewQuestion, PlanVerdict};
use crate::images::ImageRef;
use crate::tools::ToolOutput;

/// Events emitted by the agent loop. The TUI renders them; the headless
/// runner logs them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentEvent {
    /// Streaming assistant text delta.
    TextDelta(String),
    /// Streaming model reasoning ("thinking") delta. Rendered dimmed by the
    /// TUI; never part of the assistant message or the session history.
    ThinkingDelta(String),
    /// A tool call is being executed.
    ToolStarted { name: String, args: Value },
    /// A tool call finished. `output.images` carries any images the tool
    /// produced, as base64; the [`AgentEvent::Images`] that follows says where
    /// they landed on disk, which is what a renderer wants.
    ToolFinished { name: String, output: ToolOutput },
    /// Images produced during this turn — by a tool, or by the model itself —
    /// and written to the session's image directory
    /// (`~/.wizard/images/<session>/`).
    ///
    /// This is the event the surfaces render off. Each [`ImageRef`] names a
    /// file on disk plus its media type and size: the TUI prints the path when
    /// the terminal cannot draw the image, the GUI links to it for "open full
    /// size". No base64 rides on this event — the payload the model needs stays
    /// in history, and a transcript frame references the image rather than
    /// embedding it.
    ///
    /// Ordering: for a tool's images this arrives immediately after that tool's
    /// [`AgentEvent::ToolFinished`]; for the model's own images, immediately
    /// after the last [`AgentEvent::TextDelta`] of the reply that produced them.
    Images {
        source: ImageSource,
        images: Vec<ImageRef>,
    },
    /// One agent step (model round-trip) completed. 1-based.
    StepCompleted { step: u32 },
    /// Non-fatal error surfaced to the user; the loop may continue.
    Error(String),
    /// Informational progress notice (e.g. history compaction); never an
    /// error.
    Notice(String),
    /// A completion stream died mid-response and is about to be retried from
    /// scratch. Whatever partial text was streamed so far never entered
    /// history and will be re-generated — consumers rendering deltas must
    /// discard their partial buffer or the retry duplicates it.
    ///
    /// "Partial" means since the last [`AgentEvent::StepCompleted`], not since
    /// the start of the turn: a multi-step turn has already committed the text
    /// of earlier steps, and dropping that would lose the answer instead of
    /// de-duplicating it.
    StreamRetrying,
    /// A lifecycle hook did something worth surfacing (rewrote arguments,
    /// appended context, blocked, or failed). Plain successes are silent.
    /// Rendered as a dim log line.
    HookFired {
        /// Lifecycle event name (e.g. `"pre_tool_use"`). Owned rather than
        /// `&'static str`: an event that can be read back off disk cannot
        /// borrow, not even from the binary that wrote it.
        event: String,
        /// The hook's shell command.
        command: String,
        /// What the hook did.
        outcome: crate::hooks::HookOutcome,
    },
    /// Plan mode: the model presented a plan via `exit_plan` and the turn is
    /// paused awaiting a verdict. The plan markdown is also persisted to
    /// `.wizard/plan.md`.
    ///
    /// The consumer must [`PlanGate::claim`] the gate and send exactly one
    /// [`PlanVerdict`] (the TUI renders a review; headless, gateway, fleet and
    /// ACP auto-approve, because no human is watching). Dropping the claimed
    /// channel counts as no verdict and keeps plan mode on. Never claiming it
    /// leaves the turn parked inside `exit_plan` until the event channel
    /// closes, so claim first and decide after.
    PlanReady { plan: String, gate: PlanGate },
    /// Plan mode: the model asked clarifying questions via the `interview`
    /// tool and the turn is paused awaiting answers. Read-only, so it is
    /// allowed mid-plan.
    ///
    /// The consumer must [`InterviewGate::claim`] the gate and send exactly one
    /// response: `Some(answers)` aligned with `questions` (empty string = the
    /// user skipped that one), or `None` to decline the interview entirely (no
    /// interactive user, or the user dismissed it). Dropping the claimed
    /// channel counts as `None`.
    Interview {
        /// The questions to put to the user, in order.
        questions: Vec<InterviewQuestion>,
        gate: InterviewGate,
    },
    /// Omakase (chef's-choice) mode: the model finished planning and, because
    /// there is no human review gate, is proceeding to execute. Informational
    /// only — the plan markdown for the surface to display. The plan is also
    /// persisted to `.wizard/plan.md`.
    OmakaseProceeding {
        /// The plan markdown the chef chose.
        plan: String,
    },
    /// Token usage of one completed model call, when the backend reported
    /// counts. Surfaces accumulate these (status bar lifetime totals via
    /// `/cost`, headless summary). The TUI context meter uses
    /// `prompt_tokens` as the size of the next call until compaction or
    /// `/clear` replaces it with an estimate via [`AgentEvent::ContextSize`].
    /// Emitted for the parent's own calls and for every subagent call made
    /// under it (`spawn_subagent`, `/ultra`'s candidates and judges), so the
    /// counter reflects what the turn actually spent.
    Usage {
        prompt_tokens: u64,
        completion_tokens: u64,
    },
    /// Tokens that will load into the next model call after history shrank
    /// (`/compact`, auto-compaction). Replaces the context meter without
    /// touching session lifetime totals.
    ContextSize { tokens: u64 },
    /// The `/ultra` pre-phase produced its guidance: `label` is the roster that
    /// ran (`"ultra ×3 · implementer+skeptic+minimalist · 1 judge"`), `guidance`
    /// the candidate drafts and the judge's verdict exactly as they were
    /// injected into the turn.
    ///
    /// The TUI folds it into a collapsed transcript card, which is the durable
    /// record of a fan-out the user paid several× a normal turn for: the
    /// candidates' panes retire off the rail within seconds of finishing, while
    /// the main agent is still working. Surfaces that only print the turn's
    /// answer ignore it — the drafts are advice, not the answer.
    UltraGuidance { label: String, guidance: String },
    /// The todo list was replaced via the `todo` tool. Carries the full new
    /// list; the TUI mirrors it in a compact overlay above the composer,
    /// headless prints a one-line summary, the gateway ignores it.
    TodoUpdated(Vec<crate::tools::todo::TodoItem>),
    /// A background task (`execute` with `run_in_background`) was just
    /// spawned. The TUI mirrors it into the dashboard's task list; other
    /// surfaces ignore it.
    TaskStarted { id: u32, command: String },
    /// A background task (`execute` with `run_in_background`) finished; its
    /// output tail was injected into history. The TUI and headless surfaces
    /// print a one-liner, the gateway ignores it.
    TaskFinished {
        id: u32,
        command: String,
        status: crate::tools::tasks::TaskStatus,
    },
    /// `spawn_subagent` was called with `background: true` and just detached.
    /// The TUI mirrors it into the dashboard's subagent list; other surfaces
    /// ignore it.
    SubagentStarted { id: u32, name: String, task: String },
    /// A backgrounded subagent finished; its report was injected into
    /// history. The TUI and headless surfaces print a one-liner, the gateway
    /// ignores it.
    SubagentFinished {
        id: u32,
        name: String,
        task: String,
        completed: bool,
        output: String,
    },
    /// A subagent run started, foreground or background. `run` scopes every
    /// later `SubagentRun*` event below to this run, so a surface can demux
    /// concurrent runs of the same subagent into separate panes. `bg` carries
    /// the background-registry id when the run was detached, so the surface
    /// can kill it.
    SubagentRunStarted {
        run: u64,
        bg: Option<u32>,
        name: String,
        task: String,
    },
    /// A subagent produced assistant text (its own message, between tool
    /// calls). Scoped to a run.
    SubagentRunText { run: u64, text: String },
    /// A subagent started a tool call. Scoped to a run; the tool name is bare
    /// (the pane supplies the subagent's name).
    SubagentRunToolStarted { run: u64, name: String, args: Value },
    /// A subagent's tool call finished. Scoped to a run.
    SubagentRunToolFinished {
        run: u64,
        name: String,
        output: ToolOutput,
    },
    /// [`AgentEvent::Images`], scoped to a subagent run — images produced
    /// inside a run land in the same session directory and are announced the
    /// same way, so a run's pane can render them instead of losing them.
    SubagentRunImages {
        run: u64,
        source: ImageSource,
        images: Vec<ImageRef>,
    },
    /// A subagent completed one step (model round-trip). 1-based, scoped to a
    /// run.
    SubagentRunStep { run: u64, step: u32 },
    /// A subagent run ended. Scoped to a run. `error` is set when it died on a
    /// hard error; `completed` is false when it hit its step budget.
    SubagentRunDone {
        run: u64,
        completed: bool,
        output: String,
        steps_used: u32,
        error: Option<String>,
    },
    /// A foreground shell command started with its stdin held open, and a
    /// human may drive it.
    ///
    /// This is the announcement half of the interactive-command mechanism (see
    /// [`ConsoleGate`] and the module docs). A surface that has a person in
    /// front of it claims the gate, puts its composer into console mode, and
    /// relays what the user types into the child's stdin; a surface that does
    /// not simply renders the [`AgentEvent::ConsoleOutput`] that follows. The
    /// command's stdin is a pipe either way, so the child is not surprised by
    /// who is on the far end of it.
    ///
    /// Never emitted for a subagent's commands, or for any run whose tool
    /// context did not declare
    /// [`ConsoleAccess::Interactive`](crate::tools::ConsoleAccess): those keep
    /// `/dev/null` on fd 0 and cannot prompt at all.
    ConsoleOpened {
        /// The command line, so the surface can say what is being driven.
        command: String,
        gate: ConsoleGate,
    },
    /// The command behind `gate` looks like it is waiting for an answer.
    ///
    /// [`Self::ConsoleOpened`] says a console *exists*; this says somebody
    /// should be typing into it. They are separate because almost no command
    /// ever prompts: `ls` and `cargo build` open a console and never ask
    /// anything, and a surface that repurposes its composer the moment a
    /// console opens takes Enter away from the agent for the whole of every
    /// command it runs. A surface claims the gate at `ConsoleOpened` (so the
    /// writer is held before any question can appear) and switches its composer
    /// here.
    ///
    /// The test is the one [`run_command_interactive`](crate::tools) already
    /// applies to stop the unattended-time clock: the child's last write left
    /// the cursor mid-line, and it has been quiet since. That is the shape of a
    /// question in every shell, installer and REPL. Emitted once per command —
    /// a command that has asked one question is assumed to be conversational
    /// from then on, and a composer that flipped back on the next line of
    /// output would be worse than one that stayed.
    ConsoleWaiting { gate: ConsoleGate },
    /// Output a running foreground command has produced so far, scoped to the
    /// console it came from.
    ///
    /// Emitted as the bytes arrive rather than when the command exits, which is
    /// the whole point: a prompt the user cannot see is a prompt the user
    /// cannot answer. stdout and stderr are interleaved here in arrival order,
    /// as a terminal would show them; the model still gets them separated in
    /// the tool result.
    ConsoleOutput { gate: ConsoleGate, chunk: String },
    /// The command behind `gate` ended (exited, timed out, or was killed) and
    /// its console is closed. A surface leaves console mode here; anything
    /// typed afterwards is a message to the agent again.
    ConsoleClosed { gate: ConsoleGate },
    /// The agent asked to run one of Wizard's own slash commands via the
    /// `run_command` tool. Carries the raw command line (e.g. `/effort high`).
    /// The interactive surface validates and dispatches it once the turn ends
    /// and the agent is back in its slot; other surfaces ignore it (there is
    /// no menu to drive).
    CommandRequested(String),
    /// The turn is over.
    Done { reason: DoneReason },
}

impl AgentEvent {
    /// Whether this asks the receiving machine to *do* something, as opposed to
    /// reporting what the sending machine already did.
    ///
    /// The distinction only matters once an event can arrive from somewhere
    /// else. A report is a thing to render; a request is a thing to obey, and
    /// obeying one that came off a socket is letting a peer drive this
    /// machine's menu. [`crate::mesh::turn::PeerTurn`] refuses to carry
    /// anything that answers `true` here.
    ///
    /// **The match is exhaustive on purpose, with no wildcard arm.** The rule
    /// this replaces lived in the mesh as a one-entry negative match on the
    /// serde tag, which meant a variant added later crossed to peers by
    /// default and the person adding it was never asked. Here, adding a variant
    /// does not compile until somebody decides which half it is in.
    ///
    /// [`Self::PlanReady`] and [`Self::Interview`] are reports, not requests,
    /// even though a local surface answers them: the *question* is a fact about
    /// the sender's turn and a watcher should see it. What a watcher must not
    /// get is the ability to answer, and that is taken away by voiding the
    /// gate ticket rather than by dropping the event.
    ///
    /// [`Self::ConsoleOpened`] is the same call and a sharper one, because what
    /// a claimed console gate buys is a writer into a shell on the sending
    /// machine. It stays a report — a watcher should see that a command is
    /// waiting on somebody, and the [`Self::ConsoleOutput`] that follows is the
    /// most interesting thing on the stream while it waits — and the ticket is
    /// voided by the same rule, keyed on the field being named `gate`.
    pub fn is_request(&self) -> bool {
        match self {
            Self::CommandRequested(_) => true,
            Self::TextDelta(_)
            | Self::ThinkingDelta(_)
            | Self::ToolStarted { .. }
            | Self::ToolFinished { .. }
            | Self::Images { .. }
            | Self::StepCompleted { .. }
            | Self::Error(_)
            | Self::Notice(_)
            | Self::StreamRetrying
            | Self::HookFired { .. }
            | Self::PlanReady { .. }
            | Self::Interview { .. }
            | Self::ConsoleOpened { .. }
            | Self::ConsoleWaiting { .. }
            | Self::ConsoleOutput { .. }
            | Self::ConsoleClosed { .. }
            | Self::OmakaseProceeding { .. }
            | Self::Usage { .. }
            | Self::ContextSize { .. }
            | Self::UltraGuidance { .. }
            | Self::TodoUpdated(_)
            | Self::TaskStarted { .. }
            | Self::TaskFinished { .. }
            | Self::SubagentStarted { .. }
            | Self::SubagentFinished { .. }
            | Self::SubagentRunStarted { .. }
            | Self::SubagentRunText { .. }
            | Self::SubagentRunToolStarted { .. }
            | Self::SubagentRunToolFinished { .. }
            | Self::SubagentRunImages { .. }
            | Self::SubagentRunStep { .. }
            | Self::SubagentRunDone { .. }
            | Self::Done { .. } => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Gates
// ---------------------------------------------------------------------------

/// Where the reply channels of open gates wait while their request travels the
/// event stream as plain data.
///
/// One desk per answer type, each a process-wide static. That is deliberate,
/// and it is the smaller of two evils: the alternative is to put the reply
/// channel back inside the event, and then a stream that is teed or recorded
/// holds a live sender, so dropping the copy that a surface is rendering no
/// longer unparks the turn. Ownership of an answer must not be duplicated when
/// a report is.
///
/// A `Vec` rather than a map: at most a handful of gates are open at once (one,
/// nearly always, since the turn is blocked on it), and a linear scan over a
/// few entries beats hashing.
///
/// Generic over the *sender*, not over the answer, because not every gate is
/// answered once: a plan review parks a
/// [`oneshot::Sender`](tokio::sync::oneshot::Sender), and a command's console
/// parks an [`mpsc::Sender`](tokio::sync::mpsc::Sender) that the surface writes
/// to for as long as the command runs. The desk's job — issue a ticket, hold
/// the channel, hand it over at most once — is the same either way, and
/// [`GateDesk::open`] below is the convenience the one-shot desks share.
struct GateDesk<S> {
    /// Next ticket number. Monotonic per desk and never reused, so a late
    /// answer to a closed gate finds nothing rather than someone else's gate.
    next: AtomicU64,
    /// Reply channels of the gates nobody has claimed yet, by ticket.
    waiting: Mutex<Vec<(u64, S)>>,
}

impl<S> GateDesk<S> {
    const fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
            waiting: Mutex::new(Vec::new()),
        }
    }

    /// Park `sender` and return the ticket that will fetch it back.
    fn park(&self, sender: S) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.lock().push((id, sender));
        id
    }

    /// Take the reply channel for `id`. `None` when it was already claimed, or
    /// when the ticket belongs to another process (a replayed recording).
    fn claim(&self, id: u64) -> Option<S> {
        let mut waiting = self.lock();
        let at = waiting.iter().position(|(ticket, _)| *ticket == id)?;
        Some(waiting.swap_remove(at).1)
    }

    /// A panic elsewhere cannot leave this list inconsistent: it holds owned
    /// senders, and nothing reads them while the lock is held, so a poisoned
    /// mutex is recovered rather than propagated. Refusing to hand back a
    /// reply channel would wedge every later turn in the process.
    fn lock(&self) -> MutexGuard<'_, Vec<(u64, S)>> {
        self.waiting.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<T> GateDesk<oneshot::Sender<T>> {
    /// Open a gate answered exactly once: the ticket to announce, and the
    /// receiver the caller parks on until somebody claims and answers it.
    fn open(&self) -> (u64, oneshot::Receiver<T>) {
        let (respond, wait) = oneshot::channel();
        (self.park(respond), wait)
    }
}

static PLAN_GATES: GateDesk<oneshot::Sender<PlanVerdict>> = GateDesk::new();
static INTERVIEW_GATES: GateDesk<oneshot::Sender<Option<Vec<String>>>> = GateDesk::new();
static CONSOLE_GATES: GateDesk<ConsoleSlot> = GateDesk::new();

/// Ticket for a paused plan review ([`AgentEvent::PlanReady`]).
///
/// Serializes as its number, which is all a recording or a peer can honestly
/// carry: the gate itself is a live turn in the process that opened it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PlanGate(u64);

impl PlanGate {
    /// Open a review gate: the ticket to put on the event, and the receiver
    /// `exit_plan` parks on.
    pub(crate) fn open() -> (Self, oneshot::Receiver<PlanVerdict>) {
        let (ticket, wait) = PLAN_GATES.open();
        (Self(ticket), wait)
    }

    /// Take the verdict channel, for a surface that answers later (the TUI
    /// holds it while the user reads the plan). `None` when another consumer
    /// already claimed it.
    pub fn claim(self) -> Option<oneshot::Sender<PlanVerdict>> {
        PLAN_GATES.claim(self.0)
    }

    /// Claim and answer in one step, for a surface with no human to ask.
    /// `false` when the gate was already claimed or the turn has moved on.
    pub fn answer(self, verdict: PlanVerdict) -> bool {
        self.claim()
            .is_some_and(|respond| respond.send(verdict).is_ok())
    }

    /// Close the gate without answering: the request never reached a surface,
    /// so nothing should be able to answer it afterwards.
    pub(crate) fn cancel(self) {
        drop(self.claim());
    }
}

/// Ticket for a paused interview ([`AgentEvent::Interview`]). See [`PlanGate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InterviewGate(u64);

impl InterviewGate {
    /// Open an interview gate: the ticket to put on the event, and the receiver
    /// the `interview` tool parks on.
    pub(crate) fn open() -> (Self, oneshot::Receiver<Option<Vec<String>>>) {
        let (ticket, wait) = INTERVIEW_GATES.open();
        (Self(ticket), wait)
    }

    /// Take the answers channel, for a surface that collects them over several
    /// keystrokes. `None` when another consumer already claimed it.
    pub fn claim(self) -> Option<oneshot::Sender<Option<Vec<String>>>> {
        INTERVIEW_GATES.claim(self.0)
    }

    /// Claim and answer in one step. `None` declines the interview, which is
    /// what a surface with no interactive user sends.
    pub fn answer(self, answers: Option<Vec<String>>) -> bool {
        self.claim()
            .is_some_and(|respond| respond.send(answers).is_ok())
    }

    /// Decline the interview: the model proceeds on its own judgment.
    pub fn decline(self) -> bool {
        self.answer(None)
    }

    /// Close the gate without answering; see [`PlanGate::cancel`].
    pub(crate) fn cancel(self) {
        drop(self.claim());
    }
}

// ---------------------------------------------------------------------------
// Consoles
// ---------------------------------------------------------------------------

/// How deep the queue between a surface's keystrokes and a child's stdin runs.
///
/// A human types lines; a child consumes them one `read` at a time. Sixteen is
/// far more than the backlog a person can build against a program that is
/// asking them questions, and the bound is what keeps a surface that keeps
/// typing at a wedged child from growing without limit — [`ConsoleWriter::line`]
/// reports the refusal rather than blocking the render loop.
const CONSOLE_QUEUE: usize = 16;

/// One thing a human can do to a running command's stdin.
///
/// Only two, and deliberately not "arbitrary bytes": a console is a line
/// conversation with a program that asked a question, and the surface that
/// relays it is a composer, not a terminal emulator. Anything needing raw byte
/// control wants a real pty, which is a different feature with a different cost
/// (see `docs/interactive-commands.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsoleInput {
    /// One line, without its terminator. The writer appends `\n` — a prompt
    /// answered without one is a prompt still waiting.
    Line(String),
    /// Close the child's stdin, which is what a terminal's Ctrl-D means: the
    /// program reading a list of lines is told there are no more.
    Eof,
}

/// What the desk holds for an open console: the flag that says a surface took
/// it, and the channel into the running child's stdin.
///
/// The flag is separate from "the ticket is gone from the desk" because
/// [`ConsoleGate::cancel`] also removes the ticket, and the running command
/// needs to distinguish *nobody claimed this* from *somebody did*: only the
/// second one licenses stopping the timeout clock. See
/// [`ConsoleHost::attended`].
type ConsoleSlot = (Arc<AtomicBool>, mpsc::Sender<ConsoleInput>);

/// Ticket for a running foreground command's stdin
/// ([`AgentEvent::ConsoleOpened`]).
///
/// Serializes as its number, for the same reason and with the same consequence
/// as [`PlanGate`]: a console is a live child process in the process that
/// spawned it, and a peer or a replayed recording holding the number can claim
/// nothing. `crate::mesh::turn` voids it to ticket 0 on the way out, so
/// watching a peer's session never becomes typing into a peer's shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConsoleGate(u64);

impl ConsoleGate {
    /// Open a console for a command about to be spawned: the ticket to
    /// announce, and the host end the command reads the user's lines from.
    pub(crate) fn open() -> (Self, ConsoleHost) {
        let (send, receive) = mpsc::channel(CONSOLE_QUEUE);
        let claimed = Arc::new(AtomicBool::new(false));
        let ticket = CONSOLE_GATES.park((Arc::clone(&claimed), send));
        (Self(ticket), ConsoleHost { receive, claimed })
    }

    /// Take the writer into this command's stdin. `None` when another consumer
    /// already claimed it, when the command has ended, or when the ticket came
    /// off a wire.
    ///
    /// Exactly once, like every other gate: a stream teed to a renderer, a
    /// recorder and a peer must still have exactly one author of what the
    /// child reads.
    pub fn claim(self) -> Option<ConsoleWriter> {
        let (claimed, send) = CONSOLE_GATES.claim(self.0)?;
        claimed.store(true, Ordering::SeqCst);
        Some(ConsoleWriter(send))
    }

    /// Close the console without claiming it: the command ended, so nothing
    /// should be able to type at it afterwards.
    pub(crate) fn cancel(self) {
        drop(CONSOLE_GATES.claim(self.0));
    }
}

/// The running command's end of a console: the user's lines, and whether
/// anybody is actually there.
pub struct ConsoleHost {
    /// Lines a surface has typed, in order.
    pub receive: mpsc::Receiver<ConsoleInput>,
    /// Set once a surface claims the gate. Shared with the desk entry so it
    /// stays true after the entry is removed.
    claimed: Arc<AtomicBool>,
}

impl ConsoleHost {
    /// Whether a surface claimed this console, and so whether there is a human
    /// who could be the reason the command is not making progress.
    ///
    /// This is the difference between *waiting for somebody* and *hung*, and it
    /// is a fact rather than a guess: the surface either took the writer or it
    /// did not.
    pub fn attended(&self) -> bool {
        self.claimed.load(Ordering::SeqCst)
    }
}

/// A surface's writer into one running command's stdin.
///
/// Held for the life of the command and dropped when it ends, which closes the
/// channel and is how the command learns the surface went away.
#[derive(Debug, Clone)]
pub struct ConsoleWriter(mpsc::Sender<ConsoleInput>);

impl ConsoleWriter {
    /// Send one line to the child, terminator included. `false` when the
    /// command has ended or the queue is full — both are things to tell the
    /// user rather than to block a render loop over, which is why this is
    /// non-blocking and says so in its return value rather than being `async`.
    pub fn line(&self, text: impl Into<String>) -> bool {
        self.0.try_send(ConsoleInput::Line(text.into())).is_ok()
    }

    /// Close the child's stdin (a terminal's Ctrl-D). See [`ConsoleInput::Eof`].
    pub fn eof(&self) -> bool {
        self.0.try_send(ConsoleInput::Eof).is_ok()
    }
}
