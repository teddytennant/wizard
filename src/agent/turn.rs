//! The one step loop, top to bottom: stream a completion, report it, execute
//! the tool calls it made, feed the results back — until the model stops
//! calling tools, or a step cap, the time limit, a circuit breaker or an
//! interrupt ends the run.
//!
//! # One loop, two kinds of run
//!
//! Both things in this process that talk to a model in a cycle run [`run`]: the
//! user's turn ([`Agent::run_turn`]) and every delegated sub-run
//! ([`subagent::spawn`](super::subagent::spawn) — `spawn_subagent`, `/fork`,
//! and each of `/ultra`'s candidates and judges). There used to be two loops,
//! and the second one was the first one as it had been some months earlier: no
//! compaction, no circuit breaker, no deadline, no way to interrupt it. Each of
//! those was found and fixed once, in the parent, and then found again in the
//! sub-loop — and a council fans N sub-runs out per turn, so every one of them
//! was worth N.
//!
//! What differs between the two is [`Policy`] — data, decided by the caller,
//! read by the loop — and the few operations that need whatever owns the
//! history, which is [`Host`]. Everything else, including the tool-execution
//! block ([`Dispatcher::dispatch`]), is the same code running twice. A
//! capability a sub-run does not want is a field on the policy set to `false`
//! with the reason written next to it, not a branch missing from a second copy
//! of the body; `steps_do_not_fork` in the tests below is what keeps it that
//! way.
//!
//! Only the loop and its immediate drivers live here. The machinery it calls —
//! prompt assembly, the compactor, the tool registry, usage recording — stays
//! in [`super`] and its siblings.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::dispatch::DispatchOutcome;
use crate::hooks::PromptSubmit;
use crate::images::ImageStore;
use crate::llm::provider::LlmProvider;
use crate::llm::{
    CacheTokens, ChatMessage, ChatOptions, ChatRequest, ContentBlock, Image, Role, ThinkingBlock,
    ToolCall, ToolResultBlock, ToolSpec, TruncatedToolCall, is_length_cutoff,
};
use crate::tools::{ToolContext, ToolOutput};

use super::{
    Agent, AgentEvent, CONTEXT_PRESSURE_HEADING, DoneReason, EMPTY_COMPLETION_NUDGE, ImageSource,
    LoopControl, PressureLevel, absorb_images, breaker, clear_loop_control, completion_is_empty,
    context, emit, parse_json_tool_call, read_loop_control, retry, ultra,
};

/// Where a running loop reports what it is doing.
///
/// Two shapes, and they are not interchangeable. A turn's progress *is* the
/// session: the user is watching it stream and the surface renders every
/// character. A sub-run's belongs to its own pane, keyed by a run id, and there
/// may be no pane at all (a background subagent on a surface with no rail) —
/// which is why the channel is optional on that arm and mandatory on this one.
///
/// Anything with no run-scoped shape (a finished background task, a retry
/// notice) is a turn event and nothing else. Putting the split here rather than
/// at each call site is what stops N candidates' backoff notices from landing
/// in the transcript of a turn that has not produced a word yet.
#[derive(Debug, Clone)]
pub(crate) enum Sink {
    /// The user's turn: every event goes on the channel the surface reads.
    Turn(mpsc::Sender<AgentEvent>),
    /// One delegated run, named for the log lines it writes instead of the
    /// events it has no shape for.
    Run {
        run: u64,
        name: String,
        events: Option<mpsc::Sender<AgentEvent>>,
    },
}

impl Sink {
    /// The channel, when there is one. For the two places that need it
    /// directly: [`absorb_images`], and the context a dispatched tool runs
    /// under.
    pub(crate) fn channel(&self) -> Option<&mpsc::Sender<AgentEvent>> {
        match self {
            Sink::Turn(events) => Some(events),
            Sink::Run { events, .. } => events.as_ref(),
        }
    }

    /// Emit a turn event, and report whether anyone is still listening.
    ///
    /// A turn whose receiver is gone has nowhere to put the rest of itself, so
    /// every caller of this treats `false` as the end of the run. A *run* whose
    /// pane is gone is in no such position: its answer travels back to the
    /// parent model as a tool result, which is not on this channel at all, so a
    /// closed pane costs a rendering and never a step.
    async fn emit(&self, event: AgentEvent) -> bool {
        match self {
            Sink::Turn(events) => emit(events, event).await,
            Sink::Run { events, .. } => {
                if let Some(events) = events {
                    emit(events, event).await;
                }
                true
            }
        }
    }

    /// An event only a turn produces (a finished background task, the context
    /// size after a compaction). A run reaches none of these — the policy that
    /// would have to be on for it to try is off — so there is nothing to
    /// translate.
    pub(super) async fn turn_event(&self, event: AgentEvent) -> bool {
        match self {
            Sink::Turn(events) => emit(events, event).await,
            Sink::Run { .. } => true,
        }
    }

    /// One chunk of the model's visible reply, as it arrives.
    async fn text_delta(&self, delta: String) -> bool {
        self.turn_event(AgentEvent::TextDelta(delta)).await
    }

    /// One chunk of the model's reasoning. Surfaced dimmed, never kept.
    async fn thinking_delta(&self, delta: String) -> bool {
        self.turn_event(AgentEvent::ThinkingDelta(delta)).await
    }

    /// The whole visible message a step produced, once it is complete.
    ///
    /// A run's pane gets its text this way rather than as deltas, and the loop
    /// only offers it for a step that went on to call tools. The *final*
    /// message of a run is its report, and that reaches the surface on
    /// `SubagentRunDone` and the parent model as the tool result; streaming it
    /// here as well would render a council's answer twice per candidate. A turn
    /// has already streamed every character of it, so this is where it does
    /// nothing.
    async fn step_text(&self, text: &str) -> bool {
        match self {
            Sink::Turn(_) => true,
            Sink::Run { run, .. } => {
                self.emit(AgentEvent::SubagentRunText {
                    run: *run,
                    text: text.to_string(),
                })
                .await
            }
        }
    }

    /// A tool call is about to run.
    pub(crate) async fn tool_started(&self, name: &str, args: &Value) -> bool {
        match self {
            Sink::Turn(_) => {
                self.emit(AgentEvent::ToolStarted {
                    name: name.to_string(),
                    args: args.clone(),
                })
                .await
            }
            Sink::Run { run, .. } => {
                self.emit(AgentEvent::SubagentRunToolStarted {
                    run: *run,
                    name: name.to_string(),
                    args: args.clone(),
                })
                .await
            }
        }
    }

    /// A tool call answered.
    pub(crate) async fn tool_finished(&self, name: &str, output: &ToolOutput) -> bool {
        match self {
            Sink::Turn(_) => {
                self.emit(AgentEvent::ToolFinished {
                    name: name.to_string(),
                    output: output.clone(),
                })
                .await
            }
            Sink::Run { run, .. } => {
                self.emit(AgentEvent::SubagentRunToolFinished {
                    run: *run,
                    name: name.to_string(),
                    output: output.clone(),
                })
                .await
            }
        }
    }

    /// One step is over.
    async fn step_completed(&self, step: u32) -> bool {
        match self {
            Sink::Turn(_) => self.emit(AgentEvent::StepCompleted { step }).await,
            Sink::Run { run, .. } => {
                self.emit(AgentEvent::SubagentRunStep { run: *run, step })
                    .await
            }
        }
    }

    /// Tokens this call was billed for. A run reports them on the same event a
    /// turn does — they bill the same parent, and the status bar has to add
    /// them up — rather than on a run-scoped one.
    pub(super) async fn usage(&self, prompt_tokens: u64, completion_tokens: u64) -> bool {
        self.emit(AgentEvent::Usage {
            prompt_tokens,
            completion_tokens,
        })
        .await
    }

    /// Something worth telling the user, which for a run is a log line: the
    /// only run-scoped shapes are the ones its pane renders, and a sentence
    /// injected among them would read as something the subagent said.
    pub(crate) async fn notice(&self, text: String) {
        match self {
            Sink::Turn(events) => {
                emit(events, AgentEvent::Notice(text)).await;
            }
            Sink::Run { name, .. } => tracing::info!("subagent '{name}': {text}"),
        }
    }

    /// Something that went wrong, on the same terms as [`Self::notice`].
    pub(crate) async fn error(&self, text: String) {
        match self {
            Sink::Turn(events) => {
                emit(events, AgentEvent::Error(text)).await;
            }
            Sink::Run { name, .. } => tracing::warn!("subagent '{name}': {text}"),
        }
    }

    /// Take custody of images this run produced and announce them where they
    /// belong — see [`absorb_images`], which does the keeping; this decides
    /// which event carries the news.
    async fn absorb(
        &self,
        images: Vec<Image>,
        store: Option<&Arc<ImageStore>>,
        source: ImageSource,
    ) -> Vec<Image> {
        match self {
            Sink::Turn(events) => {
                absorb_images(images, store, Some(events), |images| AgentEvent::Images {
                    source,
                    images,
                })
                .await
            }
            Sink::Run { run, events, .. } => {
                let run = *run;
                absorb_images(images, store, events.as_ref(), |images| {
                    AgentEvent::SubagentRunImages {
                        run,
                        source,
                        images,
                    }
                })
                .await
            }
        }
    }
}

/// Everything about a run the loop decides by reading rather than by asking:
/// what bounds it, what it dials, and which of the turn's gates it keeps.
///
/// This is where a sub-run's differences from a turn live, all of them, in one
/// readable place. Each `false` below is a capability declined on purpose and
/// for a reason that is written down beside it — which is the difference
/// between a sub-run that does not honour operator control and a sub-run that
/// *forgot* to, and the whole reason there is a policy here instead of a second
/// copy of the loop with a few blocks missing.
///
/// Deliberately not `Clone`-and-mutate: each constructor spells every field, so
/// a capability added to the loop cannot be silently absent from one of them.
pub(super) struct Policy {
    /// Last step number the loop will run (see
    /// [`StepBudget::last_step`](crate::config::StepBudget::last_step)).
    pub max_steps: u32,
    /// Wall-clock cap, checked between steps.
    pub deadline: Option<Instant>,
    /// The interrupt this run observes, between tool calls and inside the
    /// retry ladder's waits.
    pub cancel: Option<super::CancelHandle>,
    /// Model tag every request in this run carries.
    pub model: String,
    /// Whether the model has native tool calling; when it does not, the loop
    /// falls back to the prompt-based JSON protocol on both ends (no tool
    /// specs out, [`parse_json_tool_call`] on the way back, results as prose).
    pub native_tools: bool,
    pub temperature: f32,
    pub reasoning_effort: Option<String>,
    /// Circuit breaker over the endpoint. Shared, so an outage one run proves
    /// is one its siblings do not each have to prove again.
    pub breaker: breaker::LlmBreaker,
    /// Retries allowed per model call after the first attempt; `None` climbs
    /// for as long as the breaker permits.
    pub retry_budget: Option<u32>,
    /// Whether a tripped endpoint breaker is an outage this run waits out
    /// rather than one it ends on. See [`retry::Ladder::wait_out_outage`].
    pub wait_out_outage: bool,
    /// Whether a reply cut off mid tool call is re-asked once
    /// ([`recover_truncated`]) or ends the run.
    pub recover_truncation: bool,
    pub retry_base_secs: u64,
    pub retry_max_secs: u64,
    /// Serialized-history ceiling the pressure reading falls back to when the
    /// provider names no context window.
    pub byte_threshold: usize,
    /// Whether background tasks and subagents that finished are drained into
    /// history at the top of each step.
    pub background_drain: bool,
    /// Whether `.wizard/loop-control` is honoured between steps.
    pub operator_control: bool,
    /// Whether the ephemeral context-pressure note rides each completion.
    pub pressure_signal: bool,
}

