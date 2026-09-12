//! Sovereign headless runner: the surface that drives an [`Agent`] from the
//! command line instead of a terminal UI.
//!
//! It lives outside `crate::agent` on purpose. The agent core is UI-agnostic:
//! `turn.rs` reaches for `config`, `hooks`, `llm` and `tools` and nothing
//! else. This module is the opposite. It owns the CLI flags
//! ([`crate::cli::Cli`]), the `/command` and `@file` preprocessing
//! ([`crate::commands`]), the output sinks ([`crate::output`]) and the
//! terminal spinner ([`crate::progress`]). Keeping the two apart is what stops
//! a stray `println!` written for a headless run from landing in the ACP
//! server's JSON-RPC stream, which shares the same agent core and owns stdout.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::sync::mpsc;

use crate::agent::{
    Agent, AgentEvent, DoneReason, LoopControl, build_headless_agent, clear_loop_control,
    goal_critic, mission, read_loop_control,
};
use crate::cli::Cli;
use crate::config::Config;
use crate::output::OutputFormat;

/// How often an inter-cycle wait wakes up to re-read the world.
///
/// Every wait between cycles used to be a single opaque
/// `tokio::time::sleep(cycle_pause_secs)`. A `stop` written into
/// `.wizard/loop-control` while that call was parked did nothing until it
/// returned, and with a pause configured in minutes the kill switch looked
/// broken. Waits are now built out of ticks this long so the two things that
/// may cut a wait short — the operator and `--max-hours` — are checked at
/// human timescale, and the cost is one `read_to_string` of a tiny file per
/// tick.
const WAIT_TICK: Duration = Duration::from_millis(500);

/// How often a run held by `pause` re-reads the control file. Longer than
/// [`WAIT_TICK`] because a hold is open-ended and nobody expects it to release
/// instantly; it matches the cadence the in-turn pause already polls at.
const PAUSE_TICK: Duration = Duration::from_secs(2);

/// Floor on the wait after a cycle ends in a tripped circuit breaker.
///
/// The endpoint breaker refuses every call for its own cooldown and only then
/// admits a single recovery probe. Starting the next cycle before that elapses
/// buys nothing: the very first model call is rejected without reaching the
/// network and the cycle dies again instantly, which reads to an operator as a
/// tight failure loop. This is deliberately a little longer than the breaker's
/// own 30-second cooldown (`agent::breaker::OPEN_DURATION`) so the probe is
/// admitted rather than raced. If that constant ever grows, this one must
/// follow it — the cost of being wrong in this direction is a wasted cycle,
/// the cost of being wrong in the other is a slower recovery.
const BREAKER_COOLDOWN_SECS: u64 = 35;

/// Floor on the backoff between failed cycles, applied even when
/// `retry_base_secs` is configured to zero. Without it a cycle that fails
/// instantly — a malformed request the provider rejects before reading it —
/// spins the outer loop as fast as the CPU allows.
const MIN_FAILURE_BACKOFF_SECS: u64 = 1;

/// Wall clock below which a self-evolve re-exec is not worth performing.
///
/// The relaunched process has to rebuild the agent, reload skills and hooks,
/// and re-read the mission before it can do anything; handing it a handful of
/// seconds produces a process that pays all of that and is then killed by its
/// own deadline having advanced the mission by nothing.
const MIN_REEXEC_SECS: f64 = 60.0;

/// Longest failure detail quoted back to the model in a recovery prompt. An
/// `anyhow` chain carrying a provider's HTML error page is unbounded, and the
/// prompt is prepended to a conversation that already has a context budget.
const MAX_DETAIL_CHARS: usize = 480;

/// Why an inter-cycle wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wake {
    /// The wait ran its full course.
    Elapsed,
    /// `.wizard/loop-control` asked for a stop. The caller consumes the file.
    Stop,
    /// The `--max-hours` deadline passed mid-wait.
    Deadline,
}

/// What `.wizard/loop-control` says to do at a cycle boundary.
///
/// The in-turn handler (`agent::turn::honor_loop_control`) reads the same file
/// between *steps*, which is why `pause` and `skip` written between cycles
/// used to be ignored: the outer loop only ever looked for `stop`, so a hold
/// requested in the gap between one cycle finishing and the next starting was
/// silently overwritten by the next turn's first step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CycleGate {
    /// Nothing pending; start the cycle.
    Proceed,
    /// Graceful shutdown. The caller consumes the control file.
    Stop,
    /// Abandon the action this cycle was about to take and pick another.
    Skip,
    /// The `--max-hours` deadline passed while the gate was held.
    Deadline,
}

/// Whether the run's wall clock has run out. `None` is a run with no
/// `--max-hours`, which never expires.
pub(crate) fn deadline_passed(deadline: Option<Instant>, now: Instant) -> bool {
    deadline.is_some_and(|deadline| now >= deadline)
}

/// Hours left on the run's wall clock, or `None` for a run without one. An
/// expired deadline reports `0.0` rather than going negative, so callers can
/// compare against a floor without worrying about the sign.
pub(crate) fn remaining_hours(deadline: Option<Instant>, now: Instant) -> Option<f64> {
    deadline.map(|deadline| deadline.saturating_duration_since(now).as_secs_f64() / 3600.0)
}

/// How long to wait after `streak` consecutive failed cycles: the configured
/// retry ladder, doubled per failure, capped at `max_secs`, and never below
/// `floor_secs`.
///
/// Unlike the per-call ladder in `agent::retry` this one is not jittered.
/// Jitter exists there to keep parallel workers pointed at one endpoint from
/// retrying in lockstep; there is exactly one outer loop per project, and a
/// deterministic wait is one an operator can predict and a test can assert.
pub(crate) fn failure_backoff(
    streak: u32,
    base_secs: u64,
    max_secs: u64,
    floor_secs: u64,
) -> Duration {
    let step = base_secs.max(MIN_FAILURE_BACKOFF_SECS);
    let ceiling = max_secs.max(step);
    let doublings = streak.saturating_sub(1).min(32);
    let climbed = step.saturating_mul(1u64 << doublings);
    Duration::from_secs(climbed.min(ceiling).max(floor_secs))
}

