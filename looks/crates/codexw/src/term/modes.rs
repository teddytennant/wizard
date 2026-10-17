// OWNER: renderer
//! Terminal modes set at start and restored on exit (spec A.2).
//!
//! Set: bracketed paste, raw mode, keyboard enhancement, focus reporting. Never mouse capture,
//! never the alternate screen at startup. A panic hook and the signal thread undo exactly what
//! was set.

use std::io::{self, Write};
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

static ACTIVE: AtomicBool = AtomicBool::new(false);
static KBD_PUSHED: AtomicBool = AtomicBool::new(false);
static HOOKS: Once = Once::new();

/// Kitty keyboard flags Codex pushes: disambiguate (1) + alternate keys (4), plus event types
/// (2) unless the terminal is Ghostty or iTerm2 or tmux reports `xterm` extended-keys-format.
pub fn keyboard_flags(term_program: &str, tmux_format: Option<&str>) -> u8 {
    let p = term_program.to_ascii_lowercase();
    if p.contains("ghostty") || p.contains("iterm") || tmux_format == Some("xterm") {
        5
    } else {
        7
    }
}

fn tmux_extended_keys_format() -> Option<String> {
    std::env::var_os("TMUX")?;
    let out = std::process::Command::new("tmux")
        .args(["display-message", "-p", "#{extended-keys-format}"])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

pub fn keyboard_enhancement_disabled() -> bool {
    matches!(
        std::env::var("CODEXW_DISABLE_KEYBOARD_ENHANCEMENT")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes"
    )
}

/// Write the startup mode sequence, in Codex's order, and enter raw mode.
pub fn set_modes(out: &mut impl Write) -> io::Result<()> {
    out.write_all(b"\x1b[?2004h")?;
    crossterm::terminal::enable_raw_mode()?;
    if !keyboard_enhancement_disabled() {
        out.write_all(b"\x1b[>4;0m")?;
        let fmt = tmux_extended_keys_format();
        let prog = std::env::var("TERM_PROGRAM").unwrap_or_default();
        let flags = keyboard_flags(&prog, fmt.as_deref());
        write!(out, "\x1b[>{flags}u")?;
        if fmt.as_deref() == Some("csi-u") {
            out.write_all(b"\x1b[>4;2m")?;
        }
        KBD_PUSHED.store(true, Ordering::SeqCst);
    }
    out.write_all(b"\x1b[?1004h")?;
    out.flush()?;
    ACTIVE.store(true, Ordering::SeqCst);
    install_hooks();
    Ok(())
}

/// Undo `set_modes`. Idempotent; safe from a panic hook.
pub fn restore_after_exit() {
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let mut out = io::stdout();
    if KBD_PUSHED.swap(false, Ordering::SeqCst) {
        let _ = out.write_all(b"\x1b[<1u\x1b[<u\x1b[>4;0m");
    }
    let _ = out.write_all(b"\x1b[?2004l\x1b[?1004l");
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = out.write_all(b"\x1b[0 q\x1b[?25h");
    let _ = out.flush();
}

/// Drop what the terminal has queued for us (keys typed while an editor ran).
pub fn flush_input() {
    #[cfg(unix)]
    // SAFETY: tcflush on the stdin fd, no memory involved.
    unsafe {
        libc::tcflush(0, libc::TCIFLUSH);
    }
}

pub fn is_active() -> bool {
    ACTIVE.load(Ordering::SeqCst)
}

/// SIGTERM and SIGHUP restore the terminal before the process dies.
#[cfg(unix)]
pub fn install_signal_restore() {
    use signal_hook::consts::{SIGHUP, SIGTERM};
    use signal_hook::iterator::Signals;
    if let Ok(mut sig) = Signals::new([SIGTERM, SIGHUP]) {
        std::thread::spawn(move || {
            if let Some(s) = sig.forever().next() {
                let _ = io::stdout().write_all(b"\x1b[?1007l\x1b[?1049l\x1b[r");
                restore_after_exit();
                std::process::exit(128 + s);
            }
        });
    }
}

fn install_hooks() {
    HOOKS.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // The alt screen may be open when a panic hits an overlay.
            let _ = io::stdout().write_all(b"\x1b[?1007l\x1b[?1049l\x1b[r");
            restore_after_exit();
            prev(info);
        }));
    });
}

/// OSC 0 terminal title. Control and invisible characters are dropped, whitespace collapses,
/// 240 characters at most. Empty clears it.
pub fn set_title(out: &mut impl Write, title: &str) -> io::Result<()> {
    let mut s = String::new();
    let mut pending_space = false;
    for ch in title.chars() {
        if ch.is_whitespace() {
            pending_space = !s.is_empty();
            continue;
        }
        if ch.is_control() || is_invisible(ch) {
            continue;
        }
        if s.chars().count() >= 240 {
            break;
        }
        if pending_space {
            s.push(' ');
            pending_space = false;
        }
        s.push(ch);
    }
    write!(out, "\x1b]0;{s}\x07")?;
    out.flush()
}

fn is_invisible(c: char) -> bool {
    matches!(c as u32,
        0xAD | 0x34F | 0x61C | 0x180E | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x206F
        | 0xFE00..=0xFE0F | 0xFEFF | 0xFFF9..=0xFFFB | 0x1BCA0..=0x1BCA3 | 0xE0100..=0xE01EF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_follow_the_terminal() {
        assert_eq!(keyboard_flags("", None), 7);
        assert_eq!(keyboard_flags("ghostty", None), 5);
        assert_eq!(keyboard_flags("iTerm.app", None), 5);
        assert_eq!(keyboard_flags("", Some("xterm")), 5);
        assert_eq!(keyboard_flags("", Some("csi-u")), 7);
    }

    #[test]
    fn title_is_sanitised() {
        let mut v = Vec::new();
        set_title(&mut v, "  a \n\u{200b}b\x07 c ").unwrap();
        assert_eq!(v, b"\x1b]0;a b c\x07");
    }
}
