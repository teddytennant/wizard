//! Tool-call dispatch pipeline.
//!
//! Every tool call in every mode (TUI, headless, gateway) and at every depth
//! (the user's turn, a delegated sub-run) funnels through
//! [`Dispatcher::dispatch`]. The pipeline runs in stages, in order:
//! plan-mode gate (blocks non-read-only tools while planning) → pre-tool
//! hooks (may rewrite arguments or block) → checkpoint snapshot of
//! `Edit`-class targets (best-effort, never blocks the call) → execute →
//! post-tool hooks (may append context) → failure bookkeeping.
//!
//! Which of those stages a call passes through is [`Pipeline`], and it is the
//! only thing that differs between a turn's call and a sub-run's. A sub-run
//! used to run a hand-copied half of the block instead — the hooks and the
//! checkpoint, and none of the rest — which is how it came to have no idea what
//! a repeatedly failing tool was, and how each fix to this file had to be
//! remembered twice.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;
use tokio::sync::mpsc;

use crate::agent::subagent::SPAWN_SUBAGENT_TOOL_NAME;
use crate::agent::turn::Sink;
use crate::agent::{AgentEvent, DoneReason, normalize_args};
use crate::config::Mode;
use crate::hooks::{HookEngine, PreToolUse};
use crate::llm::ToolCall;
use crate::tools::plan::EXIT_PLAN_TOOL_NAME;
use crate::tools::{ToolAccess, ToolContext, ToolOutput, registry::ToolRegistry};

/// Identical repeats of one failing call before the model is told, in a system
/// note, to stop making it.
///
/// This used to be the *trip*, and it is the likeliest single reason a sovereign
/// run was found stopped. Three is a perfectly ordinary number of times to run
/// the same failing build: a `cargo test` that fails the same way three times
/// while the model reads the output between runs produces three byte-identical
/// results, and the third one ended the turn — and [`crate::headless`] ends the
/// whole run on a circuit breaker. Saying "stop doing that" is what was wanted;
/// it just was not what happened.
pub(crate) const IDENTICAL_FAILURE_NUDGE: u32 = 3;

/// Identical repeats of one *faulted* call before the turn ends with
/// [`DoneReason::CircuitBreaker`].
///
/// Only faults reach this (see [`Grade`]). Six is far enough past the nudge to
/// mean the nudge was read and ignored, and by then the call has failed
/// identically — same tool, same arguments, same message — six times with
/// nothing whatsoever succeeding in between, which is not a model working on a
/// hard problem, it is a model in a loop.
pub(crate) const IDENTICAL_FAULT_TRIP: u32 = 6;

/// Consecutive failures of one tool (any args, any grade) before the model is
/// nudged to change approach.
const TOOL_FAILURE_NUDGE: u32 = 5;
/// Consecutive failures of one tool (any args, any grade) before the turn ends
/// with [`DoneReason::CircuitBreaker`].
///
/// "Consecutive" now means *nothing at all succeeded in between* rather than
/// "this tool did not succeed in between" — see [`ToolFailureCounter`]. Under
/// the old reading, eight failing `cargo test` runs spread across an afternoon
/// of successful edits ended the run, because the successes were `write_file`'s
/// and the failures were `execute`'s and the two counters never met. Under this
/// one, eight failures with not one success anywhere between them is the
/// picture of an agent that has stopped being able to affect the machine.
pub(crate) const TOOL_FAILURE_TRIP: u32 = 8;

/// Which stages of the pipeline a dispatcher's calls pass through.
///
/// The stages a sub-run drops are not an oversight to be closed later; each is
/// a stage that has nothing to act on, or nowhere to report to, inside a
/// delegated run. See [`Pipeline::SubRun`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pipeline {
    /// The user's turn: every stage.
    Turn,
    /// A delegated sub-run: the pre-hooks, the checkpoint, the call, and the
    /// post-hooks — the part that decides what actually happens on the
    /// machine — and nothing that belongs to a session.
    ///
    /// Two callers, and everything below describes both: a subagent
    /// ([`crate::agent::subagent::spawn`]) and one `run_code` program
    /// ([`crate::tools::code`]), which re-enters through this pipeline so a
    /// tool a Lua program calls is hooked, snapshotted and post-hooked exactly
    /// like a tool the model called directly.
    ///
    /// - **No plan-mode gate.** A run spawned while planning was already
    ///   narrowed to the read-only tools before it started
    ///   ([`SpawnOptions::read_only`](crate::agent::subagent::SpawnOptions::read_only)),
    ///   so there is nothing left for the gate to block, and it has no
    ///   `exit_plan` to reach even if there were.
    /// - **No failure breakers.** Neither the identical-failure breaker nor
    ///   the per-tool one: a sub-run is bounded by its step budget and its
    ///   deadline, and a breaker trip here would end the run with the parent
    ///   holding nothing at all instead of a partial report.
    /// - **No surface events on the tools themselves.** The lifecycle events
    ///   are still reported — as the run's own, through [`Sink`] — but the
    ///   tool context stays unwired, so a tool that converses with the user
    ///   (`interview`, `exit_plan`'s approval round-trip) declines instead of
    ///   asking a question nobody is positioned to answer.
    SubRun,
}