/// Trim a failure description to something a prompt can carry, cutting on a
/// character boundary so a multi-byte error message cannot panic the loop that
/// is trying to survive it.
pub(crate) fn brief(detail: &str, max_chars: usize) -> String {
    let detail = detail.trim();
    if detail.chars().count() <= max_chars {
        return detail.to_string();
    }
    let mut out: String = detail.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// The happy-path continuation prompt: a cycle reported its sub-task complete
/// and the mission has not, so the agent picks its own next action.
pub(crate) fn continuation_prompt(goal: &str, cycles: u64) -> String {
    format!(
        "You are operating CONTINUOUSLY and autonomously toward this standing mission:\n\n\
         {goal}\n\nYou just reported the current sub-task complete (cycle {cycles}). \
         Re-examine the project state, then choose and carry out the single most valuable \
         next action that advances the mission. If the mission itself is genuinely and \
         fully complete, instead pick a high-value improvement to the project — better \
         tests, docs, performance, robustness — or improve your OWN capabilities using the \
         `evolve` tool. Never idle; always advance."
    )
}

/// The prompt that opens the cycle after a failed one.
///
/// The old loop reused the happy-path continuation text no matter how the
/// previous cycle ended, so a model that had just wedged itself was told
/// "you just reported the sub-task complete" and cheerfully wedged itself the
/// same way again. Two things have to be said instead. First, what actually
/// happened, because the failure is invisible from inside the conversation
/// when the turn ended without a final message. Second, that the conversation
/// is no longer a reliable account of the working tree: with
/// `rollback_failed_cycles` on, the failed cycle's edits have been restored to
/// their pre-cycle contents, so every file the model "remembers" writing may
/// be unchanged on disk.
pub(crate) fn recovery_prompt(
    goal: &str,
    cycles: u64,
    streak: u32,
    why: &str,
    detail: &str,
    rolled_back: bool,
) -> String {
    let state = if rolled_back {
        "The failed cycle's file edits have been ROLLED BACK to their pre-cycle contents, so \
         anything you remember writing during it is gone."
    } else {
        "The failed cycle's edits, if any, were left on disk in whatever half-finished state \
         it stopped in."
    };
    format!(
        "You are operating CONTINUOUSLY and autonomously toward this standing mission:\n\n\
         {goal}\n\nThe previous cycle did NOT complete. It ended in {why}: {detail}\n\
         That is {streak} failed cycle(s) in a row; {cycles} cycle(s) have completed in \
         total.\n\n{state} Re-read the files you care about from disk before you reason \
         about them — trust the working tree, not this conversation.\n\n\
         Then take a MATERIALLY DIFFERENT approach. Do not re-issue the tool call, command, \
         or edit that just failed, and do not retry it with cosmetic changes. If the same \
         obstacle blocks you again, go around it: a smaller step, a different tool, or a \
         different part of the mission. Say in one line what you are doing differently and \
         why.",
        detail = brief(detail, MAX_DETAIL_CHARS)
    )
}

/// Instruction prepended when `skip` is observed at a cycle boundary. Between
/// steps the agent core injects its own skip message; between cycles there is
/// no turn in flight to inject into, so it rides on the next cycle's prompt.
const SKIP_AT_CYCLE_BOUNDARY: &str = "Operator control: skip the sub-task you were about to start. Re-read the project state, \
     choose a different next action, and do not retry the one you just skipped.";

/// The continuation prompt used when a turn ends on its step budget rather
/// than by finishing.
const CONTINUE_AFTER_MAX_STEPS: &str = "Continue the task from where you left off. If it is already complete, summarize what \
     was done.";

/// What a perpetual run does about a cycle that ended badly: `Some((prompt,
/// wait))` to roll into another cycle, `None` to give up.
///
/// The bound is on *consecutive* failures. A perpetual run is expected to
/// outlive individual mistakes — that is the entire premise of `--continuous`
/// — but it must not outlive a setup that cannot work at all, because a run
/// that retries a structurally impossible task forever burns tokens and never
/// reports. Any cycle that lands clears the streak, so reaching the limit
/// means the run has failed `max_consecutive_failures` times without a single
/// success in between, which no transient outage produces.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_recovery(
    config: &Config,
    goal: &str,
    cycles: u64,
    streak: u32,
    why: &str,
    detail: &str,
    floor_secs: u64,
) -> Option<(String, Duration)> {
    let limit = config.max_consecutive_failures;
    if limit != 0 && streak >= limit {
        return None;
    }
    Some((
        recovery_prompt(
            goal,
            cycles,
            streak,
            why,
            detail,
            config.rollback_failed_cycles,
        ),
        failure_backoff(
            streak,
            config.retry_base_secs,
            config.retry_max_secs,
            floor_secs,
        ),
    ))
}

/// Record a failed cycle in the mission and decide whether a perpetual run
/// survives it. `Some((prompt, wait))` continues; `None` gives up.
///
/// The mission write is the operator-facing half: the note says what failed,
/// the mirrored streak says how close the run is to the bound, and the phase
/// stamp says the loop was alive at the moment it decided. All of it is
/// best-effort — see [`persist`] for why a failure to write it is not allowed
/// to end the run that is already having a bad day.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_failed_cycle(
    config: &Config,
    mission: Option<&mut mission::Mission>,
    project_root: &Path,
    goal: &str,
    streak: u32,
    why: &str,
    detail: &str,
    floor_secs: u64,
) -> Option<(String, Duration)> {
    let cycles = match mission {
        Some(mission) => {
            mission.record_failure(format!("{why}: {}", brief(detail, MAX_DETAIL_CHARS)));
            // Keep the persisted mirror in lockstep with the loop's own
            // counter rather than trusting two independent increments.
            mission.consecutive_failures = streak;
            persist(mission, project_root);
            mission.cycles
        }
        None => 0,
    };
    plan_recovery(config, goal, cycles, streak, why, detail, floor_secs)
}

/// The command line a self-evolve re-exec relaunches with, or `None` when so
/// little wall clock is left that relaunching is pure waste.
///
/// This used to be a fixed `--mode sovereign --continuous --cwd <root>`, which
/// quietly discarded the terms the run was started under. A `--max-hours 8`
/// run that evolved itself at hour one came back immortal, because the new
/// process had no deadline at all — an eight-hour job that never ends is the
/// single worst way to lose a machine overnight. A `--output-format json`
/// consumer got the same treatment in the other direction: the relaunched
/// process printed human prose into the middle of what a script was parsing as
/// JSON.
pub(crate) fn reexec_args(
    project_root: &Path,
    remaining_hours: Option<f64>,
    output_format: OutputFormat,
) -> Option<Vec<String>> {
    if let Some(hours) = remaining_hours
        && hours * 3600.0 < MIN_REEXEC_SECS
    {
        return None;
    }
    let mut args = vec![
        "--mode".to_string(),
        "sovereign".to_string(),
        "--continuous".to_string(),
        "--cwd".to_string(),
        project_root.display().to_string(),
    ];
    if let Some(hours) = remaining_hours {
        args.push("--max-hours".to_string());
        args.push(format!("{hours:.6}"));
    }
    if output_format != OutputFormat::Text {
        // Asking clap for the name rather than hand-writing it keeps this in
        // step with the flag's own spelling if a variant is ever renamed.
        if let Some(value) = clap::ValueEnum::to_possible_value(&output_format) {
            args.push("--output-format".to_string());
            args.push(value.get_name().to_string());
        }
    }
    Some(args)
}

/// Write the mission to disk, logging a failure instead of propagating it.
///
/// These saves used to be `mission.save(&project_root)?` inside the cycle
/// loop, which meant a full disk, a read-only mount, or one `EACCES` from a
/// sandbox ended a mission meant to run forever — over bookkeeping, after the
/// work of the cycle had already succeeded. The mission file is a convenience
/// for resuming and for watching; it is not worth the run.
pub(crate) fn persist(mission: &mission::Mission, project_root: &Path) {
    if let Err(err) = mission.save(project_root) {
        tracing::warn!("could not persist mission.toml: {err:#}");
    }
}

/// Ask the run to wind down at the next boundary when the process is signalled,
/// and report the flag the cycle loop reads.
///
/// A sovereign run is the one surface where a signal is routinely how the run
/// ends: `systemctl stop`, a terminal closing on a `nohup`'d mission (SIGHUP),
/// a container shutting down, or an operator's Ctrl-C. The default disposition
/// for all of them is to terminate the process outright, which drops everything
/// the loop does on the way out — the final mission stamp, the `session_end`
/// hooks, the flush of a structured output stream. A perpetual mission killed
/// that way leaves a `mission.toml` whose `phase` still claims a cycle is
/// running, so the next operator to look cannot tell a stopped run from a
/// wedged one. That is the same "did it stop or is it stuck?" question the
/// heartbeat exists to answer.
///
/// So the first signal is treated as exactly what `.wizard/loop-control` calls
/// `stop`: cancel the turn in flight (the agent unwinds it to
/// [`DoneReason::Stopped`], synthesizing results for the tool calls it skips)
/// and refuse to start another cycle. The run then ends through its ordinary
/// path and everything downstream of the loop still happens.
///
/// The second signal is left to the default handler. A graceful stop still has
/// to wait for the current tool call to return, and a run wedged on one would
/// otherwise be unkillable by the very key the user reaches for first; two
/// Ctrl-Cs must always end it.
fn install_shutdown_signals(cancel: crate::agent::CancelHandle) {
    #[cfg(unix)]
    {
        tokio::spawn(async move {
            use tokio::signal::unix::{SignalKind, signal};
            // A failure to register is not worth ending the run over: it costs
            // the graceful path, not the run, and the default disposition still
            // stops the process.
            let mut term = match signal(SignalKind::terminate()) {
                Ok(stream) => stream,
                Err(err) => {
                    tracing::warn!("could not listen for SIGTERM: {err}");
                    return;
                }
            };
            let mut hup = match signal(SignalKind::hangup()) {
                Ok(stream) => stream,
                Err(err) => {
                    tracing::warn!("could not listen for SIGHUP: {err}");
                    return;
                }
            };
            tokio::select! {
                _ = term.recv() => {}
                _ = hup.recv() => {}
                result = tokio::signal::ctrl_c() => {
                    if let Err(err) = result {
                        tracing::warn!("could not listen for SIGINT: {err}");
                        return;
                    }
                }
            }
            SHUTDOWN.store(true, Ordering::SeqCst);
            cancel.cancel();
        });
    }
    #[cfg(not(unix))]
    let _ = cancel;
}