impl Policy {
    /// The user's turn: every gate on.
    pub(super) fn turn(agent: &Agent) -> Self {
        Self {
            // Unlimited by default: the turn runs until the model stops
            // calling tools. Everything else here can still end it.
            max_steps: agent.config.max_steps.last_step(),
            deadline: agent.deadline,
            cancel: Some(agent.cancel.clone()),
            model: agent.model.clone(),
            native_tools: agent.native_tools,
            temperature: agent.mode.temperature(),
            reasoning_effort: agent
                .config
                .reasoning_effort
                .map(|effort| effort.as_str().to_string()),
            breaker: agent.llm_breaker.clone(),
            // In continuous mode the breaker is the *only* thing that shapes
            // the ladder, which is why the budget is `None` there.
            retry_budget: if agent.config.continuous {
                None
            } else {
                Some(RETRY_ATTEMPTS)
            },
            // A standing mission outlives any one outage: the breaker's job
            // there is to space the retries out, not to end the run. An
            // interactive turn does the opposite — somebody is waiting, and
            // being told the provider is down beats a silent quarter-hour.
            wait_out_outage: agent.config.continuous,
            // A turn is the last thing that will catch a cutoff: nothing above
            // it is positioned to try again, and in sovereign mode the failure
            // is the end of the run.
            recover_truncation: true,
            retry_base_secs: agent.config.retry_base_secs,
            retry_max_secs: agent.config.retry_max_secs,
            byte_threshold: agent.config.compact_threshold_bytes,
            background_drain: true,
            operator_control: agent.mode == crate::config::Mode::Sovereign,
            pressure_signal: true,
        }
    }

    /// One delegated sub-run, and everything it declines.
    ///
    /// `deadline` and `cancel` are `None` not because a sub-run has neither but
    /// because both are enforced *around* the loop rather than inside it:
    /// [`spawn`](super::subagent::spawn) races the whole thing against them, so
    /// a run parked inside a model call ends at once, where a between-steps
    /// check would not fire until the call returned. Observing them again in
    /// here would only reach the same conclusion a step later.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn sub_run(
        max_steps: u32,
        model: String,
        native_tools: bool,
        reasoning_effort: Option<String>,
        breaker: breaker::LlmBreaker,
        retry_base_secs: u64,
        retry_max_secs: u64,
        byte_threshold: usize,
    ) -> Self {
        Self {
            max_steps,
            deadline: None,
            cancel: None,
            model,
            native_tools,
            // A delegated run is autonomous by construction, whatever the
            // parent's mode is.
            temperature: crate::config::Mode::Sovereign.temperature(),
            reasoning_effort,
            breaker,
            retry_budget: Some(RETRY_ATTEMPTS),
            // Declined: a sub-run is one tool call of a turn that is itself
            // waiting, and it is raced against a deadline from the outside
            // ([`spawn`](super::subagent::spawn)). Parking it in a cooldown
            // would spend the parent's patience on a call the parent can no
            // longer see; the parent's own next model call is where the outage
            // gets waited out.
            wait_out_outage: false,
            // Declined: a sub-run's failure is not the end of anything.
            // `spawn` hands it back to the parent as the result of the
            // `spawn_subagent` call, and the parent — which has the whole
            // picture, and is where the recovery is on — decides whether to
            // ask again and how. Spending a second full prompt inside each of
            // `/ultra`'s N candidates to work around a cutoff the parent is
            // going to hear about regardless is N times the cost for a
            // judgement somebody better placed is about to make anyway.
            recover_truncation: false,
            retry_base_secs,
            retry_max_secs,
            byte_threshold,
            // Declined: the task and subagent registries in a sub-run's
            // context are the *parent's*. Draining them here would consume
            // notifications the parent has to inject into its own history and
            // persist to its own session, and a sub-run has neither.
            background_drain: false,
            // Declined: `.wizard/loop-control` is the operator's handle on the
            // session's sovereign run. A `skip` written for the parent would be
            // eaten by whichever of N concurrent sub-runs happened to look
            // first, and the parent would never see it.
            operator_control: false,
            // Declined: the note's whole content is advice to call `compact`,
            // and `compact` is parent-loop only — it is not in a sub-run's
            // scope, and a fork's denylist strips it outright. The compactor
            // still runs on every step (see [`Host::compact`]); what a sub-run
            // does without is being *told* about the pressure it cannot act on.
            pressure_signal: false,
        }
    }
}

/// Retries allowed after the first attempt at one model call, outside
/// continuous mode.
pub(super) const RETRY_ATTEMPTS: u32 = 6;

/// How long a completion stream may produce *nothing at all* before the loop
/// gives up on it and lets the retry ladder open a fresh one.
///
/// A run that is wedged and a run that has stopped look identical from the
/// outside, and the shape that produces the first is an HTTP response whose
/// body never ends: a proxy that dropped the connection without sending a FIN,
/// a load balancer that idled the socket out, a local runtime that died with
/// the socket still open. `stream.next()` on any of those never resolves and
/// never errors, so the turn parks there for as long as the process lives —
/// which is exactly what "sovereign mode randomly stops" looks like from a
/// terminal.
///
/// The HTTP layer already answers this for hosted endpoints — a cloud chat
/// client carries a five-minute `read_timeout` (see
/// [`client_read_timeout_for`](crate::llm)) — and deliberately does not for
/// local ones, because a large model prefilling a full context on CPU can
/// legitimately produce nothing for a long time and killing *that* would be a
/// regression traded for a fix. So this is not a second copy of the socket
/// timeout: it is the only one a BYOM run has, sitting one layer up where
/// every provider passes through the same code, including the ones that build
/// their own HTTP client.
///
/// Ten minutes, comfortably above the cloud read timeout so it never
/// second-guesses it, and above any prefill a local model has any business
/// taking. A timeout is raised as a transport failure, which is transient, so
/// the ladder redials rather than ending the run.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(600);

/// What one model call was billed for.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct StepUsage {
    pub prompt: Option<u64>,
    pub completion: Option<u64>,
    /// How the prompt split between the provider's cache and fresh input. A
    /// subset of `prompt`, so it is carried beside it rather than added to it.
    pub cache: CacheTokens,
}

impl StepUsage {
    /// Whether the backend reported anything at all.
    pub(super) fn reported(&self) -> bool {
        self.prompt.is_some() || self.completion.is_some()
    }
}

/// Whatever owns the history the loop is running over.
///
/// Every method here is an operation the loop genuinely cannot do for itself,
/// because it needs the owner: a turn persists each message to a session file
/// and a sub-run has none; a turn's compaction note is announced and persisted
/// and a sub-run's is a log line under a different anchor; a turn's tokens are
/// its own and a sub-run's are delegated. Everything a run merely *chooses* is
/// in [`Policy`] instead, which is why this trait has no `bool` on it.
#[async_trait]
pub(super) trait Host: Send {
    /// The endpoint this run dials.
    fn client(&self) -> &Arc<dyn LlmProvider>;

    /// The context tools run in. The loop reads the image store, the task
    /// registries and the project root off it.
    fn ctx(&self) -> &ToolContext;

    /// Tool specs advertised on each request (empty under the JSON protocol —
    /// the loop applies that rule, not the host).
    fn tool_specs(&self) -> Vec<ToolSpec>;

    fn history(&self) -> &[ChatMessage];

    /// The history for in-memory edits that must never be persisted: the
    /// pressure note and the empty-completion nudge, both pushed and popped
    /// within one step.
    fn history_mut(&mut self) -> &mut Vec<ChatMessage>;

    /// Append a message that is part of the record.
    fn push(&mut self, message: ChatMessage);

    /// The last prompt size the backend *reported*, which is what decides when
    /// this run compacts.
    fn last_prompt(&self) -> Option<u64>;

    /// Account for one model call. Takes `&self` because it is called from
    /// inside the retry ladder, which bills an attempt before judging it.
    async fn record_usage(&self, usage: &StepUsage, sink: &Sink);

    /// Cut the history down, unconditionally — the loop has already decided
    /// that it is time. The anchor, the note, and who is told all belong to
    /// the owner (see [`context::Anchor`]).
    async fn compact(&mut self, sink: &Sink);

    /// Run one tool call through [`Dispatcher::dispatch`] — the same pipeline
    /// for both hosts, under the [`Pipeline`](crate::dispatch) each was built
    /// with.
    async fn dispatch(&mut self, call: &ToolCall, sink: &Sink) -> DispatchOutcome;

    /// A call the owner answers itself instead of dispatching, or `None` to
    /// dispatch it. Exactly one tool needs this: `compact`, which mutates the
    /// history the loop is standing on.
    async fn intercept(&mut self, call: &ToolCall, sink: &Sink) -> Option<CallOutcome>;
}

/// What [`run`] produced.
pub(super) struct Ran {
    pub reason: DoneReason,
    /// Steps actually taken, which a sub-run reports to its parent.
    pub steps_used: u32,
    /// The last visible text the model produced, which is a sub-run's report.
    /// A turn's has already been streamed and persisted.
    pub last_text: String,
}

impl Ran {
    fn ended(reason: DoneReason, steps_used: u32, last_text: String) -> Self {
        Self {
            reason,
            steps_used,
            last_text,
        }
    }
}

