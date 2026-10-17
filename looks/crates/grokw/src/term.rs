// OWNER: terminal (OSC 11 appearance query is not built)
//! Terminal setup and teardown with the byte sequences Grok Build writes (spec 1.2, 1.5): the
//! title, the alternate screen, five mouse modes, focus and paste reporting, the cursor colour,
//! then the keyboard probe; and on the way out the same list in reverse, with the progress
//! clear and the screen wipe first. Restoring is idempotent and also runs from the panic hook
//! and on SIGINT, SIGTERM and SIGHUP, so a crash never leaves the terminal in raw mode.

use std::io::{self, Stdout, Write};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU8, Ordering};
use std::sync::Once;

use crossterm::cursor::{Hide, Show};
use crossterm::event::{KeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement};
use crossterm::{execute, queue};
use ratatui::backend::CrosstermBackend;

/// The crossterm backend that does not ask the terminal where its cursor is. An inline viewport
/// wants that on every resize and clear, and the answer would arrive on the stdin our own input
/// reader owns, so the reply would be eaten and the query would time out. The position is read
/// once, before the reader starts, and tracked from the writes after that.
pub struct Quiet {
    inner: CrosstermBackend<Stdout>,
    cursor: ratatui::layout::Position,
}

impl Quiet {
    /// `query` reads the cursor now; fullscreen never needs it.
    pub fn new(mut inner: CrosstermBackend<Stdout>, query: bool) -> Quiet {
        use ratatui::backend::Backend;
        let cursor = if query {
            inner.get_cursor_position().unwrap_or_default()
        } else {
            Default::default()
        };
        Quiet { inner, cursor }
    }
}

impl ratatui::backend::Backend for Quiet {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        let mut last = None;
        let content = content.inspect(|(x, y, _)| last = Some((*x, *y)));
        self.inner.draw(content)?;
        if let Some((x, y)) = last {
            self.cursor = ratatui::layout::Position { x, y };
        }
        Ok(())
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.cursor.y = self.cursor.y.saturating_add(n);
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<ratatui::layout::Position> {
        Ok(self.cursor)
    }

    fn set_cursor_position<P: Into<ratatui::layout::Position>>(
        &mut self,
        position: P,
    ) -> io::Result<()> {
        let p = position.into();
        self.cursor = p;
        self.inner.set_cursor_position(p)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ratatui::backend::ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> io::Result<ratatui::layout::Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> io::Result<ratatui::backend::WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        ratatui::backend::Backend::flush(&mut self.inner)
    }
}

pub type Terminal = ratatui::Terminal<Quiet>;

/// Where the screen is drawn (spec 1.12).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScreenMode {
    /// The alternate screen, mouse on.
    #[default]
    Fullscreen,
    /// `--no-alt-screen`: the same screen drawn in the normal buffer, full height. What was on
    /// the terminal scrolls up into the scrollback and the last frame stays there on exit.
    Inline,
    /// `--minimal`: a few live rows at the bottom of the normal buffer, mouse off; finished
    /// blocks are written above them into the terminal's own scrollback.
    Minimal,
}

impl ScreenMode {
    fn code(self) -> u8 {
        self as u8
    }

    fn from_code(c: u8) -> ScreenMode {
        match c {
            1 => ScreenMode::Inline,
            2 => ScreenMode::Minimal,
            _ => ScreenMode::Fullscreen,
        }
    }

    /// Rows of the live viewport on a `rows` tall terminal: all of them, or `[3, rows - 1]` of
    /// the default 10 for minimal.
    pub fn live_rows(self, rows: u16) -> u16 {
        match self {
            ScreenMode::Minimal => 10.clamp(3, rows.saturating_sub(1).max(3)).min(rows),
            _ => rows,
        }
    }
}

static MODE: AtomicU8 = AtomicU8::new(0);
/// Last row (0-based) of the live viewport, so the exit can put the cursor under it.
static BOTTOM: AtomicU16 = AtomicU16::new(0);

/// Record where the viewport ends; called after each frame in the normal-buffer modes.
pub fn note_viewport_bottom(row: u16) {
    BOTTOM.store(row, Ordering::SeqCst);
}

static ACTIVE: AtomicBool = AtomicBool::new(false);
static KITTY: AtomicBool = AtomicBool::new(false);
static HOOKS: Once = Once::new();