/// Runs the tool-call pipeline and owns its per-session state: the tool
/// registry and the failure counters feeding the circuit breakers.
pub struct Dispatcher {
    registry: ToolRegistry,
    /// Lifecycle hooks, shared with the agent and the subagent spawner.
    hooks: Arc<HookEngine>,
    /// Which stages a call runs through (see [`Pipeline`]).
    pipeline: Pipeline,
    /// Sovereign runs add the identical-failure circuit breaker.
    mode: Mode,
    /// Plan-mode flag, shared with the agent (`/plan`, `--plan`) and the
    /// `exit_plan` tool (cleared on approval). While set, only read-only
    /// tools and `exit_plan` may run.
    plan_mode: Arc<AtomicBool>,
    /// Signature of the last failing tool call and how many consecutive
    /// times it has failed identically (sovereign only).
    failure_streak: Option<(String, u32)>,
    /// Per-tool consecutive-failure counts (args ignored).
    tool_failures: ToolFailureCounter,
}

/// What [`Dispatcher::dispatch`] tells the agent loop to do after one call.
#[derive(Debug)]
pub struct DispatchOutcome {
    /// How the call went, as the breakers read it (see [`Grade`]).
    ///
    /// Reported rather than kept private because `output.is_error` cannot
    /// answer the question: a tool that ran and said no and a call that could
    /// not be made at all are both a `ToolOutput` with the flag set, and the
    /// difference between them is the whole reason [`Grade`] exists. The agent
    /// loop does not read this; `crate::tools::code` does, because a program
    /// has to hand a Lua caller `nil, message` for the first and a raise for
    /// the second, and deciding that by matching on the message text would be
    /// wrong in exactly the case the model can trigger.
    pub grade: Grade,
    /// Tool result to feed back to the model. `None` when the turn ended
    /// before a result could be reported (event receiver gone).
    pub output: Option<ToolOutput>,
    /// System-message nudge to inject after the feedback (repeated failures
    /// of one tool).
    pub nudge: Option<String>,
    /// Set when the turn must end early (UI gone, circuit breaker).
    pub done: Option<DoneReason>,
}

impl DispatchOutcome {
    /// The event receiver is gone: stop the turn without feedback.
    fn stopped() -> Self {
        Self {
            // Nothing was reported and nothing is known about the call, which
            // is the same standing a call that could not be made has.
            grade: Grade::Fault,
            output: None,
            nudge: None,
            done: Some(DoneReason::Stopped),
        }
    }
}

impl Dispatcher {
    pub fn new(
        registry: ToolRegistry,
        mode: Mode,
        hooks: Arc<HookEngine>,
        plan_mode: Arc<AtomicBool>,
    ) -> Self {
        Self {
            registry,
            hooks,
            pipeline: Pipeline::Turn,
            mode,
            plan_mode,
            failure_streak: None,
            tool_failures: ToolFailureCounter::default(),
        }
    }

    /// A dispatcher for one delegated sub-run: the same pipeline under
    /// [`Pipeline::SubRun`], over the run's own scoped registry.
    ///
    /// The plan-mode flag it carries is its own and always clear, because the
    /// gate is not part of this pipeline; a sub-run spawned during planning
    /// arrives with a read-only registry instead, which is a narrower answer
    /// than the gate could give.
    pub fn sub_run(registry: ToolRegistry, hooks: Arc<HookEngine>) -> Self {
        Self {
            registry,
            hooks,
            pipeline: Pipeline::SubRun,
            // Only the hooks see this, and a delegated run is autonomous by
            // construction whatever the parent's mode is.
            mode: Mode::Sovereign,
            plan_mode: Arc::new(AtomicBool::new(false)),
            failure_streak: None,
            tool_failures: ToolFailureCounter::default(),
        }
    }