/// **The loop.** Stream a completion, report it, run the tool calls it made,
/// feed the results back, repeat.
///
/// There is exactly one of these in the tree and the test
/// `steps_do_not_fork` fails if a second appears. See the module header for
/// why that matters more than it sounds like it should.
pub(super) async fn run(host: &mut impl Host, policy: &Policy, sink: &Sink) -> Result<Ran> {
    let mut steps_used = 0;
    let mut last_text = String::new();

    for step in 1..=policy.max_steps {
        steps_used = step;

        // Surface background work that finished since the last step.
        if policy.background_drain {
            drain_background(host, sink).await;
        }
        if let Some(deadline) = policy.deadline
            && Instant::now() >= deadline
        {
            return Ok(Ran::ended(DoneReason::TimeLimit, steps_used, last_text));
        }
        if policy.operator_control
            && let Some(reason) = honor_loop_control(host, policy).await
        {
            return Ok(Ran::ended(reason, steps_used, last_text));
        }

        // One reading of how full the next call's prompt is, and both things
        // that are done about it. Taken before the request rather than after
        // the step, so a reading from the previous call is acted on before the
        // next one is billed — and so a `/fork` that inherited an already-full
        // parent history is cut before it sends anything.
        let mut reading = measure(host, policy).await;
        if reading.level == PressureLevel::Critical {
            host.compact(sink).await;
            reading = measure(host, policy).await;
        }
        let signal = attach_pressure(host, policy, &reading);

        let completion = completion(host, policy, sink).await;
        detach_pressure(host, signal);

        let streamed = match completion {
            // Cancelled mid-stream: the partial completion is discarded (it
            // never entered history), so nothing dangles.
            Ok(retry::Climbed::Cancelled) => {
                return Ok(Ran::ended(DoneReason::Stopped, steps_used, last_text));
            }
            Ok(retry::Climbed::Done(streamed)) => streamed,
            // A reply cut off mid tool call: recover once rather than ending
            // the run, where the policy says this run is the one that catches
            // it. See [`recover_truncated`].
            Err(err) => {
                let truncated = match err.downcast::<TruncatedToolCall>() {
                    Ok(truncated) if policy.recover_truncation => truncated,
                    // A run whose parent will catch this, and every other
                    // error, come back out as they arrived.
                    Ok(truncated) => return Err(truncated.into()),
                    Err(err) => return Err(err),
                };
                match recover_truncated(host, policy, sink, &truncated).await? {
                    Some(retried) => retried,
                    None => {
                        return Ok(Ran::ended(DoneReason::Stopped, steps_used, last_text));
                    }
                }
            }
        };

        // Some reasoning models (xAI grok-4.3 after tool results) emit only
        // reasoning and stop, leaving the visible message empty. Nudge once;
        // if it stays empty, say so rather than ending silently. A reply that
        // produced an image but no text is not empty — it said what it had to
        // say in pixels.
        let streamed = if streamed.images.is_empty()
            && completion_is_empty(&streamed.content, &streamed.tool_calls)
        {
            match nudge_once(host, policy, sink).await? {
                Some(retried) => retried,
                None => {
                    return Ok(Ran::ended(DoneReason::Stopped, steps_used, last_text));
                }
            }
        } else {
            streamed
        };

        let Streamed {
            content,
            mut tool_calls,
            images,
            reasoning,
            ..
        } = streamed;
        if images.is_empty() && completion_is_empty(&content, &tool_calls) {
            sink.error("model returned an empty response".to_string())
                .await;
            return Ok(Ran::ended(DoneReason::Completed, steps_used, last_text));
        }

        // Images the model generated: persisted and announced before the
        // assistant message lands, so what history carries is exactly what the
        // surfaces were told about.
        let images = sink
            .absorb(images, host.ctx().images.as_ref(), ImageSource::Assistant)
            .await;

        let mut assistant =
            ChatMessage::assistant_turn(content.clone(), images, tool_calls.clone());
        // Reasoning goes in front of the text and the calls it produced, which
        // is the order the model emitted it in and the order every provider
        // that accepts reasoning back wants it replayed in. It rides on the
        // assistant message, so a turn persists it and gets it back on
        // `/resume` like everything else.
        assistant
            .content
            .splice(0..0, reasoning.into_iter().map(ContentBlock::Thinking));
        host.push(assistant);

        if !policy.native_tools
            && tool_calls.is_empty()
            && let Some(call) = parse_json_tool_call(&content)
        {
            tool_calls.push(call);
        }

        if tool_calls.is_empty() {
            return Ok(Ran::ended(DoneReason::Completed, steps_used, content));
        }
        if !content.trim().is_empty() {
            last_text = content.clone();
            // What the model said on its way to calling something, for a pane
            // that renders whole messages. Nothing to render when it said
            // nothing.
            sink.step_text(&content).await;
        }

        // A batch's results go back as ONE message, and everything else the
        // batch produced goes *after* it.
        //
        // Anthropic requires every result for an assistant turn to arrive in
        // the single message that follows it, so answering call by call was an
        // HTTP 400 for any reply with two calls in it, which both Claude and
        // GPT emit by default. OpenAI takes one `tool` message per result, but
        // only if nothing else comes between them, and the images payload and
        // the failure nudge used to be pushed mid-batch. Accumulating here
        // fixes both with one rule.
        let mut results: Vec<ToolResultBlock> = Vec::with_capacity(tool_calls.len());
        // Images tools returned, paired with the tool that produced them, in
        // call order.
        let mut tool_images: Vec<(String, Vec<Image>)> = Vec::new();
        let mut nudges: Vec<String> = Vec::new();
        let mut ended: Option<DoneReason> = None;

        for (index, call) in tool_calls.iter().enumerate() {
            // Cancellation is honored between tool calls: pending calls are
            // answered so the persisted assistant message never carries
            // dangling tool_use.
            if policy
                .cancel
                .as_ref()
                .is_some_and(super::CancelHandle::is_cancelled)
            {
                answer_skipped_calls(
                    &mut results,
                    &tool_calls[index..],
                    "(not executed — interrupted by user)",
                );
                ended = Some(DoneReason::Stopped);
                break;
            }
            let outcome = match host.intercept(call, sink).await {
                Some(outcome) => outcome,
                None => CallOutcome::dispatched(host.dispatch(call, sink).await),
            };
            results.push(ToolResultBlock {
                tool_use_id: call.id.clone(),
                name: call.function.name.clone(),
                content: outcome.body,
            });
            if !outcome.images.is_empty() {
                tool_images.push((call.function.name.clone(), outcome.images));
            }
            nudges.extend(outcome.nudge);
            if let Some(reason) = outcome.done {
                // The rest of the batch never ran, but it still has to be
                // answered or the assistant message carries dangling tool_use.
                answer_skipped_calls(
                    &mut results,
                    &tool_calls[index + 1..],
                    "(not executed — turn ended early)",
                );
                ended = Some(reason);
                break;
            }
        }

        host.push(tool_feedback(policy.native_tools, results));

        // Images a tool returned ride back to the model on a follow-up user
        // message: user messages carry images uniformly across every provider,
        // whereas a `tool` result cannot on OpenAI. A non-vision model simply
        // ignores the attachment. They are persisted and announced first, so
        // the surfaces see them attached to the tool card that produced them.
        for (tool, images) in tool_images {
            let announced = sink
                .absorb(
                    images,
                    host.ctx().images.as_ref(),
                    ImageSource::Tool(tool.clone()),
                )
                .await;
            if !announced.is_empty() {
                host.push(ChatMessage::user_with_images(
                    format!("Image(s) returned by `{tool}`:"),
                    announced,
                ));
            }
        }
        for nudge in nudges {
            host.push(ChatMessage::system(nudge));
        }
        if let Some(reason) = ended {
            return Ok(Ran::ended(reason, steps_used, last_text));
        }

        if !sink.step_completed(step).await {
            return Ok(Ran::ended(DoneReason::Stopped, steps_used, last_text));
        }
    }

    Ok(Ran::ended(DoneReason::MaxSteps, steps_used, last_text))
}

/// Inject background work that finished since the last step into history (each
/// reported exactly once) and announce it. Called at the top of every step of a
/// turn, and between cycles by the perpetual runner.
pub(super) async fn drain_background(host: &mut impl Host, sink: &Sink) {
    let tasks = host.ctx().tasks.drain_completed();
    for task in tasks {
        host.push(ChatMessage::system(super::task_note(&task)));
        sink.turn_event(AgentEvent::TaskFinished {
            id: task.id,
            command: task.command,
            status: task.status,
        })
        .await;
    }
    let subagents = host.ctx().subagents.drain_completed();
    for task in subagents {
        host.push(ChatMessage::system(super::subagent_note(&task)));
        sink.turn_event(AgentEvent::SubagentFinished {
            id: task.id,
            name: task.name,
            task: task.task,
            completed: task.completed,
            output: task.output,
        })
        .await;
    }
}

/// How often a `pause` re-reads the control file.
const PAUSE_POLL: Duration = Duration::from_secs(2);

/// Honor `.wizard/loop-control` between steps: `stop` ends the run, `pause`
/// blocks until released, `skip` injects an instruction to move on. `Some` when
/// the run must end.
pub(super) async fn honor_loop_control(
    host: &mut impl Host,
    policy: &Policy,
) -> Option<DoneReason> {
    loop {
        let root = host.ctx().cwd.clone();
        match read_loop_control(&root) {
            Some(LoopControl::Stop) => {
                clear_loop_control(&root);
                return Some(DoneReason::Stopped);
            }
            Some(LoopControl::Pause) => {
                // A pause is the operator asking the run to wait, not asking it
                // to stop obeying everything else. This loop observed neither
                // the cancel handle nor the deadline, so a `pause` left in
                // `.wizard/loop-control` — by an operator, or by a crashed
                // script that never wrote the release — survived Ctrl-C and
                // sailed straight past `--max-hours`, forever, in a two-second
                // sleep nothing could interrupt.
                if let Some(deadline) = policy.deadline
                    && Instant::now() >= deadline
                {
                    return Some(DoneReason::TimeLimit);
                }
                let woke = tokio::select! {
                    () = tokio::time::sleep(PAUSE_POLL) => Wake::Poll,
                    () = super::cancelled(policy.cancel.as_ref()) => Wake::Cancelled,
                    () = sleep_until_deadline(policy.deadline) => Wake::Deadline,
                };
                match woke {
                    Wake::Poll => {}
                    Wake::Cancelled => return Some(DoneReason::Stopped),
                    Wake::Deadline => return Some(DoneReason::TimeLimit),
                }
            }
            Some(LoopControl::Skip) => {
                clear_loop_control(&root);
                host.push(ChatMessage::user(
                    "Operator control: skip the current sub-task and move on to the next \
                     part of the task.",
                ));
                return None;
            }
            None => return None,
        }
    }
}

/// Which of the three things a paused run is waiting on woke it.
enum Wake {
    Poll,
    Cancelled,
    Deadline,
}

/// Resolves at `deadline`, or never when there is none — the counterpart of
/// [`super::cancelled`] for a run with no wall-clock cap.
async fn sleep_until_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending::<()>().await,
    }
}

/// How full the next call's prompt is, from the numbers this run has.
async fn measure(host: &impl Host, policy: &Policy) -> super::ContextPressure {
    let last_prompt = host.last_prompt();
    context::pressure(context::Measured {
        tokens: last_prompt.unwrap_or_else(|| crate::llm::estimate_history_tokens(host.history())),
        window: host.client().context_window(&policy.model).await,
        bytes: host
            .history()
            .iter()
            .map(|message| message.text().len())
            .sum(),
        byte_threshold: policy.byte_threshold,
        last_prompt,
    })
}

/// Push the ephemeral pressure note for the next completion when the policy
/// carries one and the reading warrants it; report whether one went on.
///
/// The note is a **user** message at the very end of the history, and that is
/// load-bearing rather than cosmetic. Anthropic takes its system prompt as a
/// separate top-level field, so its adapter hoists *every* `Role::System`
/// message in the history into it; a system note carrying a live token count
/// therefore rewrote the cached prefix on every single request. Prompt caching
/// then never hits and every request pays the 1.25x cache-*write* premium for a
/// prefix nothing will ever read, which is strictly worse than not caching at
/// all. As the last user block it sits after everything cacheable, so the prefix
/// stays byte-identical from one request to the next however the number moves.
fn attach_pressure(
    host: &mut impl Host,
    policy: &Policy,
    reading: &super::ContextPressure,
) -> bool {
    if !policy.pressure_signal || reading.level == PressureLevel::Ok {
        return false;
    }
    host.history_mut()
        .push(ChatMessage::user(reading.signal_line()));
    true
}

/// Drop the note [`attach_pressure`] just added, when it added one.
///
/// Positional rather than a scan of the whole history: the note is pushed last
/// and taken off before anything else is pushed, whereas a text-prefix scan over
/// user messages would eat a real prompt that happened to start with the
/// heading.
fn detach_pressure(host: &mut impl Host, injected: bool) {
    if !injected {
        return;
    }
    let history = host.history_mut();
    let last_is_signal = history.last().is_some_and(|message| {
        message.role == Role::User && message.text().starts_with(CONTEXT_PRESSURE_HEADING)
    });
    debug_assert!(
        last_is_signal,
        "the pressure note must still be the last message when it is dropped"
    );
    if last_is_signal {
        history.pop();
    }
}

/// Re-ask once after a completion came back with nothing in it, in memory only
/// so neither the nudge nor the discarded reply reaches the record. `None` when
/// the retry was cancelled.
async fn nudge_once(
    host: &mut impl Host,
    policy: &Policy,
    sink: &Sink,
) -> Result<Option<Streamed>> {
    let reading = measure(host, policy).await;
    let signal = attach_pressure(host, policy, &reading);
    host.history_mut()
        .push(ChatMessage::user(EMPTY_COMPLETION_NUDGE));
    let retried = completion(host, policy, sink).await;
    host.history_mut().pop();
    detach_pressure(host, signal);
    Ok(match retried? {
        retry::Climbed::Done(streamed) => Some(streamed),
        retry::Climbed::Cancelled => None,
    })
}

