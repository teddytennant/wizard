//! Progress reporting: the shape of it, and the plain-terminal implementation.
//!
//! Two halves, and the split matters because a plugin depends on the first. The
//! *shape* is [`Progress`] and [`ByteProgress`] — how any surface says "this is
//! going to take a while, here is how far it has got" — and it is core because
//! its implementors and its callers are scattered on both sides of the plugin
//! boundary: the TUI, the window and the gateway each implement one that writes
//! into a transcript, the Ollama plugin reports a model pull through it, and
//! llama.cpp's GGUF downloader reports a multi-gigabyte fetch through it.
//!
//! The *implementation* below is the plain-terminal one. The interactive genie
//! TUI draws its own spinner (`src/ui/mod.rs`); everything here covers the paths
//! that print to a normal terminal instead — headless sovereign turns, fleet
//! workers, and llama-server startup waits — with one shared look: the TUI's
//! braille frames, white accent, dim text.
//!
//! All drawing goes to stderr and indicatif hides itself when stderr is not
//! a terminal, so piped or redirected runs see no escape sequences; status
//! that matters on non-terminals (server wait results) falls back to plain
//! lines.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

/// A surface's sink for slow work it did not ask for a spinner about.
///
/// The two traits live here rather than beside the one subsystem that made
/// them, for the reason [`crate::tools::http`] holds the HTTP client rather
/// than the web tools: five things implement or consume this and only one of
/// them is a llama-server. [`ServerSpinner`] below is the plain-terminal
/// implementation, the TUI, the window and the gateway each supply one that
/// writes into a transcript, the Ollama plugin reports a model pull through
/// it, and llama.cpp's GGUF downloader reports through it too. A build with no
/// local backend at all still has all of that except the last.
///
/// `status` reports a one-line update; `bytes` opens a determinate byte-counted
/// phase whose guard is ticked with [`ByteProgress::inc`] and restores the
/// prior UI on finish or drop.
pub trait Progress: Send + Sync {
    /// Report a one-line status update.
    fn status(&self, line: &str);
    /// Begin a byte-counted download phase. `total` is the expected byte
    /// count when the server advertised a `Content-Length`, `None` for an
    /// indeterminate stream.
    fn bytes(&self, label: &str, total: Option<u64>) -> Box<dyn ByteProgress>;
}

/// A determinate byte-counted phase opened by [`Progress::bytes`]. Dropping
/// the guard restores the surface's prior UI; [`ByteProgress::finish`] does
/// the same after recording a closing message.
pub trait ByteProgress: Send {
    /// Account for `n` more bytes transferred.
    fn inc(&self, n: u64);
    /// Close the phase, optionally leaving `msg` as the surface's status
    /// (an empty `msg` means no closing message).
    fn finish(self: Box<Self>, msg: &str);
}

/// Braille frames matching the TUI spinner of the house skin (`WIZARD` in
/// `src/skin/mod.rs`; the TUI reads whichever skin is active), plus a final
/// check mark shown when a spinner finishes with a message.
const TICK_CHARS: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏✓";

/// Steady-tick cadence, close to the TUI's redraw feel.
const TICK_INTERVAL: Duration = Duration::from_millis(80);

/// Spinner style: accent (white) spinner, dim message — the plain-terminal
/// cousin of the TUI busy line.
fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.white} {msg:.dim}")
        .expect("static spinner template is valid")
        .tick_chars(TICK_CHARS)
}

/// Counted-bar style (fleet slots, multi-item work): position/length, current item, elapsed.
fn bar_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.white} [{bar:24.white/black}] {pos}/{len} {msg:.dim} {elapsed:.dim}",
    )
    .expect("static bar template is valid")
    .tick_chars(TICK_CHARS)
    .progress_chars("█▓░")
}

/// A counted progress bar over `len` items, drawn to stderr (hidden
/// automatically when stderr is not a terminal). Wrap interleaved output in
/// [`ProgressBar::suspend`] so lines land above the bar.
pub fn bar(len: u64) -> ProgressBar {
    let bar = ProgressBar::new(len).with_style(bar_style());
    bar.enable_steady_tick(TICK_INTERVAL);
    bar
}

/// Byte-counted style for downloads: a determinate bar with transfer rate and
/// ETA when the total is known, an indeterminate spinner+counter otherwise.
fn download_style(total_known: bool) -> ProgressStyle {
    let template = if total_known {
        "{spinner:.white} {msg:.dim} [{bar:24.white/black}] {bytes}/{total_bytes} \
         {binary_bytes_per_sec:.dim} {eta:.dim}"
    } else {
        "{spinner:.white} {msg:.dim} {bytes} {binary_bytes_per_sec:.dim}"
    };
    ProgressStyle::with_template(template)
        .expect("static download template is valid")
        .tick_chars(TICK_CHARS)
        .progress_chars("█▓░")
}