    /// The registered tools (for specs and lookups).
    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    /// Swap the tool registry (after `/reload` or `/evolve`).
    pub fn set_registry(&mut self, registry: ToolRegistry) {
        self.registry = registry;
    }

    /// Track a mode switch (the identical-failure breaker is sovereign-only).
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    /// Forget all failure state (`/clear`).
    pub fn reset_failures(&mut self) {
        self.failure_streak = None;
        self.tool_failures.reset();
    }

    /// Run one tool call through the pipeline.
    pub(crate) async fn dispatch(
        &mut self,
        call: &ToolCall,
        ctx: &ToolContext,
        sink: &Sink,
    ) -> DispatchOutcome {
        let name = call.function.name.clone();
        let mut args = normalize_args(&call.function.arguments);

        // Plan-mode gate, first stage: while planning, only read-only tools
        // and exit_plan may run. The block feeds back to the model as an
        // ordinary tool error but is exempt from the failure breakers — a
        // model probing for write access mid-plan must not end the turn.
        // Unknown tools fall through so the real "unknown tool" error
        // surfaces instead. Delegation stays available but demoted: the
        // spawn call is tagged so the subagent runs with a read-only scope.
        if self.pipeline == Pipeline::Turn
            && self.plan_mode.load(Ordering::SeqCst)
            && name != EXIT_PLAN_TOOL_NAME
        {
            if name == SPAWN_SUBAGENT_TOOL_NAME {
                if let Some(object) = args.as_object_mut() {
                    object.insert("plan_mode".to_string(), Value::Bool(true));
                }
            } else if self
                .registry
                .get(&name)
                .is_some_and(|tool| tool.access() != ToolAccess::ReadOnly)
            {
                let output = ToolOutput::error(
                    "blocked by plan mode: only read-only tools are allowed; finish your plan \
                     and call exit_plan",
                );
                if !sink.tool_finished(&name, &output).await {
                    return DispatchOutcome::stopped();
                }
                return DispatchOutcome {
                    // The gate refused it, so the call did not happen. Named
                    // here, but exempt from the breakers by the early return.
                    grade: Grade::Fault,
                    output: Some(output),
                    nudge: None,
                    done: None,
                };
            }
        }

        // Pre-tool hooks: may rewrite the arguments or veto the call. A veto
        // feeds back to the model as an ordinary tool error (not fatal), so
        // the failure breakers cover repeated blocked calls too.
        match self
            .hooks
            .pre_tool_use(&name, &args, self.mode, self.surface(sink))
            .await
        {
            PreToolUse::Continue(updated) => {
                if let Some(updated) = updated {
                    args = updated;
                }
            }
            PreToolUse::Block(reason) => {
                let output = ToolOutput::error(format!("blocked by pre_tool_use hook: {reason}"));
                // A fault: the call did not happen, so nothing in the machine's
                // state changed and nothing the model reads in this result is
                // news about the world. A model that keeps making a call policy
                // refuses is talking to itself.
                return self
                    .bookkeep(&name, &args, output, Grade::Fault, sink)
                    .await;
            }
        }

        // Checkpoint stage: snapshot the target of an Edit-class tool so the
        // turn can be rewound. Runs after the pre-hooks (which may have
        // rewritten the path) and never fails the call. A sub-run's edits are
        // snapshotted under the parent's current turn — the context carries
        // the parent's store — so `/rewind` undoes delegated work too.
        crate::checkpoint::snapshot_edit_target(&self.registry, &name, &args, ctx);

        let Some((mut output, grade)) = self.execute(&name, args.clone(), ctx, sink).await else {
            return DispatchOutcome::stopped();
        };

        // Post-tool hooks: stdout becomes extra context on the tool result.
        if let Some(extra) = self
            .hooks
            .post_tool_use_with_output(
                &name,
                &args,
                &output.content,
                output.is_error,
                self.mode,
                self.surface(sink),
            )
            .await
        {
            crate::hooks::append_context(&mut output.content, &extra);
        }

        self.bookkeep(&name, &args, output, grade, sink).await
    }

