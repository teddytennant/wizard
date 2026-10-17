//! Terminal lifecycle and the event pump.
//!
//! [`TermGuard`] puts the terminal in full-screen mode and puts it back on drop, on panic, and
//! on SIGINT/SIGTERM/SIGHUP. [`EventPump`] merges crossterm events, a tick, and the app's own
//! message channel into one stream, collapsing resize bursts.

use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event as CtEvent, EventStream, KeyEvent, KeyEventKind,
    KeyboardEnhancementFlags, MouseEvent, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, queue};
use futures_util::{Stream, StreamExt};
use ratatui::backend::CrosstermBackend;
use std::io::{self, Stdout, Write};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Once};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub type Terminal = ratatui::Terminal<CrosstermBackend<Stdout>>;

#[derive(Clone, Copy, Debug)]
pub struct TermOptions {
    pub alt_screen: bool,
    pub mouse: bool,
    pub bracketed_paste: bool,
    pub focus_events: bool,
    /// Ask for the kitty keyboard protocol (so shift+enter and ctrl+i are distinguishable)
    /// when the terminal supports it.
    pub keyboard_enhancement: bool,
    pub hide_cursor: bool,
}

impl Default for TermOptions {
    fn default() -> Self {
        Self {
            alt_screen: true,
            mouse: true,
            bracketed_paste: true,
            focus_events: false,
            keyboard_enhancement: true,
            hide_cursor: false,
        }
    }
}

// What is currently switched on, so the panic hook and the signal thread can undo exactly that
// without holding a reference to the guard.
const F_ALT: u16 = 1;
const F_MOUSE: u16 = 2;
const F_PASTE: u16 = 4;
const F_FOCUS: u16 = 8;
const F_KBD: u16 = 16;
const F_RAW: u16 = 32;
/// xterm's modifyOtherKeys, which tmux and xterm answer where the kitty protocol is not there.
const F_MOK: u16 = 64;
/// A blinking block cursor in a chosen color, undone on the way out.
const F_CUR: u16 = 128;
/// Mode 2031: the terminal reports colour scheme changes.
const F_SCHEME: u16 = 256;
/// The window title was pushed on the terminal's title stack (`CSI 22 ; 0 t`).
const F_TITLE: u16 = 512;
/// Mode 2026, synchronized output, which the app wraps around each frame.
const F_SYNC: u16 = 1024;
/// Mode 2027, grapheme clusters.
const F_G2027: u16 = 2048;
static ACTIVE: AtomicU16 = AtomicU16::new(0);
static KBD_SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Ask the terminal once. The reply comes on stdin, so asking again after the app's event reader
/// is running (on `resume`) would race it; the answer is kept.
fn keyboard_enhancement_supported() -> bool {
    *KBD_SUPPORTED.get_or_init(|| probe_keyboard(PROBE_TIMEOUT))
}

/// Tell the toolkit what a startup probe already learned (the kitty keyboard protocol is
/// supported or not), so [`TermGuard::enter`] does not ask a second time. Call before `enter`;
/// the first answer, from here or from the toolkit's own query, is kept.
pub fn set_keyboard_enhancement_supported(supported: bool) {
    let _ = KBD_SUPPORTED.set(supported);
}

/// How long the startup probe waits for the terminal. crossterm's own probe waits two seconds,
/// which is what a terminal that never answers (a serial console, some multiplexers, a pipe in
/// the middle) cost at every start, with whatever was typed meanwhile thrown away.
const PROBE_TIMEOUT: Duration = Duration::from_millis(250);

mod sys {
    use std::os::raw::{c_int, c_ulong};
    #[repr(C)]
    pub struct PollFd {
        pub fd: c_int,
        pub events: i16,
        pub revents: i16,
    }
    extern "C" {
        pub fn poll(fds: *mut PollFd, n: c_ulong, timeout: c_int) -> c_int;
        pub fn read(fd: c_int, buf: *mut u8, n: usize) -> isize;
        pub fn isatty(fd: c_int) -> c_int;
    }
    pub const POLLIN: i16 = 1;
}

