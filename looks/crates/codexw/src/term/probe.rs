// OWNER: renderer
//! Startup probe (spec A.3): one batched write of CPR, OSC 10, OSC 11, kitty flags query and
//! primary DA, answers read with one shared 100 ms deadline before the event reader starts.
//!
//! Over a direct SSH session the batch also asks for secondary DA, which tells Terminal.app apart
//! (Codex 0.174.0-alpha.6 `terminal_capabilities/terminal_probe.rs` `startup`, `query_identity`):
//! its replies make `tui.alternate_screen = auto` keep the inline screen.

use std::io::{self, Write};
use std::time::{Duration, Instant};

pub const PROBE: &[u8] = b"\x1b[6n\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[?u\x1b[c";
pub const PROBE_SKIP_KEYBOARD: &[u8] = b"\x1b[6n\x1b]10;?\x1b\\\x1b]11;?\x1b\\";
/// Primary DA (the batch ends with it when keyboard is skipped) and secondary DA, the identity
/// pair `capabilities::requires_inline_screen` reads.
pub const PROBE_IDENTITY: &[u8] = b"\x1b[c\x1b[>c";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Probe {
    /// 0-based (x, y).
    pub cursor: Option<(u16, u16)>,
    pub fg: Option<(u8, u8, u8)>,
    pub bg: Option<(u8, u8, u8)>,
    /// The terminal asked for the inline screen (Codex `StartupProbe::prefer_inline_screen`).
    /// `Some(false)` when identity was not asked for; `None` when its replies did not arrive.
    pub prefer_inline_screen: Option<bool>,
}

/// Parse the terminal's replies. Colours need both fg and bg to count.
pub fn parse_replies(data: &[u8]) -> Probe {
    let mut p = Probe::default();
    let mut i = 0;
    let mut fg = None;
    let mut bg = None;
    while i < data.len() {
        if data[i] != 0x1b {
            i += 1;
            continue;
        }
        if data.get(i + 1) == Some(&b'[') {
            let mut j = i + 2;
            while j < data.len() && !(0x40..=0x7e).contains(&data[j]) {
                j += 1;
            }
            if j < data.len() && data[j] == b'R' {
                let body = String::from_utf8_lossy(&data[i + 2..j]).to_string();
                let mut it = body.split(';');
                if let (Some(r), Some(c)) = (it.next(), it.next()) {
                    if let (Ok(r), Ok(c)) = (r.parse::<u16>(), c.parse::<u16>()) {
                        p.cursor = Some((c.saturating_sub(1), r.saturating_sub(1)));
                    }
                }
            }
            i = j + 1;
        } else if data.get(i + 1) == Some(&b']') {
            let mut j = i + 2;
            while j < data.len()
                && data[j] != 0x07
                && !(data[j] == 0x1b && data.get(j + 1) == Some(&b'\\'))
            {
                j += 1;
            }
            let body = String::from_utf8_lossy(&data[i + 2..j.min(data.len())]).to_string();
            if let Some(rest) = body.strip_prefix("10;") {
                fg = parse_color(rest);
            } else if let Some(rest) = body.strip_prefix("11;") {
                bg = parse_color(rest);
            }
            i = j + if data.get(j) == Some(&0x1b) { 2 } else { 1 };
        } else {
            i += 1;
        }
    }
    if let (Some(f), Some(b)) = (fg, bg) {
        p.fg = Some(f);
        p.bg = Some(b);
    }
    p
}

/// `rgb:RRRR/GGGG/BBBB` (or 2 digit components, or `rgba:`).
pub fn parse_color(s: &str) -> Option<(u8, u8, u8)> {
    let rest = s.strip_prefix("rgb:").or_else(|| s.strip_prefix("rgba:"))?;
    let mut it = rest.split('/');
    let mut comp = || -> Option<u8> {
        let c = it.next()?;
        let v = u32::from_str_radix(c, 16).ok()?;
        match c.len() {
            2 => Some(v as u8),
            4 => Some((v / 257) as u8),
            1 => Some((v * 17) as u8),
            3 => Some((v * 255 / 4095) as u8),
            _ => None,
        }
    };
    Some((comp()?, comp()?, comp()?))
}

/// The bytes the startup probe writes.
pub fn query(keyboard: bool, identity: bool) -> Vec<u8> {
    let mut q = if keyboard { PROBE } else { PROBE_SKIP_KEYBOARD }.to_vec();
    if identity {
        if keyboard {
            // PROBE already ends with primary DA.
            q.extend_from_slice(b"\x1b[>c");
        } else {
            q.extend_from_slice(PROBE_IDENTITY);
        }
    }
    q
}

/// Run the probe on the real terminal. Raw mode must already be on. Keys typed during the
/// deadline are lost, as in Codex. `identity` adds the Terminal.app question.
#[cfg(unix)]
pub fn run(keyboard: bool, identity: bool) -> Probe {
    use std::os::fd::AsRawFd;
    let mut out = io::stdout();
    let _ = out.write_all(&query(keyboard, identity));
    let _ = out.flush();
    let fd = io::stdin().as_raw_fd();
    let deadline = Instant::now() + Duration::from_millis(100);
    let mut buf = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd, count 1.
        let n = unsafe { libc::poll(&mut pfd, 1, left.as_millis() as i32) };
        if n <= 0 {
            break;
        }
        let mut chunk = [0u8; 512];
        // SAFETY: reading into a local buffer of the stated length.
        let r = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        if r <= 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..r as usize]);
        let p = parse_replies(&buf);
        let kbd_done = !keyboard || buf.windows(3).any(|w| w == b"\x1b[?") && buf.ends_with(b"c");
        let identity_done =
            !identity || super::capabilities::requires_inline_screen(&buf).is_some();
        if p.cursor.is_some() && p.fg.is_some() && kbd_done && identity_done {
            break;
        }
    }
    let mut p = parse_replies(&buf);
    p.prefer_inline_screen = if identity {
        super::capabilities::requires_inline_screen(&buf)
    } else {
        Some(false)
    };
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_harness_replies() {
        let r = b"\x1b[12;5R\x1b]10;rgb:e6e6/e6e6/e6e6\x1b\\\x1b]11;rgb:0000/0000/0000\x07\x1b[?0u\x1b[?62c";
        let p = parse_replies(r);
        assert_eq!(p.cursor, Some((4, 11)));
        assert_eq!(p.fg, Some((230, 230, 230)));
        assert_eq!(p.bg, Some((0, 0, 0)));
    }

    #[test]
    fn two_digit_components() {
        assert_eq!(parse_color("rgb:1a/1a/1a"), Some((26, 26, 26)));
        assert_eq!(parse_color("nope"), None);
    }

    #[test]
    fn identity_is_asked_only_when_wanted() {
        assert_eq!(query(true, false), PROBE);
        assert!(query(true, true).ends_with(b"\x1b[c\x1b[>c"));
        assert!(query(false, true).ends_with(b"\x1b[c\x1b[>c"));
        assert_eq!(query(false, false), PROBE_SKIP_KEYBOARD);
    }

    #[test]
    fn colours_need_both() {
        let p = parse_replies(b"\x1b]10;rgb:ffff/ffff/ffff\x07");
        assert_eq!(p.fg, None);
    }
}