    /// Execute stage: announce the call and run the tool, with the [`Grade`]
    /// the registry's answer earns. `None` when the event receiver is gone and
    /// the turn must stop.
    async fn execute(
        &self,
        name: &str,
        args: Value,
        ctx: &ToolContext,
        sink: &Sink,
    ) -> Option<(ToolOutput, Grade)> {
        if !sink.tool_started(name, &args).await {
            return None;
        }
        // On a turn, tools run with the turn's event channel in their context,
        // so a tool that converses with the surface (exit_plan's approval
        // round-trip) can reach it. A sub-run leaves the context unwired — see
        // [`Pipeline::SubRun`].
        let ctx = match self.surface(sink) {
            Some(events) => ctx.with_events(events.clone()),
            None => ctx.clone(),
        };
        // The registry's two answers are the grading, and they are not the same
        // thing however alike they look once both are a `ToolOutput` with
        // `is_error` set. `ToolOutput::is_error` is documented as "the tool ran
        // but reported failure ... distinct from `ToolError`, which means the
        // call itself could not be carried out" — the distinction existed all
        // along and was thrown away exactly here, one line before the breakers
        // that most needed it.
        Some(match self.registry.execute(name, args, &ctx).await {
            Ok(output) if output.is_error => (output, Grade::Reported),
            Ok(output) => (output, Grade::Fine),
            Err(err) => (ToolOutput::error(err.to_string()), Grade::Fault),
        })
    }

    /// The surface a call may talk *to*, as opposed to the [`Sink`] it is
    /// reported *on*: the channel a hook's own events ride, and the one a
    /// conversational tool finds in its context. `None` on a sub-run, which is
    /// what makes `interview` and `exit_plan` decline in there rather than ask
    /// a question with nobody positioned to answer it.
    fn surface<'a>(&self, sink: &'a Sink) -> Option<&'a mpsc::Sender<AgentEvent>> {
        match self.pipeline {
            Pipeline::Turn => sink.channel(),
            Pipeline::SubRun => None,
        }
    }

    /// Failure-bookkeeping stage: report the result and update both circuit
    /// breakers.
    ///
    /// Both of them now nudge before they trip, and neither trips on a result
    /// that merely *says* something went wrong. Between them they answer two
    /// different questions — "is this one call stuck?" ([`Self::track_identical`])
    /// and "has this run stopped getting anywhere at all?"
    /// ([`ToolFailureCounter`]) — and a trip from either still ends the run
    /// once [`crate::headless`] sees it, which is why the bar for one is where
    /// it is.
    async fn bookkeep(
        &mut self,
        name: &str,
        args: &Value,
        output: ToolOutput,
        grade: Grade,
        sink: &Sink,
    ) -> DispatchOutcome {
        if !sink.tool_finished(name, &output).await {
            return DispatchOutcome::stopped();
        }

        // Both breakers are the turn's (see [`Pipeline::SubRun`]): a sub-run
        // reports the result and keeps going, bounded by its own budget.
        if self.pipeline == Pipeline::SubRun {
            return DispatchOutcome {
                grade,
                output: Some(output),
                nudge: None,
                done: None,
            };
        }

        // Both are updated on every call, whichever one has something to say:
        // the streaks they keep are only meaningful if every result feeds them.
        let identical = self.track_identical(name, args, &output, grade);
        let per_tool = self.tool_failures.record(name, grade);

        // A trip from either breaker ends the turn. The identical-call one is
        // asked first because its message names the specific call to stop
        // making, which is the more useful thing to have in the log.
        let trip = if identical == FailureAction::Trip {
            Some(format!(
                "circuit breaker: '{name}' could not be run at all, identically, \
                 {IDENTICAL_FAULT_TRIP} times in a row"
            ))
        } else if per_tool == FailureAction::Trip {
            Some(format!(
                "circuit breaker: '{name}' failed {TOOL_FAILURE_TRIP} times in a row with \
                 nothing succeeding in between"
            ))
        } else {
            None
        };
        if let Some(message) = trip {
            sink.error(message).await;
            return DispatchOutcome {
                grade,
                output: Some(output),
                nudge: None,
                done: Some(DoneReason::CircuitBreaker),
            };
        }

        // At most one nudge per call: two system notes saying the same thing in
        // different words is worse advice than either alone.
        let nudge = if identical == FailureAction::Nudge {
            Some(format!(
                "You have now called '{name}' with the same arguments and gotten the same \
                 failure back, {IDENTICAL_FAILURE_NUDGE}+ times running, with nothing \
                 succeeding in between — repeating it cannot produce a different result. \
                 Change the arguments, use a different tool, or work on something else."
            ))
        } else if per_tool == FailureAction::Nudge {
            Some(format!(
                "Repeated failures with tool '{name}' ({TOOL_FAILURE_NUDGE} in a row) — \
                 stop retrying it and change approach."
            ))
        } else {
            None
        };
        DispatchOutcome {
            grade,
            output: Some(output),
            nudge,
            done: None,
        }
    }

    /// Update identical-failure circuit-breaker state (sovereign only) and say
    /// what this repeat warrants.
    ///
    /// The streak is broken by *any* successful call, not just a success of
    /// this tool, so a length of `n` means: the same tool, with the same
    /// arguments, answered with the same bytes, `n` times, and in between them
    /// nothing anywhere succeeded. That last clause is what makes the count
    /// mean something. A build-fix-build loop cannot produce it — the model has
    /// to successfully edit *something* between two runs for the second one to
    /// be worth making — while a model that has stopped reading its own tool
    /// results produces it immediately.
    ///
    /// Only a [`Grade::Fault`] streak may trip. A repeated [`Grade::Reported`]
    /// failure is not left unbounded by that: it is by definition also a
    /// consecutive failure of one tool, so [`ToolFailureCounter`] catches it two
    /// repeats later. What the grade buys is that the *long* leash is the one a
    /// diagnostic gets.
    fn track_identical(
        &mut self,
        name: &str,
        args: &Value,
        output: &ToolOutput,
        grade: Grade,
    ) -> FailureAction {
        if self.mode != Mode::Sovereign {
            return FailureAction::Continue;
        }
        if grade == Grade::Fine {
            self.failure_streak = None;
            return FailureAction::Continue;
        }
        let signature = format!("{name}\u{1}{args}\u{1}{}", output.content);
        let count = match &self.failure_streak {
            Some((last, count)) if *last == signature => count + 1,
            _ => 1,
        };
        self.failure_streak = Some((signature, count));
        if grade == Grade::Fault && count >= IDENTICAL_FAULT_TRIP {
            return FailureAction::Trip;
        }
        // Every third repeat, not only the third: a streak that is allowed to
        // run long (a diagnostic, which never trips here) would otherwise be
        // told once and then left alone for as long as it liked.
        if count >= IDENTICAL_FAILURE_NUDGE && count % IDENTICAL_FAILURE_NUDGE == 0 {
            return FailureAction::Nudge;
        }
        FailureAction::Continue
    }
}