/// What the replies to `CSI ? u` followed by a DA1 query say: `(answered, kitty keyboard)`.
/// Every terminal that answers anything answers DA1, so its reply ends the wait.
fn parse_keyboard_replies(b: &[u8]) -> (bool, bool) {
    let (mut da1, mut kbd) = (false, false);
    let mut i = 0;
    while i + 3 < b.len() {
        if b[i] == 0x1b && b[i + 1] == b'[' && b[i + 2] == b'?' {
            let mut j = i + 3;
            while j < b.len() && (b[j].is_ascii_digit() || b[j] == b';') {
                j += 1;
            }
            match b.get(j) {
                Some(b'c') => da1 = true,
                Some(b'u') if j > i + 3 => kbd = true,
                _ => {}
            }
        }
        i += 1;
    }
    (da1, kbd)
}

/// Ask for the kitty keyboard flags and a DA1, and read until DA1 or `timeout`. Raw mode must
/// be on and nothing else may be reading the terminal yet.
fn probe_keyboard(timeout: Duration) -> bool {
    use std::os::fd::{FromRawFd, IntoRawFd};
    let mut out = io::stdout();
    if out.write_all(b"\x1b[?u\x1b[c").is_err() || out.flush().is_err() {
        return false;
    }
    let (fd, opened) = if unsafe { sys::isatty(0) } == 1 {
        (0, false)
    } else {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
        {
            Ok(f) => (f.into_raw_fd(), true),
            Err(_) => return false,
        }
    };
    let end = std::time::Instant::now() + timeout;
    let mut got = Vec::new();
    let mut buf = [0u8; 256];
    let kbd = loop {
        let (da1, kbd) = parse_keyboard_replies(&got);
        if da1 {
            break kbd;
        }
        let left = end.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break false;
        }
        let mut fds = [sys::PollFd {
            fd,
            events: sys::POLLIN,
            revents: 0,
        }];
        let ms = left.as_millis().clamp(1, i32::MAX as u128) as i32;
        // SAFETY: one valid pollfd, and a buffer of the length passed to read.
        let n = unsafe { sys::poll(fds.as_mut_ptr(), 1, ms) };
        if n <= 0 {
            break false;
        }
        let n = unsafe { sys::read(fd, buf.as_mut_ptr(), buf.len()) };
        if n <= 0 {
            break false;
        }
        got.extend_from_slice(&buf[..n as usize]);
    };
    if opened {
        // SAFETY: the descriptor was opened above and is not used again.
        drop(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) });
    }
    kbd
}
static HOOKS: Once = Once::new();

fn flag_set(flag: u16, on: bool) {
    if on {
        ACTIVE.fetch_or(flag, Ordering::SeqCst);
    } else {
        ACTIVE.fetch_and(!flag, Ordering::SeqCst);
    }
}

/// Write the escape sequences that undo `flags`. Order matters: leave the alternate screen last
/// so the cursor lands where the user's shell expects it.
fn write_leave(w: &mut impl Write, flags: u16) -> io::Result<()> {
    if flags & F_SYNC != 0 {
        w.write_all(b"\x1b[?2026l")?;
    }
    if flags & F_G2027 != 0 {
        w.write_all(b"\x1b[?2027l")?;
    }
    if flags & F_TITLE != 0 {
        w.write_all(b"\x1b[23;0t")?;
    }
    if flags & F_CUR != 0 {
        w.write_all(b"\x1b[0 q\x1b]112\x07")?;
    }
    if flags & F_SCHEME != 0 {
        w.write_all(b"\x1b[?2031l")?;
    }
    if flags & F_MOK != 0 {
        w.write_all(b"\x1b[>4;0m")?;
    }
    if flags & F_KBD != 0 {
        queue!(w, PopKeyboardEnhancementFlags)?;
    }
    if flags & F_FOCUS != 0 {
        queue!(w, DisableFocusChange)?;
    }
    if flags & F_PASTE != 0 {
        queue!(w, DisableBracketedPaste)?;
    }
    if flags & F_MOUSE != 0 {
        queue!(w, DisableMouseCapture)?;
    }
    queue!(w, Show)?;
    if flags & F_ALT != 0 {
        queue!(w, LeaveAlternateScreen)?;
    }
    w.flush()
}