/// Raised once a shutdown signal has been seen. Process-global because the
/// signal is: there is one set of dispositions per process, and every wait in
/// this module wants to observe it without the flag being threaded through a
/// signature that exists for testing the *timing* logic, not the signal.
/// Tests never set it, so it reads false throughout them.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Whether a shutdown signal has been received.
fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}

/// Stamp the mission's liveness phase and persist it, best-effort.
fn stamp(mission: Option<&mut mission::Mission>, project_root: &Path, phase: impl Into<String>) {
    if let Some(mission) = mission {
        mission.stamp(phase);
        persist(mission, project_root);
    }
}

/// Sleep for `total`, waking every `tick` to obey `.wizard/loop-control` and
/// the run deadline. See [`WAIT_TICK`] for why no wait between cycles is a
/// single `sleep`.
pub(crate) async fn wait_awake(
    project_root: &Path,
    total: Duration,
    deadline: Option<Instant>,
    tick: Duration,
    shutdown: &AtomicBool,
) -> Wake {
    let until = Instant::now() + total;
    loop {
        let now = Instant::now();
        if deadline_passed(deadline, now) {
            return Wake::Deadline;
        }
        // A signal during a backoff is the same answer as `stop` written into
        // the control file during one: a wait that can now run to
        // `retry_max_secs` must not sit on it.
        if shutdown.load(Ordering::SeqCst)
            || read_loop_control(project_root) == Some(LoopControl::Stop)
        {
            return Wake::Stop;
        }
        let left = until.saturating_duration_since(now);
        if left.is_zero() {
            return Wake::Elapsed;
        }
        tokio::time::sleep(left.min(tick)).await;
    }
}

/// Block while `.wizard/loop-control` holds `pause`, returning as soon as it
/// is released, replaced by another command, or the deadline passes.
pub(crate) async fn await_release(
    project_root: &Path,
    deadline: Option<Instant>,
    tick: Duration,
    shutdown: &AtomicBool,
) -> CycleGate {
    loop {
        if deadline_passed(deadline, Instant::now()) {
            return CycleGate::Deadline;
        }
        // An operator hold must not outrank a shutdown: a paused run is still
        // a live process, and `systemctl stop` on one would otherwise block
        // until its timeout and then SIGKILL — losing exactly the bookkeeping
        // the graceful path exists to keep.
        if shutdown.load(Ordering::SeqCst) {
            return CycleGate::Stop;
        }
        match read_loop_control(project_root) {
            Some(LoopControl::Pause) => tokio::time::sleep(tick).await,
            Some(LoopControl::Stop) => return CycleGate::Stop,
            Some(LoopControl::Skip) => {
                clear_loop_control(project_root);
                return CycleGate::Skip;
            }
            None => return CycleGate::Proceed,
        }
    }
}

/// `rollback_failed_cycles`: restore every checkpoint from the failed
/// cycle's first turn onward and note the rollback in the persisted mission.
/// Best-effort — failures are logged and the run proceeds to its normal end.
pub(crate) fn rollback_failed_cycle(
    config: &Config,
    agent: &Agent,
    mission: Option<&mut mission::Mission>,
    project_root: &Path,
    first_turn: u64,
    why: &str,
    // `None` in the structured output formats, where stdout is JSON-only.
    spinner: Option<&crate::progress::TurnSpinner>,
) {
    if !config.rollback_failed_cycles {
        return;
    }
    match agent.checkpoints().restore_turns_from(first_turn) {
        Ok(restored) => {
            if restored.is_empty() {
                return;
            }
            if let Some(spinner) = spinner {
                spinner.println(&format!(
                    "[rolled back {} file(s) after {why}]",
                    restored.len()
                ));
            }
            if let Some(mission) = mission {
                mission.note(format!(
                    "rolled back {} file(s) after {why} (cycle starting at turn {first_turn})",
                    restored.len()
                ));
                if let Err(err) = mission.save(project_root) {
                    tracing::warn!("could not record rollback in mission.toml: {err:#}");
                }
            }
        }
        Err(err) => tracing::warn!("cycle rollback failed: {err:#}"),
    }
}