/// How a dispatched call went, as the failure breakers read it.
///
/// The old code had two states — the output's `is_error` flag was either set or
/// it was not — and treated `execute("cargo test")` returning exit 3 exactly
/// like `execute` not existing. Those are opposite situations. One is the
/// machine answering a question the model asked, which is the entire job; the
/// other is the model failing to ask a question at all.
///
/// The `execute` tool's own description tells the model "non-zero exit is
/// diagnostic signal — read stderr and adapt". A run that ends because the
/// model took that advice three times is a run that punished it for reading
/// the manual.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grade {
    /// The tool ran and reported success.
    Fine,
    /// The tool ran, and what it has to report is unwelcome: a command exited
    /// non-zero, a file was not there, a patch did not apply, a test failed.
    /// Diagnostic signal, and the normal texture of engineering work.
    Reported,
    /// The call could not be carried out: no such tool, arguments that would
    /// not parse, a tool that timed out or panicked, a hook that refused it.
    /// Nothing happened on the machine, so nothing in the result is news about
    /// the world — which is what makes an identical repeat of one meaningful in
    /// a way an identical repeat of a diagnostic is not.
    Fault,
}

/// What [`ToolFailureCounter::record`] says to do after a tool result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureAction {
    Continue,
    /// Inject a system nudge telling the model to stop retrying the tool.
    Nudge,
    /// End the turn via the circuit breaker.
    Trip,
}

/// Per-tool-name consecutive-failure counter, independent of arguments
/// (catches models that jitter args to dodge the identical-failure breaker).
///
/// **Any** success clears **every** count, and that is the whole design.
///
/// It used to be that a success of a tool reset only that tool's count, which
/// sounds narrower and more careful and is neither. The workhorse tool is
/// `execute`, and the shape of ordinary work is: run the build (fails), read a
/// file (succeeds), edit it (succeeds), run the build (fails), … Under the old
/// rule the successes landed on `read_file` and `write_file` while `execute`'s
/// count climbed monotonically across the entire session, so the eighth failing
/// build of a long debugging afternoon ended the run — with seven successful
/// edits sitting between them, every one of them evidence that the agent was
/// working exactly as intended.
///
/// Counting progress globally asks the question the breaker was always for: not
/// "is this tool unhappy?", which it is allowed to be, but "has this agent
/// stopped being able to make anything at all work?" Eight results in a row,
/// not one of them a success of anything, is a good answer to that, and it is
/// not something a working session produces.
#[derive(Debug, Default)]
struct ToolFailureCounter {
    counts: std::collections::HashMap<String, u32>,
}

