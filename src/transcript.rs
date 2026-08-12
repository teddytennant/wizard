//! One reading of a conversation, for every surface that shows one.
//!
//! Wizard has grown a transcript renderer per surface, and each one re-derived
//! what a conversation *is* on the way: what counts as something the user said,
//! which tool result answers which tool call, whether a replayed result was a
//! failure, where a tool's images belong. Three independent answers to those
//! questions produced three different conversations out of the same bytes, and
//! the differences were only ever found by looking at two screens side by side.
//!
//! [`TranscriptModel`] is the one answer. It has two entry points and they are
//! deliberately the only two:
//!
//! - [`TranscriptModel::seed`] folds a stored session ([`SessionEntry`]) into
//!   items. This is replay: `--resume` in the TUI, `GET /api/tasks/{id}` in the
//!   GUI.
//! - [`TranscriptModel::apply`] folds one live [`AgentEvent`] into the same
//!   items. This is a turn as it happens.
//!
//! Both write the same [`TranscriptItem`] list through the same pairing
//! bookkeeping, so "what the conversation was" cannot depend on which door the
//! data came in. The `a_live_turn_and_its_replay_agree` test below pins that:
//! it seeds a fixture session, applies the event stream that same turn would
//! have emitted, and asserts the two item lists are equal.
//!
//! # There is no rendering here
//!
//! A [`TranscriptItem`] carries the conversation, not a decision about how to
//! draw it. Collapse state, markdown wrapping, summary lines, glyphs and colors
//! all stay with the surface, which projects these items into its own row type
//! (fold flags in [`crate::app::TranscriptView`] for the TUI, laid-out blocks
//! in `crate::native::widget::transcript` for the window). That is the seam
//! that lets one model serve several renderers without any of them leaking into
//! the others.
//!
//! # What only the live stream can say
//!
//! Two things a session file does not record, so `seed` and `apply` cannot be
//! symmetric about them, and both gaps are named rather than papered over:
//!
//! - **Failure.** [`ToolOutput::is_error`](crate::tools::ToolOutput) is not
//!   persisted, so a replayed result's status is sniffed from the dispatcher's
//!   own failure phrasings ([`looks_like_failure`]). A live result carries the
//!   real flag.
//! - **Reasoning.** [`TranscriptItem::Thinking`] exists only on the live path.
//!   Session files do keep thinking blocks, but no surface has ever rendered
//!   them on reload, and quietly starting to would change what every resumed
//!   conversation looks like. The `seed_drops_reasoning` test pins the current
//!   behaviour so the day it changes is a decision rather than a side effect.
//!
//! # Telling a renderer what moved
//!
//! Two surfaces now keep per-item view state next to these items — the TUI's
//! fold flags ([`crate::app::TranscriptView`]), and whatever the native GUI
//! caches per row. Both need to know what the last mutation did without
//! diffing the whole list, so every mutating entry point bumps
//! [`TranscriptModel::revision`] and records a [`Change`].
//!
//! The hazard that makes this more than a convenience is that item indices are
//! **not** append-only. [`TranscriptModel::feed_user`] splices a tool's images
//! in behind the row that produced them, mid-vector, and every row below shifts
//! down by one. A consumer that assumed appends would keep every cached widget
//! below the splice attached to the wrong item, so an insert has to announce
//! itself as one: `insert_reports_a_mid_vector_change` pins that.

use std::collections::VecDeque;

use serde_json::Value;

use crate::agent::session::SessionEntry;
use crate::agent::{AgentEvent, ImageSource, SESSION_START_HOOK_NOTE};
use crate::images::ImageRef;
use crate::llm::{ChatMessage, Image, Role};

/// Cap on a tool summary line. The GUI renders it muted next to the tool name,
/// so it must stay short.
const SUMMARY_CHARS: usize = 100;

/// How the agent labels the user-role message it rides a tool's images back to
/// the model on (`Agent::run_tool`). It is the one thing that tells that
/// message apart from a person attaching an image to a prompt of their own,
/// which, now that both the GUI and the TUI can upload one, is not a
/// hypothetical collision.
///
/// The tool's name follows in backticks, and `carrier_tool` reads it back out:
/// a batch that returned images from two tools writes two of these in a row,
/// and only the name says which card each one belongs on.
const TOOL_IMAGE_NOTE: &str = "Image(s) returned by `";

/// How much of a running command's live output ([`ToolItem::progress`]) is
/// kept. Only the most recent bytes matter — a prompt is the last line, and the
/// full output arrives in the tool result when the call ends — so this is a
/// screenful of scrollback rather than a transcript.
const PROGRESS_TAIL_BYTES: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// Items
// ---------------------------------------------------------------------------

/// One thing that happened in a conversation, in the order it happened.
///
/// Owned, comparable data with no rendering in it: two models built from the
/// same conversation by different doors compare equal, which is what makes the
/// equivalence test possible at all.
#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptItem {
    /// A turn boundary, from a session file's
    /// [`TurnMarker`](crate::agent::session::TurnMarker). Live streams have no
    /// equivalent: the surface that started the turn already knows where it
    /// began.
    TurnMarker { turn: u64, prompt: String },
    /// Something the user said, with whatever they attached to it.
    User { text: String, images: Vec<ImageRef> },
    /// Assistant narration.
    Text(String),
    /// Model reasoning that preceded a reply. Live only; see the module docs.
    Thinking(String),
    /// A tool call and, once it lands, its result.
    Tool(ToolItem),
    /// Images the turn produced, by the model or by a tool.
    Images {
        source: ImageSource,
        images: Vec<ImageRef>,
    },
    /// An informational line: a mode switch, a hook report, a background task
    /// finishing, an error the turn survived.
    Notice(String),
}

/// A tool call, and its result once one arrives.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolItem {
    pub name: String,
    /// Arguments as the model sent them. [`Value::Null`] when the call itself
    /// was never seen (an orphan result read out of a truncated session file),
    /// which is not the same as a call that took no arguments.
    pub args: Value,
    /// The provider's id for this call, which is what binds it to its result.
    /// Empty when nothing supplied one: live [`AgentEvent::ToolStarted`]
    /// carries no id, and neither do calls read out of a pre-v2 session file.
    /// `TranscriptModel::answer_tool` says what the pairing falls back to then.
    pub call_id: String,
    /// `None` while the call is still running, or forever if the run was
    /// interrupted before it answered.
    pub output: Option<ToolItemOutput>,
    /// Output a still-running command has produced so far
    /// ([`AgentEvent::ConsoleOutput`]), so a surface can show a prompt while
    /// the command is still blocked on it. Empty for every other tool.
    ///
    /// Deliberately a *different* field from `output`, for two reasons. The
    /// pairing in [`TranscriptModel::answer_tool`] finds a call by its row
    /// still having `output: None`, so writing partial text there would orphan
    /// the real result. And this text is transient: it is cleared the moment
    /// the call is answered, because the answer is the same bytes read back
    /// through [`render_command_result`](crate::tools::shell::render_command_result)
    /// with the exit status folded in. Keeping both would render a command's
    /// output twice, and would make a live turn and its replay disagree — see
    /// the module docs on why that equality is worth protecting.
    pub progress: String,
}

/// What a tool call returned.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolItemOutput {
    pub content: String,
    /// True when the call failed. Exact on the live path; on replay it is
    /// [`looks_like_failure`]'s reading of the text, because the session file
    /// does not record the flag.
    pub is_error: bool,
}

// ---------------------------------------------------------------------------
// Change signalling
// ---------------------------------------------------------------------------