/// Idempotent restore. Safe from a panic hook or a signal thread.
pub fn restore_terminal() {
    let flags = ACTIVE.swap(0, Ordering::SeqCst);
    if flags == 0 {
        return;
    }
    let mut out = io::stdout();
    let _ = write_leave(&mut out, flags);
    if flags & F_RAW != 0 {
        let _ = disable_raw_mode();
    }
    // Panic messages from other threads were held back while the screen was ours.
    let held = std::mem::take(&mut *HELD_PANICS.lock().unwrap_or_else(|e| e.into_inner()));
    for m in held {
        eprintln!("{m}");
    }
}

/// Messages of panics on threads other than the one that owns the terminal, kept until the
/// terminal is back: printing them over the alternate screen would only garble it.
static HELD_PANICS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
static OWNER: std::sync::OnceLock<std::thread::ThreadId> = std::sync::OnceLock::new();

#[derive(Debug, PartialEq, Eq)]
enum PanicPolicy {
    /// The panic ends the UI: put the terminal back, then print.
    Restore,
    /// Another thread panicked while the screen is ours: keep the message for later.
    Hold,
    /// Nothing is on screen: print as usual.
    Print,
}

/// Only the thread that entered full-screen mode can take the terminal down with it. A worker's
/// panic (a backend task, a helper thread) leaves the UI loop running, and restoring the screen
/// under it left the app drawing frames onto the user's shell.
fn panic_policy(
    owner: Option<std::thread::ThreadId>,
    here: std::thread::ThreadId,
    active: bool,
) -> PanicPolicy {
    if owner.is_none_or(|id| id == here) {
        PanicPolicy::Restore
    } else if active {
        PanicPolicy::Hold
    } else {
        PanicPolicy::Print
    }
}

fn install_hooks() {
    HOOKS.call_once(|| {
        let _ = OWNER.set(std::thread::current().id());
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let policy = panic_policy(
                OWNER.get().copied(),
                std::thread::current().id(),
                ACTIVE.load(Ordering::SeqCst) != 0,
            );
            if policy == PanicPolicy::Restore {
                // Restore first so the panic message is readable on the normal screen.
                restore_terminal();
                prev(info);
            } else if policy == PanicPolicy::Hold {
                let t = std::thread::current();
                let loc = info
                    .location()
                    .map_or(String::new(), |l| format!(" at {l}"));
                let msg = match info.payload().downcast_ref::<&str>() {
                    Some(m) => (*m).to_string(),
                    None => info
                        .payload()
                        .downcast_ref::<String>()
                        .cloned()
                        .unwrap_or_default(),
                };
                HELD_PANICS
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(format!(
                        "thread '{}' panicked{loc}: {msg}",
                        t.name().unwrap_or("<unnamed>")
                    ));
            } else {
                prev(info);
            }
        }));
        #[cfg(unix)]
        {
            use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGUSR1, SIGUSR2};
            use signal_hook::iterator::Signals;
            if let Ok(mut signals) = Signals::new([SIGINT, SIGTERM, SIGHUP, SIGUSR1, SIGUSR2]) {
                std::thread::Builder::new()
                    .name("tuikit-signals".into())
                    .spawn(move || {
                        if let Some(sig) = signals.forever().next() {
                            restore_terminal();
                            // No destructor runs after this thread exits the process, so the
                            // backend's tool processes are killed here or they outlive us.
                            agent_core::procs::kill_all();
                            // Run the default action (terminate) now that the screen is sane.
                            let _ = signal_hook::low_level::emulate_default_handler(sig);
                            std::process::exit(128 + sig);
                        }
                    })
                    .ok();
            }
        }
    });
}

/// Owns the full-screen terminal state. Dropping it restores the terminal.
pub struct TermGuard {
    opts: TermOptions,
    mouse_on: bool,
    scheme: bool,
    grapheme: bool,
    sync: bool,
    title: Option<String>,
}

