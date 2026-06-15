//! Unified event stream for the TUI main loop: terminal input, ticks, and
//! agent events multiplexed onto one channel.

use std::time::Duration;

use crossterm::event::{Event as CrosstermEvent, EventStream, KeyEvent, MouseEvent};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::agent::AgentEvent;

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
    /// A system notice injected from a background task (e.g. `/evolve`),
    /// appended to the transcript without blocking the main loop.
    Notice(String),
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
            while let Some(item) = stream.next().await {
                let event = match item {
                    Ok(CrosstermEvent::Key(key)) => Event::Key(key),
                    Ok(CrosstermEvent::Mouse(mouse)) => Event::Mouse(mouse),
                    Ok(CrosstermEvent::Paste(text)) => Event::Paste(text),
                    Ok(CrosstermEvent::Resize(cols, rows)) => Event::Resize(cols, rows),
                    Ok(CrosstermEvent::FocusGained | CrosstermEvent::FocusLost) => continue,
                    Err(err) => {
                        tracing::warn!("terminal event stream error: {err}");
                        continue;
                    }
                };
                if input_tx.send(event).await.is_err() {
                    // Receiver gone: the main loop has shut down.
                    break;
                }
            }
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