/// What the most recent mutation did to the item list.
///
/// One value, not a queue: the contract is that a consumer reads this after
/// every mutating call, which is what lets a renderer invalidate exactly the
/// rows that moved instead of diffing. A call that changed several items at
/// once reports the *earliest* index it touched, so "from here down is new" is
/// always a safe reading.
///
/// [`Change::Streaming`] is the one variant that names no item. The
/// uncommitted tail ([`TranscriptModel::streaming`]) is drawn below the items
/// by both surfaces, so a delta into it is a real change a renderer has to
/// repaint — but it is not an item, and reporting it as `Mutated(items.len())`
/// would hand every consumer an out-of-bounds index to special-case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Change {
    /// Items from this index to the end are new.
    Appended(usize),
    /// One item was spliced in at this index; everything below it shifted
    /// down by one.
    Inserted(usize),
    /// The item at this index changed in place. The list's length did not.
    Mutated(usize),
    /// The uncommitted streaming tail changed. No item did.
    Streaming,
    /// Nothing about the previous list can be relied on: rebuild from
    /// [`TranscriptModel::items`]. A freshly seeded (replayed) model reports
    /// this, and so does one that was cleared.
    #[default]
    Reset,
}

/// Fold two changes from the same mutating call into the one a consumer can
/// act on.
///
/// Two appends collapse to the earlier index, which is the case that actually
/// happens: committing a streamed reply and then opening a tool row is three
/// pushes in one [`TranscriptModel::apply`]. Anything else mixed inside a
/// single call is deliberately answered with [`Change::Reset`] — a full
/// rebuild is always correct, and a wrong incremental index is not.
fn merge(previous: Change, next: Change) -> Change {
    match (previous, next) {
        (Change::Appended(a), Change::Appended(b)) => Change::Appended(a.min(b)),
        (a, b) if a == b => a,
        _ => Change::Reset,
    }
}

// ---------------------------------------------------------------------------
// The model
// ---------------------------------------------------------------------------

/// The conversation so far, plus the bookkeeping needed to keep folding into
/// it. See the module docs.
///
/// [`Clone`] because a second consumer wants a snapshot: the native GUI renders
/// off the main loop, and copying the conversation is cheaper and far less
/// error-prone than sharing it behind a lock that every draw would contend on.
#[derive(Debug, Default, Clone)]
pub struct TranscriptModel {
    items: Vec<TranscriptItem>,
    /// Indices into `items` of tool rows still waiting for a result, oldest
    /// first. A parallel batch's rows are all laid down when its assistant
    /// message is read, so this is the batch, in call order.
    open: VecDeque<usize>,
    /// Reasoning streamed but not yet committed to an item.
    thinking: String,
    /// Assistant text streamed but not yet committed to an item.
    text: String,
    /// Bumped by every mutation, including ones that touch only the streaming
    /// tail. A consumer compares it against the revision it last mirrored to
    /// know whether [`TranscriptModel::last_change`] is news or a leftover.
    revision: u64,
    /// What the current mutating call has done so far.
    change: Change,
    /// `revision` as it stood when the current mutating call began, so
    /// [`TranscriptModel::record`] can tell "first change of this call"
    /// (replace) from "another change in the same call" (merge).
    group: u64,
}

impl TranscriptModel {
    /// An empty transcript, for a surface that will drive it live.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many mutations this model has seen. Monotonic, and bumped by
    /// streaming deltas as well as by item changes, so a renderer that repaints
    /// whenever this moves repaints exactly when there is something new.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// What the most recent mutating call did. See [`Change`].
    pub fn last_change(&self) -> Change {
        self.change
    }

    /// Open a mutating call. Every public mutator starts with this, and the
    /// private helpers below never do, so one call reports one change.
    fn group(&mut self) {
        self.group = self.revision;
    }

    /// Record one item- or tail-level change inside the current call.
    fn record(&mut self, change: Change) {
        self.change = if self.revision == self.group {
            change
        } else {
            merge(self.change, change)
        };
        self.revision += 1;
    }

    /// Append an item, announcing where it landed.
    fn push(&mut self, item: TranscriptItem) {
        self.record(Change::Appended(self.items.len()));
        self.items.push(item);
    }

    /// Splice an item in mid-list, announcing that everything below moved.
    fn insert(&mut self, at: usize, item: TranscriptItem) {
        self.record(Change::Inserted(at));
        self.items.insert(at, item);
    }

    /// Replay a stored session: header, turn markers and message records in
    /// file order.
    pub fn seed(entries: &[SessionEntry]) -> Self {
        let mut model = Self::new();
        for entry in entries {
            model.fold_entry(entry);
        }
        model.seeded()
    }

    /// Fold one stored entry into the conversation.
    ///
    /// The incremental replay door, for a surface that wants a [`Change`] per
    /// record rather than one [`Change::Reset`] for a whole file — which is
    /// what a renderer building per-row state as it reads needs, and the only
    /// way a [`Change::Inserted`] is ever observable from outside this module.
    pub fn fold_entry(&mut self, entry: &SessionEntry) {
        match entry {
            // The header is metadata about the file, not about the
            // conversation in it.
            SessionEntry::Header(_) => {}
            SessionEntry::Marker(marker) => self.turn_marker(marker.turn, marker.prompt.clone()),
            SessionEntry::Message(record) => self.fold_message(&record.message),
        }
    }

    /// Replay a stored session that has already been reduced to its messages,
    /// for a caller holding
    /// [`Session::load_messages`](crate::agent::session::Session::load_messages)
    /// rather than entries. Identical to [`TranscriptModel::seed`] except that
    /// there are no turn markers to place.
    ///
    /// Both entry points read a message the same way, on purpose: the moment a
    /// rule here consulted something only one of them has (the record's
    /// `system_note` flag, say) the two would start disagreeing, which is the
    /// bug this whole module exists to remove.
    pub fn seed_messages(messages: &[ChatMessage]) -> Self {
        let mut model = Self::new();
        for message in messages {
            model.fold_message(message);
        }
        model.seeded()
    }

    /// A whole conversation arriving at once is not an incremental change: the
    /// per-item bookkeeping `feed` left behind describes how the list was
    /// built, which is of no use to a consumer that has never seen any of it.
    fn seeded(mut self) -> Self {
        self.group();
        self.record(Change::Reset);
        self
    }

    /// The conversation so far.
    pub fn items(&self) -> &[TranscriptItem] {
        &self.items
    }

    /// The conversation so far, by value.
    pub fn into_items(self) -> Vec<TranscriptItem> {
        self.items
    }

    /// Reasoning and assistant text streamed since the last commit, in that
    /// order. A surface renders these below the committed items as the live
    /// tail; they become items on the next [`TranscriptModel::commit`].
    pub fn streaming(&self) -> (&str, &str) {
        (&self.thinking, &self.text)
    }

    /// Record something the user said. Prompts do not arrive as
    /// [`AgentEvent`]s (the surface is the one that sent them), so the live
    /// path hands them over here.
    pub fn user(&mut self, text: String, images: Vec<ImageRef>) {
        self.group();
        self.commit_inner();
        self.push_user(text, images);
    }

    /// Record a turn boundary, for a live surface that tracks turns.
    pub fn turn_marker(&mut self, turn: u64, prompt: String) {
        self.group();
        self.push(TranscriptItem::TurnMarker { turn, prompt });
    }

    /// Record an informational line the surface raised itself — a mode switch,
    /// a command's result, an error it recovered from.
    ///
    /// Deliberately does **not** commit the streaming tail, which is what tells
    /// it apart from [`AgentEvent::Notice`]. A surface's own notice is a remark
    /// about the session rather than something that happened inside the turn,
    /// and committing on one would chop a reply the model is still writing into
    /// two items at whatever moment the user pressed a key.
    pub fn notice(&mut self, text: String) {
        self.group();
        self.push(TranscriptItem::Notice(text));
    }

    /// Record a complete assistant message, for a surface fed whole messages
    /// rather than deltas (a subagent run reports one message at a time).
    pub fn assistant(&mut self, text: String) {
        self.group();
        self.commit_inner();
        if !text.trim().is_empty() {
            self.push(TranscriptItem::Text(text));
        }
    }

    /// Forget the conversation: `/clear` and a session rotation both start a
    /// new one in place.
    pub fn clear(&mut self) {
        self.group();
        self.items.clear();
        self.open.clear();
        self.thinking.clear();
        self.text.clear();
        self.record(Change::Reset);
    }