impl TermGuard {
    pub fn enter(opts: TermOptions) -> io::Result<Self> {
        install_hooks();
        enable_raw_mode()?;
        flag_set(F_RAW, true);
        let mut guard = Self {
            opts,
            mouse_on: false,
            scheme: false,
            grapheme: false,
            sync: false,
            title: None,
        };
        let mut out = io::stdout();
        let setup = (|| -> io::Result<()> {
            if opts.alt_screen {
                execute!(out, EnterAlternateScreen)?;
                flag_set(F_ALT, true);
            }
            if opts.bracketed_paste {
                execute!(out, EnableBracketedPaste)?;
                flag_set(F_PASTE, true);
            }
            if opts.focus_events {
                execute!(out, EnableFocusChange)?;
                flag_set(F_FOCUS, true);
            }
            if opts.keyboard_enhancement && keyboard_enhancement_supported() {
                execute!(
                    out,
                    PushKeyboardEnhancementFlags(
                        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    )
                )?;
                flag_set(F_KBD, true);
            }
            if opts.keyboard_enhancement {
                // `CSI > 4 ; 1 m`: shift+enter and friends reach us as `CSI u` in tmux and xterm,
                // as opencode asks for it too
                out.write_all(b"\x1b[>4;1m")?;
                out.flush()?;
                flag_set(F_MOK, true);
            }
            if opts.hide_cursor {
                execute!(out, Hide)?;
            }
            Ok(())
        })();
        if let Err(e) = setup {
            guard.restore();
            return Err(e);
        }
        guard.set_mouse(opts.mouse)?;
        Ok(guard)
    }

    /// Blinking block cursor in this color (`CSI 1 SP q` and OSC 12), as opencode sets it.
    /// Undone with everything else when the guard restores.
    pub fn set_cursor_color(&mut self, (r, g, b): (u8, u8, u8)) -> io::Result<()> {
        let mut out = io::stdout();
        write!(out, "\x1b[1 q\x1b]12;#{r:02x}{g:02x}{b:02x}\x07")?;
        out.flush()?;
        flag_set(F_CUR, true);
        Ok(())
    }

    /// Set the window title, keeping the one the shell had on the terminal's title stack so
    /// leaving (by quit, signal or panic) puts it back. The text is untrusted (a directory name,
    /// a session title): it goes through [`crate::width::plain_text`], so BEL or ESC in it
    /// cannot end the title early and write whatever follows.
    pub fn set_title(&mut self, title: &str) -> io::Result<()> {
        let t = crate::width::plain_text(title).replace('\n', " ");
        let t = crate::width::truncate(&t, 100);
        let mut out = io::stdout();
        write!(out, "\x1b[22;0t\x1b]2;{t}\x07")?;
        out.flush()?;
        flag_set(F_TITLE, true);
        self.title = Some(t);
        Ok(())
    }

    /// Ask for mode 2027 (grapheme clusters). Switched off again on every way out.
    pub fn enable_grapheme_clusters(&mut self) -> io::Result<()> {
        let mut out = io::stdout();
        out.write_all(b"\x1b[?2027h")?;
        out.flush()?;
        flag_set(F_G2027, true);
        self.grapheme = true;
        Ok(())
    }

    /// Record that the app wraps its frames in mode 2026 (synchronized output), so a signal or
    /// a panic in the middle of one does not leave the terminal holding its output back.
    pub fn note_synchronized_output(&mut self) {
        flag_set(F_SYNC, true);
        self.sync = true;
    }

    /// Turn mouse reporting on or off (off lets the terminal select text natively).
    pub fn set_mouse(&mut self, on: bool) -> io::Result<()> {
        if on == self.mouse_on {
            return Ok(());
        }
        let mut out = io::stdout();
        if on {
            execute!(out, EnableMouseCapture)?;
        } else {
            execute!(out, DisableMouseCapture)?;
        }
        flag_set(F_MOUSE, on);
        self.mouse_on = on;
        Ok(())
    }

    /// Ask the terminal to report light and dark scheme changes (mode 2031) and for the current
    /// scheme. Reports arrive as `Event::ColorScheme` from [`crate::input::Input`]. Terminals
    /// that do not know the mode ignore both.
    pub fn enable_scheme_reports(&mut self) -> io::Result<()> {
        let mut out = io::stdout();
        out.write_all(b"\x1b[?2031h\x1b[?996n")?;
        out.flush()?;
        flag_set(F_SCHEME, true);
        self.scheme = true;
        Ok(())
    }