/// A byte-counted progress bar drawn to stderr (hidden automatically when
/// stderr is not a terminal). `total` is the expected byte count; `None`
/// gives an indeterminate spinner that still shows bytes transferred.
pub fn download_bar(total: Option<u64>) -> ProgressBar {
    let bar = match total {
        Some(total) => ProgressBar::new(total),
        None => ProgressBar::new_spinner(),
    }
    .with_style(download_style(total.is_some()));
    bar.enable_steady_tick(TICK_INTERVAL);
    bar
}

/// The headless-turn spinner: shows a configured verb ("Conjuring…") while
/// the model is thinking or a tool is running, and gets out of the way while
/// the agent streams real output. Shared between the run loop and the
/// event-printer task, so all state is interior and methods take `&self`.
pub struct TurnSpinner {
    bar: ProgressBar,
    /// Whether stderr is a terminal at all. When false the spinner is a
    /// permanent no-op (no ticker thread, no escapes) and lines print
    /// plainly.
    enabled: bool,
    visible: AtomicBool,
}

impl TurnSpinner {
    /// Create a hidden spinner; [`TurnSpinner::show`] makes it visible.
    pub fn new() -> Self {
        let bar = ProgressBar::with_draw_target(None, ProgressDrawTarget::hidden())
            .with_style(spinner_style());
        Self {
            bar,
            enabled: std::io::stderr().is_terminal(),
            visible: AtomicBool::new(false),
        }
    }

    /// Set the verb shown next to the spinner (rendered as "{verb}…").
    pub fn set_verb(&self, verb: &str) {
        self.bar.set_message(format!("{verb}…"));
    }

    /// Show the spinner. No-op when stderr is not a terminal.
    pub fn show(&self) {
        if !self.enabled || self.visible.swap(true, Ordering::SeqCst) {
            return;
        }
        self.bar.set_draw_target(ProgressDrawTarget::stderr());
        self.bar.enable_steady_tick(TICK_INTERVAL);
    }

    /// Hide the spinner, clearing it from the terminal (streamed model
    /// output owns the screen until the next [`TurnSpinner::show`]).
    pub fn hide(&self) {
        if !self.enabled || !self.visible.swap(false, Ordering::SeqCst) {
            return;
        }
        self.bar.disable_steady_tick();
        self.bar.set_draw_target(ProgressDrawTarget::hidden());
    }

    /// Print a full line to stdout without tearing a visible spinner.
    ///
    /// Through [`crate::output::print_line`] rather than `println!` because
    /// this is the one call every headless narration line goes through, and it
    /// runs on the printer task. A `println!` here panics the moment the
    /// reader hangs up — `wizard -p "…" | head` — which kills the printer,
    /// drops the event receiver, and turns a perfectly ordinary shell pipeline
    /// into a run that stopped for no visible reason.
    pub fn println(&self, line: &str) {
        if self.visible.load(Ordering::SeqCst) {
            self.bar.suspend(|| crate::output::print_line(line));
        } else {
            crate::output::print_line(line);
        }
    }

    /// Clear the spinner for good at the end of a run.
    pub fn finish(&self) {
        self.visible.store(false, Ordering::SeqCst);
        self.bar.finish_and_clear();
    }
}

impl Default for TurnSpinner {
    fn default() -> Self {
        Self::new()
    }
}

/// Progress reporter for slow setup waits — llama-server startup (spawn +
/// model load) and Ollama model pulls: a spinner whose message tracks the
/// latest status on a terminal, plain stderr lines otherwise.
///
/// The off-terminal fallback goes to stderr, the same stream the spinner
/// itself draws on, and never to stdout. This reporter is built inside
/// [`crate::agent::build_headless_agent`], which every surface calls,
/// including the two that own stdout as a protocol transport: the ACP server
/// frames JSON-RPC there and `--output-format json` frames the run there.
pub struct ServerSpinner {
    bar: ProgressBar,
    /// Closing message shown when the wait succeeded and actually waited.
    ready: String,
    /// Whether any status line arrived — i.e. the server was actually
    /// spawned or waited on rather than already answering.
    waited: AtomicBool,
    /// Whether stderr is a terminal. When false the spinner draws nothing
    /// and status falls back to plain `eprintln!` lines.
    enabled: bool,
}

impl ServerSpinner {
    /// Start the llama-server spinner. The fast path (server already ready)
    /// finishes it before a status ever lands, leaving no output.
    pub fn start() -> Self {
        Self::start_with("Starting llama-server…", "llama-server ready")
    }