/// Recover once from a reply the provider cut off while the model was still
/// writing a tool call. `None` when the retry was cancelled.
///
/// [`retry::Ladder`] is right not to retry this on its own: the provider
/// answered, the answer was just unusable, and re-sending the identical request
/// re-bills the whole prompt to be cut off at the identical byte. What it got
/// wrong was the conclusion. A truncation used to be a hard `Err` out of the
/// loop, which [`crate::headless`] turns into a dead run — so a model that
/// wrote one over-long `write_file` call ended a standing mission that had been
/// going for hours, over a request it could have restated in two calls.
///
/// The re-ask is not the same request, and *how* it differs depends on which
/// ceiling was hit — the two have opposite remedies, which is the whole reason
/// [`crate::llm::is_context_overflow`] exists:
///
/// - **The output-token ceiling.** A per-reply budget the next request gets
///   again in full, so the fix is to ask for less in one go:
///   [`TRUNCATED_TOOL_CALL_NUDGE`], which names concrete ways to be smaller
///   because a model told only "that was too long" tends to re-emit the same
///   call and be cut off at the same byte.
/// - **The context window.** The history itself no longer fits, and no amount
///   of writing a shorter tool call changes that, so the history is compacted
///   first and the nudge merely says what happened.
///
/// In memory only, like [`nudge_once`]: neither nudge is part of the record,
/// and the truncated assistant message never entered history at all, so the
/// retried step lands exactly where the first one would have.
///
/// Exactly once. If the second reply is *also* cut off, the error surfaces as
/// it did before — another attempt costs another full prompt to learn the same
/// thing, and in the overflow case there is now nothing left to compact.
async fn recover_truncated(
    host: &mut impl Host,
    policy: &Policy,
    sink: &Sink,
    truncated: &TruncatedToolCall,
) -> Result<Option<Streamed>> {
    let overflowed = crate::llm::is_context_overflow(&truncated.reason);
    sink.error(format!(
        "{truncated}; {}",
        if overflowed {
            "compacting and asking again"
        } else {
            "asking for a smaller call"
        }
    ))
    .await;
    // Compact before the reading is taken, so the pressure note the retry
    // carries describes the history it is actually being sent with.
    if overflowed {
        host.compact(sink).await;
    }
    let reading = measure(host, policy).await;
    let signal = attach_pressure(host, policy, &reading);
    let nudge = if overflowed {
        super::CONTEXT_OVERFLOW_NUDGE.to_string()
    } else {
        super::TRUNCATED_TOOL_CALL_NUDGE.replace("{tool}", &truncated.tool)
    };
    host.history_mut().push(ChatMessage::user(nudge));
    let retried = completion(host, policy, sink).await;
    host.history_mut().pop();
    detach_pressure(host, signal);
    Ok(match retried? {
        retry::Climbed::Done(streamed) => Some(streamed),
        retry::Climbed::Cancelled => None,
    })
}

/// The request one step sends.
pub(super) fn request(
    history: &[ChatMessage],
    tools: Vec<ToolSpec>,
    policy: &Policy,
) -> ChatRequest {
    ChatRequest {
        model: policy.model.clone(),
        messages: history.to_vec(),
        tools: if policy.native_tools {
            tools
        } else {
            Vec::new()
        },
        stream: true,
        options: Some(ChatOptions {
            temperature: Some(policy.temperature),
            num_ctx: None,
            reasoning_effort: policy.reasoning_effort.clone(),
        }),
    }
}

/// One step's model call, on the shared retry ladder: a transient LLM outage
/// (server down, rate-limited, mid-stream drop) pauses and retries instead of
/// aborting the run, and the endpoint's circuit breaker decides when to stop
/// trying. A non-transient error (auth, bad request, missing model) fails
/// immediately with the provider's message.
///
/// The policy itself lives in [`retry`] rather than here, because a turn and
/// every sub-run it fans out climb the same ladder over the same breaker: an
/// outage this turn already proved must not have to be rediscovered by each of
/// `/ultra`'s N candidates.
pub(super) async fn completion(
    host: &impl Host,
    policy: &Policy,
    sink: &Sink,
) -> Result<retry::Climbed<Streamed>> {
    let request = request(host.history(), host.tool_specs(), policy);
    let ladder = retry::Ladder {
        breaker: &policy.breaker,
        budget: policy.retry_budget,
        wait_out_outage: policy.wait_out_outage,
        deadline: policy.deadline,
        base_secs: policy.retry_base_secs,
        max_secs: policy.retry_max_secs,
        cancel: policy.cancel.as_ref(),
        sink,
    };
    ladder
        .climb(|| async {
            let Some(streamed) = stream(host.client(), request.clone(), sink, policy).await? else {
                // A cancelled completion is a user interrupt, not a provider
                // outcome, and the ladder keeps it off the breaker.
                return Ok(retry::Climbed::Cancelled);
            };
            // Bill the attempt before judging it: those tokens were spent
            // whatever the finish reason turns out to be.
            host.record_usage(&streamed.usage, sink).await;
            // A reply the provider cut off at its output-token ceiling while
            // the model was still writing a tool call is refused here: every
            // decoder turns half-written arguments into `{}` or a bare string,
            // and dispatching that runs the tool with empty arguments, which
            // for a shell command or a file edit is a different action, not a
            // smaller one.
            match truncated_tool_call(streamed.done_reason.as_deref(), &streamed.tool_calls) {
                Some(truncated) => Err(truncated.into()),
                None => Ok(retry::Climbed::Done(streamed)),
            }
        })
        .await
}

/// One completed model call: what the model said, what it asked to run, what it
/// wants replayed, and what it cost.
#[derive(Debug, Default)]
pub(super) struct Streamed {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    /// Images the model produced inline in this reply, in arrival order.
    pub images: Vec<Image>,
    /// Reasoning the provider handed back for replay, in arrival order.
    ///
    /// This is not the dimmed thinking text the UI renders (that is streamed
    /// and forgotten). It is the block a provider needs to see again (an
    /// encrypted Responses payload, an Anthropic signature) to continue a
    /// multi-step run from the reasoning it already did instead of deriving it
    /// again on the next request, and being billed for it again.
    pub reasoning: Vec<ThinkingBlock>,
    /// The provider's finish reason, verbatim. Load-bearing: see
    /// [`truncated_tool_call`].
    pub done_reason: Option<String>,
    pub usage: StepUsage,
}

/// Stream one completion, reporting deltas on `sink` and collecting everything
/// the reply carried. `None` when the run's interrupt landed mid-stream.
async fn stream(
    client: &Arc<dyn LlmProvider>,
    request: ChatRequest,
    sink: &Sink,
    policy: &Policy,
) -> Result<Option<Streamed>> {
    let cancel = policy.cancel.as_ref();
    if cancel.is_some_and(super::CancelHandle::is_cancelled) {
        return Ok(None);
    }
    let mut stream = client
        .chat_stream(request)
        .await
        .context("starting chat completion")?;

    let mut streamed = Streamed::default();
    loop {
        let next = tokio::time::timeout(STREAM_IDLE_TIMEOUT, stream.next());
        let chunk = tokio::select! {
            biased;
            () = super::cancelled(cancel) => return Ok(None),
            chunk = next => match chunk {
                // A stream that has gone silent is not a stream that ended:
                // raise it as a transport failure so the ladder redials
                // instead of accepting a half-written reply as complete.
                Err(_elapsed) => {
                    return Err(crate::llm::ProviderError::transport(format!(
                        "the model stream produced nothing for {}s; giving up on it and \
                         reconnecting",
                        STREAM_IDLE_TIMEOUT.as_secs()
                    ))
                    .into());
                }
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
            },
        };
        let chunk = chunk.context("reading chat stream")?;
        // Why the reply stopped, as the provider reported it. Only the final
        // chunk normally carries one, but a provider is free to report it
        // earlier, so the last one seen wins.
        if chunk.done_reason.is_some() {
            streamed.done_reason.clone_from(&chunk.done_reason);
        }
        // Images the model generated (see `ChatChunk::images`). They are
        // collected here and taken in by `absorb_images` once the reply is
        // complete, so a cancelled or retried stream leaves nothing behind.
        streamed.images.extend(chunk.images);
        if let Some(mut message) = chunk.message {
            let delta = message.text();
            if !delta.is_empty() {
                if chunk.thinking {
                    // Reasoning is surfaced to the UI but never becomes part
                    // of the assistant message.
                    sink.thinking_delta(delta).await;
                } else {
                    streamed.content.push_str(&delta);
                    sink.text_delta(delta).await;
                }
            }
            streamed.images.extend(message.take_images());
            streamed.tool_calls.extend(message.take_tool_calls());
            // Reasoning a provider handed back for replay. It contributes no
            // text (see `ContentBlock::as_text`), so it never reached `content`
            // and the UI never renders it twice; it is collected onto the
            // assistant message so the *next* request can hand the model back
            // the thinking it already paid for.
            for block in std::mem::take(&mut message.content) {
                match block {
                    ContentBlock::Thinking(block) => streamed.reasoning.push(block),
                    ContentBlock::Text(_)
                    | ContentBlock::Image(_)
                    | ContentBlock::ToolUse(_)
                    | ContentBlock::ToolResult(_) => {}
                }
            }
        }
        if chunk.prompt_eval_count.is_some() {
            streamed.usage.prompt = chunk.prompt_eval_count;
        }
        if chunk.eval_count.is_some() {
            streamed.usage.completion = chunk.eval_count;
        }
        if !chunk.cache.is_empty() {
            streamed.usage.cache = chunk.cache;
        }
        if chunk.done {
            break;
        }
    }
    Ok(Some(streamed))
}

/// What one tool call in a batch produced. Nothing here is pushed to history
/// by the call itself: the loop accumulates the results into one message and
/// appends the images and nudges *after* the whole batch, which is the only
/// ordering both frontier APIs accept.
pub(super) struct CallOutcome {
    /// Result body as the model will see it, error prefix included.
    pub body: String,
    /// Images the tool returned, to ride back on a user message after the
    /// batch.
    pub images: Vec<Image>,
    /// Nudge to inject after the batch (repeated failures of one tool).
    pub nudge: Option<String>,
    /// Set when the turn must end early (UI gone, circuit breaker).
    pub done: Option<DoneReason>,
}

impl CallOutcome {
    /// A call that answered `body` and ends the turn.
    fn ended(body: impl Into<String>, reason: DoneReason) -> Self {
        Self {
            body: body.into(),
            images: Vec::new(),
            nudge: None,
            done: Some(reason),
        }
    }

    /// What the dispatcher's pipeline made of one call.
    fn dispatched(outcome: DispatchOutcome) -> Self {
        match outcome.output {
            Some(output) => Self {
                body: result_body(&output),
                images: output.images,
                nudge: outcome.nudge,
                done: outcome.done,
            },
            // No result (event receiver gone mid-batch): answer the call
            // anyway so the persisted history carries no dangling tool_use.
            None => Self {
                body: "(not executed — turn ended early)".to_string(),
                images: Vec::new(),
                nudge: outcome.nudge,
                done: outcome.done,
            },
        }
    }
}

/// The result body a tool's output feeds back as, with the error prefix the
/// model keys off.
pub(super) fn result_body(output: &ToolOutput) -> String {
    if output.is_error {
        format!("Error: {}", output.content)
    } else {
        output.content.clone()
    }
}