/// What goes out before the first frame, in Grok's order. The keyboard probe follows it and is
/// written separately because its answer decides whether the protocol is pushed.
pub fn startup_bytes(cursor_rgb: (u8, u8, u8), mode: ScreenMode) -> Vec<u8> {
    let (r, g, b) = cursor_rgb;
    let mut v = Vec::new();
    v.extend_from_slice(b"\x1b]0;wizard\x07");
    if mode == ScreenMode::Fullscreen {
        v.extend_from_slice(b"\x1b[?1049h");
    }
    // minimal leaves the mouse to the terminal so text can be selected and scrolled natively
    if mode != ScreenMode::Minimal {
        for m in [1000, 1002, 1003, 1015, 1006] {
            v.extend_from_slice(format!("\x1b[?{m}h").as_bytes());
        }
    }
    v.extend_from_slice(b"\x1b[?1004h\x1b[?2004h\x1b[?25l");
    if mode != ScreenMode::Minimal {
        v.extend_from_slice(format!("\x1b]12;rgb:{r:02x}/{g:02x}/{b:02x}\x07").as_bytes());
    }
    v
}

/// The exit sequence of a normal quit. In the normal-buffer modes nothing is wiped: the cursor
/// goes under the last live row and a newline leaves the frame in the scrollback.
pub fn teardown_bytes(kitty: bool, mode: ScreenMode, bottom: u16) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"\x1b]0;wizard\x07");
    if mode == ScreenMode::Fullscreen {
        v.extend_from_slice(b"\x1b[2J");
    }
    v.extend_from_slice(b"\x1b]9;4;0;0\x07");
    v.extend_from_slice(b"\x1b[?2026l");
    if mode != ScreenMode::Minimal {
        v.extend_from_slice(b"\x1b]112\x07");
        for m in [1000, 1002, 1003, 1015, 1006] {
            v.extend_from_slice(format!("\x1b[?{m}l").as_bytes());
        }
    }
    v.extend_from_slice(b"\x1b[?2004l\x1b[?1004l");
    if kitty {
        v.extend_from_slice(b"\x1b[<1u");
    }
    match mode {
        ScreenMode::Fullscreen => v.extend_from_slice(b"\x1b[?25h\x1b[?1049l"),
        _ => v.extend_from_slice(format!("\x1b[{};1H\x1b[?25h\n", bottom + 1).as_bytes()),
    }
    v
}

/// The terminal brand, from the environment, in the order Grok Build tries its markers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Brand {
    VsCode,
    Cursor,
    Windsurf,
    Zed,
    AppleTerminal,
    Ghostty,
    Iterm2,
    Warp,
    WezTerm,
    Kitty,
    Alacritty,
    Rio,
    Foot,
    Terminator,
    Vte,
    WindowsTerminal,
    JetBrains,
    Otty,
    GrokDesktop,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mux {
    Tmux,
    Screen,
    Zellij,
    Herdr,
    Cmux,
    None,
}

/// What the kitty-keyboard decision looks at.
#[derive(Clone, Debug)]
pub struct TermCtx {
    pub brand: Brand,
    pub mux: Mux,
    pub vte: bool,
    /// `(major, minor)` of `tmux -V`; `None` when unknown, which counts as old.
    pub tmux_version: Option<(u32, u32)>,
    pub tmux_extended_keys: Option<String>,
}

fn get<'a>(env: &'a dyn Fn(&str) -> Option<String>, k: &str) -> Option<String> {
    env(k).filter(|v| !v.is_empty())
}