    /// Start the spinner with custom initial and closing messages (e.g. the
    /// Ollama model check/pull at startup).
    pub fn start_with(starting: &str, ready: &str) -> Self {
        let bar = ProgressBar::new_spinner()
            .with_style(spinner_style())
            .with_message(starting.to_string());
        bar.enable_steady_tick(TICK_INTERVAL);
        Self {
            bar,
            ready: ready.to_string(),
            waited: AtomicBool::new(false),
            enabled: std::io::stderr().is_terminal(),
        }
    }

    /// Finish the wait. A successful wait that actually had to spawn or
    /// poll leaves the closing status (a plain stderr line on non-terminals);
    /// the fast path and failures clear silently — errors are reported by the
    /// caller.
    pub fn finish(&self, ok: bool) {
        if ok && self.waited.load(Ordering::SeqCst) {
            if self.enabled {
                self.bar.finish_with_message(self.ready.clone());
            } else {
                self.bar.finish_and_clear();
                crate::output::eprint_line(&self.ready);
            }
        } else {
            self.bar.finish_and_clear();
        }
    }
}

/// A spinner two callers share: one reports through it, the other finishes it.
///
/// [`crate::server::LocalServer::start`] takes an owned `Box<dyn Progress>`,
/// because the surfaces that background it need a future that outlives the
/// call. The plain-terminal callers still want to say
/// [`ServerSpinner::finish`] afterwards — the closing tick is theirs to draw,
/// since only they know the wait is over — so they keep an
/// [`Arc`](std::sync::Arc) and hand a clone of it down. Six lines rather than a
/// second lifetime on the trait.
impl Progress for std::sync::Arc<ServerSpinner> {
    fn status(&self, line: &str) {
        ServerSpinner::status(self, line);
    }

    fn bytes(&self, label: &str, total: Option<u64>) -> Box<dyn ByteProgress> {
        ServerSpinner::bytes(self, label, total)
    }
}

impl Progress for ServerSpinner {
    /// Report a status line ("waiting for the model to load…").
    fn status(&self, line: &str) {
        self.waited.store(true, Ordering::SeqCst);
        if self.enabled {
            self.bar.set_message(line.to_string());
        } else {
            crate::output::eprint_line(line);
        }
    }

    /// Open a byte-counted download phase: suspend the spinner, hand back a
    /// determinate byte bar, and restore the spinner when the guard finishes
    /// or drops. A no-op guard off-terminal, where downloads stay silent.
    fn bytes(&self, label: &str, total: Option<u64>) -> Box<dyn ByteProgress> {
        self.waited.store(true, Ordering::SeqCst);
        if !self.enabled {
            return Box::new(ServerByteProgress {
                spinner: self.bar.clone(),
                download: None,
                restored: AtomicBool::new(false),
            });
        }
        // Hand the screen to the byte bar while the download runs.
        self.bar.disable_steady_tick();
        self.bar.set_draw_target(ProgressDrawTarget::hidden());
        let download = download_bar(total);
        download.set_message(label.to_string());
        Box::new(ServerByteProgress {
            spinner: self.bar.clone(),
            download: Some(download),
            restored: AtomicBool::new(false),
        })
    }
}

/// Guard for a [`ServerSpinner`] byte phase: ticks a determinate byte bar and
/// restores the spinner (clearing the byte bar) when finished or dropped.
struct ServerByteProgress {
    /// Clone of the [`ServerSpinner`] bar (indicatif bars are `Arc`-backed,
    /// so this shares state with the original).
    spinner: ProgressBar,
    /// The byte bar, or `None` off-terminal where the phase is a no-op.
    download: Option<ProgressBar>,
    restored: AtomicBool,
}

impl ServerByteProgress {
    /// Clear the byte bar and bring the spinner back, optionally leaving
    /// `msg` as its status. Idempotent across `finish` and `Drop`.
    fn restore(&self, msg: Option<&str>) {
        if self.restored.swap(true, Ordering::SeqCst) {
            return;
        }
        match &self.download {
            Some(download) => {
                download.finish_and_clear();
                if let Some(msg) = msg.filter(|msg| !msg.is_empty()) {
                    self.spinner.set_message(msg.to_string());
                }
                self.spinner.set_draw_target(ProgressDrawTarget::stderr());
                self.spinner.enable_steady_tick(TICK_INTERVAL);
            }
            // Off-terminal: surface a closing message as a plain stderr line,
            // the way status lines fall back when there is no spinner to
            // update.
            None => {
                if let Some(msg) = msg.filter(|msg| !msg.is_empty()) {
                    crate::output::eprint_line(msg);
                }
            }
        }
    }
}

impl ByteProgress for ServerByteProgress {
    fn inc(&self, n: u64) {
        if let Some(download) = &self.download {
            download.inc(n);
        }
    }

    fn finish(self: Box<Self>, msg: &str) {
        self.restore(Some(msg));
    }
}

impl Drop for ServerByteProgress {
    fn drop(&mut self) {
        self.restore(None);
    }
}