/// Answer tool calls that will never run (turn ended early, user interrupt)
/// with a synthesized `note`, so the already-persisted assistant message
/// carries no dangling tool_use.
pub(super) fn answer_skipped_calls(
    results: &mut Vec<ToolResultBlock>,
    calls: &[ToolCall],
    note: &str,
) {
    results.extend(calls.iter().map(|call| ToolResultBlock {
        tool_use_id: call.id.clone(),
        name: call.function.name.clone(),
        content: note.to_string(),
    }));
}

/// The one message a batch's results feed back on: a `tool`-role message with
/// one `tool_result` block per call when the model has native tool calling, and
/// a single user message spelling the results out in text when it does not.
pub(super) fn tool_feedback(native_tools: bool, results: Vec<ToolResultBlock>) -> ChatMessage {
    if native_tools {
        return ChatMessage::new(
            Role::Tool,
            results.into_iter().map(ContentBlock::ToolResult).collect(),
        );
    }
    // No native tool calling: the results are prose on a user message, the
    // same way the calls themselves arrived as JSON in the model's text.
    let text = results
        .iter()
        .map(|result| format!("Tool result for `{}`:\n{}", result.name, result.content))
        .collect::<Vec<_>>()
        .join("\n\n");
    ChatMessage::user(text)
}

/// The parent turn as the loop sees it: an [`Agent`] and the channel its
/// surface is reading.
///
/// A struct rather than an `impl Host for Agent`, because the loop's reporting
/// is per-run and an agent outlives its turns — the channel is armed once per
/// turn and must not be a field the next turn can inherit.
pub(super) struct TurnHost<'a> {
    pub agent: &'a mut Agent,
}

#[async_trait]
impl Host for TurnHost<'_> {
    fn client(&self) -> &Arc<dyn LlmProvider> {
        &self.agent.client
    }

    fn ctx(&self) -> &ToolContext {
        &self.agent.ctx
    }

    fn tool_specs(&self) -> Vec<ToolSpec> {
        self.agent.dispatcher.registry().specs()
    }

    fn history(&self) -> &[ChatMessage] {
        &self.agent.history
    }

    fn history_mut(&mut self) -> &mut Vec<ChatMessage> {
        &mut self.agent.history
    }

    /// Append *and persist*: a turn is the session, so every message it keeps
    /// lands in the session file as it arrives and comes back on `/resume`.
    fn push(&mut self, message: ChatMessage) {
        self.agent.push(message);
    }

    fn last_prompt(&self) -> Option<u64> {
        self.agent.usage.last_prompt_tokens()
    }

    /// The turn's own tokens: they move the context meter, which is what
    /// decides when this history compacts.
    async fn record_usage(&self, usage: &StepUsage, sink: &Sink) {
        if !usage.reported() {
            return;
        }
        self.agent.usage.record(usage.prompt, usage.completion);
        // The cached split of the prompt this call was billed for. Kept out of
        // `record` because it is a subset of what that just took, not more
        // tokens: `record` moves the context meter and the session totals, this
        // only changes what those tokens *cost*. An adapter that reports no
        // split leaves this at zero and the turn prices as all-fresh, which is
        // the conservative reading and the one every backend without a prompt
        // cache deserves.
        self.agent
            .usage
            .record_cache(usage.cache.read, usage.cache.write);
        sink.usage(usage.prompt.unwrap_or(0), usage.completion.unwrap_or(0))
            .await;
    }

    async fn compact(&mut self, sink: &Sink) {
        let outcome = self.agent.compact_now().await;
        match &outcome {
            super::CompactOutcome::Nothing => return,
            // Success is informational; only a truncation (the summary LLM
            // genuinely failed) is an error.
            super::CompactOutcome::Summarized(_) | super::CompactOutcome::Evicted(_) => {
                sink.notice(outcome.describe()).await
            }
            super::CompactOutcome::Truncated { .. } => sink.error(outcome.describe()).await,
        }
        sink.turn_event(AgentEvent::ContextSize {
            tokens: self.agent.context_tokens(),
        })
        .await;
    }

    async fn dispatch(&mut self, call: &ToolCall, sink: &Sink) -> DispatchOutcome {
        let outcome = self
            .agent
            .dispatcher
            .dispatch(call, &self.agent.ctx, sink)
            .await;
        // Plan mode can flip mid-turn (an `exit_plan` approval lands inside the
        // call above): keep the system prompt's plan-mode block in step with
        // the flag before the next completion is composed.
        self.agent.sync_plan_prompt();
        outcome
    }

    /// `compact` mutates the history the loop is standing on, through
    /// [`Agent::compact_now`] — intercepted so it runs mid-turn on every
    /// surface. The tool's own execute path only runs outside this loop.
    async fn intercept(&mut self, call: &ToolCall, sink: &Sink) -> Option<CallOutcome> {
        if call.function.name != crate::tools::compact::COMPACT_TOOL_NAME {
            return None;
        }
        Some(self.agent.dispatch_compact(sink).await)
    }
}

impl Agent {
    /// Run one user turn: append `input`, then loop
    /// (stream completion → emit deltas → execute tool calls → feed results
    /// back) until the model stops calling tools — or, when `max_steps` is
    /// capped, until the budget runs out.
    /// Always finishes with [`AgentEvent::Done`]. Each message is appended
    /// to the session file as it lands.
    pub async fn run_turn(
        &mut self,
        input: &str,
        events: mpsc::Sender<AgentEvent>,
    ) -> Result<DoneReason> {
        self.run_turn_with_images(input, Vec::new(), events).await
    }

    /// Like [`Self::run_turn`], but attach filesystem image paths to the user
    /// message for vision-capable models.
    pub async fn run_turn_with_images(
        &mut self,
        input: &str,
        images: Vec<std::path::PathBuf>,
        events: mpsc::Sender<AgentEvent>,
    ) -> Result<DoneReason> {
        if let Some(warning) = self.load_warning.take() {
            let _ = emit(&events, AgentEvent::Error(warning)).await;
        }
        // Arm cancellation for this turn; a stale request from a previous
        // turn must not kill this one.
        self.cancel.clear();
        self.usage.begin_turn();
        let result = match self.turn_inner(input, &images, &events).await {
            Ok(reason) => {
                let _ = emit(&events, AgentEvent::Done { reason }).await;
                Ok(reason)
            }
            Err(err) => {
                let _ = emit(&events, AgentEvent::Error(format!("{err:#}"))).await;
                let _ = emit(
                    &events,
                    AgentEvent::Done {
                        reason: DoneReason::Stopped,
                    },
                )
                .await;
                Err(err)
            }
        };
        // However the turn ended, ultra's guidance goes with it.
        self.drop_ultra_guidance();
        // turn_end hooks: observational, fired however the turn ended.
        self.hooks.turn_end(self.mode, Some(&events)).await;
        self.record_turn_usage();
        result
    }

    async fn turn_inner(
        &mut self,
        input: &str,
        images: &[std::path::PathBuf],
        events: &mpsc::Sender<AgentEvent>,
    ) -> Result<DoneReason> {
        // user_prompt_submit hooks: may veto the turn before the model sees
        // the prompt (the message is never pushed to history), or append
        // extra context to it.
        let input = match self
            .hooks
            .user_prompt_submit_with_prompt(input, self.mode, Some(events))
            .await
        {
            PromptSubmit::Block(reason) => {
                let _ = emit(
                    events,
                    AgentEvent::Error(format!(
                        "prompt blocked by user_prompt_submit hook: {reason}"
                    )),
                )
                .await;
                return Ok(DoneReason::Stopped);
            }
            PromptSubmit::Continue(Some(extra)) => {
                format!("{input}\n\n[user_prompt_submit hook]\n{extra}")
            }
            PromptSubmit::Continue(None) => input.to_string(),
        };
        // Turn boundary: a fresh checkpoint turn for the dispatcher's
        // snapshots, anchored in the session file so /rewind can truncate
        // here. Best-effort — a marker failure never blocks the turn.
        let turn = self.checkpoints.begin_turn();
        if let Err(err) = self.session.append_marker(turn, &input) {
            tracing::warn!("could not append turn marker: {err}");
        }
        // Images the user attached (pasted into the TUI, passed on the command
        // line). They are read off disk here — size-capped and media-typed from
        // their bytes — and ride the user message as `Image`s, the same shape a
        // tool's or the model's own images travel in. One that cannot be read
        // is reported and skipped: the rest of the turn still runs.
        let mut attachments: Vec<crate::llm::Image> = Vec::with_capacity(images.len());
        for path in images {
            match crate::llm::Image::from_path(path) {
                Ok(image) => attachments.push(image),
                Err(err) => {
                    let notice = format!("could not attach {}: {err}", path.display());
                    tracing::warn!("{notice}");
                    emit(events, AgentEvent::Notice(notice)).await;
                }
            }
        }
        if attachments.is_empty() {
            self.push(ChatMessage::user(input.clone()));
        } else {
            self.push(ChatMessage::user_with_images(input.clone(), attachments));
        }

        // Ultra: the mixture-of-agents pre-phase. Candidates propose and judges
        // compare *before* the main loop starts, so their conclusions enter the
        // turn as one system note and the loop below — the only thing in this
        // session that may write — proceeds unchanged.
        //
        // Position is load-bearing. After the user push, so a cancellation here
        // leaves history exactly as a cancelled model stream does. Before
        // the loop, so a large guidance block is *accounted for* by the
        // compactor's first pass instead of overflowing the window behind it.
        // `ultra::run`
        // borrows `self` immutably and hands back an owned outcome, so those
        // borrows are over by the time history needs `&mut self`.
        if let Some(engine) = self.ultra.clone() {
            let outcome = ultra::run(
                &engine,
                &input,
                // The history as it stood *before* this request: it is already
                // pushed, and a candidate must not read its own brief twice.
                &self.history[..self.history.len() - 1],
                &self.client,
                &self.model,
                self.dispatcher.registry(),
                &self.hooks,
                // Bare: `ultra::run` wires this turn's event channel into the
                // context itself, since that is what its candidates' panes hang
                // off (see its doc comment).
                &self.ctx,
                &self.cancel,
                // The council's candidates dial the endpoint this turn is
                // already dialing, so they share its breaker: an outage one of
                // them hits is one the others do not have to prove again.
                &self.llm_breaker,
                events,
            )
            .await;
            match outcome {
                ultra::UltraOutcome::Guidance(guidance) => {
                    // The drafts and the verdict, verbatim, for the surface to
                    // keep: the candidates' panes retire off the rail long
                    // before this turn ends, and the guidance itself is never
                    // rendered, so without this the work the user just paid N×
                    // for would be unreadable everywhere.
                    let _ = emit(
                        events,
                        AgentEvent::UltraGuidance {
                            label: engine.label(),
                            guidance: guidance.clone(),
                        },
                    )
                    .await;
                    // History only, never the session: this is advice about the
                    // *one* request below it, so it is dropped again at the end
                    // of the turn (`drop_ultra_guidance`) and must not come back
                    // on `/resume` either. `push` would persist it as a system
                    // note, which is exactly what we do not want.
                    self.history.push(ChatMessage::system(guidance));
                }
                ultra::UltraOutcome::Skipped(reason) => {
                    let _ = emit(events, AgentEvent::Notice(format!("ultra: {reason}"))).await;
                }
                ultra::UltraOutcome::Cancelled => return Ok(DoneReason::Stopped),
            }
        }

        // Plan mode may have been set before the turn started (`/plan`,
        // `--plan`): bake the block in before the first completion is composed.
        // Every later flip happens inside a tool call, and `TurnHost::dispatch`
        // syncs there.
        self.sync_plan_prompt();

        let policy = Policy::turn(self);
        let sink = Sink::Turn(events.clone());
        let mut host = TurnHost { agent: self };
        match run(&mut host, &policy, &sink).await {
            Ok(ran) => Ok(ran.reason),
            // Endpoint breaker open: end the turn as a circuit breaker (rolled
            // back and clean in sovereign) rather than a hard error.
            Err(err) if err.is::<breaker::LlmBreakerOpen>() => Ok(DoneReason::CircuitBreaker),
            Err(err) => Err(err),
        }
    }