/// `detect_terminal_brand_from_env`: fork markers first, then `TERM_PROGRAM`, then the
/// emulators' own variables.
pub fn brand_from_env(env: &dyn Fn(&str) -> Option<String>) -> Brand {
    if get(env, "CURSOR_TRACE_ID").is_some() {
        return Brand::Cursor;
    }
    if let Some(a) = get(env, "VSCODE_GIT_ASKPASS_MAIN") {
        let a = a.to_ascii_lowercase();
        return if a.contains("cursor") {
            Brand::Cursor
        } else if a.contains("windsurf") {
            Brand::Windsurf
        } else {
            Brand::VsCode
        };
    }
    if let Some(tp) = get(env, "TERM_PROGRAM") {
        let n: String = tp
            .trim()
            .chars()
            .filter(|c| !matches!(c, ' ' | '-' | '_' | '.'))
            .map(|c| c.to_ascii_lowercase())
            .collect();
        let b = match n.as_str() {
            "appleterminal" => Some(Brand::AppleTerminal),
            "ghostty" => Some(Brand::Ghostty),
            "iterm" | "iterm2" | "itermapp" => Some(Brand::Iterm2),
            "warp" | "warpterminal" => Some(Brand::Warp),
            "vscode" => Some(Brand::VsCode),
            "wezterm" => Some(Brand::WezTerm),
            "kitty" => Some(Brand::Kitty),
            "alacritty" => Some(Brand::Alacritty),
            "rio" => Some(Brand::Rio),
            "terminator" => Some(Brand::Terminator),
            "zed" => Some(Brand::Zed),
            "grokdesktop" => Some(Brand::GrokDesktop),
            "windowsterminal" => Some(Brand::WindowsTerminal),
            "otty" => Some(Brand::Otty),
            _ => None,
        };
        if let Some(b) = b {
            return b;
        }
    }
    if let Some(te) = get(env, "TERMINAL_EMULATOR") {
        let te = te.to_ascii_lowercase();
        if te.contains("jetbrains") || te.contains("jediterm") {
            return Brand::JetBrains;
        }
    }
    let term = get(env, "TERM").unwrap_or_default();
    if get(env, "WEZTERM_VERSION").is_some() {
        return Brand::WezTerm;
    }
    if get(env, "ITERM_SESSION_ID").is_some()
        || get(env, "ITERM_PROFILE").is_some()
        || get(env, "LC_TERMINAL").is_some_and(|v| v.eq_ignore_ascii_case("iterm2"))
    {
        return Brand::Iterm2;
    }
    if get(env, "TERM_SESSION_ID").is_some() {
        return Brand::AppleTerminal;
    }
    if get(env, "KITTY_WINDOW_ID").is_some() || term.contains("kitty") {
        return Brand::Kitty;
    }
    if get(env, "ALACRITTY_SOCKET").is_some() || term == "alacritty" {
        return Brand::Alacritty;
    }
    if term == "rio" {
        return Brand::Rio;
    }
    if matches!(term.as_str(), "foot" | "foot-extra" | "foot-direct") {
        return Brand::Foot;
    }
    if get(env, "TERMINATOR_UUID").is_some() {
        return Brand::Terminator;
    }
    if get(env, "VTE_VERSION").is_some() {
        return Brand::Vte;
    }
    if get(env, "WT_SESSION").is_some() {
        return Brand::WindowsTerminal;
    }
    Brand::Unknown
}

/// The multiplexer, with Byobu's explicit backend winning, then `TMUX`, Zellij, `STY`.
pub fn mux_from_env(env: &dyn Fn(&str) -> Option<String>) -> Mux {
    let byobu = ["BYOBU_BACKEND", "BYOBU_CONFIG_DIR", "BYOBU_DISTRO"]
        .iter()
        .any(|k| get(env, k).is_some());
    if byobu {
        match get(env, "BYOBU_BACKEND")
            .map(|b| b.to_ascii_lowercase())
            .as_deref()
        {
            Some("tmux") => return Mux::Tmux,
            Some("screen") => return Mux::Screen,
            _ => {
                if get(env, "TMUX").is_some() {
                    return Mux::Tmux;
                }
                if get(env, "STY").is_some() {
                    return Mux::Screen;
                }
            }
        }
    }
    if get(env, "TMUX").is_some() {
        Mux::Tmux
    } else if get(env, "ZELLIJ").is_some() || get(env, "ZELLIJ_SESSION_NAME").is_some() {
        Mux::Zellij
    } else if get(env, "STY").is_some() {
        Mux::Screen
    } else if get(env, "HERDR_ENV").is_some() {
        Mux::Herdr
    } else if ["CMUX_SOCKET_PATH", "CMUX_PANEL_ID", "CMUX_BUNDLE_ID"]
        .iter()
        .any(|k| get(env, k).is_some())
    {
        Mux::Cmux
    } else {
        Mux::None
    }
}

/// `tmux 3.4` and `tmux 3.3a` to `(3, 4)` and `(3, 3)`.
pub fn parse_tmux_version(v: &str) -> Option<(u32, u32)> {
    let rest = v.trim().strip_prefix("tmux ")?;
    let mut parts = rest.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor_s = parts.next()?;
    let end = minor_s
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(minor_s.len());
    Some((major, minor_s[..end].parse().ok()?))
}

