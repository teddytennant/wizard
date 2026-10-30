//! Staying alive: the primitives the genie event loop uses to turn a failure
//! that would have ended the session into a line in the transcript.
//!
//! Every mechanism here exists because of the same complaint, which is that
//! Wizard "randomly stops". Almost none of the ways it stopped were the agent
//! deciding to; they were a background task taking a latch to the grave, a
//! turn task unwinding where nothing was watching, a panic hook stripping the
//! terminal out from under a process that was still running, or one transient
//! `write` to stdout being treated as proof the terminal had gone. In each of
//! those the user could plausibly have retried, and in each of those the code
//! took the decision away from them.
//!
//! The rule these follow: report the failure, keep the surface usable, and let
//! the person at the keyboard decide whether to try again.

use std::any::Any;
use std::panic::AssertUnwindSafe;
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use tokio::sync::mpsc;

use crate::event::Event;

/// Best-effort human-readable text out of a panic payload.
///
/// `catch_unwind` hands back a `Box<dyn Any>`, and the two shapes that carry
/// anything useful are the ones `panic!` itself produces: a `&'static str` for
/// a literal message and a `String` for a formatted one. Anything else is a
/// payload from a `panic_any` somewhere and has no printable form, so it gets
/// a placeholder rather than nothing — the point of the message is to make the
/// transcript line say *which* thing broke, and "a panic with no message" is
/// still an answer to that.
pub(super) fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "panicked with no message".to_string()
    }
}

/// Spawn a background task that cannot fail to answer the main loop.
///
/// Several of the TUI's latches — `App::rebuilding`, `App::compacting`,
/// `App::btw_inflight`, `App::mcp_connecting` — are set on the event loop and
/// cleared *only* by the [`Event`] the background task promises to send back.
/// A task that panics never sends it, and the consequences are not cosmetic:
/// `rebuilding` being stuck is enough on its own to make the session inert,
/// because `drain_message_queue` refuses to start a turn while it is set. The
/// user then types, presses Enter, watches the message queue up, and gets
/// nothing back forever. That is precisely the report this whole exercise is
/// about.
///
/// So the task body runs inside `catch_unwind` and an unwind is converted into
/// `fallback` — whatever event releases the latch this task was holding. The
/// answer is a value rather than a side effect so the task cannot forget to
/// send it: returning `None` is the explicit "this one holds nothing" case
/// (used by tasks that already reported through their own `notify` clone).
///
/// A send failure is not an error here. The receiver is gone only when the
/// main loop has already finished, at which point there is no latch left to
/// release and nothing to tell.
pub(super) fn spawn_answering<F>(
    notify: mpsc::Sender<Event>,
    fallback: Event,
    task: F,
) -> tokio::task::JoinHandle<()>
where
    F: std::future::Future<Output = Option<Event>> + Send + 'static,
{
    tokio::spawn(async move {
        let answer = match AssertUnwindSafe(task).catch_unwind().await {
            Ok(answer) => answer,
            Err(payload) => {
                tracing::error!(
                    "background task panicked: {} — releasing its UI latch",
                    panic_message(&*payload)
                );
                Some(fallback)
            }
        };
        if let Some(event) = answer
            && notify.send(event).await.is_err()
        {
            tracing::debug!("main loop already gone; background answer dropped");
        }
    })
}

/// Turn one finished turn task into the message the transcript should show,
/// or `None` when the turn ended the ordinary way and has already said so.
///
/// The three outcomes are deliberately not collapsed. A clean return means
/// `run_turn` emitted its own `Done` and anything added here would be a second
/// account of the same turn. An `Err` is the agent reporting a failure it
/// understood, and its own text is the useful part. A panic is the case this
/// function exists for: the turn task unwound, no `Done` was ever sent, and
/// without a substitute the UI stays busy forever with a spinner over a turn
/// that ended some time ago. The wording tells the user the conversation
/// survived, because the agent is handed back to its slot rather than rebuilt
/// — retrying is one keypress, and they should know that.
pub(super) fn turn_failure<T>(
    outcome: Result<Result<T, anyhow::Error>, Box<dyn Any + Send>>,
) -> Option<String> {
    match outcome {
        Ok(Ok(_)) => None,
        Ok(Err(err)) => Some(format!("turn failed: {err:#}")),
        Err(payload) => Some(format!(
            "the turn crashed: {} — the conversation is intact; send it again",
            panic_message(&*payload)
        )),
    }
}

