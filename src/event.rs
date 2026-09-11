//! Unified event stream for the TUI main loop: terminal input, ticks, and
//! agent events multiplexed onto one channel.

use std::time::Duration;

use crossterm::event::{
    Event as CrosstermEvent, EventStream, KeyCode, KeyEvent, KeyModifiers, MouseEvent,
};
use futures_util::StreamExt;
use tokio::sync::{mpsc, watch};

use crate::agent::AgentEvent;

/// Consecutive terminal-read failures after which input is declared gone.
/// Small on purpose: the errors this counts are not the recoverable kind
/// arriving in a burst, they are the same failure repeating as fast as the
/// reader can ask for it.
const INPUT_FAULT_LIMIT: u32 = 64;

/// How long [`ReplyFilter`] holds keys that look like the start of a late
/// terminal reply before deciding they were typed. The reply arrives in one
/// read, so a real one never waits this long; a user who pressed Alt+_ sees
/// it land this much later.
const REPLY_HOLD: Duration = Duration::from_millis(100);

/// Longest kitty graphics reply the filter will swallow, in keys. The real
/// one (`ESC _ G i=31;OK ESC \`) is ten; an error reply is a few more.
const REPLY_MAX_KEYS: usize = 64;

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

/// Drops a terminal's answer to the image query when it arrives after the
/// event stream has taken stdin, instead of letting crossterm type it.
///
/// The query ([`crate::image_view`]) is answered before the stream starts,
/// with a short wait; a terminal that answers later (a slow hop that did not
/// set `SSH_TTY`) hands the reply to crossterm, which parses the kitty
/// reply `ESC _ G i=31;OK ESC \` as Alt+_ followed by plain keys and Alt+\.
/// The DA1, cell-size and status replies are parsed or refused by crossterm
/// itself and never reach the composer; this one does. Keys from Alt+_ on
/// are held until the sequence ends (dropped), turns out not to be one
/// (forwarded in order), or [`REPLY_HOLD`] passes (forwarded).
#[derive(Debug, Default)]
struct ReplyFilter {
    held: Vec<KeyEvent>,
}

impl ReplyFilter {
    /// True while keys are being held, so the reader bounds its next wait.
    fn holding(&self) -> bool {
        !self.held.is_empty()
    }

    /// Feed one key; get back the keys to forward, in order.
    fn push(&mut self, key: KeyEvent) -> Vec<KeyEvent> {
        let plain = key.modifiers.difference(KeyModifiers::SHIFT);
        if self.held.is_empty() {
            if key.code == KeyCode::Char('_') && plain == KeyModifiers::ALT {
                self.held.push(key);
                return Vec::new();
            }
            return vec![key];
        }
        let fits = match key.code {
            KeyCode::Char('\\') if plain == KeyModifiers::ALT => {
                self.held.clear();
                return Vec::new();
            }
            KeyCode::Char('G') if self.held.len() == 1 => plain.is_empty(),
            KeyCode::Char(_) if self.held.len() > 1 => plain.is_empty(),
            _ => false,
        };
        if fits && self.held.len() < REPLY_MAX_KEYS {
            self.held.push(key);
            return Vec::new();
        }
        let mut out = self.flush();
        out.push(key);
        out
    }

    /// Give back everything held: it was typed after all.
    fn flush(&mut self) -> Vec<KeyEvent> {
        std::mem::take(&mut self.held)
    }
}

/// Owns the merged event channel. A background task pumps crossterm's
/// `EventStream` and a tick interval into the channel; the agent task sends
/// [`Event::Agent`] through a cloned sender.
pub struct EventLoop {
    rx: mpsc::Receiver<Event>,
    tx: mpsc::Sender<Event>,
    /// True while the reader task must stay off stdin ([`Self::pause`]).
    pause: watch::Sender<bool>,
    /// True once the reader task has let go of stdin.
    released: watch::Receiver<bool>,
}