    /// Write `output` into the most recent tool row `wanted` accepts, and say
    /// whether one was found.
    ///
    /// This is for a result that arrives long after the call it answers, which
    /// [`TranscriptModel::apply`]'s pairing cannot help with: a detached
    /// subagent's report lands minutes after its `spawn_subagent` call returned
    /// the "running in the background" placeholder, with turns in between.
    pub fn amend_tool(
        &mut self,
        wanted: impl Fn(&ToolItem) -> bool,
        output: ToolItemOutput,
    ) -> bool {
        let row = self.items.iter().rposition(|item| match item {
            TranscriptItem::Tool(tool) => wanted(tool),
            _ => false,
        });
        let Some(row) = row else {
            return false;
        };
        self.group();
        self.record(Change::Mutated(row));
        if let TranscriptItem::Tool(tool) = &mut self.items[row] {
            tool.output = Some(output);
        }
        true
    }

    /// Move streamed reasoning and text into items. Reasoning commits first
    /// because it streams first: the model thinks, then answers.
    pub fn commit(&mut self) {
        self.group();
        self.commit_inner();
    }

    /// [`TranscriptModel::commit`] without opening a change group, for the
    /// mutators that commit on their way to doing something else.
    fn commit_inner(&mut self) {
        if !self.thinking.is_empty() {
            let thinking = std::mem::take(&mut self.thinking);
            self.push(TranscriptItem::Thinking(thinking));
        }
        if !self.text.is_empty() {
            let text = std::mem::take(&mut self.text);
            self.push(TranscriptItem::Text(text));
        }
    }

    // -- The live path ----------------------------------------------------

    /// Fold one live event into the conversation.
    ///
    /// Takes the event by reference and **never claims a gate**: a
    /// [`AgentEvent::PlanReady`] or [`AgentEvent::Interview`] is a request for
    /// an answer, and answering it is the surface's job, not a transcript's.
    /// A model that claimed one would silently take the reply channel away
    /// from the modal that was going to fill it, and park the turn forever.
    ///
    /// The match is exhaustive with no wildcard arm, so a new event has to be
    /// classified here rather than silently ignored.
    pub fn apply(&mut self, event: &AgentEvent) {
        self.group();
        match event {
            AgentEvent::TextDelta(delta) => {
                self.text.push_str(delta);
                self.record(Change::Streaming);
            }
            AgentEvent::ThinkingDelta(delta) => {
                self.thinking.push_str(delta);
                self.record(Change::Streaming);
            }
            AgentEvent::ToolStarted { name, args } => {
                self.commit_inner();
                self.start_tool(name.clone(), args.clone(), String::new());
            }
            AgentEvent::ToolFinished { name, output } => {
                self.answer_tool(
                    "",
                    name,
                    ToolItemOutput {
                        content: output.content.clone(),
                        is_error: output.is_error,
                    },
                );
            }
            // The model's own images arrive right after its reply and a tool's
            // right after that tool's row, so appending puts each one under the
            // thing that made it. Replay has to place them by hand instead
            // (see [`TranscriptModel::feed`]) because a session file carries
            // them on a later message than the one they belong to.
            AgentEvent::Images { source, images } => {
                self.commit_inner();
                self.push(TranscriptItem::Images {
                    source: source.clone(),
                    images: images.clone(),
                });
            }
            AgentEvent::Error(message) => {
                self.commit_inner();
                self.push(TranscriptItem::Notice(format!("error: {message}")));
            }
            AgentEvent::Notice(message) => {
                self.commit_inner();
                self.push(TranscriptItem::Notice(message.clone()));
            }
            // The partial completion is about to be re-generated from scratch;
            // committing it would double the text once the retry streams.
            AgentEvent::StreamRetrying => {
                self.text.clear();
                self.thinking.clear();
                self.record(Change::Streaming);
            }
            AgentEvent::HookFired {
                event,
                command,
                outcome,
            } => self.push(TranscriptItem::Notice(format!(
                "hook {event}: {outcome} ({command})"
            ))),
            // The chef's-choice plan has no review gate, so the plan itself is
            // the only record of what was decided: it lands as a tool row,
            // which is the shape every surface already folds.
            AgentEvent::OmakaseProceeding { plan } => {
                self.commit_inner();
                self.finished_tool(
                    "omakase plan (chef's choice)".to_string(),
                    Value::Null,
                    plan.clone(),
                );
            }
            // The ultra pre-phase's drafts and verdict. This is their only
            // durable record: the candidates' panes retire off the rail seconds
            // after they finish, minutes before the main agent is done working
            // from what they wrote, and the guidance itself is a system message
            // that never reaches history.
            AgentEvent::UltraGuidance { label, guidance } => {
                self.commit_inner();
                self.finished_tool(label.clone(), Value::Null, guidance.clone());
            }
            AgentEvent::TaskFinished {
                id,
                command,
                status,
            } => self.push(TranscriptItem::Notice(format!(
                "background task #{id} finished ({}): {command}",
                status.describe()
            ))),
            AgentEvent::SubagentFinished {
                id,
                name,
                task,
                completed,
                ..
            } => {
                let kind = if name.as_str() == crate::agent::subagent::FORK_NAME {
                    "fork"
                } else {
                    "background subagent"
                };
                let verdict = if *completed {
                    "finished"
                } else {
                    "hit its step budget"
                };
                self.push(TranscriptItem::Notice(format!(
                    "{kind} #{id} '{name}' {verdict}: {task}"
                )));
            }
            AgentEvent::CommandRequested(line) => self.push(TranscriptItem::Notice(format!(
                "agent requested {line} (runs after this turn)"
            ))),
            // A gate pauses the turn, so whatever was streaming is final until
            // it is answered: commit it, and leave the answering to the
            // surface.
            AgentEvent::PlanReady { .. } | AgentEvent::Interview { .. } => self.commit_inner(),
            // A running command's output, folded into the `execute` row that is
            // still open. This is the only thing in the model that changes an
            // item *before* it is answered, and the reason is the whole point of
            // the feature: the user has to be able to read the question while
            // the command is still blocked on it.
            AgentEvent::ConsoleOutput { chunk, .. } => self.append_progress(chunk),
            // Opening and closing a console are facts about a child process,
            // not about the conversation. The row is already there
            // (`ToolStarted` laid it down), and `ToolFinished` will answer it.
            AgentEvent::ConsoleOpened { .. }
            | AgentEvent::ConsoleWaiting { .. }
            | AgentEvent::ConsoleClosed { .. } => {}
            // The turn is over; nothing may still be streaming.
            AgentEvent::Done { .. } => self.commit_inner(),
            // Everything below reports on the *run*, not on the conversation:
            // step counters and token counts drive a status bar, the todo list
            // an overlay, and every `SubagentRun*` event belongs to that run's
            // own pane (which is a transcript of its own, fed by its own
            // model). None of them are things that were said.
            AgentEvent::StepCompleted { .. }
            | AgentEvent::Usage { .. }
            | AgentEvent::ContextSize { .. }
            | AgentEvent::TodoUpdated(_)
            | AgentEvent::TaskStarted { .. }
            | AgentEvent::SubagentStarted { .. }
            | AgentEvent::SubagentRunStarted { .. }
            | AgentEvent::SubagentRunText { .. }
            | AgentEvent::SubagentRunToolStarted { .. }
            | AgentEvent::SubagentRunToolFinished { .. }
            | AgentEvent::SubagentRunImages { .. }
            | AgentEvent::SubagentRunStep { .. }
            | AgentEvent::SubagentRunDone { .. } => {}
        }
    }

    // -- The replay path --------------------------------------------------