impl ToolFailureCounter {
    /// Record one graded tool result and return the action it warrants.
    fn record(&mut self, name: &str, grade: Grade) -> FailureAction {
        if grade == Grade::Fine {
            // Progress anywhere is progress: the run is not wedged, so no
            // tool's streak is evidence that it is.
            self.counts.clear();
            return FailureAction::Continue;
        }
        let count = self.counts.entry(name.to_string()).or_insert(0);
        *count += 1;
        match *count {
            count if count >= TOOL_FAILURE_TRIP => FailureAction::Trip,
            TOOL_FAILURE_NUDGE => FailureAction::Nudge,
            _ => FailureAction::Continue,
        }
    }

    fn reset(&mut self) {
        self.counts.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_failures_nudge_then_trip() {
        let mut counter = ToolFailureCounter::default();
        for i in 1..TOOL_FAILURE_NUDGE {
            assert_eq!(
                counter.record("execute", Grade::Fault),
                FailureAction::Continue,
                "failure {i}"
            );
        }
        assert_eq!(
            counter.record("execute", Grade::Fault),
            FailureAction::Nudge
        );
        for i in TOOL_FAILURE_NUDGE + 1..TOOL_FAILURE_TRIP {
            assert_eq!(
                counter.record("execute", Grade::Fault),
                FailureAction::Continue,
                "failure {i}"
            );
        }
        assert_eq!(counter.record("execute", Grade::Fault), FailureAction::Trip);
    }

    #[test]
    fn tool_failures_reset_on_success_of_that_tool() {
        let mut counter = ToolFailureCounter::default();
        for _ in 0..TOOL_FAILURE_NUDGE - 1 {
            counter.record("execute", Grade::Fault);
        }
        assert_eq!(
            counter.record("execute", Grade::Fine),
            FailureAction::Continue
        );
        // The streak starts over after the success.
        for i in 1..TOOL_FAILURE_NUDGE {
            assert_eq!(
                counter.record("execute", Grade::Fault),
                FailureAction::Continue,
                "failure {i}"
            );
        }
        assert_eq!(
            counter.record("execute", Grade::Fault),
            FailureAction::Nudge
        );
    }

    /// The counts are still per tool — one unhappy tool must not spend another
    /// tool's leash — but any success at all clears every one of them.
    ///
    /// The second half is the fix for the complaint. `execute` fails, the model
    /// edits a file successfully, `execute` fails again: that is a debugging
    /// loop, not a wedged agent, and the old rule counted it as eight strikes
    /// against `execute` because the successes all landed on `write_file`.
    #[test]
    fn tool_failures_count_per_tool_but_any_success_clears_them_all() {
        let mut counter = ToolFailureCounter::default();
        for _ in 0..TOOL_FAILURE_NUDGE - 1 {
            counter.record("execute", Grade::Fault);
            counter.record("write_file", Grade::Fault);
        }
        // Each tool climbs on its own: `write_file`'s four failures have not
        // pushed `execute` past its own fourth.
        assert_eq!(
            counter.record("execute", Grade::Fault),
            FailureAction::Nudge
        );

        // Now a success — of a *different* tool — and everything resets.
        assert_eq!(
            counter.record("read_file", Grade::Fine),
            FailureAction::Continue
        );
        for i in 1..TOOL_FAILURE_TRIP {
            assert_eq!(
                counter.record("execute", Grade::Reported),
                if i == TOOL_FAILURE_NUDGE {
                    FailureAction::Nudge
                } else {
                    FailureAction::Continue
                },
                "failing build {i} after a successful edit is not a wedged run"
            );
        }
    }

    /// A build that fails all afternoon while the model actually fixes things
    /// never trips, however many times it fails, because something keeps
    /// succeeding in between.
    #[test]
    fn an_interleaved_build_fix_loop_never_trips() {
        let mut counter = ToolFailureCounter::default();
        for round in 0..40 {
            assert_eq!(
                counter.record("execute", Grade::Reported),
                FailureAction::Continue,
                "round {round}: the build still fails"
            );
            assert_eq!(
                counter.record("edit_file", Grade::Fine),
                FailureAction::Continue,
                "round {round}: and the model fixes something"
            );
        }
    }

    #[test]
    fn tool_failures_reset_clears_all_counts() {
        let mut counter = ToolFailureCounter::default();
        for _ in 0..TOOL_FAILURE_TRIP {
            counter.record("execute", Grade::Fault);
        }
        counter.reset();
        assert_eq!(
            counter.record("execute", Grade::Fault),
            FailureAction::Continue
        );
    }

    /// One tool that always answers the same way, either grade.
    struct Always(bool);

    #[async_trait::async_trait]
    impl crate::tools::Tool for Always {
        fn name(&self) -> &str {
            "execute"
        }
        fn description(&self) -> &str {
            "Always answers the same way."
        }
        fn parameters(&self) -> Value {
            serde_json::json!({ "type": "object", "properties": {} })
        }
        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolContext,
        ) -> Result<ToolOutput, crate::tools::ToolError> {
            if self.0 {
                // The tool ran; the command it ran exited non-zero. Byte for
                // byte the same output every time, which is exactly what a
                // deterministic failing build produces.
                Ok(ToolOutput::error(
                    "error[E0308]: mismatched types\nstderr:\nerror: could not compile\nexit code: 101",
                ))
            } else {
                // The call could not be carried out at all.
                Err(crate::tools::ToolError::InvalidArgs {
                    tool: "execute".to_string(),
                    message: "command must not be empty".to_string(),
                })
            }
        }
    }