    /// Run [`Agent::compact_now`] for a `compact` tool call: report the
    /// outcome as the tool result, emit UI notices, and never end the turn on
    /// its own account.
    async fn dispatch_compact(&mut self, sink: &Sink) -> CallOutcome {
        let name = crate::tools::compact::COMPACT_TOOL_NAME;
        if !sink.tool_started(name, &serde_json::json!({})).await {
            return CallOutcome::ended("(not executed — turn ended early)", DoneReason::Stopped);
        }

        let outcome = self.compact_now().await;
        let pressure = self.context_pressure().await;
        let body = match &outcome {
            crate::agent::CompactOutcome::Nothing => {
                format!(
                    "{}; {}",
                    outcome.describe(),
                    pressure
                        .signal_line()
                        .trim_start_matches(CONTEXT_PRESSURE_HEADING)
                        .trim()
                )
            }
            crate::agent::CompactOutcome::Summarized(_)
            | crate::agent::CompactOutcome::Evicted(_)
            | crate::agent::CompactOutcome::Truncated { .. } => {
                format!(
                    "{}. Next-call pressure: {}",
                    outcome.describe(),
                    pressure
                        .signal_line()
                        .trim_start_matches(CONTEXT_PRESSURE_HEADING)
                        .trim()
                )
            }
        };
        let output = match &outcome {
            crate::agent::CompactOutcome::Truncated { .. } => ToolOutput::error(body),
            _ => ToolOutput::ok(body),
        };

        match &outcome {
            crate::agent::CompactOutcome::Summarized(_)
            | crate::agent::CompactOutcome::Evicted(_) => {
                sink.notice(outcome.describe()).await;
            }
            crate::agent::CompactOutcome::Truncated { .. } => {
                sink.error(outcome.describe()).await;
            }
            crate::agent::CompactOutcome::Nothing => {}
        }
        if outcome != crate::agent::CompactOutcome::Nothing {
            sink.turn_event(AgentEvent::ContextSize {
                tokens: self.context_tokens(),
            })
            .await;
        }

        if !sink.tool_finished(name, &output).await {
            return CallOutcome::ended(result_body(&output), DoneReason::Stopped);
        }
        CallOutcome {
            body: result_body(&output),
            images: Vec::new(),
            nudge: None,
            done: None,
        }
    }
}