    /// Fold one stored message into the conversation. One stored message is
    /// one change group, so a replay driven message-at-a-time (rather than
    /// through [`TranscriptModel::seed`], which reports one `Reset` for the
    /// whole file) still reports usable per-message changes.
    pub fn fold_message(&mut self, message: &ChatMessage) {
        self.group();
        let text = message.text();
        match message.role {
            // Only flagged system notes are persisted mid-conversation:
            // background-task reports, subagent reports, hook output, and the
            // compaction summary. Every one of them is something the live
            // surface showed as a notice while it happened, so dropping them on
            // reload loses events the user watched go by. (Stale system prompts
            // from files old enough to have persisted one render the same way,
            // which is the price of reading the message rather than a flag only
            // one of the two entry points has.)
            //
            // Hook *context* is the exception: it is a payload written for the
            // model, often long and often about the repo rather than the
            // conversation. The hook is still reported, by its own one-line
            // `hook session_start: ...` notice.
            Role::System => {
                if !text.starts_with(SESSION_START_HOOK_NOTE) && !text.trim().is_empty() {
                    self.push(TranscriptItem::Notice(text));
                }
            }
            Role::User => self.feed_user(message, text),
            Role::Assistant => {
                // A new assistant turn abandons any call the file left
                // unanswered: its result is never coming.
                self.open.clear();
                if !text.trim().is_empty() {
                    self.push(TranscriptItem::Text(text));
                }
                // Images the model produced itself go with its reply, before
                // the calls it made, which is exactly where the live stream
                // puts them.
                let images = image_refs(&message.images());
                if !images.is_empty() {
                    self.push(TranscriptItem::Images {
                        source: ImageSource::Assistant,
                        images,
                    });
                }
                for call in message.tool_calls() {
                    self.start_tool(
                        call.function.name.clone(),
                        call.function.arguments.clone(),
                        call.id.clone(),
                    );
                }
            }
            // One `tool` message answers a whole parallel batch, so each of its
            // result blocks fills a row of its own.
            Role::Tool => {
                for result in message.tool_results() {
                    self.answer_tool(
                        &result.tool_use_id,
                        &result.name,
                        ToolItemOutput {
                            is_error: looks_like_failure(&result.content),
                            content: result.content.clone(),
                        },
                    );
                }
            }
        }
    }

    /// Fold a stored user-role message, which is one of two quite different
    /// things.
    fn feed_user(&mut self, message: &ChatMessage, text: String) {
        let images = image_refs(&message.images());
        // Images a tool returned ride back to the model on a user message (see
        // `Agent::run_tool`), the one message with this role that is not
        // something a person said. It is that tool call's images and it belongs
        // on that tool's row, not as a prompt of its own.
        //
        // The row is wherever it was, not the end of the list: one assistant
        // message makes several calls and its rows are all laid down when it is
        // read, so appending would put `render`'s image under whatever ran
        // after it. Reading the tool's name out of the note (rather than
        // assuming the most recently answered call) is what makes a batch that
        // returned images from two tools land on two different rows: the agent
        // writes one of these messages per tool, back to back.
        if !images.is_empty()
            && let Some(tool) = carrier_tool(&text)
            && let Some(row) = self.last_answered_row(tool)
        {
            self.insert(
                row + 1,
                TranscriptItem::Images {
                    source: ImageSource::Tool(tool.to_string()),
                    images,
                },
            );
            // The still-open calls stand, they are waiting for results that
            // come after this message, but their rows have shifted down by the
            // one just spliced in.
            for index in self.open.iter_mut().filter(|index| **index > row) {
                *index += 1;
            }
            return;
        }
        // A real prompt ends the previous turn's batch: nothing after it is
        // going to answer a call made before it.
        self.open.clear();
        self.push_user(text, images);
    }

    // -- Shared bookkeeping -----------------------------------------------

    /// Append a prompt, unless it says nothing at all. An empty message with
    /// no attachment is a shape histories occasionally carry (a cancelled
    /// submission, a nudge whose text was stripped) and it renders as an empty
    /// bubble, which reads as a bug rather than as silence.
    fn push_user(&mut self, text: String, images: Vec<ImageRef>) {
        if text.trim().is_empty() && images.is_empty() {
            return;
        }
        self.push(TranscriptItem::User { text, images });
    }

    /// Open a tool row and remember it as awaiting a result.
    fn start_tool(&mut self, name: String, args: Value, call_id: String) {
        self.open.push_back(self.items.len());
        self.push(TranscriptItem::Tool(ToolItem {
            name,
            args,
            call_id,
            output: None,
            progress: String::new(),
        }));
    }

    /// Append a row that is already answered, for the surface-authored cards
    /// (the omakase plan, the ultra guidance) that are shaped like a tool call
    /// without being one.
    fn finished_tool(&mut self, name: String, args: Value, content: String) {
        self.push(TranscriptItem::Tool(ToolItem {
            name,
            args,
            call_id: String::new(),
            output: Some(ToolItemOutput {
                content,
                is_error: false,
            }),
            progress: String::new(),
        }));
    }

    /// Attach `output` to the call it answers.
    ///
    /// Three readings, in strict order of how much they know:
    ///
    /// 1. **By call id**, when there is one and it names an open row. This is
    ///    the only correlation that is actually correct: a model that emits two
    ///    calls to the same tool in one turn is ordinary (both Claude and GPT
    ///    do it by default), and a provider is under no obligation to return
    ///    the results in call order.
    /// 2. **By name, oldest open row first**, when the id says nothing. Live
    ///    [`AgentEvent::ToolFinished`] carries no id at all, and dispatch is
    ///    sequential, so at most one row of a given name is ever open at once
    ///    and the name is enough.
    /// 3. **Oldest open row of any name**, which is what a pre-v2 session file
    ///    means: it recorded neither call ids nor result ids, and its ordering
    ///    is the whole of the correlation it has.
    ///
    /// A result that matches nothing is an orphan (an old or truncated file, or
    /// a tool that was denied before its start was announced) and gets a row of
    /// its own, without arguments, rather than being dropped.
    fn answer_tool(&mut self, call_id: &str, name: &str, output: ToolItemOutput) {
        let mut row = None;
        if !call_id.is_empty() {
            row = self.take_open(|item| item.call_id == call_id);
        }
        if row.is_none() {
            row = self.take_open(|item| item.name == name);
        }
        // Pure order is only ever the reading of a file that has no ids
        // anywhere. A live stream always names its tool, so letting one reach
        // here would let one tool's result fill another tool's row.
        if row.is_none() && call_id.is_empty() {
            row = self.take_open(|_| true);
        }
        match row {
            Some(row) => {
                self.record(Change::Mutated(row));
                if let TranscriptItem::Tool(item) = &mut self.items[row] {
                    item.output = Some(output);
                    // The live tail has been superseded by the real result,
                    // which contains the same bytes plus the exit status. See
                    // `ToolItem::progress`.
                    item.progress.clear();
                }
            }
            None => {
                // A pre-v2 file can carry a result whose tool it never named.
                let name = if name.is_empty() { "tool" } else { name };
                self.push(TranscriptItem::Tool(ToolItem {
                    name: name.to_string(),
                    args: Value::Null,
                    call_id: call_id.to_string(),
                    output: Some(output),
                    progress: String::new(),
                }));
            }
        }
    }

    /// Echo a line the user typed into a running command's console into that
    /// command's card.
    ///
    /// Nothing else will. A terminal echoes what you type because the tty
    /// driver does it; a pipe does not, so the answer a person gave to a prompt
    /// would otherwise leave no trace in the conversation at all — neither on
    /// screen nor, since `progress` is cleared when the call is answered, in
    /// the result the model reads. Marked with `❯ ` so a reader can tell the
    /// human's line from the program's output around it.
    pub fn console_echo(&mut self, line: &str) {
        self.group();
        self.append_progress(&format!("❯ {line}\n"));
    }