    /// Mode 2031 alone, without asking for the current scheme: the terminal tells us when it
    /// switches, which is all a program that re-queries its colours then needs. Pi sends exactly
    /// this.
    pub fn enable_scheme_notifications(&mut self) -> io::Result<()> {
        let mut out = io::stdout();
        out.write_all(b"\x1b[?2031h")?;
        out.flush()?;
        flag_set(F_SCHEME, true);
        self.scheme = true;
        Ok(())
    }

    pub fn mouse_enabled(&self) -> bool {
        self.mouse_on
    }

    /// Whether the kitty keyboard protocol was negotiated.
    pub fn keyboard_enhanced(&self) -> bool {
        ACTIVE.load(Ordering::SeqCst) & F_KBD != 0
    }

    /// Hand the terminal back (for `$EDITOR` or a subshell). Call [`resume`](Self::resume) after.
    pub fn suspend(&mut self) {
        restore_terminal();
        self.mouse_on = false;
    }

    pub fn resume(&mut self) -> io::Result<()> {
        let scheme = self.scheme;
        let mut new = Self::enter(self.opts)?;
        if scheme {
            // Without the query: the scheme did not change while the editor ran, or the
            // terminal will say so by itself.
            let mut out = io::stdout();
            let _ = out.write_all(b"\x1b[?2031h");
            let _ = out.flush();
            flag_set(F_SCHEME, true);
            new.scheme = true;
        }
        let (grapheme, sync, title) = (self.grapheme, self.sync, self.title.take());
        // Dropping the old guard would run restore_terminal() on the state just entered.
        std::mem::forget(std::mem::replace(self, new));
        // `suspend` undid these on the way out.
        if grapheme {
            let _ = self.enable_grapheme_clusters();
        }
        if sync {
            self.note_synchronized_output();
        }
        if let Some(t) = title {
            let _ = self.set_title(&t);
        }
        Ok(())
    }

    pub fn restore(&mut self) {
        restore_terminal();
        self.mouse_on = false;
    }

    /// A ratatui terminal on stdout.
    pub fn terminal(&self) -> io::Result<Terminal> {
        ratatui::Terminal::new(CrosstermBackend::new(io::stdout()))
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Everything the app loop reacts to. `T` is the app's own message type.
#[derive(Debug, Clone, PartialEq)]
pub enum Event<T> {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String),
    Resize(u16, u16),
    FocusGained,
    FocusLost,
    /// The terminal reported a colour scheme change (mode 2031); `true` is dark.
    ColorScheme(bool),
    Tick,
    Msg(T),
}

/// Drops key-release events (the kitty protocol reports them only if asked, but be safe) and
/// holds a resize until the burst ends. Pure, so it is tested without a terminal.
#[derive(Debug, Default)]
pub struct Coalescer {
    pending_resize: Option<(u16, u16)>,
}

impl Coalescer {
    pub fn has_pending(&self) -> bool {
        self.pending_resize.is_some()
    }

    /// Feed one terminal event; returns events that are ready now, in order.
    pub fn push<T>(&mut self, ev: CtEvent) -> Vec<Event<T>> {
        let mut out = Vec::new();
        match ev {
            CtEvent::Resize(w, h) => self.pending_resize = Some((w, h)),
            other => {
                // Anything else flushes a pending resize first so ordering is preserved.
                out.extend(self.flush());
                match other {
                    CtEvent::Key(k) if k.kind == KeyEventKind::Release => {}
                    CtEvent::Key(k) => out.push(Event::Key(k)),
                    CtEvent::Mouse(m) => out.push(Event::Mouse(m)),
                    CtEvent::Paste(s) => out.push(Event::Paste(s)),
                    CtEvent::FocusGained => out.push(Event::FocusGained),
                    CtEvent::FocusLost => out.push(Event::FocusLost),
                    CtEvent::Resize(..) => unreachable!(),
                }
            }
        }
        out
    }

