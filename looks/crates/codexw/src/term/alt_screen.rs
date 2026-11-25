// OWNER: renderer
//! Which screen the chat runs on, and the alternate screen's own input modes.
//!
//! Codex 0.174.0-alpha.6 (`openai/codex` 06f334aa) runs full screen by default: the chat owns the
//! alternate screen and draws its transcript there, scrollable, instead of writing finished
//! history into the terminal's scrollback. `tui.alternate_screen` (`auto`, `always`, `never`) and
//! `--no-alt-screen` choose; `auto` asks the startup probe, which prefers the inline screen for
//! Terminal.app over SSH. The inline renderer (`inline.rs`, `history_insert.rs`) is the `never`
//! path.
//!
//! Ported from `codex-rs/tui/src/lib.rs` (`determine_alt_screen_mode`),
//! `codex-rs/protocol/src/config_types.rs` (`AltScreenMode`), `codex-rs/tui/src/transcript_mode.rs`
//! and `codex-rs/tui/src/tui/alternate_screen.rs`, Apache-2.0. Modified: the alternate screen's
//! state is plain bytes on codexw's writer, not crossterm commands, and the pointer-shape and
//! tmux mouse-policy sampling are left out.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// `tui.alternate_screen` (Codex `AltScreenMode`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AltScreenMode {
    /// Alternate screen unless the terminal asks for the inline screen.
    #[default]
    Auto,
    Always,
    /// Inline only; the terminal keeps the scrollback.
    Never,
}

impl AltScreenMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().trim_matches('"').to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "always" => Some(Self::Always),
            "never" => Some(Self::Never),
            _ => None,
        }
    }
}

/// Codex `lib.rs` `determine_alt_screen_mode`.
///
/// - `--no-alt-screen` always disables the alternate screen.
/// - Otherwise `tui.alternate_screen`: `always` uses it, `never` keeps the inline screen, and
///   `auto` (the default) uses it unless the startup probe preferred the inline screen.
pub fn determine_alt_screen_mode(
    no_alt_screen: bool,
    tui_alternate_screen: AltScreenMode,
    prefer_inline_screen: bool,
) -> bool {
    if no_alt_screen {
        return false;
    }
    match tui_alternate_screen {
        AltScreenMode::Always => true,
        AltScreenMode::Never => false,
        AltScreenMode::Auto => !prefer_inline_screen,
    }
}

/// Codex `transcript_mode.rs` `TranscriptMode`: the session owns the screen (and its scrollable
/// transcript) only when the alternate screen is allowed and `tui.fullscreen_transcript` is on.
/// With the alternate screen allowed but the owned transcript off, overlays still use it, as in
/// Codex 0.147.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TranscriptMode {
    Owned,
    Terminal,
}

impl TranscriptMode {
    pub fn resolve(owned_enabled: bool, alternate_screen_enabled: bool) -> Self {
        if owned_enabled && alternate_screen_enabled {
            Self::Owned
        } else {
            Self::Terminal
        }
    }

    pub fn is_owned(self) -> bool {
        self == Self::Owned
    }
}

/// `?1049h`.
pub const ENTER_ALT: &[u8] = b"\x1b[?1049h";
/// `?1049l`.
pub const LEAVE_ALT: &[u8] = b"\x1b[?1049l";
/// Codex `EnablePointerCapture`: buttons, drags and plain motion, so the return-to-bottom
/// control can show hover. Followed by SGR encoding (`?1006h`).
pub const POINTER_CAPTURE_ON: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h";
/// crossterm `DisableMouseCapture`, the order Codex writes it in.
pub const POINTER_CAPTURE_OFF: &[u8] = b"\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l";
/// Codex `DisableAlternateScroll`: with the mouse captured the wheel must not turn into arrows.
pub const ALTERNATE_SCROLL_OFF: &[u8] = b"\x1b[?1007l";
/// Pop the alternate screen's keyboard frame (kitty keeps one stack per screen).
pub const KEYBOARD_POP: &[u8] = b"\x1b[<1u";

/// The real terminal's alternate-screen state, kept apart from its owner so the panic hook and
/// the signal thread can undo it (Codex `tui/alternate_screen.rs` `ALTERNATE_SCREEN`).
pub struct AlternateScreen {
    active: AtomicBool,
    mouse_active: AtomicBool,
    keyboard_active: AtomicBool,
    /// Kitty flags pushed on entry; 0 pushes none.
    keyboard_flags: AtomicU8,
}

pub static ALTERNATE_SCREEN: AlternateScreen = AlternateScreen {
    active: AtomicBool::new(false),
    mouse_active: AtomicBool::new(false),
    keyboard_active: AtomicBool::new(false),
    keyboard_flags: AtomicU8::new(0),
};

/// What one alternate-screen session wrote, so the owner can undo exactly that. The static above
/// mirrors it for the panic hook; a headless test reads this one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScreenState {
    pub active: bool,
    pub mouse: bool,
    pub keyboard: bool,
}

impl AlternateScreen {
    /// The kitty flags the main screen pushed (`modes::set_modes`); the alternate screen pushes
    /// the same ones on entry.
    pub fn set_keyboard_flags(&self, flags: Option<u8>) {
        self.keyboard_flags
            .store(flags.unwrap_or(0), Ordering::SeqCst);
    }