    /// Append live command output to the newest still-open tool row.
    ///
    /// Newest rather than oldest, which is the opposite of how a *result* is
    /// paired: a result answers the call that has been waiting longest, but
    /// output belongs to the call that is running, and dispatch is sequential,
    /// so the row that opened last is the one producing bytes. A chunk with no
    /// open row (a console that outlived its call, or a replayed stream that
    /// never had one) is dropped rather than inventing a row for it — there is
    /// no honest place to put output from a call nobody recorded.
    fn append_progress(&mut self, chunk: &str) {
        let items = &self.items;
        let Some(row) = self.open.iter().rev().copied().find(
            |row| matches!(&items[*row], TranscriptItem::Tool(item) if item.output.is_none()),
        ) else {
            return;
        };
        self.record(Change::Mutated(row));
        if let TranscriptItem::Tool(item) = &mut self.items[row] {
            item.progress.push_str(chunk);
            // A command can produce output without bound (`yes`, a chatty
            // build) and this buffer is live, so it needs one. The **tail** is
            // what survives, which is the opposite of what a finished result
            // wants and the reason `truncate_output` cannot be reused: the line
            // a person has to answer is the last one, never the first.
            if item.progress.len() > PROGRESS_TAIL_BYTES {
                let cut = item.progress.len() - PROGRESS_TAIL_BYTES;
                let cut = (cut..item.progress.len())
                    .find(|at| item.progress.is_char_boundary(*at))
                    .unwrap_or(item.progress.len());
                // Open on a line boundary when one is close, so the kept tail
                // does not start mid-word.
                let tail = &item.progress[cut..];
                let tail = match tail.find('\n') {
                    Some(at) if at < PROGRESS_TAIL_BYTES / 8 => &tail[at + 1..],
                    _ => tail,
                };
                item.progress = tail.to_string();
            }
        }
    }

    /// Take the oldest open tool row whose item satisfies `wanted`, removing it
    /// from the open list.
    fn take_open(&mut self, wanted: impl Fn(&ToolItem) -> bool) -> Option<usize> {
        let items = &self.items;
        let at = self.open.iter().position(|row| match &items[*row] {
            TranscriptItem::Tool(item) => item.output.is_none() && wanted(item),
            _ => false,
        })?;
        self.open.remove(at)
    }

    /// The most recent answered row for tool `name`: where that tool's images
    /// belong. Searching the items rather than keeping a "last answered"
    /// pointer is what lets a batch's second image carrier find its own row
    /// instead of the one the first carrier already consumed.
    fn last_answered_row(&self, name: &str) -> Option<usize> {
        self.items.iter().rposition(|item| {
            matches!(item, TranscriptItem::Tool(tool) if tool.name == name && tool.output.is_some())
        })
    }
}

// ---------------------------------------------------------------------------
// Readings of raw text
// ---------------------------------------------------------------------------

/// The tool named by a tool-image carrier message, if this text is one.
///
/// The agent writes `` Image(s) returned by `read_file`: `` and nothing else
/// does; anything that does not have that exact shape (including a person who
/// types the words and attaches a screenshot without the backticks) is an
/// ordinary prompt.
fn carrier_tool(text: &str) -> Option<&str> {
    let rest = text.strip_prefix(TOOL_IMAGE_NOTE)?;
    let name = rest.split('`').next()?;
    (!name.is_empty()).then_some(name)
}

/// Whether a replayed tool result was a failure.
///
/// [`ToolOutput::is_error`](crate::tools::ToolOutput) is not persisted, so this
/// recognizes the dispatcher's own failure phrasings and calls anything else a
/// success. It is a guess, but it is a far better one than the alternative the
/// TUI used to make: hardcoding success meant every ✗ in a resumed conversation
/// came back as a ✓, so scrolling back could not tell you which calls had
/// failed, which is precisely what you scroll back to find out.
pub fn looks_like_failure(content: &str) -> bool {
    let head = content.trim_start();
    head.starts_with("error")
        || head.starts_with("Error")
        || head.starts_with("unknown tool:")
        || head.starts_with("invalid arguments")
        || head.starts_with("blocked by")
        || head.starts_with("(not executed")
}

/// A message's images as the references a surface renders from.
///
/// The image store tagged each one with where it wrote it
/// ([`ImageStore::save_all`](crate::images::ImageStore::save_all)), so a
/// transcript replayed from disk names the same files the live
/// [`AgentEvent::Images`] frames did, and the byte count comes off the base64
/// rather than a stat of every file. An image that never landed on disk carries
/// no path and nothing to fetch, so it is not announced at all.
fn image_refs(images: &[&Image]) -> Vec<ImageRef> {
    images
        .iter()
        .filter_map(|image| {
            Some(ImageRef {
                path: image.path.clone()?,
                mime: image.mime.clone(),
                bytes: image.decoded_len(),
            })
        })
        .collect()
}

/// One short human line describing a finished tool call: file paths and counts
/// where the tool has an obvious subject, otherwise the first line of its
/// output. Shared by the GUI's live `tool_finished` frames and by every
/// transcript replay.
pub fn summarize_tool(name: &str, args: &Value, output: &str) -> String {
    let arg = |key: &str| args.get(key).and_then(Value::as_str).map(str::trim);
    let first_line = output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let lines = output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();

    let summary = match name {
        "read_file" => {
            arg("path").map(|path| format!("{path} ({})", count(lines, "line", "lines")))
        }
        "write_file" | "edit_file" => arg("path").map(str::to_string),
        "list_files" | "search_files" => {
            let counted = if name == "list_files" {
                count(lines, "file", "files")
            } else {
                count(lines, "match", "matches")
            };
            Some(match arg("pattern").or_else(|| arg("path")) {
                Some(subject) => format!("{subject}: {counted}"),
                None => counted,
            })
        }
        "execute" => arg("command").map(|command| first_of(command).to_string()),
        "web_fetch" => arg("url").map(str::to_string),
        "web_search" | "x_search" => {
            arg("query").map(|query| format!("{query}: {}", count(lines, "result", "results")))
        }
        // The parameter is `subagent`, not `name`: see
        // [`SpawnSubagentTool`](crate::agent::subagent::SpawnSubagentTool)'s
        // schema. Reading the wrong key here fell through to `task`, so the
        // GUI's card said which tool ran but never which subagent, and on a
        // spawn with no task text it fell through again to the output line.
        "spawn_subagent" => arg("subagent")
            .or_else(|| arg("task"))
            .map(|subject| first_of(subject).to_string()),
        _ => None,
    };

    let summary = summary.unwrap_or_else(|| {
        if first_line.is_empty() {
            "(no output)".to_string()
        } else {
            first_line.to_string()
        }
    });
    truncate_chars(&summary, SUMMARY_CHARS)
}

/// `3 lines`, `1 file`, and so on: a count with the right noun form.
fn count(n: usize, singular: &str, plural: &str) -> String {
    if n == 1 {
        format!("1 {singular}")
    } else {
        format!("{n} {plural}")
    }
}

/// First line of `text`, trimmed.
fn first_of(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}