    /// Like [`push`](Self::push), for the events of the [`input`](crate::input) reader.
    pub fn push_raw<T>(&mut self, raw: crate::input::Raw) -> Vec<Event<T>> {
        match raw {
            crate::input::Raw::Ct(ev) => self.push(ev),
            crate::input::Raw::Scheme(dark) => {
                let mut out: Vec<Event<T>> = self.flush().into_iter().collect();
                out.push(Event::ColorScheme(dark));
                out
            }
        }
    }

    pub fn flush<T>(&mut self) -> Option<Event<T>> {
        self.pending_resize.take().map(|(w, h)| Event::Resize(w, h))
    }
}

/// How long a resize waits for more resizes before it is delivered.
pub const RESIZE_SETTLE: Duration = Duration::from_millis(30);

/// One async stream of terminal events, ticks and app messages.
pub struct EventPump<T> {
    rx: mpsc::UnboundedReceiver<Event<T>>,
    tick_pending: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl<T: Send + 'static> EventPump<T> {
    /// Read the real terminal. `tick` of `Duration::ZERO` disables ticks.
    pub fn spawn(tick: Duration, app_rx: mpsc::UnboundedReceiver<T>) -> Self {
        Self::spawn_with(EventStream::new(), tick, app_rx)
    }

    /// Same, over any source of terminal events (tests feed a channel).
    pub fn spawn_with<S>(source: S, tick: Duration, app_rx: mpsc::UnboundedReceiver<T>) -> Self
    where
        S: Stream<Item = io::Result<CtEvent>> + Send + 'static,
    {
        Self::spawn_raw(source.map(|r| r.map(crate::input::Raw::Ct)), tick, app_rx)
    }

    /// Over the decoded input of [`crate::input::Input`], which also reports colour scheme
    /// changes.
    pub fn spawn_raw<S>(source: S, tick: Duration, mut app_rx: mpsc::UnboundedReceiver<T>) -> Self
    where
        S: Stream<Item = io::Result<crate::input::Raw>> + Send + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel();
        let tick_pending = Arc::new(AtomicBool::new(false));
        let tick_flag = tick_pending.clone();
        let task = tokio::spawn(async move {
            let mut source = Box::pin(source);
            let mut co = Coalescer::default();
            let mut ticker = (!tick.is_zero()).then(|| {
                let mut i = tokio::time::interval(tick);
                i.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                i
            });
            let mut app_open = true;
            loop {
                let settle = async {
                    if co.has_pending() {
                        tokio::time::sleep(RESIZE_SETTLE).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                };
                let tick_fut = async {
                    match ticker.as_mut() {
                        Some(t) => {
                            t.tick().await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                };
                tokio::select! {
                    ev = source.next() => match ev {
                        Some(Ok(ev)) => {
                            for e in co.push_raw::<T>(ev) {
                                if tx.send(e).is_err() { return; }
                            }
                        }
                        // A read error or EOF means the terminal is gone. Closing the
                        // channel lets the app loop end instead of spinning.
                        Some(Err(_)) | None => {
                            if let Some(e) = co.flush::<T>() { let _ = tx.send(e); }
                            return;
                        }
                    },
                    _ = settle => {
                        if let Some(e) = co.flush::<T>() {
                            if tx.send(e).is_err() { return; }
                        }
                    }
                    _ = tick_fut => {
                        // Skip the tick if the app has not consumed the last one.
                        if !tick_flag.swap(true, Ordering::SeqCst) && tx.send(Event::Tick).is_err() {
                            return;
                        }
                    }
                    msg = app_rx.recv(), if app_open => match msg {
                        Some(m) => if tx.send(Event::Msg(m)).is_err() { return; },
                        None => app_open = false,
                    },
                }
            }
        });
        Self {
            rx,
            tick_pending,
            task,
        }
    }

    /// Next event, or `None` when the terminal source has closed.
    pub async fn next(&mut self) -> Option<Event<T>> {
        let ev = self.rx.recv().await;
        if matches!(ev, Some(Event::Tick)) {
            self.tick_pending.store(false, Ordering::SeqCst);
        }
        ev
    }
}

impl<T> Drop for EventPump<T> {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl<T: Send + 'static> Stream for EventPump<T> {
    type Item = Event<T>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let poll = self.rx.poll_recv(cx);
        if let Poll::Ready(Some(Event::Tick)) = &poll {
            self.tick_pending.store(false, Ordering::SeqCst);
        }
        poll
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers, MouseEventKind};

    fn key(c: char) -> CtEvent {
        CtEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    #[test]
    fn keyboard_replies_are_told_apart_and_da1_ends_the_wait() {
        // kitty flags answer, then DA1
        assert_eq!(parse_keyboard_replies(b"\x1b[?0u\x1b[?62;4c"), (true, true));
        // a terminal without the protocol answers only DA1
        assert_eq!(parse_keyboard_replies(b"\x1b[?1;2c"), (true, false));
        // nothing, or half a reply: keep waiting
        assert_eq!(parse_keyboard_replies(b""), (false, false));
        assert_eq!(parse_keyboard_replies(b"\x1b[?0u"), (false, true));
        assert_eq!(parse_keyboard_replies(b"\x1b[?"), (false, false));
        // `CSI ? u` with no digits is not a flags reply
        assert_eq!(parse_keyboard_replies(b"\x1b[?u\x1b[?1c"), (true, false));
    }

    #[test]
    fn coalescer_keeps_only_the_last_resize_of_a_burst() {
        let mut co = Coalescer::default();
        assert!(co.push::<()>(CtEvent::Resize(80, 24)).is_empty());
        assert!(co.push::<()>(CtEvent::Resize(100, 30)).is_empty());
        assert!(co.has_pending());
        assert_eq!(co.flush::<()>(), Some(Event::Resize(100, 30)));
        assert_eq!(co.flush::<()>(), None);
    }

    #[test]
    fn coalescer_flushes_resize_before_the_next_event() {
        let mut co = Coalescer::default();
        co.push::<()>(CtEvent::Resize(1, 2));
        let out = co.push::<()>(key('x'));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], Event::Resize(1, 2));
        assert!(matches!(out[1], Event::Key(_)));
    }