    pub fn keyboard_flags(&self) -> Option<u8> {
        match self.keyboard_flags.load(Ordering::SeqCst) {
            0 => None,
            f => Some(f),
        }
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    fn mirror(&self, s: ScreenState) {
        self.active.store(s.active, Ordering::SeqCst);
        self.mouse_active.store(s.mouse, Ordering::SeqCst);
        self.keyboard_active.store(s.keyboard, Ordering::SeqCst);
    }

    /// Codex `AlternateScreen::restore` for a panic or a signal: release the pointer and the
    /// keyboard frame, then leave the screen, whatever the owner was doing.
    pub fn emergency_restore(&self, out: &mut impl Write) {
        if self.mouse_active.swap(false, Ordering::SeqCst) {
            let _ = out.write_all(POINTER_CAPTURE_OFF);
        }
        if self.keyboard_active.swap(false, Ordering::SeqCst) {
            let _ = out.write_all(KEYBOARD_POP);
        }
        if self.active.swap(false, Ordering::SeqCst) {
            let _ = out.write_all(ALTERNATE_SCROLL_OFF);
            let _ = out.write_all(LEAVE_ALT);
        }
        let _ = out.flush();
    }
}

/// Codex `AlternateScreen::enter`: the screen, then its own keyboard frame, then the input modes
/// (`configure_input`: pointer capture on, alternate scroll off).
pub fn enter(out: &mut impl Write, capture_mouse: bool) -> io::Result<ScreenState> {
    out.write_all(ENTER_ALT)?;
    let mut s = ScreenState {
        active: true,
        ..Default::default()
    };
    ALTERNATE_SCREEN.mirror(s);
    if let Some(flags) = ALTERNATE_SCREEN.keyboard_flags() {
        write!(out, "\x1b[>{flags}u")?;
        s.keyboard = true;
    }
    if capture_mouse {
        out.write_all(ALTERNATE_SCROLL_OFF)?;
        out.write_all(POINTER_CAPTURE_ON)?;
        s.mouse = true;
    }
    ALTERNATE_SCREEN.mirror(s);
    out.flush()?;
    Ok(s)
}

/// Codex `AlternateScreen::release_input`: give up the pointer and pop the keyboard frame
/// without changing the screen.
pub fn release_input(out: &mut impl Write, s: &mut ScreenState) -> io::Result<()> {
    if s.mouse {
        out.write_all(POINTER_CAPTURE_OFF)?;
        s.mouse = false;
    }
    if s.keyboard {
        out.write_all(KEYBOARD_POP)?;
        s.keyboard = false;
    }
    out.write_all(ALTERNATE_SCROLL_OFF)?;
    ALTERNATE_SCREEN.mirror(*s);
    Ok(())
}

/// Codex `AlternateScreen::leave`: release the input modes, then the screen.
pub fn leave(out: &mut impl Write, s: &mut ScreenState) -> io::Result<()> {
    let input = release_input(out, s);
    out.write_all(LEAVE_ALT)?;
    s.active = false;
    ALTERNATE_SCREEN.mirror(*s);
    out.flush()?;
    input
}

#[cfg(test)]
mod tests {
    use super::*;

    // The cases of Codex `lib.rs` `determine_alt_screen_mode`.
    #[test]
    fn no_alt_screen_flag_wins() {
        for mode in [
            AltScreenMode::Auto,
            AltScreenMode::Always,
            AltScreenMode::Never,
        ] {
            for prefer_inline in [false, true] {
                assert!(!determine_alt_screen_mode(true, mode, prefer_inline));
            }
        }
    }

    #[test]
    fn auto_follows_the_probe_and_the_others_do_not() {
        assert!(determine_alt_screen_mode(false, AltScreenMode::Auto, false));
        assert!(!determine_alt_screen_mode(false, AltScreenMode::Auto, true));
        assert!(determine_alt_screen_mode(
            false,
            AltScreenMode::Always,
            true
        ));
        assert!(!determine_alt_screen_mode(
            false,
            AltScreenMode::Never,
            false
        ));
    }

    #[test]
    fn modes_parse() {
        assert_eq!(
            AltScreenMode::parse("\"never\""),
            Some(AltScreenMode::Never)
        );
        assert_eq!(AltScreenMode::parse("Always"), Some(AltScreenMode::Always));
        assert_eq!(AltScreenMode::parse("auto"), Some(AltScreenMode::Auto));
        assert_eq!(AltScreenMode::parse("sometimes"), None);
        assert_eq!(AltScreenMode::default(), AltScreenMode::Auto);
    }

    #[test]
    fn transcript_ownership_needs_the_alternate_screen() {
        assert!(TranscriptMode::resolve(true, true).is_owned());
        assert!(!TranscriptMode::resolve(true, false).is_owned());
        assert!(!TranscriptMode::resolve(false, true).is_owned());
    }

    #[test]
    fn a_panic_or_signal_restores_what_is_active_and_only_once() {
        let screen = AlternateScreen {
            active: AtomicBool::new(false),
            mouse_active: AtomicBool::new(false),
            keyboard_active: AtomicBool::new(false),
            keyboard_flags: AtomicU8::new(0),
        };
        let mut out = Vec::new();
        screen.emergency_restore(&mut out);
        assert!(out.is_empty(), "nothing to undo on the main screen");
        screen.mirror(ScreenState {
            active: true,
            mouse: true,
            keyboard: true,
        });
        screen.emergency_restore(&mut out);
        let text = String::from_utf8(out.clone()).unwrap();
        assert_eq!(
            text,
            "\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[<1u\x1b[?1007l\x1b[?1049l"
        );
        out.clear();
        screen.emergency_restore(&mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn leave_undoes_what_enter_did() {
        let mut out = Vec::new();
        let mut s = enter(&mut out, true).unwrap();
        assert!(s.active && s.mouse);
        leave(&mut out, &mut s).unwrap();
        assert_eq!(s, ScreenState::default());
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("\x1b[?1049h"));
        assert!(text.contains("\x1b[?1003h"));
        assert!(text.contains("\x1b[?1000l"));
        assert!(text.ends_with("\x1b[?1049l"));
    }
}