/// How long a background task that has taken the agent out of its slot may
/// run before the loop stops waiting for it.
///
/// Three minutes covers a rebuild that has to spawn `llama-server` and load a
/// model, or a provider probe against a cloud endpoint having a bad day. What
/// it does not cover is forever, and forever is reachable: every one of these
/// tasks takes the shared `McpManager` lock, so a stdio server that accepted a
/// connection and then stopped answering its `initialize` handshake parks the
/// rebuild on a mutex that will never be released. With the agent gone and
/// `App::rebuilding` set, the composer keeps accepting text and nothing will
/// ever run again — and nothing on screen says so.
pub(super) const AGENT_REBUILD_DEADLINE: Duration = Duration::from_secs(180);

/// The same bound for `/compact`, which is one model call over the entire
/// conversation. Far longer, because on a local backend summarizing a full
/// context legitimately takes minutes and cutting a working compaction short
/// would be its own bug.
pub(super) const COMPACTION_DEADLINE: Duration = Duration::from_secs(600);

/// Run `task`, giving up if it has not finished within `limit`.
///
/// The `Err` carries a sentence for the transcript rather than an error type,
/// because there is only ever one thing to say and exactly one caller shape:
/// a background task holding the agent, whose failure has to become an
/// `AgentRebuild` notice. Timing out drops the future, which drops the agent
/// with it — that is the intent. An agent parked on a lock nobody will release
/// is already lost; the loop's recovery path rebuilds a working one from the
/// session file, which is a strictly better place to be than waiting.
pub(super) async fn within<T>(
    what: &str,
    limit: Duration,
    task: impl std::future::Future<Output = T>,
) -> Result<T, String> {
    match tokio::time::timeout(limit, task).await {
        Ok(value) => Ok(value),
        Err(_) => Err(format!(
            "{what} did not finish within {}s — giving up on it",
            limit.as_secs()
        )),
    }
}

/// How many consecutive failed frames are tolerated before the event loop
/// concludes the terminal is genuinely gone.
///
/// At the loop's 100 ms tick this is about three seconds of trying. A single
/// failed `write` to a terminal is routinely nothing — an `EINTR` from a
/// signal, a `EAGAIN` from a full pty buffer while something else is draining
/// it, a resize landing mid-frame — and exiting on the first one turned a
/// hiccup into a lost session. Three seconds of *every* frame failing is a
/// different claim, and at that point exiting with the error is the honest
/// thing to do rather than spinning forever against a dead fd.
const DRAW_FAULT_LIMIT: u32 = 30;

/// Consecutive-failure counter for `Terminal::draw`.
///
/// Exists so the "one bad frame is not a dead terminal" judgement is a tested
/// property rather than a `?` in the middle of the event loop.
#[derive(Debug, Default)]
pub(super) struct DrawFaults {
    consecutive: u32,
}

impl DrawFaults {
    pub(super) const fn new() -> Self {
        Self { consecutive: 0 }
    }

    /// A frame landed: the terminal is fine, forget the history.
    pub(super) fn succeeded(&mut self) {
        self.consecutive = 0;
    }

    /// A frame failed. Returns `true` when the failures have gone on long
    /// enough that the loop should give up and propagate the error.
    pub(super) fn failed(&mut self) -> bool {
        self.consecutive = self.consecutive.saturating_add(1);
        self.consecutive >= DRAW_FAULT_LIMIT
    }

    /// How many frames have failed in a row. Used to decide whether the
    /// failure is worth a transcript line yet.
    pub(super) const fn consecutive(&self) -> u32 {
        self.consecutive
    }
}

/// How long to wait between attempts to put the terminal back into the state
/// the TUI needs. Long enough that a terminal which cannot be re-armed at all
/// does not produce a notice every tick, short enough that recovery feels
/// immediate to whoever is watching.
const REARM_COOLDOWN: Duration = Duration::from_secs(2);

/// Watches for the terminal being torn down while the TUI is still running,
/// and paces the attempts to put it back.
///
/// The tear-down is not hypothetical, and it is not a bug in someone else's
/// code: Wizard's own panic hook (`src/main.rs`) calls
/// [`restore_terminal_best_effort`](super::term::restore_terminal_best_effort)
/// so that a crash never leaves an unusable shell behind. But a panic hook has
/// no way to know whether the panicking thread is about to end the process.
/// When the panic happens on a *spawned task* — an MCP connect, a `/btw`, the
/// event forwarder — tokio catches it, the process lives on, and the hook has
/// already left raw mode and returned to the primary screen. From the user's
/// side the application is now drawing frames into their scrollback, their
/// keystrokes are line-buffered somewhere they can't see, and Wizard looks
/// like it died standing up.
///
/// Fixing the hook is not the answer: it must keep restoring, because the
/// alternative is a real crash leaving a wrecked terminal. The answer is for
/// the loop that owns the terminal to notice it was taken and take it back.
#[derive(Debug, Default)]
pub(super) struct TerminalWatchdog {
    /// When the next re-arm attempt is allowed. `None` means the terminal was
    /// in the expected state last time we looked.
    retry_at: Option<Instant>,
}

