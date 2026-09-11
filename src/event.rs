//! Unified event stream for the TUI main loop: terminal input, ticks, and
//! agent events multiplexed onto one channel.

use std::time::Duration;

use crossterm::event::{Event as CrosstermEvent, EventStream, KeyEvent, MouseEvent};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::agent::AgentEvent;

/// Consecutive terminal-read failures after which input is declared gone.
/// Small on purpose: the errors this counts are not the recoverable kind
/// arriving in a burst, they are the same failure repeating as fast as the
/// reader can ask for it.
const INPUT_FAULT_LIMIT: u32 = 64;

/// Everything the TUI main loop reacts to.
#[derive(Debug)]
pub enum Event {
    Key(KeyEvent),
    Mouse(MouseEvent),
    /// Bracketed paste.
    Paste(String),
    /// Terminal resize (columns, rows).
    Resize(u16, u16),
    /// Periodic redraw tick.
    Tick,
    /// Event from the running agent turn.
    Agent(AgentEvent),
    /// Out-of-band notice from a background task (`/evolve`, `/reload`),
    /// rendered as a [`crate::app::TranscriptEntry::Notice`].
    Notice(String),
    /// The background MCP connect (spawned at startup so the first paint isn't
    /// blocked on slow stdio servers) finished populating the shared manager.
    /// The main loop rebuilds the tool registry from it so the servers' tools
    /// merge into the live agent. Carries `{connected, configured}` so the loop
    /// can stay quiet when nothing connected and surface a shortfall when some
    /// (but not all) configured servers came up.
    McpConnected {
        connected: usize,
        configured: usize,
    },
    /// The deferred cloud-provider health probe failed. Carries the error
    /// string; the main loop stores it in `App::provider_health_error` so the
    /// breakage shows at launch (welcome screen + status bar) rather than only
    /// when the first message fails.
    ProviderHealthFailed(String),
    /// The starter prompts read off the cwd, once the git calls that derive
    /// them are done. Sent after the first frame, so a slow `git status`
    /// never delays the first paint.
    StarterPrompts(Vec<String>),
    /// A background agent rebuild (model switch, crash recovery) finished.
    /// Carries the agent back to the main loop's slot (boxed: an [`Agent`]
    /// is large next to the input variants).
    AgentRebuilt(Box<crate::app::AgentRebuild>),
    /// A background sign-in (xAI OAuth) succeeded: add this provider and switch
    /// to it. Owned by the main loop (it holds the config + agent slot); boxed
    /// because [`ProviderConfig`](crate::config::ProviderConfig) is large next
    /// to the input variants.
    ProviderActivated(Box<crate::config::ProviderConfig>),
    /// A background `/btw` side-question finished. The answer (or error) was
    /// already sent as [`Event::Notice`]; this only clears the in-flight flag
    /// so another `/btw` can run.
    BtwFinished,
    /// Terminal input has ended and will never resume: stdin closed, the pty
    /// was detached, or the reader gave up on a stream that only produces
    /// errors. Carries a short reason for the farewell notice.
    ///
    /// The main loop must **quit** on this. Without it the reader task simply
    /// returned while the tick task kept ticking, so the TUI went on repainting
    /// at its full cadence — a session that looks completely alive and can
    /// never receive another keystroke. The only way out is killing it from
    /// another terminal, which is as close to "it randomly stopped" as this
    /// program gets.
    InputClosed(String),
}

/// Owns the merged event channel. A background task pumps crossterm's
/// `EventStream` and a tick interval into the channel; the agent task sends
/// [`Event::Agent`] through a cloned sender.
pub struct EventLoop {
    rx: mpsc::Receiver<Event>,
    tx: mpsc::Sender<Event>,
}

impl EventLoop {
    /// Start the terminal reader and tick tasks. `tick_rate` is the redraw
    /// cadence (e.g. 100 ms).
    pub fn new(tick_rate: Duration) -> Self {
        let (tx, rx) = mpsc::channel(256);

        let input_tx = tx.clone();
        tokio::spawn(async move {
            let mut stream = EventStream::new();
            // Consecutive read errors. A single one is worth ignoring — a
            // signal arriving mid-read shows up here — but a detached pty
            // fails *every* read, and the old `continue` turned that into a
            // spin that pegged a core for as long as the session was left
            // open. Past this many in a row the stream is treated as gone.
            let mut faults: u32 = 0;
            let ended = loop {
                let Some(item) = stream.next().await else {
                    break "terminal input ended (stdin closed)".to_string();
                };
                let event = match item {
                    Ok(CrosstermEvent::Key(key)) => Event::Key(key),
                    Ok(CrosstermEvent::Mouse(mouse)) => Event::Mouse(mouse),
                    Ok(CrosstermEvent::Paste(text)) => Event::Paste(text),
                    Ok(CrosstermEvent::Resize(cols, rows)) => Event::Resize(cols, rows),
                    Ok(CrosstermEvent::FocusGained | CrosstermEvent::FocusLost) => {
                        faults = 0;
                        continue;
                    }
                    Err(err) => {
                        tracing::warn!("terminal event stream error: {err}");
                        faults += 1;
                        if faults >= INPUT_FAULT_LIMIT {
                            break format!("terminal input failed repeatedly: {err}");
                        }
                        continue;
                    }
                };
                faults = 0;
                if input_tx.send(event).await.is_err() {
                    // Receiver gone: the main loop has shut down. Nothing to
                    // report to — return rather than announce.
                    return;
                }
            };
            // Say so rather than returning quietly, or the tick task keeps the
            // frame repainting over a session nobody can type into.
            let _ = input_tx.send(Event::InputClosed(ended)).await;
        });

        let tick_tx = tx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tick_rate);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                if tick_tx.send(Event::Tick).await.is_err() {
                    break;
                }
            }
        });

        Self { rx, tx }
    }

    /// A sender for injecting events from other tasks (the agent forwards
    /// its [`AgentEvent`]s wrapped in [`Event::Agent`]).
    pub fn sender(&self) -> mpsc::Sender<Event> {
        self.tx.clone()
    }

    /// Next event, in arrival order. `None` when all senders are gone
    /// (shutdown).
    pub async fn next(&mut self) -> Option<Event> {
        self.rx.recv().await
    }
}