    #[test]
    fn coalescer_drops_key_releases() {
        let mut co = Coalescer::default();
        let mut k = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        k.kind = KeyEventKind::Release;
        assert!(co.push::<()>(CtEvent::Key(k)).is_empty());
        k.kind = KeyEventKind::Repeat;
        assert_eq!(co.push::<()>(CtEvent::Key(k)).len(), 1);
    }

    #[test]
    fn leave_sequence_undoes_exactly_what_was_enabled() {
        let mut v = Vec::new();
        write_leave(&mut v, F_ALT | F_MOUSE | F_PASTE).unwrap();
        let s = String::from_utf8(v).unwrap();
        assert!(s.contains("\x1b[?1049l"), "alt screen: {s:?}");
        assert!(s.contains("\x1b[?2004l"), "paste: {s:?}");
        assert!(s.contains("\x1b[?1000l"), "mouse: {s:?}");
        assert!(s.contains("\x1b[?25h"), "cursor: {s:?}");
        assert!(
            !s.contains("\x1b[<1u"),
            "keyboard flags were never pushed: {s:?}"
        );
        // The alternate screen is left last.
        assert!(s.rfind("\x1b[?1049l").unwrap() > s.rfind("\x1b[?2004l").unwrap());

        let mut v = Vec::new();
        write_leave(&mut v, F_KBD).unwrap();
        assert!(String::from_utf8(v).unwrap().contains("\x1b[<1u"));
    }

    #[test]
    fn a_leave_undoes_title_sync_and_grapheme_modes() {
        let mut v = Vec::new();
        write_leave(&mut v, F_ALT | F_TITLE | F_SYNC | F_G2027).unwrap();
        let s = String::from_utf8(v).unwrap();
        for want in ["\x1b[?2026l", "\x1b[?2027l", "\x1b[23;0t", "\x1b[?1049l"] {
            assert!(s.contains(want), "{want:?} missing from {s:?}");
        }
        // and writes none of them when they were never turned on
        let mut v = Vec::new();
        write_leave(&mut v, F_ALT).unwrap();
        let s = String::from_utf8(v).unwrap();
        assert!(
            !s.contains("2026") && !s.contains("2027") && !s.contains("23;0t"),
            "{s:?}"
        );
    }