impl TerminalWatchdog {
    pub(super) const fn new() -> Self {
        Self { retry_at: None }
    }

    /// Decide whether to attempt a re-arm now. `armed` is whether the terminal
    /// is currently in the state the TUI set up (raw mode on).
    ///
    /// The first observation of a disarmed terminal always attempts
    /// immediately; every later one waits out [`REARM_COOLDOWN`], so a
    /// terminal that genuinely cannot be re-armed costs one attempt every two
    /// seconds instead of one per frame.
    pub(super) fn should_rearm(&mut self, armed: bool, now: Instant) -> bool {
        if armed {
            self.retry_at = None;
            return false;
        }
        match self.retry_at {
            Some(at) if now < at => false,
            _ => {
                self.retry_at = Some(now + REARM_COOLDOWN);
                true
            }
        }
    }
}

/// How many times the loop will rebuild a lost agent from its session before
/// it stops trying.
///
/// A rebuild that fails once is usually transient (a provider that just went
/// down, a session file being written by something else). A rebuild that fails
/// three times in a row is a configuration problem, and re-spawning it forever
/// would bury the one notice that says what is actually wrong under a stream
/// of identical failures.
const MAX_REBUILD_RETRIES: u32 = 2;

/// What the event loop should do when a rebuild came back without an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RebuildRecovery {
    /// Nothing to do: an agent is in the slot.
    Idle,
    /// The slot is empty and it is worth trying again.
    Retry,
    /// The slot is empty and retrying has stopped being useful; say so once.
    GiveUp,
}