/// Clip to `max` characters with an ellipsis.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut clipped: String = text.chars().take(max.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::agent::DoneReason;
    use crate::agent::session::{SessionRecord, TurnMarker};
    use crate::llm::{FunctionCall, ToolCall};
    use crate::tools::ToolOutput;

    fn record(message: ChatMessage) -> SessionEntry {
        SessionEntry::Message(SessionRecord {
            timestamp: chrono::Utc::now(),
            message,
            system_note: false,
        })
    }

    fn png(path: &str) -> Image {
        Image::new("aGk=", "image/png").at_path(path.into())
    }

    fn image_ref(path: &str) -> ImageRef {
        ImageRef {
            path: path.into(),
            mime: "image/png".to_string(),
            bytes: 2,
        }
    }

    /// A model with one `execute` call open, ready to be fed console output.
    fn running_command() -> TranscriptModel {
        let mut model = TranscriptModel::new();
        model.apply(&AgentEvent::ToolStarted {
            name: "execute".to_string(),
            args: json!({ "command": "npm init" }),
        });
        model
    }

    /// The one open tool row, or a panic naming what was there instead.
    fn only_tool(model: &TranscriptModel) -> &ToolItem {
        match model
            .items()
            .iter()
            .find(|item| matches!(item, TranscriptItem::Tool(_)))
        {
            Some(TranscriptItem::Tool(tool)) => tool,
            other => panic!("expected one tool row, got {other:?}"),
        }
    }

    /// A prompt has to be readable *while* the call is still open — that is
    /// the whole point — and it has to land on the call that is running rather
    /// than filling the slot its result is going to want.
    #[test]
    fn console_output_shows_on_the_running_call_without_answering_it() {
        let mut model = running_command();
        model.apply(&AgentEvent::ConsoleOutput {
            gate: serde_json::from_str("1").expect("a ticket is a number"),
            chunk: "package name: ".to_string(),
        });

        let tool = only_tool(&model);
        assert_eq!(tool.progress, "package name: ");
        assert!(
            tool.output.is_none(),
            "live output must not count as the call's result"
        );
        assert_eq!(model.last_change(), Change::Mutated(0));
    }

    /// And once the result lands, the live tail goes: the result is the same
    /// bytes with the exit status folded in, so keeping both would print the
    /// command's output twice.
    #[test]
    fn a_result_replaces_the_live_tail_it_streamed() {
        let mut model = running_command();
        model.apply(&AgentEvent::ConsoleOutput {
            gate: serde_json::from_str("1").expect("a ticket"),
            chunk: "package name: ".to_string(),
        });
        model.apply(&AgentEvent::ToolFinished {
            name: "execute".to_string(),
            output: ToolOutput::ok("package name: wizard\ndone"),
        });

        let tool = only_tool(&model);
        assert!(tool.progress.is_empty(), "the tail was superseded");
        assert_eq!(
            tool.output.as_ref().expect("answered").content,
            "package name: wizard\ndone"
        );
    }

    /// What the user typed has to appear somewhere. A pipe does not echo the
    /// way a tty does, so without this the answer that unblocked the command
    /// leaves no trace in the conversation at all.
    #[test]
    fn a_typed_answer_is_echoed_into_the_command() {
        let mut model = running_command();
        model.apply(&AgentEvent::ConsoleOutput {
            gate: serde_json::from_str("1").expect("a ticket"),
            chunk: "name: ".to_string(),
        });
        model.console_echo("wizard");
        assert_eq!(only_tool(&model).progress, "name: ❯ wizard\n");
    }

    /// A live buffer needs a bound, and it has to keep the *tail*: the line a
    /// person must answer is the last one.
    #[test]
    fn live_output_keeps_the_tail_not_the_head() {
        let mut model = running_command();
        let gate = serde_json::from_str("1").expect("a ticket");
        model.apply(&AgentEvent::ConsoleOutput {
            gate,
            chunk: "x\n".repeat(PROGRESS_TAIL_BYTES),
        });
        model.apply(&AgentEvent::ConsoleOutput {
            gate,
            chunk: "continue? [Y/n] ".to_string(),
        });

        let tool = only_tool(&model);
        assert!(tool.progress.len() <= PROGRESS_TAIL_BYTES + 16, "bounded");
        assert!(
            tool.progress.ends_with("continue? [Y/n] "),
            "the question survives the cap"
        );
    }

    /// Output with nothing open is dropped rather than inventing a row: a
    /// replayed stream has no running command to attach it to.
    #[test]
    fn console_output_with_no_open_call_is_dropped() {
        let mut model = TranscriptModel::new();
        model.apply(&AgentEvent::ConsoleOutput {
            gate: serde_json::from_str("1").expect("a ticket"),
            chunk: "orphan".to_string(),
        });
        assert!(model.items().is_empty(), "{:?}", model.items());
    }

    /// The acceptance bar for this workstream: one conversation, entered by
    /// both doors, has to come out as the same conversation.
    ///
    /// The fixture is a full turn with everything that made the two readings
    /// disagree in it: a prompt, narration, a parallel batch of two calls to
    /// the *same* tool (which name-matching gets backwards), a tool that
    /// returned an image (which has to land on its own row rather than at the
    /// end), and a system note (which one surface used to drop outright).
    #[test]
    fn a_live_turn_and_its_replay_agree() {
        let call = |name: &str, id: &str, args: Value| ToolCall {
            id: id.to_string(),
            function: FunctionCall {
                name: name.to_string(),
                arguments: args,
            },
        };

        let mut assistant = ChatMessage::assistant("reading both, then drawing");
        assistant.push_tool_call(call("read_file", "toolu_a", json!({ "path": "a.rs" })));
        assistant.push_tool_call(call("read_file", "toolu_b", json!({ "path": "b.rs" })));
        assistant.push_tool_call(call("render", "toolu_c", json!({ "shape": "hat" })));

        // The provider answered out of call order, which is the case only the
        // id can get right.
        let mut results = ChatMessage::tool_result("toolu_b", "read_file", "body b");
        results.push_tool_result("toolu_a", "read_file", "body a");
        results.push_tool_result("toolu_c", "render", "rendered");

        let replayed = TranscriptModel::seed(&[
            SessionEntry::Marker(TurnMarker {
                timestamp: chrono::Utc::now(),
                turn: 7,
                prompt: "read both and draw".to_string(),
            }),
            record(ChatMessage::user("read both and draw")),
            record(assistant),
            record(results),
            // The images `render` returned, riding back to the model.
            record(ChatMessage::user_with_images(
                "Image(s) returned by `render`:",
                vec![png("/img/hat.png")],
            )),
            record(ChatMessage::system("[note] background task #1 finished")),
        ]);

        let mut live = TranscriptModel::new();
        live.turn_marker(7, "read both and draw".to_string());
        live.user("read both and draw".to_string(), Vec::new());
        for delta in ["reading both, ", "then drawing"] {
            live.apply(&AgentEvent::TextDelta(delta.to_string()));
        }
        for (name, args, body) in [
            ("read_file", json!({ "path": "a.rs" }), "body a"),
            ("read_file", json!({ "path": "b.rs" }), "body b"),
            ("render", json!({ "shape": "hat" }), "rendered"),
        ] {
            live.apply(&AgentEvent::ToolStarted {
                name: name.to_string(),
                args,
            });
            live.apply(&AgentEvent::ToolFinished {
                name: name.to_string(),
                output: ToolOutput::ok(body),
            });
        }
        live.apply(&AgentEvent::Images {
            source: ImageSource::Tool("render".to_string()),
            images: vec![image_ref("/img/hat.png")],
        });
        live.apply(&AgentEvent::Notice(
            "[note] background task #1 finished".to_string(),
        ));
        live.apply(&AgentEvent::Done {
            reason: DoneReason::Completed,
        });

        // The call ids are the one thing only the stored session has: the live
        // events carry none, and comparing them would be comparing the two
        // sources rather than the two readings.
        let strip = |model: TranscriptModel| -> Vec<TranscriptItem> {
            model
                .into_items()
                .into_iter()
                .map(|item| match item {
                    TranscriptItem::Tool(tool) => TranscriptItem::Tool(ToolItem {
                        call_id: String::new(),
                        ..tool
                    }),
                    other => other,
                })
                .collect()
        };
        assert_eq!(strip(replayed), strip(live));
    }

    /// The pairing the id exists for. Two calls to one tool, answered in the
    /// other order: matching by name or by arrival order swaps the bodies, and
    /// the transcript then shows each file's contents under the other file's
    /// path.
    #[test]
    fn results_pair_by_call_id_not_by_name_or_order() {
        let mut assistant = ChatMessage::assistant("");
        for (id, path) in [("toolu_a", "a.rs"), ("toolu_b", "b.rs")] {
            assistant.push_tool_call(ToolCall {
                id: id.to_string(),
                function: FunctionCall {
                    name: "read_file".to_string(),
                    arguments: json!({ "path": path }),
                },
            });
        }
        let mut results = ChatMessage::tool_result("toolu_b", "read_file", "body b");
        results.push_tool_result("toolu_a", "read_file", "body a");

        let items = TranscriptModel::seed(&[record(assistant), record(results)]).into_items();
        let bodies: Vec<(&str, &str)> = items
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::Tool(tool) => Some((
                    tool.args["path"].as_str().expect("the call's path"),
                    tool.output.as_ref().expect("answered").content.as_str(),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(bodies, [("a.rs", "body a"), ("b.rs", "body b")]);
    }

    /// A pre-v2 file carries no ids anywhere, and its ordering is the whole of
    /// the correlation it has. `Session::entries` and `Session::load_messages`
    /// (unlike `load_history`) do not mint ids on the way out, so this is the
    /// shape both replay surfaces actually receive for an old session.
    #[test]
    fn a_file_without_ids_still_pairs_in_order() {
        let bare = |name: &str, args: Value| ToolCall {
            id: String::new(),
            function: FunctionCall {
                name: name.to_string(),
                arguments: args,
            },
        };
        let mut assistant = ChatMessage::assistant("on it");
        assistant.push_tool_call(bare("read_file", json!({ "path": "a.rs" })));
        assistant.push_tool_call(bare("execute", json!({ "command": "ls" })));
        let mut results = ChatMessage::new(Role::Tool, Vec::new());
        results.push_tool_result("", "read_file", "body a");
        results.push_tool_result("", "execute", "listing");

        let items = TranscriptModel::seed(&[record(assistant), record(results)]).into_items();
        let paired: Vec<(&str, &str)> = items
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::Tool(tool) => Some((
                    tool.name.as_str(),
                    tool.output.as_ref().expect("answered").content.as_str(),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(paired, [("read_file", "body a"), ("execute", "listing")]);
    }

    /// One batch, two tools that each returned an image. The agent writes one
    /// carrier message per tool, back to back, and only the tool named in the
    /// note says which row each one belongs on.
    #[test]
    fn each_carrier_lands_on_its_own_tool_row() {
        let mut assistant = ChatMessage::assistant("");
        for (id, name) in [("toolu_a", "render"), ("toolu_b", "screenshot")] {
            assistant.push_tool_call(ToolCall {
                id: id.to_string(),
                function: FunctionCall {
                    name: name.to_string(),
                    arguments: json!({}),
                },
            });
        }
        let mut results = ChatMessage::tool_result("toolu_a", "render", "drew it");
        results.push_tool_result("toolu_b", "screenshot", "shot it");

        let items = TranscriptModel::seed(&[
            record(assistant),
            record(results),
            record(ChatMessage::user_with_images(
                "Image(s) returned by `render`:",
                vec![png("/img/hat.png")],
            )),
            record(ChatMessage::user_with_images(
                "Image(s) returned by `screenshot`:",
                vec![png("/img/shot.png")],
            )),
        ])
        .into_items();

        // render's row, render's image, screenshot's row, screenshot's image.
        assert_eq!(items.len(), 4, "{items:?}");
        let placed: Vec<String> = items
            .iter()
            .map(|item| match item {
                TranscriptItem::Tool(tool) => tool.name.clone(),
                TranscriptItem::Images { source, .. } => {
                    format!("image from {}", source.tool().unwrap_or("?"))
                }
                other => panic!("unexpected item {other:?}"),
            })
            .collect();
        assert_eq!(
            placed,
            [
                "render",
                "image from render",
                "screenshot",
                "image from screenshot"
            ]
        );
    }

    /// A person's attachment after a tool answered is their attachment, not
    /// that tool's output. Only the agent's own carrier note claims a row.
    #[test]
    fn a_prompt_with_an_image_is_not_a_tools_image() {
        let items = TranscriptModel::seed(&[
            record(ChatMessage::tool_result("toolu_a", "render", "drew it")),
            record(ChatMessage::user_with_images(
                "and this?",
                vec![png("/img/shot.png")],
            )),
        ])
        .into_items();
        assert!(
            matches!(&items[1], TranscriptItem::User { text, images } if text == "and this?" && images.len() == 1),
            "{items:?}"
        );
        assert!(
            !items
                .iter()
                .any(|item| matches!(item, TranscriptItem::Images { .. })),
            "it did not land on the tool's row: {items:?}"
        );
    }

    /// The TUI used to drop every system message on reload, so a resumed
    /// conversation lost the background-task and subagent reports the user had
    /// watched arrive. Hook *context* stays dropped: it is a payload for the
    /// model, and the hook has a one-line notice of its own.
    #[test]
    fn system_notes_replay_as_notices_but_hook_context_does_not() {
        let items = TranscriptModel::seed(&[
            record(ChatMessage::system("[note] background task #1 finished")),
            record(ChatMessage::system(format!(
                "{SESSION_START_HOOK_NOTE}\n{}",
                "a wall of repo context written for the model"
            ))),
        ])
        .into_items();
        assert_eq!(items.len(), 1, "{items:?}");
        assert!(
            matches!(&items[0], TranscriptItem::Notice(text) if text.starts_with("[note]")),
            "{items:?}"
        );
    }

    /// The session file does not record the failure flag, so replay reads the
    /// dispatcher's phrasings back out of the text.
    ///
    /// The interrupted-call placeholder is taken from the constant that writes
    /// it rather than copied, so rewording it fails here instead of quietly
    /// turning every interrupted call in every resumed conversation green.
    #[test]
    fn replay_flags_dispatcher_failures() {
        for (content, failed) in [
            ("invalid arguments for 'execute': missing field", true),
            ("unknown tool: nope", true),
            ("blocked by pre_tool_use hook", true),
            (crate::agent::session::INTERRUPTED_TOOL_RESULT, true),
            ("Error: no such file", true),
            ("On branch main", false),
        ] {
            let items = TranscriptModel::seed(&[record(ChatMessage::tool_result(
                "toolu_a", "execute", content,
            ))])
            .into_items();
            match &items[0] {
                TranscriptItem::Tool(tool) => assert_eq!(
                    tool.output.as_ref().expect("answered").is_error,
                    failed,
                    "{content}"
                ),
                other => panic!("expected a tool row, got {other:?}"),
            }
        }
    }

    /// Reasoning is live-only: a session file keeps thinking blocks, but no
    /// surface has ever rendered them on reload. See the module docs.
    #[test]
    fn seed_drops_reasoning() {
        let message = ChatMessage::new(
            Role::Assistant,
            vec![
                crate::llm::ContentBlock::thinking("the user wants x", None),
                crate::llm::ContentBlock::text("here is x"),
            ],
        );
        let items = TranscriptModel::seed(&[record(message)]).into_items();
        assert_eq!(items, [TranscriptItem::Text("here is x".to_string())]);

        let mut live = TranscriptModel::new();
        live.apply(&AgentEvent::ThinkingDelta("the user wants x".to_string()));
        live.apply(&AgentEvent::TextDelta("here is x".to_string()));
        live.apply(&AgentEvent::Done {
            reason: DoneReason::Completed,
        });
        assert_eq!(
            live.into_items(),
            [
                TranscriptItem::Thinking("the user wants x".to_string()),
                TranscriptItem::Text("here is x".to_string()),
            ]
        );
    }

    /// A retry re-generates the whole partial completion, so the buffer has to
    /// be dropped rather than committed: keeping it would print the answer
    /// twice.
    #[test]
    fn a_stream_retry_discards_the_partial_answer() {
        let mut live = TranscriptModel::new();
        live.apply(&AgentEvent::TextDelta("half an ans".to_string()));
        live.apply(&AgentEvent::StreamRetrying);
        live.apply(&AgentEvent::TextDelta("the full answer".to_string()));
        live.apply(&AgentEvent::Done {
            reason: DoneReason::Completed,
        });
        assert_eq!(
            live.into_items(),
            [TranscriptItem::Text("the full answer".to_string())]
        );
    }

    /// A result with no open row to fill (a truncated file, or a tool denied
    /// before its start was announced) gets a row rather than vanishing.
    #[test]
    fn an_orphan_result_still_gets_a_row() {
        let items = TranscriptModel::seed(&[record(ChatMessage::tool_result(
            "toolu_a",
            "read_file",
            "body",
        ))])
        .into_items();
        assert!(
            matches!(&items[0], TranscriptItem::Tool(tool)
                if tool.name == "read_file" && tool.args.is_null() && tool.output.is_some()),
            "{items:?}"
        );
    }

    /// A call the run never answered replays as still pending, which is what
    /// an interrupted turn actually looked like.
    #[test]
    fn a_dangling_call_replays_without_a_result() {
        let mut assistant = ChatMessage::assistant("");
        assistant.push_tool_call(ToolCall::new("read_file", json!({ "path": "a.rs" })));
        assistant.push_tool_call(ToolCall::new("execute", json!({ "command": "ls" })));
        let items = TranscriptModel::seed(&[
            record(assistant),
            record(ChatMessage::tool_result("", "read_file", "body a")),
        ])
        .into_items();
        match (&items[0], &items[1]) {
            (TranscriptItem::Tool(first), TranscriptItem::Tool(second)) => {
                assert!(first.output.is_some());
                assert_eq!(second.name, "execute");
                assert!(second.output.is_none(), "{items:?}");
            }
            other => panic!("expected two tool rows, got {other:?}"),
        }
    }

    #[test]
    fn only_the_images_that_landed_on_disk_are_announced() {
        let mut assistant = ChatMessage::assistant("two, one of which never landed");
        assistant.push_image(png("/img/a.png"));
        assistant.push_image(Image::new("aGk=", "image/png"));
        let items = TranscriptModel::seed(&[record(assistant)]).into_items();
        match &items[1] {
            TranscriptItem::Images { images, .. } => assert_eq!(
                images.len(),
                1,
                "an image with no file has nothing to fetch: {images:?}"
            ),
            other => panic!("expected an images item, got {other:?}"),
        }
    }

    #[test]
    fn a_message_that_says_nothing_is_not_a_prompt() {
        let items = TranscriptModel::seed(&[
            record(ChatMessage::user("   ")),
            record(ChatMessage::user("real")),
        ])
        .into_items();
        assert_eq!(
            items,
            [TranscriptItem::User {
                text: "real".to_string(),
                images: Vec::new(),
            }]
        );
    }

    /// `spawn_subagent`'s parameter is `subagent`. The GUI read `name`, which
    /// never matched, so its card fell through to the task text and, for a
    /// background spawn, to the acknowledgement line: it never said which
    /// subagent was running.
    #[test]
    fn the_subagent_summary_names_the_subagent() {
        assert_eq!(
            summarize_tool(
                "spawn_subagent",
                &json!({ "subagent": "researcher", "task": "read the docs" }),
                "started subagent #3 'researcher' in the background"
            ),
            "researcher"
        );
        // With no subagent named at all (an old transcript, a malformed call),
        // the task is still a better line than the acknowledgement.
        assert_eq!(
            summarize_tool(
                "spawn_subagent",
                &json!({ "task": "read the docs" }),
                "started subagent #3"
            ),
            "read the docs"
        );
    }

    #[test]
    fn summaries_name_the_subject_and_count_output() {
        assert_eq!(
            summarize_tool("read_file", &json!({ "path": "src/app.rs" }), "a\nb\nc"),
            "src/app.rs (3 lines)"
        );
        assert_eq!(
            summarize_tool(
                "write_file",
                &json!({ "path": "src/gui/mod.rs" }),
                "wrote it"
            ),
            "src/gui/mod.rs"
        );
        assert_eq!(
            summarize_tool(
                "execute",
                &json!({ "command": "git status --short\n# extra" }),
                "clean"
            ),
            "git status --short"
        );
        assert_eq!(
            summarize_tool(
                "search_files",
                &json!({ "pattern": "TODO" }),
                "a.rs:1\nb.rs:9"
            ),
            "TODO: 2 matches"
        );
        assert_eq!(
            summarize_tool(
                "git_status",
                &json!({}),
                "On branch main\nnothing to commit"
            ),
            "On branch main"
        );
        assert_eq!(summarize_tool("todo", &json!({}), ""), "(no output)");
    }

    #[test]
    fn summaries_are_clipped() {
        let long = "x".repeat(400);
        let summary = summarize_tool("execute", &json!({ "command": long }), "");
        assert_eq!(summary.chars().count(), SUMMARY_CHARS);
        assert!(summary.ends_with('…'));
    }

    /// The hazard the change signal exists for.
    ///
    /// A tool's images are spliced in behind the row that produced them, not
    /// appended, so every item below moves down one. A consumer caching a
    /// widget per index would keep the stale mapping if this were reported as
    /// an append — the notice below the splice would redraw as the image.
    ///
    /// Deliberately built one message at a time rather than through `seed`,
    /// which reports `Reset` for the whole conversation: what is under test is
    /// the individual fold, and the last row is checked to be sure the splice
    /// actually landed mid-vector rather than at the end (where `Inserted` and
    /// `Appended` would be indistinguishable).
    #[test]
    fn insert_reports_a_mid_vector_change() {
        let mut model = TranscriptModel::new();
        model.fold_message(&ChatMessage::tool_result("toolu_a", "render", "drew it"));
        assert!(matches!(model.last_change(), Change::Appended(0)));
        model.fold_message(&ChatMessage::system("[note] and then this happened"));
        assert!(matches!(model.last_change(), Change::Appended(1)));

        let before = model.revision();
        model.fold_message(&ChatMessage::user_with_images(
            "Image(s) returned by `render`:",
            vec![png("/img/hat.png")],
        ));
        assert_eq!(
            model.last_change(),
            Change::Inserted(1),
            "the carrier lands behind render's row, not at the end: {:?}",
            model.items()
        );
        assert!(model.revision() > before, "an insert is a mutation");
        // Not vacuous: the notice really did move down, so an append-only
        // consumer would now be drawing the image where the notice is.
        assert!(
            matches!(model.items()[2], TranscriptItem::Notice(_)),
            "{:?}",
            model.items()
        );
    }

    /// Every mutating door bumps the revision and names what it did, including
    /// the ones that touch only the uncommitted tail — a renderer that
    /// repainted on item changes alone would freeze mid-reply.
    #[test]
    fn every_mutation_is_announced() {
        let mut model = TranscriptModel::new();
        assert_eq!(model.revision(), 0);
        assert_eq!(model.last_change(), Change::Reset);

        let mut steps: Vec<(u64, Change)> = Vec::new();
        let step = |model: &TranscriptModel| (model.revision(), model.last_change());

        model.apply(&AgentEvent::TextDelta("half".to_string()));
        steps.push(step(&model));
        model.apply(&AgentEvent::ToolStarted {
            name: "read_file".to_string(),
            args: json!({ "path": "a.rs" }),
        });
        steps.push(step(&model));
        model.apply(&AgentEvent::ToolFinished {
            name: "read_file".to_string(),
            output: ToolOutput::ok("body"),
        });
        steps.push(step(&model));
        model.notice("a remark".to_string());
        steps.push(step(&model));
        model.clear();
        steps.push(step(&model));

        assert_eq!(
            steps,
            [
                // The delta is tail-only.
                (1, Change::Streaming),
                // Committing "half" and opening the row is one call: it
                // reports the earlier of the two indices it appended.
                (3, Change::Appended(0)),
                // The result fills the row that was already there.
                (4, Change::Mutated(1)),
                (5, Change::Appended(2)),
                (6, Change::Reset),
            ]
        );
        assert!(model.items().is_empty());
    }

    /// A clone is a snapshot, not a view: the GUI renders one off the main
    /// loop, and a later live event must not reach into it.
    #[test]
    fn a_clone_stops_tracking_the_original() {
        let mut model = TranscriptModel::new();
        model.apply(&AgentEvent::Notice("first".to_string()));
        let snapshot = model.clone();
        model.apply(&AgentEvent::Notice("second".to_string()));

        assert_eq!(snapshot.items().len(), 1);
        assert_eq!(model.items().len(), 2);
        assert_eq!(snapshot.revision(), 1);
        assert_eq!(model.revision(), 2);
    }

    #[test]
    fn a_carrier_note_is_recognized_only_in_the_agents_own_shape() {
        assert_eq!(
            carrier_tool("Image(s) returned by `render`:"),
            Some("render")
        );
        assert_eq!(carrier_tool("Image(s) returned by ``:"), None);
        assert_eq!(carrier_tool("Image(s) returned by render"), None);
        assert_eq!(carrier_tool("look at this"), None);
    }
}