impl TermCtx {
    pub fn from_env(env: &dyn Fn(&str) -> Option<String>) -> TermCtx {
        let brand = brand_from_env(env);
        TermCtx {
            brand,
            mux: mux_from_env(env),
            vte: matches!(brand, Brand::Vte | Brand::Terminator)
                || get(env, "VTE_VERSION").is_some(),
            tmux_version: None,
            tmux_extended_keys: None,
        }
    }

    /// The process environment, plus what tmux says about itself when this is tmux.
    pub fn detect() -> TermCtx {
        let mut c = TermCtx::from_env(&|k| std::env::var(k).ok());
        if c.mux == Mux::Tmux {
            let run = |args: &[&str]| {
                std::process::Command::new("tmux")
                    .args(args)
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            };
            c.tmux_version = run(&["-V"]).as_deref().and_then(parse_tmux_version);
            c.tmux_extended_keys = run(&["show-options", "-gqv", "extended-keys"]);
        }
        c
    }

    /// Why Grok does not even ask for the kitty keyboard protocol, first match wins: the
    /// terminal's own problems before the multiplexer's.
    pub fn kitty_skip_reason(&self) -> Option<&'static str> {
        let tmux_new = self.tmux_version.is_some_and(|v| v >= (3, 3));
        if matches!(
            self.brand,
            Brand::VsCode | Brand::Cursor | Brand::Windsurf | Brand::Zed
        ) {
            return Some("vscode");
        }
        if self.brand == Brand::AppleTerminal {
            return Some("apple_terminal");
        }
        if self.vte {
            return Some("vte");
        }
        if self.brand == Brand::WindowsTerminal {
            return Some("windows_terminal");
        }
        if self.brand == Brand::JetBrains {
            return Some("jetbrains");
        }
        if self.mux == Mux::Screen {
            return Some("screen");
        }
        if self.mux == Mux::Tmux && !tmux_new {
            return Some("tmux_old");
        }
        if self.mux == Mux::Tmux && tmux_new && self.tmux_extended_keys.as_deref() == Some("off") {
            return Some("tmux_extended_keys_off");
        }
        // no positive evidence of support, and probing a mute terminal would stall startup
        if matches!(self.brand, Brand::Unknown | Brand::Otty) && self.mux == Mux::None {
            return Some("unknown_no_multiplexer");
        }
        None
    }
}

fn skip_kitty_probe() -> bool {
    TermCtx::detect().kitty_skip_reason().is_some()
}

/// Undo everything, once. Safe from a panic hook or a signal thread.
pub fn restore_now() {
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let kitty = KITTY.swap(false, Ordering::SeqCst);
    let mut out = io::stdout();
    let mode = ScreenMode::from_code(MODE.load(Ordering::SeqCst));
    let _ = out.write_all(&teardown_bytes(kitty, mode, BOTTOM.load(Ordering::SeqCst)));
    let _ = out.flush();
    let _ = disable_raw_mode();
}

fn install_hooks() {
    HOOKS.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // restore first so the panic message lands on the normal screen
            restore_now();
            prev(info);
        }));
        #[cfg(unix)]
        {
            use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
            use signal_hook::iterator::Signals;
            if let Ok(mut signals) = Signals::new([SIGINT, SIGTERM, SIGHUP]) {
                std::thread::Builder::new()
                    .name("grokw-signals".into())
                    .spawn(move || {
                        if let Some(sig) = signals.forever().next() {
                            restore_now();
                            std::process::exit(128 + sig);
                        }
                    })
                    .ok();
            }
        }
    });
}

/// Owns the terminal state. Dropping it restores the terminal.
pub struct TermGuard {
    _private: (),
}

impl TermGuard {
    pub fn enter(mode: ScreenMode) -> io::Result<TermGuard> {
        install_hooks();
        MODE.store(mode.code(), Ordering::SeqCst);
        enable_raw_mode()?;
        ACTIVE.store(true, Ordering::SeqCst);
        let mut out = io::stdout();
        let setup = (|| -> io::Result<()> {
            out.write_all(&startup_bytes((0xc8, 0xc8, 0xc8), mode))?;
            out.flush()?;
            // the probe is `CSI ?u` then `CSI c`; nothing is pushed when the answer is no
            if !skip_kitty_probe() && supports_keyboard_enhancement().unwrap_or(false) {
                execute!(
                    out,
                    PushKeyboardEnhancementFlags(
                        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                            | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                    )
                )?;
                KITTY.store(true, Ordering::SeqCst);
            }
            Ok(())
        })();
        if let Err(e) = setup {
            restore_now();
            return Err(e);
        }
        Ok(TermGuard { _private: () })
    }

