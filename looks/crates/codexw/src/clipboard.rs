//! `/copy` and Ctrl+O. The route is Codex's (`clipboard_copy.rs` and
//! `terminal_capabilities.rs` `ClipboardCapabilities`, 0.174.0-alpha.6): inside tmux the text goes
//! to tmux's own clipboard (`load-buffer -w`), which forwards it to the attached terminal; over
//! SSH it goes out as an OSC 52 escape; a local session uses the platform tool (`wl-copy`,
//! `xclip`, `xsel`, `pbcopy`). Whatever fails falls back to OSC 52, wrapped for tmux or GNU screen
//! when one is in the way.

use std::io::Write;
use std::process::{Command, Stdio};

/// The most the OSC 52 route sends; larger text is refused.
pub const OSC52_MAX_BYTES: usize = 100_000;

pub fn copy(text: &str) -> Result<(), String> {
    use crate::term::capabilities::{ClipboardCopyMode, clipboard_copy_mode, process_env};
    match clipboard_copy_mode(&process_env) {
        ClipboardCopyMode::ClientBuffer if text.len() <= OSC52_MAX_BYTES => {
            match crate::term::capabilities::tmux_client_buffer_copy(text) {
                Ok(()) => Ok(()),
                Err(tmux) => osc52_to_tty(text).map_err(|osc| format!("{tmux}; {osc}")),
            }
        }
        ClipboardCopyMode::ClientBuffer | ClipboardCopyMode::Osc52 => osc52_to_tty(text),
        ClipboardCopyMode::FallbackOsc52 => {
            if native(text) {
                Ok(())
            } else {
                osc52_to_tty(text)
            }
        }
    }
}

fn native(text: &str) -> bool {
    let tools: [(&str, &[&str]); 4] = [
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
        ("pbcopy", &[]),
    ];
    for (bin, args) in tools {
        if bin == "wl-copy" && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            continue;
        }
        if matches!(bin, "xclip" | "xsel") && std::env::var_os("DISPLAY").is_none() {
            continue;
        }
        let Ok(mut child) = Command::new(bin)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            if stdin.write_all(text.as_bytes()).is_err() {
                let _ = child.kill();
                continue;
            }
        }
        if child.wait().is_ok_and(|s| s.success()) {
            return true;
        }
    }
    false
}

/// `ESC ] 52 ; c ; <base64> BEL`, or tmux's passthrough form.
pub fn osc52_sequence(text: &str, tmux: bool) -> Result<Vec<u8>, String> {
    use crate::term::capabilities::Osc52Target;
    osc52_for(
        text,
        if tmux {
            Osc52Target::EscapedDcs
        } else {
            Osc52Target::Direct
        },
    )
}

/// The OSC 52 write for what sits between codexw and the terminal (Codex `osc52_sequence`).
pub fn osc52_for(
    text: &str,
    target: crate::term::capabilities::Osc52Target,
) -> Result<Vec<u8>, String> {
    use crate::term::capabilities::{Osc52Target, chunked_dcs, escaped_dcs};
    if text.len() > OSC52_MAX_BYTES {
        return Err(format!(
            "text is {} bytes; the terminal clipboard route takes at most {OSC52_MAX_BYTES}",
            text.len()
        ));
    }
    let osc = format!("\x1b]52;c;{}\x07", base64(text.as_bytes()));
    Ok(match target {
        Osc52Target::Direct => osc,
        Osc52Target::EscapedDcs => escaped_dcs(&osc),
        Osc52Target::ChunkedDcs => chunked_dcs(&osc),
    }
    .into_bytes())
}

fn osc52_to_tty(text: &str) -> Result<(), String> {
    let target =
        crate::term::capabilities::Osc52Target::detect(&crate::term::capabilities::process_env);
    let seq = osc52_for(text, target)?;
    let mut out: Box<dyn Write> = match std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        Ok(f) => Box::new(f),
        Err(_) => Box::new(std::io::stdout()),
    };
    out.write_all(&seq)
        .and_then(|_| out.flush())
        .map_err(|e| e.to_string())
}

/// Standard base64 with padding.
pub fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn the_sequence_is_bel_terminated_and_wrapped_for_tmux() {
        assert_eq!(osc52_sequence("hi", false).unwrap(), b"\x1b]52;c;aGk=\x07");
        assert_eq!(
            osc52_sequence("hi", true).unwrap(),
            b"\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\"
        );
    }

    #[test]
    fn big_text_is_refused() {
        assert!(osc52_sequence(&"x".repeat(OSC52_MAX_BYTES + 1), false).is_err());
        assert!(osc52_sequence(&"x".repeat(OSC52_MAX_BYTES), false).is_ok());
    }
}