impl EventLoop {
    /// Start the terminal reader and tick tasks. `tick_rate` is the redraw
    /// cadence (e.g. 100 ms).
    pub fn new(tick_rate: Duration) -> Self {
        let (tx, rx) = mpsc::channel(256);
        let (pause, mut paused) = watch::channel(false);
        let (release, released) = watch::channel(false);

        let input_tx = tx.clone();
        tokio::spawn(async move {
            let mut stream = Some(EventStream::new());
            let mut filter = ReplyFilter::default();
            // Consecutive read errors. A single one is worth ignoring — a
            // signal arriving mid-read shows up here — but a detached pty
            // fails *every* read, and the old `continue` turned that into a
            // spin that pegged a core for as long as the session was left
            // open. Past this many in a row the stream is treated as gone.
            let mut faults: u32 = 0;
            let ended = loop {
                if *paused.borrow_and_update() {
                    // Dropping the stream wakes crossterm's helper thread and
                    // frees the reader lock, so whoever asked for the pause
                    // can read stdin directly.
                    drop(stream.take());
                    release.send_replace(true);
                    if paused.wait_for(|paused| !*paused).await.is_err() {
                        return;
                    }
                    stream = Some(EventStream::new());
                    release.send_replace(false);
                }
                let Some(live) = stream.as_mut() else {
                    continue;
                };
                // While keys are held, the wait is bounded: on a timeout
                // they were typed, and go through.
                let hold = filter.holding().then_some(REPLY_HOLD);
                let next = async {
                    match hold {
                        Some(hold) => tokio::time::timeout(hold, live.next()).await,
                        None => Ok(live.next().await),
                    }
                };
                let item = tokio::select! {
                    item = next => match item {
                        Ok(item) => item,
                        Err(_) => {
                            for key in filter.flush() {
                                if input_tx.send(Event::Key(key)).await.is_err() {
                                    return;
                                }
                            }
                            continue;
                        }
                    },
                    changed = paused.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        continue;
                    }
                };
                let Some(item) = item else {
                    break "terminal input ended (stdin closed)".to_string();
                };
                let events = match item {
                    Ok(CrosstermEvent::Key(key)) => {
                        filter.push(key).into_iter().map(Event::Key).collect()
                    }
                    Ok(CrosstermEvent::Mouse(mouse)) => vec![Event::Mouse(mouse)],
                    Ok(CrosstermEvent::Paste(text)) => vec![Event::Paste(text)],
                    Ok(CrosstermEvent::Resize(cols, rows)) => vec![Event::Resize(cols, rows)],
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
                for event in events {
                    if input_tx.send(event).await.is_err() {
                        // Receiver gone: the main loop has shut down. Nothing to
                        // report to — return rather than announce.
                        return;
                    }
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

        Self {
            rx,
            tx,
            pause,
            released,
        }
    }

    /// Take the reader off stdin and return once it has let go. For the
    /// stretches where something else reads the terminal directly: the
    /// setup wizard, `$EDITOR`. Without this the two readers race for each
    /// key and the one the stream wins lands in the composer afterwards.
    pub async fn pause(&mut self) {
        self.pause.send_replace(true);
        let _ = self.released.wait_for(|released| *released).await;
    }

    /// Put the reader back on stdin after [`Self::pause`].
    pub fn resume(&self) {
        self.pause.send_replace(false);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn key(c: char, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), modifiers)
    }

    /// The keys crossterm makes of `ESC _ G i=31;OK ESC \\`.
    fn kitty_ok() -> Vec<KeyEvent> {
        let mut keys = vec![key('_', KeyModifiers::ALT), key('G', KeyModifiers::SHIFT)];
        keys.extend("i=31;".chars().map(|c| key(c, KeyModifiers::NONE)));
        keys.extend("OK".chars().map(|c| key(c, KeyModifiers::SHIFT)));
        keys.push(key('\\', KeyModifiers::ALT));
        keys
    }

    #[test]
    fn a_late_kitty_reply_is_swallowed_whole() {
        let mut filter = ReplyFilter::default();
        for k in kitty_ok() {
            assert!(filter.push(k).is_empty(), "{k:?} leaked");
        }
        assert!(!filter.holding());
    }

    #[test]
    fn a_kitty_error_reply_is_swallowed_too() {
        let mut filter = ReplyFilter::default();
        let mut keys = vec![key('_', KeyModifiers::ALT), key('G', KeyModifiers::SHIFT)];
        keys.extend("i=31;EINVAL:unknown key".chars().map(|c| {
            key(
                c,
                if c.is_ascii_uppercase() {
                    KeyModifiers::SHIFT
                } else {
                    KeyModifiers::NONE
                },
            )
        }));
        keys.push(key('\\', KeyModifiers::ALT));
        for k in keys {
            assert!(filter.push(k).is_empty(), "{k:?} leaked");
        }
    }

    #[test]
    fn ordinary_typing_passes_straight_through() {
        let mut filter = ReplyFilter::default();
        for c in "Gi=31;OK".chars() {
            assert_eq!(
                filter.push(key(c, KeyModifiers::NONE)),
                vec![key(c, KeyModifiers::NONE)]
            );
        }
        assert_eq!(
            filter.push(key('\\', KeyModifiers::ALT)),
            vec![key('\\', KeyModifiers::ALT)]
        );
    }

    #[test]
    fn a_real_alt_underscore_comes_back_in_order() {
        let mut filter = ReplyFilter::default();
        assert!(filter.push(key('_', KeyModifiers::ALT)).is_empty());
        assert!(filter.holding());
        // The next key is not `G`: everything held is typed after all.
        assert_eq!(
            filter.push(key('x', KeyModifiers::NONE)),
            vec![key('_', KeyModifiers::ALT), key('x', KeyModifiers::NONE)]
        );
        assert!(!filter.holding());
        // And on a timeout the held key comes back by itself.
        assert!(filter.push(key('_', KeyModifiers::ALT)).is_empty());
        assert_eq!(filter.flush(), vec![key('_', KeyModifiers::ALT)]);
    }

    #[test]
    fn a_control_key_or_a_non_char_ends_the_hold() {
        let mut filter = ReplyFilter::default();
        assert!(filter.push(key('_', KeyModifiers::ALT)).is_empty());
        assert!(filter.push(key('G', KeyModifiers::SHIFT)).is_empty());
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            filter.push(enter),
            vec![
                key('_', KeyModifiers::ALT),
                key('G', KeyModifiers::SHIFT),
                enter
            ]
        );
        assert!(filter.push(key('_', KeyModifiers::ALT)).is_empty());
        let ctrl_c = key('c', KeyModifiers::CONTROL);
        assert_eq!(
            filter.push(ctrl_c),
            vec![key('_', KeyModifiers::ALT), ctrl_c]
        );
    }

    #[test]
    fn the_hold_is_bounded() {
        let mut filter = ReplyFilter::default();
        assert!(filter.push(key('_', KeyModifiers::ALT)).is_empty());
        assert!(filter.push(key('G', KeyModifiers::SHIFT)).is_empty());
        let mut leaked = Vec::new();
        for _ in 0..REPLY_MAX_KEYS {
            leaked.extend(filter.push(key('a', KeyModifiers::NONE)));
        }
        // 64 held keys come back with the one that overflowed them, and the
        // one after that passes straight through.
        assert_eq!(leaked.len(), REPLY_MAX_KEYS + 2);
        assert!(!filter.holding());
    }
}
