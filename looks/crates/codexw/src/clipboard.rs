//! `/copy` and Ctrl+O. A local session copies through the platform tool (`wl-copy`, `xclip`,
//! `xsel`, `pbcopy`); with none of them, or over SSH, the text goes out as an OSC 52 escape for
//! the terminal to put on the clipboard, wrapped for tmux when `$TMUX` is set (Codex
//! `clipboard_copy.rs`).

use std::io::Write;
use std::process::{Command, Stdio};

/// The most the OSC 52 route sends; larger text is refused.
pub const OSC52_MAX_BYTES: usize = 100_000;

pub fn copy(text: &str) -> Result<(), String> {
    let ssh = std::env::var_os("SSH_TTY").is_some() || std::env::var_os("SSH_CONNECTION").is_some();
    if !ssh && native(text) {
        return Ok(());
    }
    osc52_to_tty(text)
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
    if text.len() > OSC52_MAX_BYTES {
        return Err(format!(
            "text is {} bytes; the terminal clipboard route takes at most {OSC52_MAX_BYTES}",
            text.len()
        ));
    }
    let b64 = base64(text.as_bytes());
    Ok(if tmux {
        format!("\x1bPtmux;\x1b\x1b]52;c;{b64}\x07\x1b\\").into_bytes()
    } else {
        format!("\x1b]52;c;{b64}\x07").into_bytes()
    })
}

fn osc52_to_tty(text: &str) -> Result<(), String> {
    let seq = osc52_sequence(text, std::env::var_os("TMUX").is_some())?;
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