/// Sovereign-mode headless runner: builds an [`Agent`] and drives it in an
/// outer loop. The goal comes from `cli.prompt`, or (on a self-evolve
/// re-exec) from the persisted [`mission::Mission`]. With `--continuous` it
/// runs perpetually — persisting a mission, self-directing the next action
/// after each completed cycle, sleeping-and-waking through transient LLM
/// outages, compacting context, and re-exec'ing itself after a self-evolve —
/// until stopped via `.wizard/loop-control`, `--max-hours`, or
/// `max_consecutive_failures` cycles in a row that ended badly. A hard error
/// or a tripped circuit breaker is a failed *cycle*, not the end of the run:
/// it is rolled back, recorded, backed off from, and followed by a cycle told
/// what went wrong. Otherwise it honors the `--loop N` bound, where a hard
/// error still ends the run. Prints progress to
/// stdout instead of the TUI (`--output-format` selects the
/// [`crate::output::EventSink`]); the returned exit code encodes the
/// outcome (see [`crate::output::exit_code`]).
pub async fn run(config: Config, cli: Cli) -> Result<i32> {
    let project_root = std::env::current_dir().context("determining project root")?;

    // Goal resolution: an explicit `-p` wins; otherwise resume the standing
    // mission (this is the path taken after a self-evolve re-exec, which
    // relaunches without `-p`); otherwise there is nothing to do.
    let goal = if let Some(prompt) = cli.prompt.clone() {
        prompt
    } else if let Some(existing) = mission::Mission::load(&project_root)? {
        existing.goal
    } else {
        return Err(anyhow::anyhow!(
            "headless mode needs a task: pass -p \"<task>\""
        ));
    };
    // The same preprocessing the TUI applies on submit: custom `/command`
    // expansion and `@file` references (including image attachments).
    let custom_commands = crate::commands::load(&project_root);
    let prepared = crate::commands::preprocess(&goal, &custom_commands, &project_root);
    let goal = prepared.text;
    let goal_images = prepared.images;

    let active = config.active();
    let model = active.model.clone();
    let endpoint = active.base_url.clone();

    // Settle the per-project trust question before the agent is built (which
    // is what loads the project's hooks) and before anything else has taken
    // stdio over.
    //
    // A sovereign run is headless, not necessarily unattended: `wizard -p
    // "..."` in a terminal has a human in front of it, and `Console::Owned` is
    // exactly the declaration for that. It is only permission, not a promise
    // that a terminal exists: `crate::trust::can_ask` still requires a tty on
    // both ends and this process in the foreground, so `echo x | wizard -p`,
    // a systemd unit, and a CI job all refuse without blocking on a pipe.
    //
    // The structured output formats are excluded on purpose: their stdout is
    // a machine-readable stream and a prompt would be the first thing in it.
    // Those runs take the ordinary unattended path (refuse, log, and name
    // WIZARD_TRUST_PROJECT as the way through).
    if cli.output_format == crate::output::OutputFormat::Text
        && let Some(why) = crate::trust::preflight(&project_root)
    {
        // Before the spinner and the sink exist, so stderr is still plain.
        crate::output::eprint_line(&format!("wizard: {why}"));
    }

    let mut agent = build_headless_agent(&config, &project_root, cli.resume).await?;
    // Kept alongside the agent's copy: the agent checks it between steps
    // inside a turn, and the outer loop checks it at the cycle boundary and
    // inside every wait. Before that second copy existed, `cycle_pause_secs`
    // and any backoff ran unbounded past `--max-hours` — a run told to stop
    // after two hours could sit in a five-minute sleep for as long as it kept
    // failing, because nothing outside a turn had ever heard of the deadline.
    //
    // Through the shared converter rather than `Duration::from_secs_f64`
    // directly: that call panics on a negative, a NaN and an infinity, and
    // this is the second entry point for the number (the first is
    // `schedule.toml`). `--max-hours` now has a clap validator, so a bad value
    // is refused before the run starts; this stays a `?` rather than an
    // `unwrap` so the flag's parser is the only thing that has to hold.
    let deadline = cli
        .max_hours
        .map(crate::schedule::max_hours_duration)
        .transpose()?
        .map(|budget| Instant::now() + budget);
    agent.set_deadline(deadline);
    // Quality gates (`--gate`, the `gates` config key, the project's
    // `.wizard/gates.toml`). `None` when none are configured, which is the
    // default and must cost nothing. See [`crate::gates`] for why a run that
    // finishes is not the same thing as a run that succeeded.
    let mut gates = crate::gates::Gates::for_run(&config, &cli, &project_root);
    // `--plan` / `plan_first = true`: the first turn starts in plan mode.
    // The model investigates read-only, presents a plan via exit_plan, the
    // printer below auto-approves it, and the same turn proceeds to execute
    // — a natural two-phase turn with no human in the loop.
    if config.plan_first {
        agent.set_plan_mode(true);
    }
    // `--omakase` / `omakase = true`: chef's choice. It implies plan mode (set
    // just above, since `apply_cli` turns `plan_first` on with it) and adds the
    // prompt that tells the model to decide for itself rather than come back
    // with questions, plus the `interview` tool's omakase behaviour.
    //
    // This was the one surface that never applied it. The TUI, the GUI and the
    // gateway all call `set_omakase`; here only `plan_first` was honoured, so
    // `wizard --omakase "..."` in sovereign, headless and continuous ran plain
    // plan mode and the flag did nothing at all.
    if config.omakase {
        agent.set_omakase(true);
    }

    // Dashboard-dispatched background session (`--bg`): register in the session
    // registry and keep a heartbeat ticking so `/dashboard` shows it as a live
    // "Working" row. The terminal state is written once the run ends, below.
    let bg_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut bg_record: Option<crate::session_registry::SessionRecord> = None;
    let mut bg_ticker: Option<tokio::task::JoinHandle<()>> = None;
    if cli.bg {
        let headline = goal
            .lines()
            .next()
            .unwrap_or("background run")
            .chars()
            .take(48)
            .collect::<String>();
        let record = crate::session_registry::SessionRecord {
            id: agent.session().id.clone(),
            name: if headline.is_empty() {
                "background run".to_string()
            } else {
                headline.clone()
            },
            cwd: project_root.display().to_string(),
            model: model.clone(),
            mode: "sovereign".to_string(),
            state: crate::session_registry::SessionState::Working,
            activity: format!("working: {headline}"),
            pid: std::process::id(),
            started_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            updated_unix: 0,
        };
        crate::session_registry::write(&record);
        let stop = Arc::clone(&bg_stop);
        let ticker_record = record.clone();
        bg_ticker = Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(3)).await;
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                crate::session_registry::write(&ticker_record);
            }
        }));
        bg_record = Some(record);
    }

    // Busy spinner ("Conjuring…") shown while the model thinks or a tool
    // runs, hidden while output streams. Shared with the text sink; a
    // no-op when stderr is not a terminal. The structured formats never
    // show it and keep stdout pure JSON (`text_mode` gates every plain
    // stdout line below).
    let spinner = Arc::new(crate::progress::TurnSpinner::new());
    let text_mode = cli.output_format == crate::output::OutputFormat::Text;

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    // The sink consumes every agent event off the run loop; it is returned
    // so `finish` can emit the run summary once the outcome is known.
    let mut sink: Box<dyn crate::output::EventSink> = match cli.output_format {
        crate::output::OutputFormat::Text => {
            Box::new(crate::output::TextSink::new(Arc::clone(&spinner)))
        }
        crate::output::OutputFormat::Json => Box::new(crate::output::JsonSink::stdout()),
        crate::output::OutputFormat::StreamJson => {
            Box::new(crate::output::StreamJsonSink::stdout())
        }
    };
    let printer = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            sink.event(event);
        }
        sink
    });

    if text_mode {
        crate::output::print_line(&format!(
            "wizard {} — model {model} @ {endpoint} — task: {goal}",
            config.mode
        ));
        if let Some(gates) = gates.as_ref() {
            crate::output::print_line(&format!(
                "gates (all must exit 0 before this run may finish): {}",
                gates
                    .commands()
                    .iter()
                    .map(|command| format!("`{command}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }

    // session_start hooks fire once for the whole run.
    agent.fire_session_start(&tx).await;

    // Continuous mode persists a long-lived mission so the loop survives
    // restarts and binary self-replacement (deep evolve re-exec).
    let mut mission_state = if config.continuous {
        let mission = match mission::Mission::load(&project_root)? {
            Some(existing) => existing,
            None => {
                let fresh = mission::Mission::new(goal.clone());
                fresh.save(&project_root)?;
                fresh
            }
        };
        Some(mission)
    } else {
        None
    };
    // A fresh process starts with a clean streak. The failures that produced a
    // stored count belong to a run that is over; the notes still record them,
    // but inheriting the number would let a run that failed four times last
    // week give up on its first stumble today.
    if let Some(mission) = mission_state.as_mut() {
        mission.clear_failures();
        mission.stamp("run started");
        persist(mission, &project_root);
    }

    let max_iterations = cli.loop_limit.unwrap_or(1).max(1);
    let mut input = goal.clone();
    let mut final_reason = DoneReason::Completed;
    let mut run_error: Option<anyhow::Error> = None;
    // Set when a self-evolve marker is consumed: after draining the printer we
    // re-exec into the freshly built/extended binary.
    let mut reexec_after = false;
    let mut iteration: u32 = 0;
    // Iterations spent fixing a failing gate rather than advancing the task.
    // They extend the `--loop` bound instead of consuming it: `--loop` is the
    // budget for the *work*, and its default of 1 would otherwise mean a gate
    // could report a failure and never be given a turn to have it fixed, which
    // is a gate that does nothing but slow the run down. The remediation turns
    // have their own bound (`gate_max_attempts`) and the same wall clock as
    // everything else.
    let mut gate_iterations: u32 = 0;
    // Cycles that ended in a hard error or a tripped breaker since the last
    // one that landed. Mirrored into the mission so it is visible from outside
    // the process; the local copy is what the bound is checked against.
    let mut failure_streak: u32 = 0;
    // Consecutive `PLATEAU` verdicts from the goal critic. Two in a row end a
    // continuous run: the critic can name no gap another round would close, so
    // there is nothing left for the loop to do but report it (see
    // `crate::agent::goal_critic`).
    let mut plateau_streak: u32 = 0;
    // Raised by SIGTERM/SIGHUP/SIGINT. The handler also cancels the turn in
    // flight, so this is read at the boundary to stop the *next* cycle from
    // starting — a signal that lands between cycles has no turn to cancel, and
    // the cancel flag resets itself at the start of the next turn.
    install_shutdown_signals(agent.cancel_handle());

    loop {
        iteration += 1;
        if !config.continuous && iteration > max_iterations.saturating_add(gate_iterations) {
            break;
        }

        // The wall clock, at the cycle boundary. The agent enforces it between
        // steps, which means a run whose deadline passes during a pause, a
        // backoff, or a long-running final tool call would otherwise start a
        // whole new cycle before noticing.
        if deadline_passed(deadline, Instant::now()) {
            final_reason = DoneReason::TimeLimit;
            break;
        }

        // A signal that arrived during the last cycle, or in the gap since it
        // ended. Checked here as well as through the cancel handle because a
        // signal landing between cycles has no turn to interrupt.
        if shutdown_requested() {
            if text_mode {
                spinner.println("[signalled: stopping after the current cycle]");
            }
            final_reason = DoneReason::Stopped;
            break;
        }

        // Operator control at the cycle boundary — all three commands, not
        // just `stop`. `pause` and `skip` are handled inside a turn, between
        // steps; written into the gap between cycles they used to be read by
        // nothing and cleared by the next turn's first step.
        let gate = match read_loop_control(&project_root) {
            Some(LoopControl::Stop) => CycleGate::Stop,
            Some(LoopControl::Pause) => {
                if text_mode {
                    spinner.println(
                        "[held by .wizard/loop-control: write `resume` or remove the file]",
                    );
                }
                stamp(
                    mission_state.as_mut(),
                    &project_root,
                    "held by operator pause (.wizard/loop-control)",
                );
                await_release(&project_root, deadline, PAUSE_TICK, &SHUTDOWN).await
            }
            Some(LoopControl::Skip) => {
                clear_loop_control(&project_root);
                CycleGate::Skip
            }
            None => CycleGate::Proceed,
        };
        match gate {
            CycleGate::Proceed => {}
            CycleGate::Stop => {
                clear_loop_control(&project_root);
                final_reason = DoneReason::Stopped;
                break;
            }
            CycleGate::Deadline => {
                final_reason = DoneReason::TimeLimit;
                break;
            }
            CycleGate::Skip => {
                input = format!("{SKIP_AT_CYCLE_BOUNDARY}\n\n{input}");
            }
        }
        if config.continuous {
            if text_mode {
                spinner.println(&format!("\n=== cycle {iteration} ==="));
            }
            // `plan_each_cycle = true`: every cycle starts by planning again
            // (the previous cycle's exit_plan approval cleared the flag).
            if config.plan_each_cycle {
                agent.set_plan_mode(true);
            }
        } else if max_iterations > 1 && text_mode {
            spinner.println(&format!("\n=== iteration {iteration}/{max_iterations} ==="));
        }

        // Fresh verb per turn (same mechanism as the TUI's busy spinner), so
        // one turn reads as one activity. Structured formats never spin.
        if text_mode {
            let verb_seed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos() as u64)
                .wrapping_add(u64::from(iteration));
            spinner.set_verb(config.ui.spinner_verb(verb_seed));
            spinner.show();
        }

        // Surface background tasks that finished between cycles (the turn
        // loop also drains at the top of every step).
        agent.drain_background(&tx).await;

        // First checkpoint turn of this cycle, for rollback_failed_cycles
        // (run_turn assigns the next id via begin_turn).
        let cycle_first_turn = agent.checkpoints().current_turn() + 1;
        // Images only ride on the first cycle's initial user prompt; later
        // continuation prompts are pure text.
        let turn_images = if iteration == 1 {
            goal_images.clone()
        } else {
            Vec::new()
        };
        // Set by a failed cycle a perpetual run intends to survive: the prompt
        // the next cycle opens with, and how long to wait before starting it.
        let mut recovery: Option<(String, Duration)> = None;
        match agent
            .run_turn_with_images(&input, turn_images, tx.clone())
            .await
        {
            Ok(reason) => {
                final_reason = reason;
                match reason {
                    DoneReason::MaxSteps => {
                        // The turn spent its step budget, which means it did
                        // real work and simply ran out of room. Not a failure:
                        // the streak clears.
                        failure_streak = 0;
                        if let Some(mission) = mission_state.as_mut() {
                            mission.clear_failures();
                            mission.stamp(format!("cycle {iteration}: step budget reached"));
                            persist(mission, &project_root);
                        }
                        input = CONTINUE_AFTER_MAX_STEPS.to_string();
                    }
                    DoneReason::Completed => {
                        // "Done" is a claim, not evidence. With gates
                        // configured, the claim has to survive them before the
                        // loop is allowed to act on it, in continuous mode
                        // too, where acting on it means recording a completed
                        // cycle and moving the mission on.
                        let decision = match gates.as_mut() {
                            Some(gates) => gates.check(deadline, &tx).await,
                            None => crate::gates::GateDecision::Finish,
                        };
                        match decision {
                            crate::gates::GateDecision::Retry(prompt) => {
                                gate_iterations = gate_iterations.saturating_add(1);
                                stamp(
                                    mission_state.as_mut(),
                                    &project_root,
                                    format!("cycle {iteration}: a quality gate failed"),
                                );
                                input = prompt;
                            }
                            crate::gates::GateDecision::GiveUp => {
                                // The gates are still failing and there is no
                                // budget left to fix them. Reaching a limit is
                                // not success: the reported reason stays the
                                // truthful one and the exit code comes from
                                // the gate verdict (see `gates::exit_code`).
                                if deadline_passed(deadline, Instant::now()) {
                                    final_reason = DoneReason::TimeLimit;
                                }
                                break;
                            }
                            crate::gates::GateDecision::Finish => {
                                if config.continuous {
                                    // "Done" survived the gates; now it must
                                    // survive an INDEPENDENT critic before the
                                    // cycle is allowed to land. A goal loop that
                                    // trusts the builder's own verdict is
                                    // grading its own homework — the critic is
                                    // a fresh subagent that never saw this turn.
                                    stamp(
                                        mission_state.as_mut(),
                                        &project_root,
                                        format!(
                                            "cycle {iteration}: verifying with an independent critic"
                                        ),
                                    );
                                    match agent.critique_goal(&goal).await {
                                        Ok(verdict) => {
                                            if text_mode {
                                                spinner.println(&format!(
                                                    "[critic: {}]",
                                                    verdict.summary()
                                                ));
                                            }
                                            plateau_streak = match &verdict {
                                                goal_critic::GoalVerdict::Plateau => {
                                                    plateau_streak + 1
                                                }
                                                _ => 0,
                                            };
                                            match goal_critic::plan_after_verdict(
                                                &verdict,
                                                plateau_streak,
                                            ) {
                                                goal_critic::CriticAction::Accept => {
                                                    // Verified done: record the
                                                    // cycle and self-direct the
                                                    // next action. Never idle.
                                                    failure_streak = 0;
                                                    let cycles = match mission_state.as_mut() {
                                                        Some(mission) => {
                                                            mission.record_cycle(Some(
                                                                "cycle done: critic returned OURS"
                                                                    .to_string(),
                                                            ));
                                                            mission.stamp(format!(
                                                                "cycle {iteration}: completed \
                                                                 (critic OURS)"
                                                            ));
                                                            persist(mission, &project_root);
                                                            mission.cycles
                                                        }
                                                        // Unreachable while
                                                        // `continuous` implies a
                                                        // mission, but a missing
                                                        // one must not leave
                                                        // `input` unchanged and
                                                        // re-issue the goal
                                                        // verbatim forever.
                                                        None => u64::from(iteration),
                                                    };
                                                    input = continuation_prompt(&goal, cycles);
                                                }
                                                goal_critic::CriticAction::Rework(prompt) => {
                                                    // Not done. The cycle does
                                                    // not land; the builder gets
                                                    // the one gap and tries
                                                    // again. Not a failure
                                                    // either — the streak that
                                                    // trips the breaker is for
                                                    // errors, not honest rework.
                                                    stamp(
                                                        mission_state.as_mut(),
                                                        &project_root,
                                                        format!(
                                                            "cycle {iteration}: critic sent it \
                                                             back — {}",
                                                            verdict.summary()
                                                        ),
                                                    );
                                                    input = prompt;
                                                }
                                                goal_critic::CriticAction::Stop(why) => {
                                                    if text_mode {
                                                        spinner.println(&format!(
                                                            "[goal loop stopping: {why}]"
                                                        ));
                                                    }
                                                    stamp(
                                                        mission_state.as_mut(),
                                                        &project_root,
                                                        format!("cycle {iteration}: {why}"),
                                                    );
                                                    final_reason = DoneReason::Completed;
                                                    break;
                                                }
                                            }
                                        }
                                        Err(err) => {
                                            // A critic that could not run is not
                                            // a pass. Fail the cycle so the
                                            // backoff and breaker apply, rather
                                            // than landing unverified work.
                                            failure_streak += 1;
                                            recovery = record_failed_cycle(
                                                &config,
                                                mission_state.as_mut(),
                                                &project_root,
                                                &goal,
                                                failure_streak,
                                                "the goal critic could not run",
                                                &format!("{err:#}"),
                                                0,
                                            );
                                            if recovery.is_none() {
                                                run_error = Some(err.context(format!(
                                                    "continuous run gave up after \
                                                     {failure_streak} consecutive failed cycles"
                                                )));
                                                break;
                                            }
                                        }
                                    }
                                } else {
                                    break;
                                }
                            }
                        }
                    }
                    DoneReason::Stopped | DoneReason::TimeLimit => {
                        break;
                    }
                    DoneReason::CircuitBreaker => {
                        rollback_failed_cycle(
                            &config,
                            &agent,
                            mission_state.as_mut(),
                            &project_root,
                            cycle_first_turn,
                            "circuit breaker",
                            text_mode.then_some(&*spinner),
                        );
                        if !config.continuous {
                            break;
                        }
                        // The breaker is self-healing by construction: it
                        // refuses calls for a cooldown and then admits one
                        // recovery probe. Ending a perpetual run on it turned
                        // a designed-in thirty-second outage into a permanent
                        // one, and contradicted what `docs/modes.md` promised.
                        //
                        // One caveat this loop cannot fix from here: the *tool*
                        // failure counters in `crate::dispatch` are the
                        // agent's, they survive the end of a turn, and nothing
                        // outside `Agent::clear` / `Agent::rewind_to` clears
                        // them. When the trip came from a tool rather than the
                        // provider, the recovery cycle starts with the counter
                        // still at its threshold, so it survives only until its
                        // first tool error. That is why the recovery prompt
                        // insists on a materially different approach — a cycle
                        // whose first tool call succeeds resets the counter,
                        // and one that repeats the failing call does not
                        // deserve to continue anyway. An `Agent` method to
                        // clear them outright is requested; see the branch
                        // notes.
                        failure_streak += 1;
                        let detail = "the circuit breaker opened — the provider failed \
                                      repeatedly, or one tool failed the same way over and over";
                        recovery = record_failed_cycle(
                            &config,
                            mission_state.as_mut(),
                            &project_root,
                            &goal,
                            failure_streak,
                            "a tripped circuit breaker",
                            detail,
                            BREAKER_COOLDOWN_SECS,
                        );
                        if recovery.is_none() {
                            final_reason = DoneReason::CircuitBreaker;
                            break;
                        }
                    }
                }
            }
            Err(err) => {
                rollback_failed_cycle(
                    &config,
                    &agent,
                    mission_state.as_mut(),
                    &project_root,
                    cycle_first_turn,
                    "hard error",
                    text_mode.then_some(&*spinner),
                );
                if !config.continuous {
                    run_error = Some(err);
                    break;
                }
                // A perpetual run outlives its own errors. One malformed tool
                // call or one unreadable path is a failed cycle, not the end
                // of a mission that was asked to run forever; only a streak of
                // them with nothing landing in between is evidence the setup
                // itself is broken.
                failure_streak += 1;
                let detail = format!("{err:#}");
                recovery = record_failed_cycle(
                    &config,
                    mission_state.as_mut(),
                    &project_root,
                    &goal,
                    failure_streak,
                    "a hard error",
                    &detail,
                    0,
                );
                if recovery.is_none() {
                    run_error = Some(err.context(format!(
                        "continuous run gave up after {failure_streak} consecutive failed cycles"
                    )));
                    break;
                }
            }
        }

        // A failed cycle a perpetual run intends to survive: say so, wait, and
        // open the next cycle with a prompt that names what went wrong.
        if let Some((next_input, wait)) = recovery {
            if text_mode {
                spinner.println(&format!(
                    "[cycle {iteration} failed ({failure_streak} in a row); retrying in {}s]",
                    wait.as_secs()
                ));
            }
            stamp(
                mission_state.as_mut(),
                &project_root,
                format!(
                    "cycle {iteration}: backing off {}s after a failed cycle",
                    wait.as_secs()
                ),
            );
            match wait_awake(&project_root, wait, deadline, WAIT_TICK, &SHUTDOWN).await {
                Wake::Elapsed => {}
                Wake::Stop => {
                    clear_loop_control(&project_root);
                    final_reason = DoneReason::Stopped;
                    break;
                }
                Wake::Deadline => {
                    final_reason = DoneReason::TimeLimit;
                    break;
                }
            }
            input = next_input;
            continue;
        }

        // After the turn, react to self-evolution markers: a deep rebuild
        // (`evolve-reexec`) or a tier-1 extension (`evolve-reload`) both mean
        // the running image is stale, so we re-exec to reload everything.
        // Only meaningful in continuous mode, where the persisted mission lets
        // the relaunched process resume without a `-p` goal; a one-shot run
        // just finishes and the next launch picks up the new binary.
        let reexec = mission::reexec_marker(&project_root);
        let reload = mission::reload_marker(&project_root);
        if config.continuous && (reexec.exists() || reload.exists()) {
            if let Some(mission) = mission_state.as_mut() {
                mission.stamp(format!("cycle {iteration}: re-exec after self-evolve"));
                persist(mission, &project_root);
            }
            let _ = std::fs::remove_file(&reexec);
            let _ = std::fs::remove_file(&reload);
            reexec_after = true;
            break;
        }

        if config.cycle_pause_secs > 0 {
            stamp(
                mission_state.as_mut(),
                &project_root,
                format!(
                    "cycle {iteration}: idle pause ({}s)",
                    config.cycle_pause_secs
                ),
            );
            match wait_awake(
                &project_root,
                Duration::from_secs(config.cycle_pause_secs),
                deadline,
                WAIT_TICK,
                &SHUTDOWN,
            )
            .await
            {
                Wake::Elapsed => {}
                Wake::Stop => {
                    clear_loop_control(&project_root);
                    final_reason = DoneReason::Stopped;
                    break;
                }
                Wake::Deadline => {
                    final_reason = DoneReason::TimeLimit;
                    break;
                }
            }
        }
    }

    // Last stamp of the run, so a mission file left behind says how it ended
    // rather than freezing on whatever the final cycle was doing.
    if let Some(mission) = mission_state.as_mut() {
        mission.stamp(match (&run_error, reexec_after) {
            (Some(err), _) => format!(
                "run ended with an error: {}",
                brief(&format!("{err:#}"), 200)
            ),
            (None, true) => "run handing off to a re-exec".to_string(),
            (None, false) => format!("run ended: {final_reason:?}"),
        });
        persist(mission, &project_root);
    }

    // The gate verdict is the run's last word on whether the work is verified,
    // and it has to be legible on every output format. The notice covers text
    // and stream-json; the json sink keeps stdout to one summary object and
    // drops notices, so its consumer gets the line on stderr next to the exit
    // code it will actually branch on.
    if let Some(gates) = gates.as_ref()
        && let Some(summary) = gates.summary(final_reason)
    {
        let _ = tx.send(AgentEvent::Notice(summary.clone())).await;
        if !text_mode {
            crate::output::eprint_line(&format!("wizard: {summary}"));
        }
    }

    // session_end hooks fire however the run ended (including just before a
    // self-evolve re-exec replaces the process).
    agent.fire_session_end(Some(&tx)).await;

    drop(tx);
    // A printer that died takes its sink with it, and with the sink goes the
    // run summary. That is worth a word on stderr, but not the run: by this
    // point every cycle has already happened, so returning an error here would
    // throw away a completed mission's exit code over a failure to *narrate*
    // it. The outcome still reaches the caller through `final_reason`.
    let mut sink = match printer.await {
        Ok(sink) => Some(sink),
        Err(err) => {
            crate::output::eprint_line(&format!(
                "wizard: output task ended abnormally ({err}); the run summary is missing"
            ));
            None
        }
    };
    spinner.finish();

    // Background session: stop the heartbeat and record the terminal state so
    // the dashboard shows the result (completed/failed) rather than the row
    // vanishing. The terminal record is retained (not removed) by the registry.
    if let Some(mut record) = bg_record.take() {
        bg_stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(ticker) = bg_ticker.take() {
            ticker.abort();
        }
        if run_error.is_some() {
            record.state = crate::session_registry::SessionState::Failed;
            record.activity = "failed".to_string();
        } else {
            record.state = crate::session_registry::SessionState::Completed;
            record.activity = "completed".to_string();
        }
        crate::session_registry::write(&record);
    }

    if reexec_after {
        use std::os::unix::process::CommandExt;
        match reexec_args(
            &project_root,
            remaining_hours(deadline, Instant::now()),
            cli.output_format,
        ) {
            Some(args) => {
                let exe =
                    std::env::current_exe().context("locating current executable for re-exec")?;
                if text_mode {
                    crate::output::print_line(&format!(
                        "[re-exec into evolved binary {}]",
                        exe.display()
                    ));
                }
                let err = std::process::Command::new(exe).args(&args).exec(); // never returns on success
                return Err(anyhow::anyhow!("re-exec after evolve failed: {err}"));
            }
            None => {
                // The evolved binary is on disk and the markers are consumed;
                // the next launch picks it up. Relaunching now would only
                // rebuild an agent for a deadline that has already passed.
                if text_mode {
                    crate::output::print_line(
                        "[skipping re-exec: the run's time limit has run out]",
                    );
                }
                final_reason = DoneReason::TimeLimit;
            }
        }
    }

    if let Some(err) = run_error {
        return Err(err);
    }
    // The sink emits the run summary (the text trailer line, or the final
    // JSON object / `done` JSONL line) and leaves stdout flushed. Absent only
    // when the printer task died, which was reported above.
    if let Some(sink) = sink.as_mut() {
        sink.finish(final_reason);
    }
    // Gates outrank the loop's own reason for stopping: a caller scripting
    // `wizard … && deploy` is asking whether the work is verified, not why the
    // loop exited.
    Ok(crate::gates::exit_code(final_reason, gates.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Temp project dir removed on drop.
    struct TempProject(PathBuf);

    impl TempProject {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "wizard-headless-test-{}-{}",
                std::process::id(),
                n
            ));
            std::fs::create_dir_all(dir.join(".wizard")).expect("create temp project");
            Self(dir)
        }

        fn control(&self, value: &str) {
            std::fs::write(self.0.join(".wizard").join("loop-control"), value)
                .expect("write loop-control");
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn continuous_config() -> Config {
        Config {
            continuous: true,
            retry_base_secs: 5,
            retry_max_secs: 300,
            ..Config::default()
        }
    }

    /// `--omakase` reaches the agent on this surface.
    ///
    /// `apply_cli` turns `--omakase` into `omakase = true` *and*
    /// `plan_first = true`, and this file honoured only the second. So
    /// `wizard --omakase "…"`, and every sovereign, headless and continuous
    /// run configured with `omakase = true`, got plain plan mode: no omakase
    /// system prompt, no chef's-choice `interview` behaviour, no error, no
    /// warning.
    ///
    /// The `/omakase` slash handlers (`app/command.rs`, `gui/command.rs`,
    /// `gateway/command.rs`) always called `set_omakase` — it was startup
    /// wiring that dropped the flag, and it dropped it on more than one
    /// surface: `app/runtime.rs` and `gateway/mod.rs` had the same hole, each
    /// guarded by its own copy of this test.
    ///
    /// Grep, in the manner of `every_shared_registry_handle_is_held_by_a_surface`:
    /// the defect is the *absence* of a call, which nothing observable at
    /// runtime can distinguish from a run that simply was not asked for
    /// omakase.
    #[test]
    fn the_headless_runner_applies_omakase_and_not_only_plan_mode() {
        let source = include_str!("headless.rs");
        let (production, _) = source
            .split_once("#[cfg(test)]\nmod tests {")
            .expect("this module ends with its test module");
        assert!(
            production.contains("config.plan_first"),
            "plan_first is still read here"
        );
        assert!(
            production.contains("agent.set_omakase("),
            "--omakase must reach the agent on this surface too, not just --plan"
        );
    }

    /// Quality gates reach the finish path, the loop bound, and the exit code.
    ///
    /// Grep, for the same reason as the omakase test above: the defect is an
    /// *absent* call. A `Gates` that is built and never consulted is a run that
    /// prints its gate list at startup, never runs a gate, and exits 0, which
    /// is indistinguishable at runtime from a run whose gates all passed, and
    /// is exactly the "finished but not verified" outcome the feature exists to
    /// make impossible.
    #[test]
    fn the_headless_runner_actually_checks_its_gates() {
        let source = include_str!("headless.rs");
        let (production, _) = source
            .split_once("#[cfg(test)]\nmod tests {")
            .expect("this module ends with its test module");
        assert!(
            production.contains("Gates::for_run("),
            "the run has to build its gate set"
        );
        assert!(
            production.contains("gates.check(deadline, &tx)"),
            "a built gate set that is never checked is worse than none"
        );
        assert!(
            production.contains("crate::gates::exit_code(final_reason, gates.as_ref())"),
            "a failing gate has to reach the process exit code"
        );
        assert!(
            production.contains("max_iterations.saturating_add(gate_iterations)"),
            "gate remediation must not be capped by the default --loop of 1"
        );
    }

    /// A run with no gates configured exits exactly as it always did.
    #[test]
    fn a_run_without_gates_keeps_the_exit_codes_it_always_had() {
        for reason in [
            DoneReason::Completed,
            DoneReason::MaxSteps,
            DoneReason::TimeLimit,
            DoneReason::Stopped,
            DoneReason::CircuitBreaker,
        ] {
            assert_eq!(
                crate::gates::exit_code(reason, None),
                crate::output::exit_code(reason),
                "{reason:?} must not change for the runs that configured no gate"
            );
        }
    }

    #[test]
    fn deadline_only_bites_when_one_was_set() {
        let now = Instant::now();
        assert!(!deadline_passed(None, now), "no --max-hours never expires");
        assert!(deadline_passed(Some(now), now));
        assert!(deadline_passed(Some(now - Duration::from_secs(1)), now));
        assert!(!deadline_passed(Some(now + Duration::from_secs(1)), now));
    }

    #[test]
    fn remaining_hours_saturates_at_zero_rather_than_going_negative() {
        let now = Instant::now();
        assert_eq!(remaining_hours(None, now), None);
        let left = remaining_hours(Some(now + Duration::from_secs(1800)), now).expect("a deadline");
        assert!((left - 0.5).abs() < 1e-3, "half an hour left, got {left}");
        assert_eq!(
            remaining_hours(Some(now - Duration::from_secs(600)), now),
            Some(0.0),
            "an expired deadline must not report negative hours to --max-hours"
        );
    }

    #[test]
    fn failure_backoff_climbs_caps_and_honours_its_floor() {
        // The ladder doubles per consecutive failure.
        assert_eq!(failure_backoff(1, 5, 300, 0), Duration::from_secs(5));
        assert_eq!(failure_backoff(2, 5, 300, 0), Duration::from_secs(10));
        assert_eq!(failure_backoff(4, 5, 300, 0), Duration::from_secs(40));
        // ...and stops at the configured cap rather than overflowing.
        assert_eq!(failure_backoff(30, 5, 300, 0), Duration::from_secs(300));
        assert_eq!(
            failure_backoff(u32::MAX, 5, 300, 0),
            Duration::from_secs(300)
        );
        // A zero base must not spin the outer loop.
        assert_eq!(
            failure_backoff(1, 0, 0, 0),
            Duration::from_secs(MIN_FAILURE_BACKOFF_SECS)
        );
        // The breaker's cooldown wins over a short ladder: starting sooner
        // just gets the first model call refused without reaching the network.
        assert_eq!(
            failure_backoff(1, 1, 2, BREAKER_COOLDOWN_SECS),
            Duration::from_secs(BREAKER_COOLDOWN_SECS)
        );
    }

    #[test]
    fn brief_truncates_on_a_character_boundary() {
        assert_eq!(brief("  short  ", 10), "short");
        // Multi-byte throughout: a byte-wise cut here would panic.
        let wide = "é".repeat(50);
        let cut = brief(&wide, 10);
        assert_eq!(cut.chars().count(), 11, "10 kept plus the ellipsis");
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn a_perpetual_run_survives_a_failure_and_gives_up_on_a_streak() {
        let config = continuous_config();
        assert_eq!(config.max_consecutive_failures, 5);

        for streak in 1..5 {
            let (prompt, wait) =
                plan_recovery(&config, "harden it", 12, streak, "a hard error", "boom", 0)
                    .expect("a run asked to go forever survives one bad cycle");
            assert!(prompt.contains("did NOT complete"));
            assert!(wait >= Duration::from_secs(config.retry_base_secs));
        }
        assert!(
            plan_recovery(&config, "harden it", 12, 5, "a hard error", "boom", 0).is_none(),
            "five failures with nothing landing in between is a broken setup, not bad luck"
        );
    }

    #[test]
    fn zero_max_consecutive_failures_never_gives_up() {
        let config = Config {
            max_consecutive_failures: 0,
            ..continuous_config()
        };
        assert!(
            plan_recovery(&config, "endure", 3, 10_000, "a hard error", "boom", 0).is_some(),
            "0 disables the bound; it does not mean give up immediately"
        );
    }

    #[test]
    fn a_recovery_prompt_names_the_failure_and_distrusts_the_conversation() {
        let prompt = recovery_prompt(
            "harden it",
            7,
            2,
            "a tripped circuit breaker",
            "provider returned 503 eleven times",
            true,
        );
        assert!(prompt.contains("harden it"), "the mission is restated");
        assert!(prompt.contains("a tripped circuit breaker"));
        assert!(prompt.contains("provider returned 503 eleven times"));
        assert!(prompt.contains("2 failed cycle(s) in a row"));
        assert!(prompt.contains("ROLLED BACK"), "rollback must be disclosed");
        assert!(prompt.contains("MATERIALLY DIFFERENT"));
        assert!(
            !prompt.contains("sub-task complete"),
            "the happy-path text is exactly what taught the model to repeat itself"
        );

        // With rollback off, the model is told the opposite thing about disk.
        let kept = recovery_prompt("harden it", 7, 1, "a hard error", "boom", false);
        assert!(!kept.contains("ROLLED BACK"));
        assert!(kept.contains("left on disk"));
    }

    #[test]
    fn a_long_error_chain_cannot_swamp_the_recovery_prompt() {
        let huge = "x".repeat(20_000);
        let prompt = recovery_prompt("m", 1, 1, "a hard error", &huge, false);
        assert!(prompt.len() < 2_000, "prompt was {} bytes", prompt.len());
        assert!(prompt.contains('…'));
    }

    #[test]
    fn the_happy_path_prompt_still_quotes_the_cycle_count() {
        let prompt = continuation_prompt("ship it", 42);
        assert!(prompt.contains("cycle 42"));
        assert!(prompt.contains("ship it"));
        assert!(prompt.contains("Never idle"));
    }

    #[test]
    fn reexec_carries_the_runs_terms_and_refuses_when_the_clock_is_gone() {
        let root = Path::new("/tmp/proj");

        // No deadline: the command line is what it always was.
        let args =
            reexec_args(root, None, OutputFormat::Text).expect("no deadline, always re-exec");
        assert_eq!(
            args,
            vec!["--mode", "sovereign", "--continuous", "--cwd", "/tmp/proj"]
        );

        // A deadline rides across, so an 8-hour run that evolves at hour one
        // does not come back immortal.
        let args = reexec_args(root, Some(7.0), OutputFormat::Text).expect("plenty of time left");
        let at = args
            .iter()
            .position(|a| a == "--max-hours")
            .expect("carried");
        assert_eq!(args[at + 1], "7.000000");

        // So does the output format, or a JSON consumer gets prose mid-stream.
        let args =
            reexec_args(root, Some(7.0), OutputFormat::StreamJson).expect("plenty of time left");
        let at = args
            .iter()
            .position(|a| a == "--output-format")
            .expect("carried");
        assert_eq!(args[at + 1], "stream-json");

        // Nearly out of time: relaunching costs more than it can ever return.
        assert!(reexec_args(root, Some(0.0), OutputFormat::Text).is_none());
        assert!(reexec_args(root, Some(10.0 / 3600.0), OutputFormat::Text).is_none());
    }

    // These wait tests run on the real clock on purpose. Tokio's paused clock
    // only fast-forwards its own timers, and the deadline these waits compare
    // against is a `std::time::Instant` handed down from `--max-hours`, so a
    // paused runtime would spin the loop for the full wall-clock duration
    // instead of skipping it. Short durations keep them honest and quick.

    #[tokio::test]
    async fn an_inter_cycle_wait_runs_its_course_when_nothing_intervenes() {
        let project = TempProject::new();
        let started = Instant::now();
        let wake = wait_awake(
            &project.0,
            Duration::from_millis(120),
            None,
            Duration::from_millis(10),
            &AtomicBool::new(false),
        )
        .await;
        assert_eq!(wake, Wake::Elapsed);
        assert!(
            started.elapsed() >= Duration::from_millis(120),
            "ticking must not cut the wait short"
        );
    }

    #[tokio::test]
    async fn an_inter_cycle_wait_obeys_the_kill_switch() {
        let project = TempProject::new();
        project.control("stop");
        let wake = wait_awake(
            &project.0,
            Duration::from_secs(86_400),
            None,
            Duration::from_millis(10),
            &AtomicBool::new(false),
        )
        .await;
        assert_eq!(
            wake,
            Wake::Stop,
            "a day-long pause must not swallow `stop` until it ends"
        );
    }

    #[tokio::test]
    async fn an_inter_cycle_wait_obeys_a_shutdown_signal() {
        let project = TempProject::new();
        let wake = wait_awake(
            &project.0,
            Duration::from_secs(86_400),
            None,
            Duration::from_millis(10),
            &AtomicBool::new(true),
        )
        .await;
        assert_eq!(
            wake,
            Wake::Stop,
            "a SIGTERM during a backoff must not wait out the backoff"
        );
    }

    #[tokio::test]
    async fn a_shutdown_signal_outranks_an_operator_hold() {
        let project = TempProject::new();
        project.control("pause");
        assert_eq!(
            await_release(
                &project.0,
                None,
                Duration::from_millis(10),
                &AtomicBool::new(true),
            )
            .await,
            CycleGate::Stop,
            "a paused run is still a live process; `systemctl stop` must not \
             block on the hold until it is SIGKILLed"
        );
    }

    #[tokio::test]
    async fn an_inter_cycle_wait_stops_at_the_deadline_it_would_have_overrun() {
        let project = TempProject::new();
        let deadline = Instant::now() + Duration::from_millis(60);
        let wake = wait_awake(
            &project.0,
            Duration::from_secs(3_600),
            Some(deadline),
            Duration::from_millis(10),
            &AtomicBool::new(false),
        )
        .await;
        assert_eq!(
            wake,
            Wake::Deadline,
            "a one-hour pause inside a run with five seconds left is not allowed to finish"
        );
    }

    #[tokio::test]
    async fn a_hold_releases_when_the_control_file_is_cleared() {
        let project = TempProject::new();
        // No `pause` on disk: nothing to hold for.
        assert_eq!(
            await_release(
                &project.0,
                None,
                Duration::from_millis(10),
                &AtomicBool::new(false)
            )
            .await,
            CycleGate::Proceed
        );

        // `resume` parses as no command, which is the documented release.
        project.control("resume");
        assert_eq!(
            await_release(
                &project.0,
                None,
                Duration::from_millis(10),
                &AtomicBool::new(false)
            )
            .await,
            CycleGate::Proceed
        );
    }

    #[tokio::test]
    async fn a_hold_re_dispatches_to_stop_and_skip() {
        let project = TempProject::new();
        project.control("stop");
        assert_eq!(
            await_release(
                &project.0,
                None,
                Duration::from_millis(10),
                &AtomicBool::new(false)
            )
            .await,
            CycleGate::Stop,
            "an operator who gives up on a paused run must not have to un-pause it first"
        );

        project.control("skip");
        assert_eq!(
            await_release(
                &project.0,
                None,
                Duration::from_millis(10),
                &AtomicBool::new(false)
            )
            .await,
            CycleGate::Skip
        );
        assert!(
            read_loop_control(&project.0).is_none(),
            "a consumed one-shot command is cleared"
        );
    }

    #[tokio::test]
    async fn a_hold_still_ends_at_the_runs_deadline() {
        let project = TempProject::new();
        project.control("pause");
        let deadline = Instant::now() + Duration::from_millis(60);
        assert_eq!(
            await_release(
                &project.0,
                Some(deadline),
                Duration::from_millis(10),
                &AtomicBool::new(false)
            )
            .await,
            CycleGate::Deadline,
            "`pause` must not outlive --max-hours"
        );
    }

    #[test]
    fn a_failed_cycle_is_recorded_in_the_mission_without_counting_as_progress() {
        let project = TempProject::new();
        let config = continuous_config();
        let mut mission = mission::Mission::new("harden it");
        mission.record_cycle(Some("first".to_string()));

        let plan = record_failed_cycle(
            &config,
            Some(&mut mission),
            &project.0,
            "harden it",
            1,
            "a hard error",
            "the disk went away",
            0,
        );
        assert!(plan.is_some(), "one error does not end a perpetual run");
        assert_eq!(mission.cycles, 1, "a failure is not a completed cycle");
        assert_eq!(mission.consecutive_failures, 1);
        assert!(
            mission
                .notes
                .last()
                .expect("a note")
                .contains("the disk went away")
        );

        let reloaded = mission::Mission::load(&project.0)
            .expect("load")
            .expect("the failure was persisted for an operator to find");
        assert_eq!(reloaded.consecutive_failures, 1);
    }

    #[test]
    fn a_mission_that_cannot_be_written_does_not_end_the_run() {
        // A path under a regular file can never be a directory, which is the
        // cheapest reliable stand-in for a full disk or a read-only mount.
        let project = TempProject::new();
        let blocked = project.0.join("not-a-dir");
        std::fs::write(&blocked, "").expect("create blocking file");
        let root = blocked.join("project");

        let config = continuous_config();
        let mut mission = mission::Mission::new("harden it");
        // The point of the assertion is that this returns at all.
        let plan = record_failed_cycle(
            &config,
            Some(&mut mission),
            &root,
            "harden it",
            1,
            "a hard error",
            "boom",
            0,
        );
        assert!(
            plan.is_some(),
            "a perpetual run must not die of a bookkeeping write it cannot perform"
        );
    }
}