/// A bare spinner for an indeterminate wait (e.g. `wizard doctor`'s network
/// probes): shows `msg`, animates on a terminal, and is silent otherwise.
/// [`Spinner::finish`] clears it before any report is printed.
pub struct Spinner {
    bar: ProgressBar,
}

impl Spinner {
    /// Start a spinner showing `msg`.
    pub fn start(msg: &str) -> Self {
        let bar = ProgressBar::new_spinner()
            .with_style(spinner_style())
            .with_message(msg.to_string());
        bar.enable_steady_tick(TICK_INTERVAL);
        Self { bar }
    }

    /// Clear the spinner.
    pub fn finish(self) {
        self.bar.finish_and_clear();
    }
}

/// One [`MultiProgress`] with a spinner per worker slot, sharing the TUI
/// look. Callers route interleaved lines through [`MultiProgress::println`]
/// (or `suspend`) so they land above the bars without tearing. Off-terminal
/// the bars draw nothing; callers TTY-gate the whole surface upstream.
pub fn fleet_bars(slots: usize) -> (MultiProgress, Vec<ProgressBar>) {
    let multi = MultiProgress::new();
    let bars = (0..slots)
        .map(|_| {
            let bar = multi.add(ProgressBar::new_spinner().with_style(spinner_style()));
            bar.enable_steady_tick(TICK_INTERVAL);
            bar
        })
        .collect();
    (multi, bars)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UiConfig;

    #[test]
    fn download_bar_counts_in_both_modes() {
        // Determinate: a known total caps the bar's length.
        let bar = download_bar(Some(1_000));
        bar.inc(400);
        assert_eq!(bar.position(), 400);
        bar.finish_and_clear();

        // Indeterminate: no length, but bytes still accumulate.
        let bar = download_bar(None);
        bar.inc(128);
        assert_eq!(bar.position(), 128);
        bar.finish_and_clear();
    }

    #[test]
    fn server_byte_progress_is_safe_off_terminal() {
        // Under captured stderr `bytes()` returns a no-op guard; ticking and
        // finishing it must stay quiet and not touch the spinner state.
        let spinner = ServerSpinner::start();
        let bar = spinner.bytes("downloading model", Some(2_048));
        bar.inc(1_024);
        bar.finish("saved /tmp/model.gguf");
        // A dropped (unfinished) guard restores cleanly too.
        let bar = spinner.bytes("downloading again", None);
        bar.inc(10);
        drop(bar);
        spinner.finish(true);
    }

    #[test]
    fn spinner_lifecycle_is_safe_off_terminal() {
        let spinner = Spinner::start("running checks…");
        spinner.finish();
    }

    #[test]
    fn fleet_bars_yields_one_bar_per_slot() {
        let (multi, bars) = fleet_bars(3);
        assert_eq!(bars.len(), 3);
        for (i, bar) in bars.iter().enumerate() {
            bar.set_message(format!("task-{i} · 0s"));
        }
        // Routing a line through the group is a quiet no-op off-terminal.
        let _ = multi.println("→ task-0 started");
        for bar in &bars {
            bar.finish_and_clear();
        }
        let _ = multi.clear();
    }

    #[test]
    fn bar_counts_and_finishes() {
        let bar = bar(3);
        bar.set_message("case-1");
        bar.inc(1);
        bar.suspend(|| {});
        assert_eq!(bar.position(), 1);
        // finish_and_clear advances the bar to its length.
        bar.finish_and_clear();
        assert_eq!(bar.position(), 3);
        assert!(bar.is_finished());
    }

    #[test]
    fn turn_spinner_lifecycle_is_safe_off_terminal() {
        // Under `cargo test` stderr is captured (not a terminal), so every
        // call must be a quiet no-op rather than escape-sequence garbage.
        let spinner = TurnSpinner::new();
        spinner.set_verb("Conjuring");
        spinner.show();
        spinner.hide();
        spinner.show();
        spinner.finish();
    }

    #[test]
    fn turn_spinner_accepts_configured_verbs() {
        // The headless path feeds verbs from the same UiConfig mechanism the
        // TUI uses; any verb from any seed must be representable.
        let ui = UiConfig::default();
        let spinner = TurnSpinner::new();
        for seed in 0..16 {
            spinner.set_verb(ui.spinner_verb(seed));
        }
        spinner.finish();
    }

    #[test]
    fn server_spinner_is_silent_without_a_wait() {
        let wait = ServerSpinner::start();
        assert!(!wait.waited.load(Ordering::SeqCst));
        wait.finish(true); // fast path: no "ready" line, just a clear

        let wait = ServerSpinner::start();
        wait.status("waiting for the model to load…");
        assert!(wait.waited.load(Ordering::SeqCst));
        wait.finish(false); // failure: cleared, the caller prints the error
    }
}
