// OWNER: visual
//! Terminal plumbing that `tuikit::term` does not cover: the startup capability probe, one
//! `write(2)` per frame wrapped in synchronized output, and the extra restore bytes.

use std::fs::File;
use std::io::{self, Write};
use std::mem::ManuallyDrop;
use std::os::fd::FromRawFd;
use std::os::raw::{c_int, c_ulong};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: i16,
    revents: i16,
}

extern "C" {
    fn poll(fds: *mut PollFd, n: c_ulong, timeout: c_int) -> c_int;
    fn read(fd: c_int, buf: *mut u8, n: usize) -> isize;
    fn raise(sig: c_int) -> c_int;
}

const POLLIN: i16 = 1;
const SIGTSTP: c_int = 20;

/// Stop this process the way `ctrl+z` would in a cooked terminal. Returns after `SIGCONT`.
pub fn suspend_process() {
    unsafe {
        raise(SIGTSTP);
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Caps {
    /// Mode 2026 (synchronized output) is supported.
    pub sync: bool,
    /// Mode 2027 (grapheme clusters) is supported.
    pub grapheme: bool,
    /// Terminal background from OSC 11, 8 bits per channel.
    pub bg: Option<(u8, u8, u8)>,
    pub xtversion: Option<String>,
    pub kitty_kbd: bool,
    /// DECRQSS read back a 24-bit background that was just set, so the terminal really takes
    /// `48;2;r;g;b` whatever `COLORTERM` says.
    pub truecolor: bool,
    /// The terminal answered the DA1 sentinel, so the other `None`s are real answers.
    pub answered: bool,
}

impl Caps {
    /// True when the OSC 11 reply says the background is light.
    pub fn light_background(&self) -> Option<bool> {
        let (r, g, b) = self.bg?;
        let lum = 0.2126 * r as f64 + 0.7152 * g as f64 + 0.0722 * b as f64;
        Some(lum > 140.0)
    }
}

// The truecolor probe sets a background no theme uses, asks for the SGR back with DECRQSS and
// resets, all before the DA1 sentinel. A terminal that cannot do 24-bit colour either stays
// silent or reports an indexed colour, and the alt screen has not been painted yet.
const QUERY: &str = "\x1b[48;2;1;2;3m\x1bP$qm\x1b\\\x1b[0m\x1b]11;?\x1b\\\x1b[?2026$p\x1b[?2027$p\x1b[>0q\x1b[?u\x1b[c";

/// One batched write ended by a DA1 sentinel, then read until the sentinel arrives or
/// `timeout` passes. Must run in raw mode, before anything else reads stdin.
pub fn probe(timeout: Duration) -> Caps {
    probe_keeping_input(timeout).0
}

/// [`probe`], also returning what the user typed while it waited, which is on the same stream
/// as the replies and would otherwise be lost. Feed it to `Input::spawn_with`.
pub fn probe_keeping_input(timeout: Duration) -> (Caps, Vec<u8>) {
    let mut out = io::stdout();
    if out.write_all(QUERY.as_bytes()).is_err() || out.flush().is_err() {
        return (Caps::default(), Vec::new());
    }
    let deadline = Instant::now() + timeout;
    let mut got = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        if has_da1(&got) {
            break;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let mut fds = [PollFd {
            fd: 0,
            events: POLLIN,
            revents: 0,
        }];
        let ms = left.as_millis().clamp(1, i32::MAX as u128) as c_int;
        let n = unsafe { poll(fds.as_mut_ptr(), 1, ms) };
        if n <= 0 {
            break;
        }
        let n = unsafe { read(0, buf.as_mut_ptr(), buf.len()) };
        if n <= 0 {
            break;
        }
        got.extend_from_slice(&buf[..n as usize]);
    }
    (parse_replies(&got), strip_replies(&got))
}

/// `got` without the terminal's answers to [`QUERY`]: an OSC string, a DCS string, or a
/// `CSI ?` report. What is left is what the user typed.
fn strip_replies(got: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(got.len());
    let mut i = 0;
    while i < got.len() {
        if got[i] != 0x1b || i + 1 >= got.len() {
            out.push(got[i]);
            i += 1;
            continue;
        }
        match got[i + 1] {
            b']' | b'P' => {
                // up to BEL or ST (ESC \)
                let mut j = i + 2;
                let mut end = None;
                while j < got.len() {
                    if got[j] == 0x07 {
                        end = Some(j + 1);
                        break;
                    }
                    if got[j] == 0x1b && got.get(j + 1) == Some(&b'\\') {
                        end = Some(j + 2);
                        break;
                    }
                    j += 1;
                }
                match end {
                    Some(e) => i = e,
                    None => {
                        out.extend_from_slice(&got[i..]);
                        break;
                    }
                }
            }
            b'[' if got.get(i + 2) == Some(&b'?') => {
                let mut j = i + 3;
                while j < got.len() && (0x20..=0x3f).contains(&got[j]) {
                    j += 1;
                }
                if j < got.len() && (0x40..=0x7e).contains(&got[j]) {
                    i = j + 1;
                } else {
                    out.extend_from_slice(&got[i..]);
                    break;
                }
            }
            _ => {
                out.push(got[i]);
                i += 1;
            }
        }
    }
    out
}

fn has_da1(b: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < b.len() {
        if b[i] == 0x1b && b[i + 1] == b'[' && b[i + 2] == b'?' {
            let mut j = i + 3;
            while j < b.len() && (b[j].is_ascii_digit() || b[j] == b';') {
                j += 1;
            }
            if j < b.len() && b[j] == b'c' {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Pure parser for the probe replies, so it is tested without a terminal.
pub fn parse_replies(b: &[u8]) -> Caps {
    let s = String::from_utf8_lossy(b);
    let mut caps = Caps {
        answered: has_da1(b),
        ..Caps::default()
    };
    if let Some(i) = s.find("\x1b]11;rgb:") {
        let rest = &s[i + "\x1b]11;rgb:".len()..];
        let end = rest.find(['\x07', '\x1b']).unwrap_or(rest.len());
        let parts: Vec<&str> = rest[..end].split('/').collect();
        if parts.len() == 3 {
            let ch = |p: &str| -> Option<u8> {
                let v = u32::from_str_radix(p, 16).ok()?;
                let max = (1u32 << (4 * p.len().clamp(1, 4))) - 1;
                Some((v * 255 / max) as u8)
            };
            if let (Some(r), Some(g), Some(bl)) = (ch(parts[0]), ch(parts[1]), ch(parts[2])) {
                caps.bg = Some((r, g, bl));
            }
        }
    }
    let mode = |n: u32| -> bool {
        let key = format!("\x1b[?{n};");
        s.find(&key).is_some_and(|i| {
            s[i + key.len()..]
                .chars()
                .next()
                .is_some_and(|c| matches!(c, '1'..='4'))
        })
    };
    caps.truecolor = decrqss_truecolor(&s);
    caps.sync = mode(2026);
    caps.grapheme = mode(2027);
    if let Some(i) = s.find("\x1bP>|") {
        let rest = &s[i + 4..];
        let end = rest.find('\x1b').unwrap_or(rest.len());
        caps.xtversion = Some(rest[..end].to_string());
    }
    caps.kitty_kbd = s.contains("\x1b[?0u")
        || s.contains("\x1b[?1u")
        || (s.contains("\x1b[?")
            && s.contains('u')
            && s.find("\x1b[?").is_some_and(|i| {
                let r = &s[i + 3..];
                r.chars().take_while(char::is_ascii_digit).count() > 0
                    && r.trim_start_matches(|c: char| c.is_ascii_digit())
                        .starts_with('u')
            }));
    caps
}

/// The DECRQSS reply to `m` is `DCS 1 $ r <sgr> m ST`. Terminals write the 24-bit colour with
/// `;` or `:` (with or without an empty colour-space slot), so compare the numbers only.
fn decrqss_truecolor(s: &str) -> bool {
    let Some(i) = s.find("\x1bP1$r") else {
        return false;
    };
    let rest = &s[i + 5..];
    let end = rest.find('\x1b').unwrap_or(rest.len());
    let nums: Vec<&str> = rest[..end]
        .trim_end_matches('m')
        .split([';', ':'])
        .filter(|t| !t.is_empty())
        .collect();
    nums.windows(5).any(|w| w == ["48", "2", "1", "2", "3"])
}

/// Bytes that undo everything openc turns on, for `openc --reset` and the exit path. The
/// panic hook and signal handler in `tuikit::term` cover the rest.
pub const RESTORE: &str = "\x1b[?2026l\x1b[?2027l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[?2031l\x1b[<u\x1b[?25h\x1b[0 q\x1b[0m\x1b[?1049l";

/// The window title text, `openc <sep> <directory name>`. The directory name comes from the
/// file system, so a name taken from an archive may hold BEL or ESC; it goes through
/// [`tuikit::width::plain_text`] here and again in `TermGuard::set_title`, which writes it.
pub fn window_title(sep: &str, cwd: &std::path::Path) -> String {
    let name = cwd
        .file_name()
        .map_or(String::new(), |n| n.to_string_lossy().into_owned());
    let name = tuikit::width::plain_text(&name).replace('\n', " ");
    let name = tuikit::width::truncate(&name, 80);
    let sep = tuikit::width::plain_text(sep);
    format!("openc {sep} {name}")
}

struct Inner {
    buf: Vec<u8>,
    sync: bool,
    out: ManuallyDrop<File>,
    last_bytes: usize,
    /// Whether the terminal's cursor is shown after the last frame, as far as the bytes say.
    cursor_shown: bool,
}

/// What ratatui's backend writes to. Bytes are only collected: ratatui and crossterm call
/// `flush` after every command (a clear on resize, a cursor move), and each of those would
/// become its own `write(2)` and its own synchronized block. The loop calls
/// [`FrameHandle::commit`] once per draw, which writes the whole frame in a single `write(2)`
/// inside `?2026h` ... `?2026l` when the terminal supports it.
const CURSOR_HIDE: &[u8] = b"\x1b[?25l";
const CURSOR_SHOW: &[u8] = b"\x1b[?25h";

/// One frame as it goes to the terminal: the cursor is hidden before the first changed cell
/// and shown again at the end, so a terminal that ignores synchronized output never sees the
/// caret hop from cell to cell (design section e). ratatui ends a draw with its own show or
/// hide; when a frame carries neither, the cursor goes back to the state it was in. Returns
/// the bytes and whether the cursor is shown afterwards.
pub fn wrap_frame(body: &[u8], sync: bool, was_shown: bool) -> (Vec<u8>, bool) {
    let last = |pat: &[u8]| body.windows(pat.len()).rposition(|w| w == pat);
    let shown = match (last(CURSOR_SHOW), last(CURSOR_HIDE)) {
        (Some(s), Some(h)) => s > h,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => was_shown,
    };
    let mut out = Vec::with_capacity(body.len() + 24);
    if sync {
        out.extend_from_slice(b"\x1b[?2026h");
    }
    out.extend_from_slice(CURSOR_HIDE);
    out.extend_from_slice(body);
    if shown && last(CURSOR_SHOW).is_none() {
        out.extend_from_slice(CURSOR_SHOW);
    }
    if sync {
        out.extend_from_slice(b"\x1b[?2026l");
    }
    (out, shown)
}

pub struct FrameWriter {
    st: Arc<Mutex<Inner>>,
}

/// The other end of a [`FrameWriter`], kept by the main loop.
#[derive(Clone)]
pub struct FrameHandle {
    st: Arc<Mutex<Inner>>,
}

impl FrameWriter {
    pub fn new(sync: bool) -> (FrameWriter, FrameHandle) {
        // fd 1 is borrowed, never closed: ManuallyDrop keeps File from closing it.
        let out = ManuallyDrop::new(unsafe { File::from_raw_fd(1) });
        let st = Arc::new(Mutex::new(Inner {
            buf: Vec::with_capacity(16 * 1024),
            sync,
            out,
            last_bytes: 0,
            cursor_shown: true,
        }));
        (FrameWriter { st: st.clone() }, FrameHandle { st })
    }
}

impl Write for FrameWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.st
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .buf
            .extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl FrameHandle {
    /// Write everything collected since the last commit. Returns the bytes written.
    pub fn commit(&self) -> io::Result<usize> {
        let mut g = self.st.lock().unwrap_or_else(|e| e.into_inner());
        if g.buf.is_empty() {
            return Ok(0);
        }
        let (frame, shown) = wrap_frame(&g.buf, g.sync, g.cursor_shown);
        g.buf.clear();
        g.cursor_shown = shown;
        g.last_bytes = frame.len();
        g.out.write_all(&frame)?;
        g.out.flush()?;
        Ok(frame.len())
    }

    pub fn last_bytes(&self) -> usize {
        self.st.lock().unwrap_or_else(|e| e.into_inner()).last_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_reply() {
        let r = b"\x1b]11;rgb:1414/1111/0f0f\x1b\\\x1b[?2026;2$y\x1b[?2027;0$y\x1bP>|foot 1.20\x1b\\\x1b[?0u\x1b[?62;4c";
        let c = parse_replies(r);
        assert_eq!(c.bg, Some((0x14, 0x11, 0x0f)));
        assert!(c.sync);
        assert!(!c.grapheme);
        assert_eq!(c.xtversion.as_deref(), Some("foot 1.20"));
        assert!(c.kitty_kbd);
        assert!(c.answered);
        assert_eq!(c.light_background(), Some(false));
    }

    #[test]
    fn light_background_and_short_hex() {
        let c = parse_replies(b"\x1b]11;rgb:ff/ff/ff\x07\x1b[?1;2c");
        assert_eq!(c.bg, Some((255, 255, 255)));
        assert_eq!(c.light_background(), Some(true));
    }

    #[test]
    fn decrqss_reply_with_the_colour_means_truecolor() {
        // The three spellings seen in the wild: `;`, `:` and `:` with an empty colour space.
        for reply in [
            "\x1bP1$r0;48;2;1;2;3m\x1b\\",
            "\x1bP1$r0;48:2:1:2:3m\x1b\\",
            "\x1bP1$r0;48:2::1:2:3m\x1b\\",
        ] {
            let c = parse_replies(format!("{reply}\x1b[?62;4c").as_bytes());
            assert!(c.truecolor, "{reply:?}");
        }
        // An indexed colour, an invalid-request reply and silence are not truecolor.
        for reply in ["\x1bP1$r0;48;5;16m\x1b\\", "\x1bP0$r\x1b\\", ""] {
            let c = parse_replies(format!("{reply}\x1b[?62;4c").as_bytes());
            assert!(!c.truecolor, "{reply:?}");
        }
    }

    #[test]
    fn a_256_colour_env_is_raised_by_the_probe_only() {
        use crate::palette::{upgrade_depth, Depth};
        let tc = Caps {
            truecolor: true,
            ..Caps::default()
        };
        assert_eq!(upgrade_depth(Depth::Ansi256, &tc), Depth::True);
        assert_eq!(upgrade_depth(Depth::Ansi16, &tc), Depth::True);
        assert_eq!(upgrade_depth(Depth::Mono, &tc), Depth::Mono);
        assert_eq!(
            upgrade_depth(Depth::Ansi256, &Caps::default()),
            Depth::Ansi256
        );
    }

    #[test]
    fn every_frame_hides_the_cursor_first_and_ends_with_it_where_it_was() {
        let body = b"\x1b[3;4Hhello\x1b[?25h\x1b[9;9H";
        for sync in [false, true] {
            let (f, shown) = wrap_frame(body, sync, true);
            let s = String::from_utf8(f).unwrap();
            let hide = s.find("\x1b[?25l").unwrap();
            assert!(hide < s.find("hello").unwrap(), "{s:?}");
            assert_eq!(s.matches("\x1b[?25l").count(), 1, "{s:?}");
            assert!(shown && s.contains("\x1b[?25h"));
            assert_eq!(s.starts_with("\x1b[?2026h"), sync);
            assert_eq!(s.ends_with("\x1b[?2026l"), sync);
        }
        // ratatui's own trailing hide wins; nothing re-shows the cursor.
        let (f, shown) = wrap_frame(b"x\x1b[?25h\x1b[?25l", false, true);
        assert!(!shown);
        assert_eq!(String::from_utf8(f).unwrap().matches("?25h").count(), 1);
        // A frame with no cursor command restores the previous state.
        let (f, shown) = wrap_frame(b"x", false, true);
        assert!(shown && f.ends_with(b"\x1b[?25h"));
        let (f, shown) = wrap_frame(b"x", false, false);
        assert!(!shown && !f.windows(6).any(|w| w == b"\x1b[?25h"));
    }

    #[test]
    fn silence_means_no_capabilities() {
        let c = parse_replies(b"");
        assert_eq!(c, Caps::default());
        assert!(!c.answered);
    }

    #[test]
    fn a_hostile_directory_name_cannot_leave_the_title() {
        let cwd = std::path::Path::new("/tmp/repo\x07\x1b]52;c;cHduZWQ=\x07\x1b[31mred\x1b\\x");
        let t = window_title("·", cwd);
        assert!(t.starts_with("openc · repo"), "{t:?}");
        assert!(!t.chars().any(|c| c.is_control()), "{t:?}");
        assert!(!t.contains("52;c"), "{t:?}");
        let long = std::path::Path::new("/tmp/").join("x".repeat(500));
        assert!(window_title("·", &long).len() < 120);
    }

    #[test]
    fn what_the_user_typed_during_the_probe_survives_it() {
        let r = b"he\x1b]11;rgb:1414/1111/0f0f\x1b\\llo\x1b[?2026;2$y\x1b[?2027;0$y\x1bP>|foot 1.20\x1b\\\x1b[?0u\x1b[?62;4c\r";
        assert_eq!(strip_replies(r), b"hello\r");
        // a real escape key and an arrow are not replies
        assert_eq!(strip_replies(b"\x1b\x1b[A\x1b[?62;4c"), b"\x1b\x1b[A");
        assert_eq!(strip_replies(b""), b"");
    }

    #[test]
    fn da1_is_not_confused_with_a_mode_report() {
        assert!(!has_da1(b"\x1b[?2026;2$y"));
        assert!(has_da1(b"junk\x1b[?62;4c"));
    }
}