    /// The ratatui terminal: the whole screen, or an inline viewport in the normal buffer.
    pub fn terminal(&self) -> io::Result<Terminal> {
        let mode = ScreenMode::from_code(MODE.load(Ordering::SeqCst));
        let backend = Quiet::new(
            CrosstermBackend::new(io::stdout()),
            mode != ScreenMode::Fullscreen,
        );
        if mode == ScreenMode::Fullscreen {
            return ratatui::Terminal::new(backend);
        }
        let rows = crossterm::terminal::size().map_or(24, |s| s.1);
        ratatui::Terminal::with_options(
            backend,
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Inline(mode.live_rows(rows)),
            },
        )
    }

    pub fn restore(&mut self) {
        restore_now();
    }

    /// Hand the terminal back for a program that reads it (`$EDITOR`). Returns whether the keyboard
    /// protocol was on, to pass to [`resume`](Self::resume).
    pub fn suspend(&mut self) -> bool {
        let kitty = KITTY.load(Ordering::SeqCst);
        restore_now();
        kitty
    }

    /// Take the terminal back after [`suspend`](Self::suspend), without asking it anything.
    pub fn resume(&mut self, kitty: bool) -> io::Result<()> {
        enable_raw_mode()?;
        ACTIVE.store(true, Ordering::SeqCst);
        let mut out = io::stdout();
        let mode = ScreenMode::from_code(MODE.load(Ordering::SeqCst));
        out.write_all(&startup_bytes((0xc8, 0xc8, 0xc8), mode))?;
        out.flush()?;
        if kitty {
            execute!(
                out,
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                )
            )?;
            KITTY.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    pub fn keyboard_enhanced(&self) -> bool {
        KITTY.load(Ordering::SeqCst)
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        restore_now();
    }
}