    /// Dispatch the same call `rounds` times through a real sovereign
    /// dispatcher, collecting what each one asked the loop to do.
    async fn repeat(reported: bool, rounds: u32) -> Vec<(Option<String>, Option<DoneReason>)> {
        let dir = std::env::temp_dir().join(format!("wizard-breaker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let hooks = Arc::new(HookEngine::new(Vec::new(), dir.clone(), "test".to_string()));
        let ctx = ToolContext::new(&dir);
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(Always(reported)));
        let mut dispatcher = Dispatcher::new(
            registry,
            Mode::Sovereign,
            hooks,
            Arc::new(AtomicBool::new(false)),
        );
        let (tx, _rx) = mpsc::channel(256);
        let sink = Sink::Turn(tx);
        let call = ToolCall::new("execute", serde_json::json!({ "command": "cargo test" }));

        let mut seen = Vec::new();
        for _ in 0..rounds {
            let outcome = dispatcher.dispatch(&call, &ctx, &sink).await;
            let done = outcome.done;
            seen.push((outcome.nudge, done));
            if done.is_some() {
                break;
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        seen
    }

    /// **The complaint, reproduced.** A build that fails the same way three
    /// times running used to end the turn — and a sovereign turn ending on a
    /// circuit breaker ends the whole run.
    ///
    /// A non-zero exit is a `ToolOutput::error`, so three `cargo test` runs
    /// against unchanged code were three byte-identical failures and the third
    /// one was fatal. That is not a malfunction, it is the middle of a fix. Now
    /// the third one is told to stop repeating itself and the run carries on.
    #[tokio::test]
    async fn three_identical_build_failures_nudge_instead_of_ending_the_run() {
        let seen = repeat(true, IDENTICAL_FAILURE_NUDGE).await;
        assert_eq!(seen.len(), IDENTICAL_FAILURE_NUDGE as usize);
        assert!(
            seen.iter().all(|(_, done)| done.is_none()),
            "no repeat of a failing build may end the run this early: {seen:#?}"
        );
        let (nudge, _) = seen.last().expect("three rounds");
        let nudge = nudge.as_deref().expect("the third one says something");
        assert!(nudge.contains("same arguments"), "{nudge}");
    }

    /// It is still bounded, just at the bar that means something: eight results
    /// in a row without one success anywhere. Nothing a working session does.
    #[tokio::test]
    async fn a_diagnostic_repeated_forever_still_ends_at_the_backstop() {
        let seen = repeat(true, TOOL_FAILURE_TRIP + 4).await;
        assert_eq!(
            seen.len(),
            TOOL_FAILURE_TRIP as usize,
            "the per-tool backstop is what ends it, five repeats later than before"
        );
        assert_eq!(
            seen.last().expect("rounds").1,
            Some(DoneReason::CircuitBreaker)
        );
    }

    /// A call that could not be *made* is the other animal. Repeating one
    /// verbatim teaches the model nothing, because nothing happened, so it gets
    /// the nudge at three and the trip at six rather than the diagnostic's much
    /// longer leash.
    #[tokio::test]
    async fn an_identical_fault_trips_once_the_nudge_has_been_ignored() {
        let seen = repeat(false, IDENTICAL_FAULT_TRIP + 4).await;
        assert_eq!(seen.len(), IDENTICAL_FAULT_TRIP as usize);
        assert!(
            seen[IDENTICAL_FAILURE_NUDGE as usize - 1].0.is_some(),
            "nudged first: {seen:#?}"
        );
        assert!(
            seen[..IDENTICAL_FAULT_TRIP as usize - 1]
                .iter()
                .all(|(_, done)| done.is_none()),
            "and not ended before the nudge had a chance: {seen:#?}"
        );
        assert_eq!(
            seen.last().expect("rounds").1,
            Some(DoneReason::CircuitBreaker)
        );
    }

    /// A sub-run's pipeline declines the turn's gates, and this is what that
    /// costs and buys, run through the real block rather than asserted about
    /// it.
    ///
    /// The same tool fails the same way under both. On a turn the per-tool
    /// breaker trips and ends it; on a sub-run nothing trips, because a trip
    /// there would hand the parent an empty result instead of a partial report,
    /// and the run is bounded by its step budget anyway. The tool also sees the
    /// surface on a turn and does not on a sub-run, which is what makes
    /// `interview` decline in there rather than ask a question nobody is
    /// positioned to answer.
    #[tokio::test]
    async fn a_sub_run_declines_the_gates_a_turn_keeps() {
        use std::sync::Mutex;

        use crate::agent::turn::Sink;
        use crate::tools::Tool;

        /// Always fails, and records whether it could see the surface.
        struct Failing(Arc<Mutex<Vec<bool>>>);

        #[async_trait::async_trait]
        impl Tool for Failing {
            fn name(&self) -> &str {
                "probe"
            }
            fn description(&self) -> &str {
                "Always fails."
            }
            fn parameters(&self) -> Value {
                serde_json::json!({ "type": "object", "properties": {} })
            }
            async fn execute(
                &self,
                _args: Value,
                ctx: &ToolContext,
            ) -> Result<ToolOutput, crate::tools::ToolError> {
                self.0.lock().unwrap().push(ctx.events.is_some());
                Ok(ToolOutput::error("nope"))
            }
        }

        let dir = std::env::temp_dir().join(format!("wizard-dispatch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let hooks = Arc::new(HookEngine::new(Vec::new(), dir.clone(), "test".to_string()));
        let ctx = ToolContext::new(&dir);
        let call = ToolCall::new("probe", serde_json::json!({}));

        let mut saw = Vec::new();
        for sub_run in [false, true] {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let mut registry = ToolRegistry::new();
            registry.register(Arc::new(Failing(Arc::clone(&seen))));
            let (tx, _rx) = mpsc::channel(64);
            let (mut dispatcher, sink) = if sub_run {
                (
                    Dispatcher::sub_run(registry, Arc::clone(&hooks)),
                    Sink::Run {
                        run: 1,
                        name: "worker".to_string(),
                        events: Some(tx),
                    },
                )
            } else {
                (
                    Dispatcher::new(
                        registry,
                        Mode::Sovereign,
                        Arc::clone(&hooks),
                        Arc::new(AtomicBool::new(false)),
                    ),
                    Sink::Turn(tx),
                )
            };
            let mut ended = None;
            for _ in 0..TOOL_FAILURE_TRIP {
                let outcome = dispatcher.dispatch(&call, &ctx, &sink).await;
                assert!(outcome.output.expect("a result either way").is_error);
                if let Some(reason) = outcome.done {
                    ended = Some(reason);
                    break;
                }
            }
            saw.push((ended, seen.lock().unwrap().clone()));
        }
        let _ = std::fs::remove_dir_all(&dir);

        let (turn_ended, turn_seen) = &saw[0];
        assert_eq!(
            *turn_ended,
            Some(DoneReason::CircuitBreaker),
            "a turn's per-tool breaker trips on a tool that keeps failing"
        );
        assert!(
            turn_seen.iter().all(|wired| *wired),
            "and its tools run wired to the surface, so a conversational one can ask"
        );

        let (sub_ended, sub_seen) = &saw[1];
        assert_eq!(
            *sub_ended, None,
            "a sub-run answers every failure and keeps going; its budget is what bounds it"
        );
        assert_eq!(
            sub_seen.len(),
            TOOL_FAILURE_TRIP as usize,
            "all of them ran"
        );
        assert!(
            sub_seen.iter().all(|wired| !*wired),
            "and none of them could reach a surface"
        );
    }
}