/// Decide how to respond to a rebuild that produced no agent.
///
/// An empty agent slot is not a visible state — `App` keeps drawing, the
/// composer keeps accepting text, and `drain_message_queue` quietly declines
/// to start anything — so a failed rebuild that nobody retries reads exactly
/// like the application having stopped for no reason. This is the loop's
/// answer: try again, a bounded number of times, and then say plainly that the
/// session needs relaunching instead of pretending to work.
pub(super) fn rebuild_recovery(slot_filled: bool, consecutive_failures: u32) -> RebuildRecovery {
    if slot_filled {
        RebuildRecovery::Idle
    } else if consecutive_failures <= MAX_REBUILD_RETRIES {
        RebuildRecovery::Retry
    } else {
        RebuildRecovery::GiveUp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_payload_still_names_what_broke() {
        let literal = std::panic::catch_unwind(|| panic!("index out of bounds"))
            .expect_err("the closure panicked");
        assert_eq!(panic_message(&*literal), "index out of bounds");

        let formatted = std::panic::catch_unwind(|| panic!("row {} of {}", 9, 4))
            .expect_err("the closure panicked");
        assert_eq!(panic_message(&*formatted), "row 9 of 4");

        // A `panic_any` payload has no text at all; the caller still needs a
        // string it can put in the transcript.
        let opaque = std::panic::catch_unwind(|| std::panic::panic_any(7u8))
            .expect_err("the closure panicked");
        assert_eq!(panic_message(&*opaque), "panicked with no message");
    }

    #[tokio::test]
    async fn a_background_task_that_panics_still_releases_its_latch() {
        // `btw_inflight` is cleared only by `Event::BtwFinished`. Before this,
        // a panic anywhere inside the side-question task meant `/btw` was
        // refused for the rest of the session with no explanation.
        let (tx, mut rx) = mpsc::channel(4);
        spawn_answering(tx, Event::BtwFinished, async {
            panic!("the side question blew up");
        });
        let event = rx.recv().await.expect("the fallback must be delivered");
        assert!(matches!(event, Event::BtwFinished));
    }

    #[tokio::test]
    async fn a_background_task_that_succeeds_answers_with_its_own_event() {
        let (tx, mut rx) = mpsc::channel(4);
        spawn_answering(tx, Event::BtwFinished, async {
            Some(Event::Notice("done".to_string()))
        });
        match rx.recv().await.expect("the answer must be delivered") {
            Event::Notice(text) => assert_eq!(text, "done"),
            other => panic!("unexpected answer: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_task_that_holds_no_latch_answers_with_nothing() {
        let (tx, mut rx) = mpsc::channel(4);
        spawn_answering(tx, Event::BtwFinished, async { None });
        // The sender is dropped when the task ends, so the channel closes
        // without ever producing an event.
        assert!(rx.recv().await.is_none());
    }

    #[test]
    fn a_crashed_turn_reports_itself_instead_of_hanging_the_ui() {
        // A turn that returned cleanly already sent its own `Done`; a second
        // notice here would be the same turn told twice.
        assert!(turn_failure::<()>(Ok(Ok(()))).is_none());

        // A reported failure keeps the agent's own words, with the full
        // `anyhow` chain — the cause is usually the part that matters.
        let err = anyhow::anyhow!("provider is down").context("streaming completion");
        let message = turn_failure::<()>(Ok(Err(err))).expect("a failure is reported");
        assert!(
            message.starts_with("turn failed: streaming completion"),
            "{message}"
        );
        assert!(message.contains("provider is down"), "{message}");

        // And the case that used to wedge the session: an unwind sends no
        // `Done`, so this message is the only thing that will ever unblock the
        // composer.
        let payload = std::panic::catch_unwind(|| panic!("attempt to index out of bounds"))
            .expect_err("the closure panicked");
        let message = turn_failure::<()>(Err(payload)).expect("a crash is reported");
        assert!(
            message.contains("attempt to index out of bounds"),
            "the crash must name itself: {message}"
        );
        assert!(
            message.contains("send it again"),
            "the agent survives the crash, and the user should be told: {message}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_task_holding_the_agent_is_given_up_on_rather_than_waited_on_forever() {
        // The happy path is untouched: a task that finishes inside its budget
        // hands its value straight back.
        let done = within("restarting the agent", Duration::from_secs(60), async { 7 }).await;
        assert_eq!(done, Ok(7));

        // And the one that used to be unrecoverable: a future parked on
        // something that will never complete (in the field, the shared
        // `McpManager` mutex behind a stdio server that stopped answering).
        let stuck = within(
            "restarting the agent",
            AGENT_REBUILD_DEADLINE,
            std::future::pending::<()>(),
        )
        .await;
        let message = stuck.expect_err("a future that never resolves must be abandoned");
        assert!(
            message.contains("restarting the agent") && message.contains("giving up"),
            "the notice has to name what was abandoned: {message}"
        );
    }

    #[test]
    fn one_bad_frame_is_not_a_dead_terminal() {
        let mut faults = DrawFaults::new();
        for _ in 0..DRAW_FAULT_LIMIT - 1 {
            assert!(
                !faults.failed(),
                "a transient write error must be ridden out"
            );
        }
        // A frame that lands proves the terminal is alive; the streak resets.
        faults.succeeded();
        assert_eq!(faults.consecutive(), 0);
        for _ in 0..DRAW_FAULT_LIMIT - 1 {
            assert!(!faults.failed());
        }
        assert!(
            faults.failed(),
            "an unbroken streak of failures must eventually be believed"
        );
    }

    #[test]
    fn the_watchdog_retakes_the_terminal_at_once_then_paces_itself() {
        let mut watchdog = TerminalWatchdog::new();
        let start = Instant::now();

        // Healthy: nothing to do, and nothing recorded.
        assert!(!watchdog.should_rearm(true, start));

        // A spawned task panicked and the panic hook left raw mode: take it
        // back on the very next frame, not two seconds later.
        assert!(watchdog.should_rearm(false, start));
        // Still disarmed a frame later (the re-arm failed): do not try again
        // yet, or every tick posts another failure notice.
        assert!(!watchdog.should_rearm(false, start + Duration::from_millis(100)));
        assert!(!watchdog.should_rearm(false, start + REARM_COOLDOWN - Duration::from_millis(1)));
        assert!(watchdog.should_rearm(false, start + REARM_COOLDOWN));

        // Once it comes back, the next loss is again handled immediately.
        assert!(!watchdog.should_rearm(true, start + REARM_COOLDOWN));
        assert!(watchdog.should_rearm(false, start + REARM_COOLDOWN));
    }

    #[test]
    fn a_lost_agent_is_retried_a_bounded_number_of_times() {
        // An agent in the slot: never retry, whatever the history.
        assert_eq!(rebuild_recovery(true, 0), RebuildRecovery::Idle);
        assert_eq!(rebuild_recovery(true, 99), RebuildRecovery::Idle);

        // An empty slot is invisible on screen, so it must not be left alone.
        assert_eq!(rebuild_recovery(false, 1), RebuildRecovery::Retry);
        assert_eq!(
            rebuild_recovery(false, MAX_REBUILD_RETRIES),
            RebuildRecovery::Retry
        );
        assert_eq!(
            rebuild_recovery(false, MAX_REBUILD_RETRIES + 1),
            RebuildRecovery::GiveUp,
            "retrying forever buries the notice that says what is wrong"
        );
    }
}