    #[test]
    fn only_the_owning_thread_takes_the_terminal_down() {
        let me = std::thread::current().id();
        let other = std::thread::spawn(|| std::thread::current().id())
            .join()
            .unwrap();
        assert_eq!(panic_policy(Some(me), me, true), PanicPolicy::Restore);
        assert_eq!(panic_policy(Some(me), other, true), PanicPolicy::Hold);
        assert_eq!(panic_policy(Some(me), other, false), PanicPolicy::Print);
        assert_eq!(panic_policy(None, other, true), PanicPolicy::Restore);
    }

    #[test]
    fn smoke_restore_without_a_guard_does_not_panic() {
        // Nothing is active in the test process, so this must not touch the tty.
        restore_terminal();
        restore_terminal();
    }

    fn chan_stream(
        rx: mpsc::UnboundedReceiver<io::Result<CtEvent>>,
    ) -> impl Stream<Item = io::Result<CtEvent>> + Send + 'static {
        futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|v| (v, rx)) })
    }

    #[tokio::test]
    async fn pump_merges_terminal_app_messages_and_ticks() {
        let (ct_tx, ct_rx) = mpsc::unbounded_channel();
        let (app_tx, app_rx) = mpsc::unbounded_channel::<&'static str>();
        let mut pump = EventPump::spawn_with(chan_stream(ct_rx), Duration::from_millis(10), app_rx);
        ct_tx.send(Ok(key('a'))).unwrap();
        app_tx.send("hello").unwrap();
        let mut saw_key = false;
        let mut saw_msg = false;
        let mut saw_tick = false;
        for _ in 0..20 {
            match tokio::time::timeout(Duration::from_secs(2), pump.next())
                .await
                .unwrap()
            {
                Some(Event::Key(_)) => saw_key = true,
                Some(Event::Msg("hello")) => saw_msg = true,
                Some(Event::Tick) => saw_tick = true,
                other => panic!("unexpected {other:?}"),
            }
            if saw_key && saw_msg && saw_tick {
                break;
            }
        }
        assert!(saw_key && saw_msg && saw_tick);
    }

    #[tokio::test]
    async fn pump_collapses_resize_burst_and_ends_when_terminal_closes() {
        let (ct_tx, ct_rx) = mpsc::unbounded_channel();
        let (_app_tx, app_rx) = mpsc::unbounded_channel::<()>();
        let mut pump = EventPump::spawn_with(chan_stream(ct_rx), Duration::ZERO, app_rx);
        for w in 1..=20 {
            ct_tx.send(Ok(CtEvent::Resize(w, 10))).unwrap();
        }
        let first = tokio::time::timeout(Duration::from_secs(2), pump.next())
            .await
            .unwrap();
        assert_eq!(first, Some(Event::Resize(20, 10)));
        drop(ct_tx);
        let end = tokio::time::timeout(Duration::from_secs(2), pump.next())
            .await
            .unwrap();
        assert_eq!(end, None);
    }

    #[tokio::test]
    async fn slow_consumer_does_not_pile_up_ticks() {
        let (_ct_tx, ct_rx) = mpsc::unbounded_channel();
        let (_app_tx, app_rx) = mpsc::unbounded_channel::<()>();
        let mut pump = EventPump::spawn_with(chan_stream(ct_rx), Duration::from_millis(5), app_rx);
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(pump.next().await, Some(Event::Tick));
        // 24 intervals elapsed but only the one unconsumed tick was ever queued (the test
        // runtime is single-threaded, so the ticker cannot have refilled the channel yet).
        assert!(pump.rx.try_recv().is_err());
    }

    #[test]
    fn mouse_events_pass_through() {
        let mut co = Coalescer::default();
        let m = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 1,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(co.push::<()>(CtEvent::Mouse(m)), vec![Event::Mouse(m)]);
    }
}