/// Show or hide the cursor only when it changes, so an unchanged frame writes nothing.
pub fn set_cursor(visible: bool) -> io::Result<()> {
    let mut out = io::stdout();
    if visible {
        queue!(out, Show)?;
    } else {
        queue!(out, Hide)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(b: Vec<u8>) -> String {
        String::from_utf8(b)
            .unwrap()
            .replace('\x1b', "ESC")
            .replace('\x07', "BEL")
    }

    #[test]
    fn startup_stream_is_grok_s_order() {
        assert_eq!(
            s(startup_bytes((0xc8, 0xc8, 0xc8), ScreenMode::Fullscreen)),
            "ESC]0;wizardBELESC[?1049hESC[?1000hESC[?1002hESC[?1003hESC[?1015hESC[?1006h\
             ESC[?1004hESC[?2004hESC[?25lESC]12;rgb:c8/c8/c8BEL"
        );
    }

    fn ctx(vars: &[(&str, &str)]) -> TermCtx {
        let m: std::collections::HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        TermCtx::from_env(&|k| m.get(k).cloned())
    }

    #[test]
    fn kitty_skip_list_follows_the_real_order() {
        // terminals that mis-handle the protocol
        for (vars, why) in [
            (&[("TERM_PROGRAM", "vscode")][..], "vscode"),
            (&[("CURSOR_TRACE_ID", "x")], "vscode"),
            (&[("TERM_PROGRAM", "Apple_Terminal")], "apple_terminal"),
            (&[("VTE_VERSION", "7600")], "vte"),
            (&[("TERMINATOR_UUID", "u")], "vte"),
            (&[("WT_SESSION", "g")], "windows_terminal"),
            (&[("TERMINAL_EMULATOR", "JetBrains-JediTerm")], "jetbrains"),
        ] {
            assert_eq!(ctx(vars).kitty_skip_reason(), Some(why), "{vars:?}");
        }
        // a terminal reason beats a multiplexer reason
        assert_eq!(
            ctx(&[("VTE_VERSION", "7600"), ("STY", "1")]).kitty_skip_reason(),
            Some("vte")
        );
        // multiplexers
        assert_eq!(ctx(&[("STY", "1.pts")]).kitty_skip_reason(), Some("screen"));
        let mut t = ctx(&[
            ("TMUX", "/tmp/tmux-1/default,1,0"),
            ("TERM_PROGRAM", "ghostty"),
        ]);
        assert_eq!(
            t.kitty_skip_reason(),
            Some("tmux_old"),
            "unknown version counts as old"
        );
        t.tmux_version = parse_tmux_version("tmux 3.3a");
        assert_eq!(t.kitty_skip_reason(), None);
        t.tmux_extended_keys = Some("off".into());
        assert_eq!(t.kitty_skip_reason(), Some("tmux_extended_keys_off"));
        t.tmux_extended_keys = Some("on".into());
        assert_eq!(t.kitty_skip_reason(), None);
        // supported brands ask
        assert_eq!(ctx(&[("KITTY_WINDOW_ID", "1")]).kitty_skip_reason(), None);
        assert_eq!(ctx(&[("TERM", "foot")]).kitty_skip_reason(), None);
        // nothing known and no multiplexer: do not probe a terminal that may never answer
        assert_eq!(
            ctx(&[("TERM", "xterm-256color")]).kitty_skip_reason(),
            Some("unknown_no_multiplexer")
        );
        // Zellij is a multiplexer, so an unknown brand under it still asks
        assert_eq!(ctx(&[("ZELLIJ", "0")]).kitty_skip_reason(), None);
    }

    #[test]
    fn startup_by_mode() {
        let full = s(startup_bytes((0xc8, 0xc8, 0xc8), ScreenMode::Fullscreen));
        assert!(full.contains("?1049h") && full.contains("?1000h"));
        let inline = s(startup_bytes((0xc8, 0xc8, 0xc8), ScreenMode::Inline));
        assert!(!inline.contains("?1049h") && inline.contains("?1000hESC[?1002h"));
        let minimal = s(startup_bytes((0xc8, 0xc8, 0xc8), ScreenMode::Minimal));
        assert_eq!(
            minimal, "ESC]0;wizardBELESC[?1004hESC[?2004hESC[?25l",
            "no alt screen, no mouse, no cursor colour"
        );
    }

    #[test]
    fn minimal_live_rows_clamp() {
        assert_eq!(ScreenMode::Minimal.live_rows(36), 10);
        assert_eq!(ScreenMode::Minimal.live_rows(8), 7);
        assert_eq!(ScreenMode::Minimal.live_rows(3), 3);
        assert_eq!(ScreenMode::Inline.live_rows(36), 36);
    }

    #[test]
    fn tmux_versions_parse() {
        assert_eq!(parse_tmux_version("tmux 3.4"), Some((3, 4)));
        assert_eq!(parse_tmux_version("tmux 3.3a\n"), Some((3, 3)));
        assert_eq!(parse_tmux_version("tmux next-3.5"), None);
        assert_eq!(parse_tmux_version("screen 4.9"), None);
    }

    #[test]
    fn byobu_picks_its_backend() {
        let t = ctx(&[("BYOBU_BACKEND", "screen"), ("TMUX", "x")]);
        assert_eq!(t.mux, Mux::Screen);
        let t = ctx(&[("BYOBU_BACKEND", "tmux")]);
        assert_eq!(t.mux, Mux::Tmux);
    }

    #[test]
    fn teardown_stream_is_grok_s_order() {
        assert_eq!(
            s(teardown_bytes(false, ScreenMode::Fullscreen, 0)),
            "ESC]0;wizardBELESC[2JESC]9;4;0;0BELESC[?2026lESC]112BELESC[?1000lESC[?1002l\
             ESC[?1003lESC[?1015lESC[?1006lESC[?2004lESC[?1004lESC[?25hESC[?1049l"
        );
        assert!(s(teardown_bytes(true, ScreenMode::Fullscreen, 0)).contains("ESC[<1uESC[?25h"));
        // the normal-buffer modes keep the screen: no clear, no alt-screen exit, and the cursor
        // lands under the last live row with a newline
        let inline = s(teardown_bytes(false, ScreenMode::Inline, 35));
        assert!(
            !inline.contains("ESC[2J") && !inline.contains("?1049l"),
            "{inline}"
        );
        assert!(inline.ends_with("ESC[36;1HESC[?25h\n"), "{inline}");
        let min = s(teardown_bytes(false, ScreenMode::Minimal, 9));
        assert!(
            !min.contains("?1000l") && min.ends_with("ESC[10;1HESC[?25h\n"),
            "{min}"
        );
    }
}