/// The error for a completion the provider cut off at its output-token
/// ceiling while the model was still emitting tool calls, or `None` when the
/// reply ended for any other reason.
///
/// A plain text truncation is survivable: what arrived is still what the
/// model said. A truncated *tool call* is not: the provider's decoder has to
/// turn a half-written arguments string into some JSON value, so it degrades
/// to `{}` or to a bare string, and dispatching that runs the tool with empty
/// arguments. For a shell command or a file edit that is not a smaller
/// version of what the model meant, it is a different action.
pub(crate) fn truncated_tool_call(
    done_reason: Option<&str>,
    tool_calls: &[ToolCall],
) -> Option<TruncatedToolCall> {
    let reason = done_reason?;
    if !is_length_cutoff(reason) {
        return None;
    }
    // The call that was in flight when the ceiling hit is the last one; the
    // ones before it are complete, but the batch is reported as a whole and
    // partial dispatch would leave the model's own message half-answered.
    let cut_off = tool_calls.last()?;
    Some(TruncatedToolCall {
        reason: reason.to_string(),
        tool: cut_off.function.name.clone(),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use futures_util::stream;
    use serde_json::{Value, json};

    use super::*;
    use crate::agent::session::Session;
    use crate::config::Config;
    use crate::hooks::HookEngine;
    use crate::llm::provider::LlmProvider;
    use crate::llm::{ChatChunk, ChatStream};
    use crate::tools::registry::ToolRegistry;
    use crate::tools::{Tool, ToolContext, ToolError};

    /// One tool call, as a provider hands it back after decoding a truncated
    /// arguments string: the name survived, the arguments did not.
    fn empty_argument_call(name: &str) -> ToolCall {
        ToolCall::new(name, json!({}))
    }

    #[test]
    fn a_length_cutoff_mid_tool_call_is_an_error_not_a_dispatch() {
        let calls = vec![empty_argument_call("execute")];
        let truncated = truncated_tool_call(Some("length"), &calls).expect("must refuse");
        assert_eq!(truncated.tool, "execute");
        assert_eq!(truncated.reason, "length");
        // Anthropic spells the same cutoff differently.
        assert!(truncated_tool_call(Some("max_tokens"), &calls).is_some());

        // A normal stop, a tool-call stop, or no reason at all: nothing to
        // refuse.
        for reason in [Some("stop"), Some("tool_calls"), Some("tool_use"), None] {
            assert!(
                truncated_tool_call(reason, &calls).is_none(),
                "{reason:?} is not a cutoff"
            );
        }
        // A length cutoff with no tool call is ordinary text truncation: the
        // partial answer is still the model's answer, so it is kept.
        assert!(truncated_tool_call(Some("length"), &[]).is_none());
    }

    /// Provider that replays one canned chunk sequence per call, keeping every
    /// request that asked for one so a test can assert on what actually went
    /// out.
    #[derive(Debug)]
    struct ScriptedProvider {
        responses: Mutex<VecDeque<Vec<ChatChunk>>>,
        requests: Mutex<Vec<ChatRequest>>,
        /// Context window this provider reports, which is what the pressure
        /// bands are measured against.
        window: Option<u32>,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<Vec<ChatChunk>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
                window: None,
            }
        }

        fn with_window(responses: Vec<Vec<ChatChunk>>, window: u32) -> Self {
            Self {
                window: Some(window),
                ..Self::new(responses)
            }
        }

        /// The requests this provider was sent, in order.
        fn requests(&self) -> Vec<ChatRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for ScriptedProvider {
        async fn health(&self) -> Result<()> {
            Ok(())
        }
        async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
            Ok(true)
        }
        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
        async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream> {
            self.requests.lock().unwrap().push(request);
            let chunks = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted response available");
            Ok(futures_util::StreamExt::boxed(stream::iter(
                chunks.into_iter().map(Ok),
            )))
        }
        async fn context_window(&self, _model: &str) -> Option<u32> {
            self.window
        }
        fn label(&self) -> String {
            "scripted:test".to_string()
        }
    }

    /// One `done` chunk carrying `message`, with no usage counters so it does
    /// not disturb the pressure bands a test set up.
    fn final_chunk(message: ChatMessage) -> ChatChunk {
        ChatChunk {
            message: Some(message),
            images: Vec::new(),
            thinking: false,
            done: true,
            done_reason: Some("stop".to_string()),
            eval_count: None,
            prompt_eval_count: None,
            cache: CacheTokens::NONE,
        }
    }

    /// An agent wired to `provider`, with `registry`'s tools, in `root`.
    fn test_agent(
        root: &std::path::Path,
        provider: Arc<ScriptedProvider>,
        registry: ToolRegistry,
    ) -> Agent {
        let session = Session::create(root).expect("create session");
        let hooks = Arc::new(HookEngine::new(
            Vec::new(),
            root.to_path_buf(),
            session.id.clone(),
        ));
        let mut agent = Agent::new(
            provider,
            registry,
            Config {
                // No retry ladder: a failing step must fail on the first
                // attempt rather than sleep through a backoff.
                retry_base_secs: 0,
                retry_max_secs: 0,
                ..Config::default()
            },
            Vec::new(),
            root.to_path_buf(),
            session,
            true,
            hooks,
        )
        .expect("build agent");
        agent.set_usage_log(Some(root.join("usage.jsonl")));
        agent
    }

    /// A tool that counts how often it was actually dispatched.
    struct CountingTool(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl Tool for CountingTool {
        fn name(&self) -> &str {
            "execute"
        }
        fn description(&self) -> &str {
            "Run a shell command."
        }
        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": { "command": { "type": "string" } } })
        }
        async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput::ok("ran"))
        }
    }

    /// What a provider emits when the output-token ceiling lands in the middle
    /// of the arguments object: the tool name made it out, the arguments
    /// decoded to `{}`, and the finish reason says why.
    fn truncated_chunk() -> Vec<ChatChunk> {
        let mut assistant = ChatMessage::assistant("");
        assistant.push_tool_call(empty_argument_call("execute"));
        vec![ChatChunk {
            message: Some(assistant),
            images: Vec::new(),
            thinking: false,
            done: true,
            done_reason: Some("length".to_string()),
            eval_count: None,
            prompt_eval_count: None,
            cache: CacheTokens::NONE,
        }]
    }

    /// The same call, this time complete: what the re-ask is supposed to
    /// produce.
    fn whole_call_chunk() -> Vec<ChatChunk> {
        let mut assistant = ChatMessage::assistant("");
        assistant.push_tool_call(ToolCall::new(
            "execute",
            json!({ "command": "rm -f /tmp/one" }),
        ));
        vec![ChatChunk {
            message: Some(assistant),
            images: Vec::new(),
            thinking: false,
            done: true,
            done_reason: Some("tool_calls".to_string()),
            eval_count: None,
            prompt_eval_count: None,
            cache: CacheTokens::NONE,
        }]
    }

    /// Provider whose first stream opens and then never says anything, and
    /// whose second behaves.
    ///
    /// This is not a contrived shape: it is what a dropped connection looks
    /// like when the peer never sends a FIN, what a load balancer that idled a
    /// socket out looks like, and what a local runtime that died with its
    /// socket still open looks like. `stream.next()` neither resolves nor
    /// errors, forever.
    #[derive(Debug)]
    struct StallingProvider {
        stalled: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl LlmProvider for StallingProvider {
        async fn health(&self) -> Result<()> {
            Ok(())
        }
        async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
            Ok(true)
        }
        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
        async fn chat_stream(&self, _request: ChatRequest) -> Result<ChatStream> {
            if self.stalled.swap(true, Ordering::SeqCst) {
                return Ok(futures_util::StreamExt::boxed(stream::iter(vec![Ok(
                    final_chunk(ChatMessage::assistant("back")),
                )])));
            }
            Ok(futures_util::StreamExt::boxed(stream::pending()))
        }
        async fn context_window(&self, _model: &str) -> Option<u32> {
            None
        }
        fn label(&self) -> String {
            "stalling:test".to_string()
        }
    }

    /// A wedged run and a stopped run look identical from a terminal, and this
    /// is the shape that produces the first one: a response body that never
    /// ends. The turn used to park inside `stream.next()` for as long as the
    /// process lived.
    ///
    /// Time is paused, so the ten-minute idle ceiling costs the test nothing.
    #[tokio::test(start_paused = true)]
    async fn a_stream_that_goes_silent_is_abandoned_and_redialed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let provider = Arc::new(StallingProvider {
            stalled: std::sync::atomic::AtomicBool::new(false),
        });
        let session = Session::create(&root).expect("create session");
        let hooks = Arc::new(HookEngine::new(
            Vec::new(),
            root.clone(),
            session.id.clone(),
        ));
        let mut agent = Agent::new(
            provider,
            ToolRegistry::new(),
            Config {
                retry_base_secs: 0,
                retry_max_secs: 0,
                ..Config::default()
            },
            Vec::new(),
            root.clone(),
            session,
            true,
            hooks,
        )
        .expect("build agent");

        let (tx, mut rx) = mpsc::channel(256);
        let reason = agent
            .run_turn("say something", tx)
            .await
            .expect("the silent stream is a transient failure, not the end of the run");
        assert_eq!(reason, DoneReason::Completed);
        assert!(
            agent
                .history()
                .iter()
                .any(|message| message.text().contains("back")),
            "the redial produced the reply"
        );
        let mut errors = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::Error(message) = event {
                errors.push(message);
            }
        }
        assert!(
            errors
                .iter()
                .any(|message| message.contains("produced nothing")),
            "and the wait was reported rather than being silent: {errors:?}"
        );
    }

    /// A reply cut off mid tool call used to be a hard error out of the loop,
    /// and [`crate::headless`] turns a hard error into a dead run — so a model
    /// that wrote one over-long `write_file` call ended a mission that had been
    /// running for hours.
    ///
    /// Refusing to *dispatch* it was always right; refusing to *ask again* was
    /// not. The turn now re-asks once with a nudge to send something smaller,
    /// and carries on with whatever comes back.
    #[tokio::test]
    async fn a_truncated_tool_call_is_re_asked_smaller_rather_than_ending_the_turn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let provider = Arc::new(ScriptedProvider::new(vec![
            truncated_chunk(),
            whole_call_chunk(),
            vec![final_chunk(ChatMessage::assistant("deleted it"))],
        ]));

        let dispatches = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(CountingTool(Arc::clone(&dispatches))));
        let mut agent = test_agent(&root, Arc::clone(&provider), registry);

        let (tx, _rx) = mpsc::channel(256);
        let reason = agent
            .run_turn("delete the temp files", tx)
            .await
            .expect("the turn survives a truncation");
        assert_eq!(reason, DoneReason::Completed);
        assert_eq!(
            dispatches.load(Ordering::SeqCst),
            1,
            "the whole call ran once; the truncated one never did"
        );

        // The re-ask is what makes the second attempt different from the first.
        let requests = provider.requests();
        assert_eq!(requests.len(), 3, "truncated, re-asked, then continued");
        let nudge = requests[1]
            .messages
            .last()
            .expect("the re-ask carries a message");
        assert_eq!(nudge.role, Role::User);
        assert!(
            nudge.text().contains("output-token limit") && nudge.text().contains("execute"),
            "the nudge names the call that was cut off: {}",
            nudge.text()
        );

        // And it is advice about one request, so it is gone by the next one and
        // never reaches the record.
        assert!(
            !requests[2]
                .messages
                .iter()
                .any(|message| message.text().contains("output-token limit")),
            "the nudge does not linger into the following request"
        );
        assert!(
            !agent
                .history()
                .iter()
                .any(|message| message.text().contains("output-token limit")),
            "nor into history"
        );
    }

    /// The other cutoff, which has the opposite remedy.
    ///
    /// `is_context_overflow` has always existed and has always said it was
    /// "exposed so a caller that can drive compaction, rather than only report,
    /// can tell them apart" — and there was no such caller: both cutoffs ended
    /// the run identically. A history that no longer fits gets smaller by being
    /// compacted, not by the model writing a shorter tool call, and telling it
    /// to write a shorter one is advice that cannot work.
    #[tokio::test]
    async fn a_context_overflow_compacts_before_asking_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();

        let mut cut_off = ChatMessage::assistant("");
        cut_off.push_tool_call(empty_argument_call("execute"));
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![ChatChunk {
                message: Some(cut_off),
                images: Vec::new(),
                thinking: false,
                done: true,
                done_reason: Some("context_length_exceeded".to_string()),
                eval_count: None,
                prompt_eval_count: None,
                cache: CacheTokens::NONE,
            }],
            // The compaction pass's own summary call.
            vec![final_chunk(ChatMessage::assistant("everything so far"))],
            vec![final_chunk(ChatMessage::assistant("carrying on"))],
        ]));
        let mut agent = test_agent(&root, Arc::clone(&provider), ToolRegistry::new());
        // Enough history for the compactor to have a middle span to cut.
        for step in 0..(crate::agent::context::KEEP_RECENT + 6) {
            agent.push(ChatMessage::user(format!("turn {step}")));
        }
        let before = agent.history().len();

        let (tx, _rx) = mpsc::channel(256);
        let reason = agent
            .run_turn("keep going", tx)
            .await
            .expect("an overflow is recovered, not fatal");
        assert_eq!(reason, DoneReason::Completed);
        assert!(
            agent.history().len() < before,
            "the history was compacted before the retry: {before} → {}",
            agent.history().len()
        );

        let requests = provider.requests();
        assert_eq!(requests.len(), 3, "cut off, summarized, then re-asked");
        let nudge = requests[2].messages.last().expect("the re-ask's nudge");
        assert!(
            nudge.text().contains("context") && nudge.text().contains("compacted"),
            "the nudge names the right ceiling and the remedy applied: {}",
            nudge.text()
        );
        assert!(
            !nudge.text().contains("output-token limit"),
            "and does not give the other cutoff's advice: {}",
            nudge.text()
        );
    }

    #[tokio::test]
    async fn a_second_truncation_in_a_row_still_fails_the_turn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();

        // Cut off, nudged to send something smaller, and cut off again: the
        // model is not listening, and a third full prompt would only buy the
        // same answer.
        let provider = Arc::new(ScriptedProvider::new(vec![
            truncated_chunk(),
            truncated_chunk(),
        ]));

        let dispatches = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(CountingTool(Arc::clone(&dispatches))));

        let mut agent = test_agent(&root, provider, registry);

        let (tx, mut rx) = mpsc::channel(256);
        let err = agent
            .run_turn("delete the temp files", tx)
            .await
            .expect_err("a truncated tool call fails the turn");
        let chain = format!("{err:#}");
        assert!(chain.contains("output-token limit"), "got: {chain}");
        assert!(chain.contains("execute"), "got: {chain}");

        assert_eq!(
            dispatches.load(Ordering::SeqCst),
            0,
            "the tool must never run with the arguments that survived truncation"
        );
        // The failure is surfaced, not swallowed.
        let mut errors = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::Error(message) = event {
                errors.push(message);
            }
        }
        assert!(
            errors.iter().any(|message| message.contains("execute")),
            "the user is told which call was cut off: {errors:?}"
        );
    }

    /// The cached-prefix invariant, stated against two real requests.
    ///
    /// The live pressure note carries a token count that moves every step. It
    /// used to ride a `Role::System` message, and Anthropic's adapter hoists
    /// every one of those into the request's top-level `system` field, so the
    /// cached prefix was rewritten on every single request. That does not
    /// merely fail to hit: each miss is also a cache *write*, billed at 1.25x,
    /// so the whole feature cost more than never caching at all. As a trailing
    /// user block the note sits after everything cacheable and the prefix does
    /// not move.
    #[tokio::test]
    async fn the_pressure_note_leaves_the_cached_prefix_byte_identical() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        // Two identical replies: the two requests below differ in nothing but
        // whether the pressure note rides along.
        let provider = Arc::new(ScriptedProvider::with_window(
            vec![
                vec![final_chunk(ChatMessage::assistant("ok"))],
                vec![final_chunk(ChatMessage::assistant("ok"))],
            ],
            200_000,
        ));
        let mut agent = test_agent(&root, Arc::clone(&provider), ToolRegistry::new());
        agent.push(ChatMessage::user("do the thing"));

        let (tx, _rx) = mpsc::channel(64);
        let sink = Sink::Turn(tx);
        let policy = Policy::turn(&agent);
        let mut host = TurnHost { agent: &mut agent };

        // First request: comfortable headroom, so there is no note to add.
        let reading = measure(&host, &policy).await;
        let injected = attach_pressure(&mut host, &policy, &reading);
        assert!(!injected, "a fresh session is not under pressure");
        completion(&host, &policy, &sink)
            .await
            .expect("first completion streams");

        // Second request: same history, but the backend has now reported a
        // prompt filling 60% of the window, which is the elevated band.
        host.agent.usage.record(Some(120_000), Some(1));
        let reading = measure(&host, &policy).await;
        let injected = attach_pressure(&mut host, &policy, &reading);
        assert!(injected, "60% of the window must raise the signal");
        completion(&host, &policy, &sink)
            .await
            .expect("second completion streams");
        detach_pressure(&mut host, injected);

        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        let (plain, pressured) = (&requests[0].messages, &requests[1].messages);
        assert_eq!(
            pressured.len(),
            plain.len() + 1,
            "the note must be the only difference between the two requests"
        );
        for (index, (before, after)) in plain.iter().zip(pressured.iter()).enumerate() {
            assert_eq!(
                serde_json::to_string(before).expect("serialize"),
                serde_json::to_string(after).expect("serialize"),
                "message {index} of the prefix moved between the two requests"
            );
        }

        // The note is a trailing user block, which is what keeps the prefix
        // still: an adapter that hoists system messages has nothing to hoist.
        let note = pressured.last().expect("the note");
        assert_eq!(note.role, Role::User, "a system note would move the prefix");
        assert!(note.text().starts_with(CONTEXT_PRESSURE_HEADING));

        // The projection Anthropic actually caches: every Role::System message
        // in the history, joined. Byte-identical, with and without pressure.
        let system_of = |messages: &[ChatMessage]| -> Vec<String> {
            messages
                .iter()
                .filter(|message| message.role == Role::System)
                .map(ChatMessage::text)
                .collect()
        };
        assert_eq!(
            system_of(plain),
            system_of(pressured),
            "the cached system prefix must not mutate with the token count"
        );

        // And it is gone again the moment the completion it was for is over.
        assert!(
            agent
                .history
                .iter()
                .all(|message| !message.text().starts_with(CONTEXT_PRESSURE_HEADING)),
            "the note is ephemeral"
        );
    }

    /// Reasoning has to survive the step boundary, or a provider that keeps no
    /// server-side state (the Responses API with `store: false`) hands the
    /// model its own tool results with none of the thinking that asked for
    /// them, and it derives the lot again on every step, billed every time.
    #[tokio::test]
    async fn reasoning_rides_the_assistant_turn_into_the_next_request() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();

        // Step one: the model reasons, then calls a tool. The reasoning
        // arrives as a thinking block on the reply, the shape every adapter
        // that can replay reasoning decodes into.
        let mut thinking_reply = ChatMessage::new(
            Role::Assistant,
            vec![ContentBlock::Thinking(ThinkingBlock {
                thinking: "list the directory first".to_string(),
                signature: Some("rs_1".to_string()),
                data: Some("gAAAAAB-opaque".to_string()),
            })],
        );
        thinking_reply.push_tool_call(ToolCall::new("execute", json!({ "command": "ls" })));

        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![final_chunk(thinking_reply)],
            vec![final_chunk(ChatMessage::assistant("all done"))],
        ]));
        let dispatches = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(CountingTool(Arc::clone(&dispatches))));
        let mut agent = test_agent(&root, Arc::clone(&provider), registry);

        let (tx, _rx) = mpsc::channel(256);
        agent
            .run_turn("what is in here", tx)
            .await
            .expect("turn ok");
        assert_eq!(dispatches.load(Ordering::SeqCst), 1, "the tool ran");

        let requests = provider.requests();
        assert_eq!(requests.len(), 2, "one call, then the follow-up");
        let assistant = requests[1]
            .messages
            .iter()
            .find(|message| message.role == Role::Assistant)
            .expect("the first step's turn is in the second request");

        // The thinking block is there, verbatim, and it comes *before* the
        // call it produced: every API that takes reasoning back replays a turn
        // in the order the model emitted it.
        let ContentBlock::Thinking(replayed) = &assistant.content[0] else {
            panic!("reasoning must lead the turn: {:?}", assistant.content);
        };
        assert_eq!(replayed.data.as_deref(), Some("gAAAAAB-opaque"));
        assert_eq!(replayed.signature.as_deref(), Some("rs_1"));
        assert_eq!(replayed.thinking, "list the directory first");
        assert_eq!(assistant.tool_calls().len(), 1);

        // Reasoning is not answer text, so it stays out of what the message
        // says and out of every surface that renders it.
        assert!(
            !assistant.text().contains("list the directory"),
            "reasoning is not the reply: {:?}",
            assistant.text()
        );

        // It is persisted with the turn, so `/resume` brings it back rather
        // than making the next session re-derive it.
        let persisted = agent.session().load_history().expect("session loads");
        assert!(
            persisted.iter().any(|message| message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Thinking(block)
                    if block.data.as_deref() == Some("gAAAAAB-opaque")))),
            "the session file keeps the reasoning: {persisted:?}"
        );
    }

    /// Whether `line` opens a step loop: the `for` header of a run that
    /// cycles a model over a step budget.
    ///
    /// Two shapes, because a fork can arrive under either name. The bound is
    /// what the loop is always counted against, whatever the binding ends up
    /// being; the binding is what it has always been called.
    ///
    /// A step loop counts a budget, so the name alone is not enough to be one.
    /// `for step ` matched anything that happened to iterate something called
    /// steps — `for step in sudo_install_plan(…)` in `update.rs` walks two
    /// `sudo` argv vectors, and the guard read it as a second agent loop and
    /// forced a rename that had nothing to do with the thing being guarded.
    /// Under the name-only rule the binding must therefore also range over
    /// something, which iterating a collection does not.
    fn is_a_step_loop(line: &str) -> bool {
        let line = line.trim_start();
        if !line.starts_with("for ") {
            return false;
        }
        if line.contains("max_steps") || line.contains("last_step()") {
            return true;
        }
        (line.starts_with("for step ") || line.starts_with("for _step ")) && line.contains("..")
    }

    /// Production source of `path`: everything before its test module. A
    /// fixture that walks eight fake steps is not a second agent loop.
    fn without_tests(source: &str) -> &str {
        source
            .split_once("#[cfg(test)]")
            .map_or(source, |(before, _)| before)
    }

    /// Every `.rs` file under `dir` that is not itself a test file.
    fn sources(dir: &std::path::Path, found: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                sources(&path, found);
            } else if path.extension().is_some_and(|ext| ext == "rs")
                && path.file_name().is_some_and(|name| name != "tests.rs")
            {
                found.push(path);
            }
        }
    }

    /// **The anti-fork test.** There is one step loop in this program.
    ///
    /// It had two. The second was `subagent::spawn`'s, and it was the first one
    /// as it had stood some months earlier: no compaction, no circuit breaker,
    /// no deadline, nothing that could interrupt it. Each of those was found
    /// and fixed in the parent and then found again in the copy, and a council
    /// fans N sub-runs out per turn, so every one of them was worth N. The
    /// fixes eventually landed in both — and the *body* stayed forked, which is
    /// how the next one would have gone the same way.
    ///
    /// Grepping is the honest instrument here, for the same reason it is in
    /// `commands::surface`: a runtime assertion cannot observe a loop that a
    /// future call site would introduce, and what this guards against is not a
    /// wrong answer but a second place to have to remember.
    #[test]
    fn steps_do_not_fork() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        sources(&root, &mut files);
        assert!(files.len() > 50, "the scan found the tree: {}", files.len());

        let mut loops: Vec<String> = Vec::new();
        for path in &files {
            let source = std::fs::read_to_string(path).expect("read a source file");
            let rel = path.strip_prefix(&root).expect("under src");
            for (number, line) in without_tests(&source).lines().enumerate() {
                if is_a_step_loop(line) {
                    loops.push(format!("{}:{}", rel.display(), number + 1));
                }
            }
        }
        assert_eq!(
            loops.len(),
            1,
            "there is one step loop, in agent/turn.rs, and every run in the process \
             goes through it. These are the loops found:\n{}",
            loops.join("\n")
        );
        assert!(
            loops[0].starts_with("agent/turn.rs:"),
            "and it is this module's: {}",
            loops[0]
        );
    }

    /// The scan above has to recognize the shapes a second loop is written in,
    /// or it passes because it cannot see one rather than because there is
    /// none.
    #[test]
    fn the_scan_knows_what_a_second_step_loop_looks_like() {
        // The loop as it stands, and as it stood in the copy.
        assert!(is_a_step_loop("    for step in 1..=policy.max_steps {"));
        assert!(is_a_step_loop("    for step in 1..=max_steps {"));
        // Renaming the binding does not hide it, and neither does taking the
        // budget from somewhere else.
        assert!(is_a_step_loop("        for turn in 1..=config.max_steps {"));
        assert!(is_a_step_loop("        for n in 1..=budget.last_step() {"));
        // Loops that are not this one stay out of it: a council's rounds, a
        // retry ladder, a scan over history.
        assert!(!is_a_step_loop(
            "                for round in 1..=*rounds {"
        ));
        assert!(!is_a_step_loop("        for message in history {"));
        assert!(!is_a_step_loop("    let max_steps = config.max_steps;"));
        // And a loop over a collection that happens to hold steps is not a
        // step loop either, however its binding is spelled. This exact line
        // cost `update.rs` a rename.
        assert!(!is_a_step_loop(
            "        for step in sudo_install_plan(staged, dest_exe, &backup) {"
        ));
        assert!(!is_a_step_loop("    for step in plan.steps {"));
        assert!(!is_a_step_loop("    for _step in recipe.iter() {"));
    }

    /// A second loop that talks to a model has to climb the retry ladder — or
    /// it is a loop with no circuit breaker, which is the worse bug and the one
    /// that made `/ultra` spend N × 7 requests proving one outage. Pinning the
    /// call site is what makes "one loop" mean "one place that dials", the way
    /// `trust` pins which files may prompt on a terminal.
    #[test]
    fn only_the_loop_climbs_the_ladder() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        sources(&root, &mut files);
        // Assembled, or the needle would match this assertion.
        let needle = concat!(".cli", "mb(");
        let mut callers: Vec<String> = Vec::new();
        for path in &files {
            let source = std::fs::read_to_string(path).expect("read a source file");
            if without_tests(&source).contains(needle) {
                callers.push(
                    path.strip_prefix(&root)
                        .expect("under src")
                        .display()
                        .to_string(),
                );
            }
        }
        assert_eq!(
            callers,
            vec!["agent/turn.rs".to_string()],
            "every model call in the process climbs the one ladder, from the one loop"
        );
    }

    /// And a second loop that runs tools has to go through the one dispatcher.
    /// Only this module (the loop, and the turn's host), the sub-run's host and
    /// code mode's bridge may hand it a call; anything else is a caller quietly
    /// running a tool with some pipeline stage missing — a hook, a checkpoint, a
    /// breaker.
    ///
    /// `tools/code.rs` is on the list for the same reason `agent/subagent.rs`
    /// is, and adding it is the point of the feature rather than an exception to
    /// it: a Lua program calls Wizard's tools through `Dispatcher::sub_run`, so
    /// every call it makes is hooked, snapshotted and post-hooked exactly like
    /// one the model made itself. A fourth name appearing here without that
    /// argument behind it is the defect this test exists to catch.
    #[test]
    fn only_the_hosts_dispatch_a_tool_call() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        sources(&root, &mut files);
        let needle = concat!(".dis", "patch(");
        let mut callers: Vec<String> = Vec::new();
        for path in &files {
            let source = std::fs::read_to_string(path).expect("read a source file");
            if without_tests(&source).contains(needle) {
                callers.push(
                    path.strip_prefix(&root)
                        .expect("under src")
                        .display()
                        .to_string(),
                );
            }
        }
        callers.sort();
        assert_eq!(
            callers,
            vec![
                "agent/subagent.rs".to_string(),
                "agent/turn.rs".to_string(),
                "tools/code.rs".to_string(),
            ],
            "the turn's host, the sub-run's host and code mode's bridge, and nothing else"
        );
    }

    /// **The declines, stated.** A sub-run turns three of the turn's gates off,
    /// and this is where each one is on the record as a decision.
    ///
    /// The `Policy` is destructured whole, with no `..`: a capability added to
    /// the loop cannot compile until it has been answered for here, which is
    /// the difference between a sub-run that declines operator control and a
    /// sub-run that has simply never heard of it.
    #[tokio::test]
    async fn a_sub_run_declines_the_turns_gates_on_purpose() {
        let policy = Policy::sub_run(
            50,
            "a-model".to_string(),
            true,
            None,
            breaker::LlmBreaker::new(),
            1,
            30,
            48_000,
        );
        let Policy {
            max_steps,
            deadline,
            cancel,
            model,
            native_tools,
            temperature,
            reasoning_effort,
            breaker: _,
            retry_budget,
            wait_out_outage,
            recover_truncation,
            retry_base_secs,
            retry_max_secs,
            byte_threshold,
            background_drain,
            operator_control,
            pressure_signal,
        } = policy;

        // What it carries.
        assert_eq!(max_steps, 50);
        assert_eq!(model, "a-model");
        assert!(native_tools);
        assert_eq!(reasoning_effort, None);
        assert_eq!(retry_budget, Some(RETRY_ATTEMPTS));
        assert_eq!((retry_base_secs, retry_max_secs), (1, 30));
        assert_eq!(byte_threshold, 48_000);
        assert_eq!(temperature, crate::config::Mode::Sovereign.temperature());

        // What it declines, and why. The deadline and the interrupt are not
        // missing: `spawn` races the whole loop against both, which also ends a
        // run parked inside a model call, where a between-steps check would
        // not fire until the call returned.
        assert!(deadline.is_none(), "spawn owns the deadline");
        assert!(cancel.is_none(), "spawn owns the interrupt");
        // These three are declined outright.
        assert!(
            !background_drain,
            "the registries in a sub-run's context are the parent's"
        );
        assert!(
            !operator_control,
            "loop-control is the operator's handle on the session, not on one delegated run"
        );
        assert!(
            !pressure_signal,
            "the note's content is advice to call `compact`, which a sub-run cannot"
        );
        assert!(
            !recover_truncation,
            "a sub-run's cutoff comes back to the parent as a tool result, and the parent is \
             better placed to decide whether to ask again"
        );
        assert!(
            !wait_out_outage,
            "a sub-run is raced against a deadline from the outside; parking it in a \
             breaker cooldown would spend the parent's patience on a call it can no longer see"
        );

        // And each of those is something the turn genuinely has, so declining
        // it is a choice rather than a gap in the loop.
        let dir = tempfile::tempdir().expect("tempdir");
        let agent = test_agent(
            dir.path(),
            Arc::new(ScriptedProvider::new(Vec::new())),
            ToolRegistry::new(),
        );
        let turn = Policy::turn(&agent);
        assert!(turn.background_drain);
        assert!(turn.pressure_signal);
        assert!(turn.cancel.is_some());
        // Operator control is sovereign-only on a turn too, so it is asserted
        // where the mode is, not here.
        assert_eq!(
            turn.operator_control,
            agent.mode == crate::config::Mode::Sovereign
        );
    }
}
