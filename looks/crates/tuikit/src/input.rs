//! Terminal input read and decoded here instead of by crossterm's event stream.
//!
//! Why: crossterm drops sequences it does not know, and two of them matter. tmux and xterm
//! send `shift+enter` as `CSI 27;2;13~` (modifyOtherKeys) and terminals that implement mode
//! 2031 tell you the colour scheme changed with `CSI ? 997;N n`. Reading the bytes ourselves
//! also lets a program that needs the terminal (`$EDITOR`) take it over without this reader
//! swallowing half of what is typed: [`Input::pause`] stops it between two reads.
//!
//! [`Parser`] is a pure byte decoder and carries the tests. [`Input`] is the thread and the
//! `SIGWINCH` listener around it, a `Stream` of [`Raw`] for `term::EventPump`.

use std::io;
use std::os::raw::{c_int, c_ulong};
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use crossterm::event::{
    Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton,
    MouseEvent, MouseEventKind,
};
use futures_util::Stream;
use tokio::sync::mpsc;

/// What the decoder produces: a terminal event, or the colour scheme report of mode 2031.
#[derive(Debug, Clone, PartialEq)]
pub enum Raw {
    Ct(CtEvent),
    /// `true` for a dark scheme.
    Scheme(bool),
}

/// Which colour an OSC 10, 11 or 4 reply is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorTarget {
    Foreground,
    Background,
    Palette(u8),
}

/// A terminal's answer to a query. These never reach the key stream; the reader hands them to
/// whoever asked ([`Input::spawn_with_replies`]) and drops them otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    /// OSC 10, 11 or `4;n`. `rgb` is `None` when the reply is there but its colour is not
    /// readable.
    Color {
        target: ColorTarget,
        rgb: Option<(u8, u8, u8)>,
    },
    /// Primary device attributes. Terminals answer in order, so this ends a batch of queries.
    DeviceAttributes,
}

const ESC: u8 = 0x1b;

fn key(code: KeyCode, mods: KeyModifiers) -> Raw {
    Raw::Ct(CtEvent::Key(KeyEvent {
        code,
        modifiers: mods,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }))
}

fn mods_from(param: u32) -> KeyModifiers {
    // The parameter is 1 + a bit set: shift 1, alt 2, ctrl 4, super 8, hyper 16, meta 32.
    let b = param.saturating_sub(1);
    let mut m = KeyModifiers::NONE;
    if b & 1 != 0 {
        m |= KeyModifiers::SHIFT;
    }
    if b & 2 != 0 {
        m |= KeyModifiers::ALT;
    }
    if b & 4 != 0 {
        m |= KeyModifiers::CONTROL;
    }
    if b & 8 != 0 {
        m |= KeyModifiers::SUPER;
    }
    if b & 16 != 0 {
        m |= KeyModifiers::HYPER;
    }
    if b & 32 != 0 {
        m |= KeyModifiers::META;
    }
    m
}

/// A character as a key: uppercase letters carry shift, as crossterm reports them.
fn char_key(c: char, mut mods: KeyModifiers) -> Raw {
    if c.is_uppercase() {
        mods |= KeyModifiers::SHIFT;
    }
    key(KeyCode::Char(c), mods)
}

enum Step {
    /// An event and the bytes it used.
    Got(Raw, usize),
    /// Bytes to skip with no event (a reply to a query, a sequence nobody asked for).
    Skip(usize),
    /// A reply to a query and the bytes it used.
    Reply(Reply, usize),
    /// The buffer ends inside a sequence.
    More,
    /// The buffer ends inside a bracketed paste, which can take as long as it likes. The number
    /// is how many bytes of the buffer are known to hold no end marker, so the next read
    /// continues the search from there instead of starting over.
    Paste(usize),
}

/// Longest escape sequence kept while waiting for its end.
const MAX_SEQ: usize = 256;
/// Longest string sequence (OSC, DCS) kept; clipboard replies can be long.
const MAX_STR: usize = 65_536;
const PASTE_END: &[u8] = b"\x1b[201~";
/// `ESC [ 2 0 0 ~`, the bytes before a paste's text.
const PASTE_START_LEN: usize = 6;
const MAX_PASTE: usize = 32 * 1024 * 1024;

#[derive(Debug, Default)]
pub struct Parser {
    replies: Vec<Reply>,
    buf: Vec<u8>,
    in_paste: bool,
    /// While `in_paste`: bytes of `buf` already searched for the end marker.
    scanned: usize,
}

impl Parser {
    pub fn new() -> Parser {
        Parser::default()
    }

    /// Part of an escape sequence is buffered, so the next read may complete it. When nothing
    /// comes, [`Parser::timeout`] settles it. A paste in progress is not counted: it waits for
    /// its end however long that takes.
    pub fn pending(&self) -> bool {
        !self.buf.is_empty() && !self.in_paste
    }

    /// Replies to queries decoded since the last call, in order.
    pub fn take_replies(&mut self) -> Vec<Reply> {
        std::mem::take(&mut self.replies)
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Raw> {
        self.buf.extend_from_slice(bytes);
        self.drain(false)
    }

    /// Nothing more arrived in time: a lone escape is the escape key, and an unfinished
    /// sequence is dropped.
    pub fn timeout(&mut self) -> Vec<Raw> {
        self.drain(true)
    }

    fn drain(&mut self, timed_out: bool) -> Vec<Raw> {
        let mut out = Vec::new();
        let mut i = 0;
        // The paste carried over from the last read sits at the start of the buffer, and only
        // the new bytes are searched. Rescanning all of it per read made a big paste quadratic.
        let resume = std::mem::take(&mut self.in_paste).then_some(self.scanned);
        while i < self.buf.len() {
            let step = match resume.filter(|_| i == 0) {
                Some(done) => paste_from(&self.buf, PASTE_START_LEN, done),
                None => parse_one(&self.buf[i..], timed_out),
            };
            match step {
                Step::Got(ev, n) => {
                    out.push(ev);
                    i += n;
                }
                Step::Skip(n) => i += n,
                Step::Reply(r, n) => {
                    self.replies.push(r);
                    i += n;
                }
                Step::More => break,
                Step::Paste(n) => {
                    self.in_paste = true;
                    self.scanned = n;
                    break;
                }
            }
        }
        self.buf.drain(..i);
        if timed_out && !self.in_paste {
            self.buf.clear();
        }
        out
    }
}

fn parse_one(b: &[u8], timed_out: bool) -> Step {
    match b[0] {
        ESC => parse_esc(b, timed_out),
        0x0d => Step::Got(key(KeyCode::Enter, KeyModifiers::NONE), 1),
        0x09 => Step::Got(key(KeyCode::Tab, KeyModifiers::NONE), 1),
        // Some terminals send BS for the backspace key. A terminal that tells ctrl+h apart (the
        // kitty protocol, `CSI 27;5;104~`) sends it as a modified key, which stays ctrl+h.
        0x08 => Step::Got(key(KeyCode::Backspace, KeyModifiers::NONE), 1),
        0x7f => Step::Got(key(KeyCode::Backspace, KeyModifiers::NONE), 1),
        0x00 => Step::Got(key(KeyCode::Char(' '), KeyModifiers::CONTROL), 1),
        // ctrl+a .. ctrl+z, ctrl+j included: raw mode, so 0x0a is a key, not a line end.
        c @ 0x01..=0x1a => Step::Got(
            key(KeyCode::Char((c - 1 + b'a') as char), KeyModifiers::CONTROL),
            1,
        ),
        // ctrl+\ is 0x1c and arrives as ctrl+4, which is what the keymap expects.
        c @ 0x1c..=0x1f => Step::Got(
            key(
                KeyCode::Char((c - 0x1c + b'4') as char),
                KeyModifiers::CONTROL,
            ),
            1,
        ),
        _ => parse_utf8(b),
    }
}

fn parse_utf8(b: &[u8]) -> Step {
    let need = match b[0] {
        0x20..=0x7e => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return Step::Skip(1),
    };
    if b.len() < need {
        return Step::More;
    }
    match std::str::from_utf8(&b[..need]) {
        Ok(s) => Step::Got(
            char_key(s.chars().next().unwrap_or(' '), KeyModifiers::NONE),
            need,
        ),
        Err(_) => Step::Skip(1),
    }
}

fn parse_esc(b: &[u8], timed_out: bool) -> Step {
    if b.len() == 1 {
        return if timed_out {
            Step::Got(key(KeyCode::Esc, KeyModifiers::NONE), 1)
        } else {
            Step::More
        };
    }
    match b[1] {
        b'[' => parse_csi(b, timed_out),
        b'O' => parse_ss3(b),
        // OSC, DCS, APC, PM, SOS: replies to queries. Skip to the string terminator; an OSC
        // colour reply is kept for whoever asked.
        b']' => match skip_string(b, timed_out) {
            Step::Skip(n) => match osc_color_reply(&b[2..n]) {
                Some(r) => Step::Reply(r, n),
                None => Step::Skip(n),
            },
            other => other,
        },
        b'P' | b'_' | b'^' | b'X' => skip_string(b, timed_out),
        // Two escapes: the first is the escape key. (rxvt sends alt+arrow as `ESC ESC [ A`, but
        // nothing modern does, while scripts and fast typists do send escape and then a key.)
        ESC => Step::Got(key(KeyCode::Esc, KeyModifiers::NONE), 1),
        // alt plus a plain key
        0x0d => Step::Got(key(KeyCode::Enter, KeyModifiers::ALT), 2),
        0x09 => Step::Got(key(KeyCode::Tab, KeyModifiers::ALT), 2),
        0x7f | 0x08 => Step::Got(key(KeyCode::Backspace, KeyModifiers::ALT), 2),
        c @ 0x01..=0x1a => Step::Got(
            key(
                KeyCode::Char((c - 1 + b'a') as char),
                KeyModifiers::ALT | KeyModifiers::CONTROL,
            ),
            2,
        ),
        _ => match parse_utf8(&b[1..]) {
            Step::Got(Raw::Ct(CtEvent::Key(mut k)), n) => {
                k.modifiers |= KeyModifiers::ALT;
                Step::Got(Raw::Ct(CtEvent::Key(k)), n + 1)
            }
            Step::More => Step::More,
            _ => Step::Skip(2),
        },
    }
}

/// `body` is what follows `ESC ]`, terminator included. Same grammar as Pi's
/// `parseOscColorResponse`: `10;`, `11;` or `4;<0..255>;`, then the value, ended by BEL or ST.
fn osc_color_reply(body: &[u8]) -> Option<Reply> {
    let body = body
        .strip_suffix(b"\x07")
        .or_else(|| body.strip_suffix(b"\x1b\\"))?;
    let text = std::str::from_utf8(body).ok()?;
    // `4;n;value` has two separators; the first split leaves `n;value` for the `4` arm
    let (head, value) = text.split_once(';')?;
    let (target, value) = match head {
        "10" => (ColorTarget::Foreground, value),
        "11" => (ColorTarget::Background, value),
        "4" => {
            let (n, v) = value.split_once(';')?;
            if n.is_empty() || n.len() > 3 || !n.bytes().all(|c| c.is_ascii_digit()) {
                return None;
            }
            (
                ColorTarget::Palette(n.parse::<u16>().ok()?.min(255) as u8),
                v,
            )
        }
        _ => return None,
    };
    Some(Reply::Color {
        target,
        rgb: osc_color_value(value),
    })
}

fn osc_hex_channel(c: &str) -> Option<u8> {
    if c.is_empty() || !c.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let max = 16f64.powi(c.len() as i32) - 1.0;
    let v = u64::from_str_radix(c, 16).ok()? as f64;
    // JavaScript's `Math.round`
    Some((v / max * 255.0 + 0.5).floor() as u8)
}

/// `#rrggbb`, `#rrrrggggbbbb`, `rgb:r/g/b` and `rgba:r/g/b/a` with 1 to 4 hex digits a channel.
fn osc_color_value(raw: &str) -> Option<(u8, u8, u8)> {
    let v = raw.trim();
    if let Some(hex) = v.strip_prefix('#') {
        return match hex.len() {
            6 => Some((
                u8::from_str_radix(&hex[0..2], 16).ok()?,
                u8::from_str_radix(&hex[2..4], 16).ok()?,
                u8::from_str_radix(&hex[4..6], 16).ok()?,
            )),
            12 if hex.is_ascii() => Some((
                osc_hex_channel(&hex[0..4])?,
                osc_hex_channel(&hex[4..8])?,
                osc_hex_channel(&hex[8..12])?,
            )),
            _ => None,
        };
    }
    let lower = v.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("rgba:")
        .or_else(|| lower.strip_prefix("rgb:"))
        .unwrap_or(&lower);
    let mut it = rest.split('/');
    let (r, g, b) = (it.next()?, it.next()?, it.next()?);
    Some((
        osc_hex_channel(r)?,
        osc_hex_channel(g)?,
        osc_hex_channel(b)?,
    ))
}

fn skip_string(b: &[u8], timed_out: bool) -> Step {
    // Ends at BEL or ESC \ .
    let mut i = 2;
    while i < b.len() {
        match b[i] {
            0x07 => return Step::Skip(i + 1),
            ESC if i + 1 < b.len() && b[i + 1] == b'\\' => return Step::Skip(i + 2),
            ESC if i + 1 >= b.len() => break,
            _ => i += 1,
        }
        if i > MAX_STR {
            return Step::Skip(i);
        }
    }
    if timed_out {
        Step::Skip(b.len())
    } else {
        Step::More
    }
}

fn parse_ss3(b: &[u8]) -> Step {
    if b.len() < 3 {
        return Step::More;
    }
    let code = match b[2] {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'P' => KeyCode::F(1),
        b'Q' => KeyCode::F(2),
        b'R' => KeyCode::F(3),
        b'S' => KeyCode::F(4),
        // Application keypad: `ESC O M` is the keypad enter, `ESC O j` .. `y` the keys.
        b'M' => KeyCode::Enter,
        _ => return Step::Skip(3),
    };
    Step::Got(key(code, KeyModifiers::NONE), 3)
}

fn parse_csi(b: &[u8], timed_out: bool) -> Step {
    // b = ESC [ params... final
    let mut i = 2;
    while i < b.len() && (0x30..=0x3f).contains(&b[i]) {
        i += 1;
    }
    let params_end = i;
    while i < b.len() && (0x20..=0x2f).contains(&b[i]) {
        i += 1;
    }
    if i >= b.len() {
        return if timed_out || i > MAX_SEQ {
            Step::Skip(b.len())
        } else {
            Step::More
        };
    }
    let fin = b[i];
    if !(0x40..=0x7e).contains(&fin) {
        // Not a CSI after all; drop what we looked at.
        return Step::Skip(i + 1);
    }
    let len = i + 1;
    let raw = &b[2..params_end];
    let inter = &b[params_end..i];
    let prefix = raw
        .first()
        .copied()
        .filter(|c| matches!(c, b'?' | b'<' | b'>' | b'='));
    let body = if prefix.is_some() { &raw[1..] } else { raw };
    let text = std::str::from_utf8(body).unwrap_or("");
    // Params as numbers; `:` sub-parameters are kept in `subs`.
    let params: Vec<Vec<u32>> = text
        .split(';')
        .map(|p| {
            p.split(':')
                .map(|n| n.parse::<u32>().unwrap_or(0))
                .collect()
        })
        .collect();
    let p = |n: usize| params.get(n).and_then(|v| v.first()).copied();

    if !inter.is_empty() {
        // `CSI ? 2026;2 $ y` mode reports and the like.
        return Step::Skip(len);
    }
    match (prefix, fin) {
        (Some(b'<'), b'M' | b'm') => match sgr_mouse(&params, fin == b'm') {
            Some(m) => Step::Got(Raw::Ct(CtEvent::Mouse(m)), len),
            None => Step::Skip(len),
        },
        // Colour scheme report: 1 dark, 2 light.
        (Some(b'?'), b'n') if p(0) == Some(997) => match p(1) {
            Some(1) => Step::Got(Raw::Scheme(true), len),
            Some(2) => Step::Got(Raw::Scheme(false), len),
            _ => Step::Skip(len),
        },
        // Primary device attributes: `CSI ? Ps ; ... c`.
        (Some(b'?'), b'c') if body.iter().all(|c| c.is_ascii_digit() || *c == b';') => {
            Step::Reply(Reply::DeviceAttributes, len)
        }
        (Some(_), _) => Step::Skip(len),
        (None, b'I') => Step::Got(Raw::Ct(CtEvent::FocusGained), len),
        (None, b'O') => Step::Got(Raw::Ct(CtEvent::FocusLost), len),
        (None, b'~') if p(0) == Some(200) => paste(b, len),
        (None, b'~') if p(0) == Some(27) => {
            // modifyOtherKeys: CSI 27 ; mods ; code ~
            let m = mods_from(p(1).unwrap_or(1));
            match p(2).and_then(char::from_u32) {
                Some(c) => Step::Got(plain_code(c, m), len),
                None => Step::Skip(len),
            }
        }
        (None, b'~') => match (tilde_key(p(0).unwrap_or(0)), p(1)) {
            (Some(code), m) => Step::Got(key(code, mods_from(m.unwrap_or(1))), len),
            (None, _) => Step::Skip(len),
        },
        (None, b'u') => kitty(&params, len),
        (None, b'Z') => Step::Got(
            key(
                KeyCode::BackTab,
                KeyModifiers::SHIFT | mods_from(p(1).unwrap_or(1)),
            ),
            len,
        ),
        (None, f @ (b'A' | b'B' | b'C' | b'D' | b'H' | b'F' | b'P' | b'Q' | b'R' | b'S')) => {
            let code = match f {
                b'A' => KeyCode::Up,
                b'B' => KeyCode::Down,
                b'C' => KeyCode::Right,
                b'D' => KeyCode::Left,
                b'H' => KeyCode::Home,
                b'F' => KeyCode::End,
                b'P' => KeyCode::F(1),
                b'Q' => KeyCode::F(2),
                b'R' => KeyCode::F(3),
                _ => KeyCode::F(4),
            };
            // `CSI 1;5 D`: the first parameter is a count, the second the modifiers. Anything
            // else (`CSI 24;80 R` is a cursor report, `CSI 5;9 H` a cursor move) is not a key.
            if p(0).is_some_and(|n| n > 1) || params.len() > 2 {
                return Step::Skip(len);
            }
            Step::Got(key(code, mods_from(p(1).unwrap_or(1))), len)
        }
        _ => Step::Skip(len),
    }
}

fn paste(b: &[u8], start: usize) -> Step {
    paste_from(b, start, start)
}

/// Look for the end marker in `b[from..]`; `start` is where the text begins. `from` can be the
/// `scanned` count of an earlier call, backed up by the marker's length so one split across two
/// reads is still found.
fn paste_from(b: &[u8], start: usize, from: usize) -> Step {
    let rest = &b[start..];
    let skip = from
        .saturating_sub(start)
        .saturating_sub(PASTE_END.len() - 1);
    let found = rest[skip..]
        .windows(PASTE_END.len())
        .position(|w| w == PASTE_END)
        .map(|i| i + skip);
    match found {
        Some(at) => {
            let s = String::from_utf8_lossy(&rest[..at]).into_owned();
            Step::Got(Raw::Ct(CtEvent::Paste(s)), start + at + PASTE_END.len())
        }
        // Cap what one paste may buffer: past that it goes out as it is.
        None if rest.len() > MAX_PASTE => Step::Got(
            Raw::Ct(CtEvent::Paste(String::from_utf8_lossy(rest).into_owned())),
            b.len(),
        ),
        None => Step::Paste(b.len()),
    }
}

fn tilde_key(n: u32) -> Option<KeyCode> {
    Some(match n {
        1 | 7 => KeyCode::Home,
        2 => KeyCode::Insert,
        3 => KeyCode::Delete,
        4 | 8 => KeyCode::End,
        5 => KeyCode::PageUp,
        6 => KeyCode::PageDown,
        11..=15 => KeyCode::F((n - 10) as u8),
        17..=21 => KeyCode::F((n - 11) as u8),
        23 | 24 => KeyCode::F((n - 12) as u8),
        25 | 26 => KeyCode::F((n - 12) as u8),
        28 | 29 => KeyCode::F((n - 13) as u8),
        31..=34 => KeyCode::F((n - 14) as u8),
        _ => return None,
    })
}

/// A code point from modifyOtherKeys or the kitty protocol as the key crossterm would report.
fn plain_code(c: char, mods: KeyModifiers) -> Raw {
    match c {
        '\r' | '\n' => key(KeyCode::Enter, mods),
        '\t' => key(
            if mods.contains(KeyModifiers::SHIFT) {
                KeyCode::BackTab
            } else {
                KeyCode::Tab
            },
            mods,
        ),
        '\x1b' => key(KeyCode::Esc, mods),
        '\x7f' | '\x08' => key(KeyCode::Backspace, mods),
        c => {
            // With shift and no shifted code point given, a letter is its capital.
            let c = if mods.contains(KeyModifiers::SHIFT) && c.is_ascii_lowercase() {
                c.to_ascii_uppercase()
            } else {
                c
            };
            key(KeyCode::Char(c), mods)
        }
    }
}

fn kitty(params: &[Vec<u32>], len: usize) -> Step {
    let code = params.first().and_then(|v| v.first()).copied().unwrap_or(0);
    let shifted = params
        .first()
        .and_then(|v| v.get(1))
        .copied()
        .filter(|n| *n > 0);
    let (m, ev) = match params.get(1) {
        Some(v) => (
            v.first().copied().unwrap_or(1),
            v.get(1).copied().unwrap_or(1),
        ),
        None => (1, 1),
    };
    if ev == 3 {
        // Releases are only sent when asked for; ignore them if one comes anyway.
        return Step::Skip(len);
    }
    let mods = mods_from(m);
    let raw = match code {
        // Keypad and function keys in the private use area.
        57376..=57398 => key(KeyCode::F((code - 57376 + 13) as u8), mods),
        57399..=57408 => key(
            KeyCode::Char(char::from_u32(u32::from(b'0') + code - 57399).unwrap_or('0')),
            mods,
        ),
        57409 => key(KeyCode::Char('.'), mods),
        57410 => key(KeyCode::Char('/'), mods),
        57411 => key(KeyCode::Char('*'), mods),
        57412 => key(KeyCode::Char('-'), mods),
        57413 => key(KeyCode::Char('+'), mods),
        57414 => key(KeyCode::Enter, mods),
        57415 => key(KeyCode::Char('='), mods),
        57417 => key(KeyCode::Left, mods),
        57418 => key(KeyCode::Right, mods),
        57419 => key(KeyCode::Up, mods),
        57420 => key(KeyCode::Down, mods),
        57421 => key(KeyCode::PageUp, mods),
        57422 => key(KeyCode::PageDown, mods),
        57423 => key(KeyCode::Home, mods),
        57424 => key(KeyCode::End, mods),
        57425 => key(KeyCode::Insert, mods),
        57426 => key(KeyCode::Delete, mods),
        // Lock keys, modifier keys, media keys: nothing to do with a prompt.
        57344..=57500 => return Step::Skip(len),
        c => match char::from_u32(c) {
            Some(ch) => match shifted.and_then(char::from_u32) {
                Some(s) if mods.contains(KeyModifiers::SHIFT) => key(KeyCode::Char(s), mods),
                _ => plain_code(ch, mods),
            },
            None => return Step::Skip(len),
        },
    };
    Step::Got(raw, len)
}

fn sgr_mouse(params: &[Vec<u32>], release: bool) -> Option<MouseEvent> {
    let n = |i: usize| params.get(i).and_then(|v| v.first()).copied();
    let cb = n(0)?;
    // A report can carry any number; clamp it instead of letting `as u16` wrap it to a
    // coordinate near the top left.
    let x = n(1)?.saturating_sub(1).min(u32::from(u16::MAX)) as u16;
    let y = n(2)?.saturating_sub(1).min(u32::from(u16::MAX)) as u16;
    let button = match cb & 0b11 {
        0 => MouseButton::Left,
        1 => MouseButton::Middle,
        _ => MouseButton::Right,
    };
    let mut modifiers = KeyModifiers::NONE;
    if cb & 4 != 0 {
        modifiers |= KeyModifiers::SHIFT;
    }
    if cb & 8 != 0 {
        modifiers |= KeyModifiers::ALT;
    }
    if cb & 16 != 0 {
        modifiers |= KeyModifiers::CONTROL;
    }
    let motion = cb & 32 != 0;
    let kind = if cb & 64 != 0 {
        match cb & 0b11 {
            0 => MouseEventKind::ScrollUp,
            1 => MouseEventKind::ScrollDown,
            2 => MouseEventKind::ScrollLeft,
            _ => MouseEventKind::ScrollRight,
        }
    } else if motion {
        if cb & 0b11 == 3 {
            MouseEventKind::Moved
        } else {
            MouseEventKind::Drag(button)
        }
    } else if release {
        MouseEventKind::Up(button)
    } else {
        MouseEventKind::Down(button)
    };
    Some(MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers,
    })
}

// ---- the reader -----------------------------------------------------------------------------

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: i16,
    revents: i16,
}

extern "C" {
    fn poll(fds: *mut PollFd, n: c_ulong, timeout: c_int) -> c_int;
    fn read(fd: c_int, buf: *mut u8, n: usize) -> isize;
    fn write(fd: c_int, buf: *const u8, n: usize) -> isize;
    fn pipe(fds: *mut c_int) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn isatty(fd: c_int) -> c_int;
}

const POLLIN: i16 = 1;
/// Wait this long for the rest of an escape sequence before calling it the escape key.
pub const ESC_TIMEOUT: Duration = Duration::from_millis(25);

#[derive(Default)]
struct State {
    paused: bool,
    /// The reader is parked at the gate and is not touching the terminal.
    idle: bool,
    stop: bool,
}

struct Ctl {
    state: Mutex<State>,
    cv: Condvar,
    /// Write end of the pipe that wakes the reader out of `poll`, so an idle reader sleeps
    /// until a key arrives instead of waking on a timer.
    wake_wr: c_int,
}

impl Ctl {
    fn wake(&self) {
        let b = 1u8;
        unsafe {
            write(self.wake_wr, &b, 1);
        }
    }
}

/// Handle to pause and resume the reader from another thread.
#[derive(Clone)]
pub struct InputControl(Arc<Ctl>);

impl InputControl {
    /// Stop reading the terminal and return once the reader has stopped. Call before running a
    /// program that reads the terminal itself.
    pub fn pause(&self) {
        let mut s = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        s.paused = true;
        self.0.wake();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !s.idle && !s.stop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            let (g, _) = self
                .0
                .cv
                .wait_timeout(s, left)
                .unwrap_or_else(|e| e.into_inner());
            s = g;
        }
    }

    pub fn resume(&self) {
        let mut s = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        s.paused = false;
        self.0.cv.notify_all();
    }
}

/// The terminal's input as a stream: a thread reads and decodes, a task turns `SIGWINCH` into
/// resize events.
pub struct Input {
    rx: mpsc::UnboundedReceiver<io::Result<Raw>>,
    ctl: Arc<Ctl>,
    winch: tokio::task::JoinHandle<()>,
}

impl Input {
    /// Start reading. Needs a tokio runtime for the resize signal.
    pub fn spawn() -> io::Result<Input> {
        Self::spawn_with(Vec::new())
    }

    /// Start reading, first decoding `typed_ahead`: bytes already taken off the terminal (by a
    /// startup query that also swallowed what the user typed meanwhile).
    pub fn spawn_with(typed_ahead: Vec<u8>) -> io::Result<Input> {
        Self::spawn_inner(typed_ahead, None)
    }

    /// Like [`spawn_with`](Self::spawn_with), and the terminal's answers to queries (OSC colour
    /// replies, device attributes) arrive on the second value instead of being dropped. They
    /// come out of the same decoder as the keys, so a reply split across reads or arriving late
    /// never turns into typed characters.
    pub fn spawn_with_replies(
        typed_ahead: Vec<u8>,
    ) -> io::Result<(Input, mpsc::UnboundedReceiver<Reply>)> {
        let (tx, rx) = mpsc::unbounded_channel();
        Ok((Self::spawn_inner(typed_ahead, Some(tx))?, rx))
    }

    fn spawn_inner(
        typed_ahead: Vec<u8>,
        replies: Option<mpsc::UnboundedSender<Reply>>,
    ) -> io::Result<Input> {
        let fd = if unsafe { isatty(0) } == 1 {
            0
        } else {
            // Input was redirected; the controlling terminal is still the keyboard.
            use std::os::fd::IntoRawFd;
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty")?
                .into_raw_fd()
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let mut p = [0 as c_int; 2];
        if unsafe { pipe(p.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let (wake_rd, wake_wr) = (p[0], p[1]);
        let ctl = Arc::new(Ctl {
            state: Mutex::new(State::default()),
            cv: Condvar::new(),
            wake_wr,
        });
        let c = ctl.clone();
        let t = tx.clone();
        std::thread::Builder::new()
            .name("input".into())
            .spawn(move || read_loop(fd, wake_rd, t, c, typed_ahead, replies))?;
        let mut sig =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
        let winch = tokio::spawn(async move {
            while sig.recv().await.is_some() {
                if let Ok((w, h)) = crossterm::terminal::size() {
                    if tx.send(Ok(Raw::Ct(CtEvent::Resize(w, h)))).is_err() {
                        return;
                    }
                }
            }
        });
        Ok(Input { rx, ctl, winch })
    }

    pub fn control(&self) -> InputControl {
        InputControl(self.ctl.clone())
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        self.winch.abort();
        let mut s = self.ctl.state.lock().unwrap_or_else(|e| e.into_inner());
        s.stop = true;
        self.ctl.cv.notify_all();
        self.ctl.wake();
    }
}

impl Stream for Input {
    type Item = io::Result<Raw>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

fn read_loop(
    fd: c_int,
    wake_rd: c_int,
    tx: mpsc::UnboundedSender<io::Result<Raw>>,
    ctl: Arc<Ctl>,
    typed_ahead: Vec<u8>,
    replies: Option<mpsc::UnboundedSender<Reply>>,
) {
    let mut parser = Parser::new();
    let forward = |parser: &mut Parser| {
        for r in parser.take_replies() {
            if let Some(sink) = &replies {
                let _ = sink.send(r);
            }
        }
    };
    if !typed_ahead.is_empty() {
        let evs = parser.feed(&typed_ahead);
        forward(&mut parser);
        for e in evs {
            if tx.send(Ok(e)).is_err() {
                return;
            }
        }
    }
    let mut buf = [0u8; 4096];
    loop {
        {
            let mut s = ctl.state.lock().unwrap_or_else(|e| e.into_inner());
            if s.stop {
                break;
            }
            while s.paused && !s.stop {
                s.idle = true;
                ctl.cv.notify_all();
                s = ctl.cv.wait(s).unwrap_or_else(|e| e.into_inner());
            }
            s.idle = false;
            if s.stop {
                break;
            }
        }
        // Sleep until a key comes or somebody wakes us; only a half-read escape sequence
        // sets a timer.
        let wait: c_int = if parser.pending() {
            ESC_TIMEOUT.as_millis() as c_int
        } else {
            -1
        };
        let mut fds = [
            PollFd {
                fd,
                events: POLLIN,
                revents: 0,
            },
            PollFd {
                fd: wake_rd,
                events: POLLIN,
                revents: 0,
            },
        ];
        let n = unsafe { poll(fds.as_mut_ptr(), 2, wait) };
        if n > 0 && fds[1].revents & POLLIN != 0 {
            // Drain the wake-up bytes; the flags are looked at at the top of the loop.
            let mut sink = [0u8; 64];
            unsafe {
                read(wake_rd, sink.as_mut_ptr(), sink.len());
            }
            continue;
        }
        let events = if n > 0 {
            let got = unsafe { read(fd, buf.as_mut_ptr(), buf.len()) };
            if got <= 0 {
                if got < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                let _ = tx.send(Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "terminal closed",
                )));
                break;
            }
            parser.feed(&buf[..got as usize])
        } else if n == 0 && parser.pending() {
            parser.timeout()
        } else {
            Vec::new()
        };
        forward(&mut parser);
        let mut gone = false;
        for e in events {
            if tx.send(Ok(e)).is_err() {
                gone = true;
                break;
            }
        }
        if gone {
            break;
        }
    }
    unsafe {
        close(wake_rd);
    }
}

/// The prompt as a file for `$EDITOR`: a name nobody can guess and nobody else can read, created
/// with `O_EXCL` so an existing file or a planted symlink is never written through.
pub fn write_editor_file(prefix: &str, text: &str) -> Option<std::path::PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    for n in 0..8u32 {
        let path =
            std::env::temp_dir().join(format!("{prefix}-{}-{stamp}-{n}.md", std::process::id()));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path);
        match file {
            Ok(mut f) => {
                return match f.write_all(text.as_bytes()) {
                    Ok(()) => Some(path),
                    Err(_) => {
                        let _ = std::fs::remove_file(&path);
                        None
                    }
                };
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

/// How a round trip through `$VISUAL` or `$EDITOR` ended.
#[derive(Debug, PartialEq, Eq)]
pub enum EditorOutcome {
    /// The editor saved and exited with success; this is the file, which may be empty.
    Edited(String),
    /// Neither `$VISUAL` nor `$EDITOR` is set and there is no default.
    NoEditor,
    /// A line for the user: the temp file was not writable, the editor would not start, or it
    /// exited with an error.
    Failed(String),
}

/// Hand the terminal to the user's editor for `text`. The reader is parked before the terminal
/// is given back (or it takes every other key typed into the editor), and picked up again after.
/// `fallback` is used when neither variable is set. The caller makes a fresh `Terminal` afterwards.
pub fn external_editor(
    guard: &mut crate::term::TermGuard,
    input: &InputControl,
    cwd: &std::path::Path,
    prefix: &str,
    text: &str,
    fallback: Option<&str>,
) -> EditorOutcome {
    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("EDITOR").ok().filter(|s| !s.is_empty()))
        .or_else(|| fallback.map(String::from));
    let Some(editor) = editor else {
        return EditorOutcome::NoEditor;
    };
    let Some(path) = write_editor_file(prefix, text) else {
        return EditorOutcome::Failed("Could not write the temporary file".into());
    };
    input.pause();
    guard.suspend();
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(&path)
        .current_dir(cwd)
        .status();
    let _ = guard.resume();
    input.resume();
    let out = match status {
        Ok(s) if s.success() => match std::fs::read(&path) {
            Ok(b) => EditorOutcome::Edited(String::from_utf8_lossy(&b).into_owned()),
            Err(e) => EditorOutcome::Failed(format!("Could not read the edited file: {e}")),
        },
        Ok(s) if s.code() == Some(127) => {
            EditorOutcome::Failed(format!("Could not run the editor `{editor}`"))
        }
        Ok(s) => EditorOutcome::Failed(format!("The editor exited with {s}")),
        Err(e) => EditorOutcome::Failed(format!("Could not start the editor: {e}")),
    };
    let _ = std::fs::remove_file(&path);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_editor_file_is_private_and_never_overwrites() {
        use std::os::unix::fs::PermissionsExt;
        let a = write_editor_file("tk-test", "one").expect("a file");
        let b = write_editor_file("tk-test", "two").expect("another file");
        assert_ne!(a, b);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "one");
        let mode = std::fs::metadata(&a).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_file(a);
        let _ = std::fs::remove_file(b);
    }

    fn feed(s: &[u8]) -> Vec<Raw> {
        Parser::new().feed(s)
    }

    fn one(s: &[u8]) -> KeyEvent {
        match feed(s).as_slice() {
            [Raw::Ct(CtEvent::Key(k))] => *k,
            other => panic!("{s:?} gave {other:?}"),
        }
    }

    fn k(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    const N: KeyModifiers = KeyModifiers::NONE;
    const C: KeyModifiers = KeyModifiers::CONTROL;
    const A: KeyModifiers = KeyModifiers::ALT;
    const S: KeyModifiers = KeyModifiers::SHIFT;

    #[test]
    fn plain_and_control_keys() {
        assert_eq!(one(b"a"), k(KeyCode::Char('a'), N));
        assert_eq!(one(b"G"), k(KeyCode::Char('G'), S));
        assert_eq!(one("é".as_bytes()), k(KeyCode::Char('é'), N));
        assert_eq!(one(b"\r"), k(KeyCode::Enter, N));
        assert_eq!(
            one(b"\n"),
            k(KeyCode::Char('j'), C),
            "ctrl+j is a key in raw mode"
        );
        assert_eq!(one(b"\t"), k(KeyCode::Tab, N));
        assert_eq!(one(b"\x7f"), k(KeyCode::Backspace, N));
        assert_eq!(one(b"\x03"), k(KeyCode::Char('c'), C));
        assert_eq!(one(b"\x1c"), k(KeyCode::Char('4'), C), "ctrl+backslash");
        assert_eq!(one(b"\x1f"), k(KeyCode::Char('7'), C), "ctrl+underscore");
        assert_eq!(one(b"\x00"), k(KeyCode::Char(' '), C));
    }

    #[test]
    fn alt_keys() {
        assert_eq!(one(b"\x1bb"), k(KeyCode::Char('b'), A));
        assert_eq!(one(b"\x1b\r"), k(KeyCode::Enter, A));
        assert_eq!(one(b"\x1b\x7f"), k(KeyCode::Backspace, A));
        // Escape, then a key typed right behind it, is two keys.
        assert_eq!(
            feed(b"\x1b\x1b[A"),
            vec![key(KeyCode::Esc, N), key(KeyCode::Up, N)]
        );
        assert_eq!(one(b"\x1bD"), k(KeyCode::Char('D'), A | S));
    }

    #[test]
    fn arrows_and_modified_arrows() {
        assert_eq!(one(b"\x1b[A"), k(KeyCode::Up, N));
        assert_eq!(one(b"\x1b[1;5D"), k(KeyCode::Left, C));
        assert_eq!(one(b"\x1b[1;3C"), k(KeyCode::Right, A));
        assert_eq!(one(b"\x1bOP"), k(KeyCode::F(1), N));
        assert_eq!(one(b"\x1b[H"), k(KeyCode::Home, N));
        assert_eq!(one(b"\x1b[F"), k(KeyCode::End, N));
        assert_eq!(one(b"\x1b[5~"), k(KeyCode::PageUp, N));
        assert_eq!(one(b"\x1b[6;5~"), k(KeyCode::PageDown, C));
        assert_eq!(one(b"\x1b[3~"), k(KeyCode::Delete, N));
        assert_eq!(one(b"\x1b[15~"), k(KeyCode::F(5), N));
        assert_eq!(one(b"\x1b[24~"), k(KeyCode::F(12), N));
        assert_eq!(one(b"\x1b[Z"), k(KeyCode::BackTab, S));
    }

    #[test]
    fn shift_enter_in_every_encoding() {
        // modifyOtherKeys, as tmux and xterm send it.
        assert_eq!(one(b"\x1b[27;2;13~"), k(KeyCode::Enter, S));
        assert_eq!(one(b"\x1b[27;5;13~"), k(KeyCode::Enter, C));
        assert_eq!(one(b"\x1b[27;2;9~"), k(KeyCode::BackTab, S));
        // kitty protocol with disambiguate.
        assert_eq!(one(b"\x1b[13;2u"), k(KeyCode::Enter, S));
        assert_eq!(one(b"\x1b[13;3u"), k(KeyCode::Enter, A));
    }

    #[test]
    fn kitty_keys() {
        assert_eq!(one(b"\x1b[27u"), k(KeyCode::Esc, N));
        assert_eq!(one(b"\x1b[103;5u"), k(KeyCode::Char('g'), C));
        assert_eq!(one(b"\x1b[92;5u"), k(KeyCode::Char('\\'), C));
        assert_eq!(one(b"\x1b[103:71;2u"), k(KeyCode::Char('G'), S));
        assert_eq!(one(b"\x1b[103;2u"), k(KeyCode::Char('G'), S));
        assert_eq!(one(b"\x1b[57399u"), k(KeyCode::Char('0'), N));
        // A release is not a key press.
        assert!(feed(b"\x1b[97;1:3u").is_empty());
        // A modifier key on its own is nothing.
        assert!(feed(b"\x1b[57441;2u").is_empty());
        // A repeat counts as a press.
        assert_eq!(one(b"\x1b[97;1:2u"), k(KeyCode::Char('a'), N));
    }

    #[test]
    fn bracketed_paste_keeps_everything_inside() {
        let evs = feed(b"\x1b[200~line 1\nline \x1b[A 2\r\n\x1b[201~");
        assert_eq!(
            evs,
            vec![Raw::Ct(CtEvent::Paste("line 1\nline \x1b[A 2\r\n".into()))]
        );
    }

    #[test]
    fn a_paste_split_across_reads_arrives_once_and_whole() {
        let mut p = Parser::new();
        assert!(p.feed(b"\x1b[200~first ").is_empty());
        assert!(p.feed(b"half").is_empty());
        let evs = p.feed(b" second\x1b[201~x");
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0], Raw::Ct(CtEvent::Paste("first half second".into())));
    }

    #[test]
    fn a_big_paste_in_small_reads_is_scanned_once_not_once_per_read() {
        let mut p = Parser::new();
        assert!(p.feed(b"\x1b[200~").is_empty());
        let t = std::time::Instant::now();
        let chunk = [b'x'; 512];
        for _ in 0..8192 {
            assert!(p.feed(&chunk).is_empty());
        }
        let evs = p.feed(b"\x1b[201~");
        assert!(
            t.elapsed() < std::time::Duration::from_secs(3),
            "{:?} for 4 MB: the whole buffer is rescanned per read",
            t.elapsed()
        );
        match &evs[..] {
            [Raw::Ct(CtEvent::Paste(s))] => assert_eq!(s.len(), 512 * 8192),
            other => panic!("{} events", other.len()),
        }
    }

    #[test]
    fn an_end_marker_split_across_reads_is_still_found() {
        let mut p = Parser::new();
        assert!(p.feed(b"\x1b[200~abc\x1b[2").is_empty());
        assert!(p.feed(b"0").is_empty());
        let evs = p.feed(b"1~z");
        assert_eq!(evs[0], Raw::Ct(CtEvent::Paste("abc".into())));
        assert_eq!(evs.len(), 2);
    }

    #[test]
    fn a_csi_with_parameters_that_is_not_a_key_report_is_not_a_key() {
        // A cursor move and a cursor position report are not Home and F3.
        assert!(feed(b"\x1b[999999999;999999999H").is_empty());
        assert!(feed(b"\x1b[24;80R").is_empty());
        assert!(feed(b"\x1b[5A").is_empty());
        // The real ones still are.
        assert_eq!(one(b"\x1b[H"), k(KeyCode::Home, N));
        assert_eq!(one(b"\x1b[1;5H"), k(KeyCode::Home, KeyModifiers::CONTROL));
        assert_eq!(one(b"\x1b[1;2A"), k(KeyCode::Up, KeyModifiers::SHIFT));
    }

    #[test]
    fn a_slow_paste_is_not_cut_by_the_escape_timeout() {
        let mut p = Parser::new();
        assert!(p.feed(b"\x1b[200~half a paste").is_empty());
        assert!(
            !p.pending(),
            "a paste in progress does not wait on the timeout"
        );
        assert!(p.timeout().is_empty());
        let evs = p.feed(b", the rest\x1b[201~");
        assert_eq!(
            evs,
            vec![Raw::Ct(CtEvent::Paste("half a paste, the rest".into()))]
        );
    }

    #[test]
    fn sgr_mouse() {
        let ev = |s: &[u8]| match feed(s).as_slice() {
            [Raw::Ct(CtEvent::Mouse(m))] => *m,
            o => panic!("{o:?}"),
        };
        let m = ev(b"\x1b[<0;10;5M");
        assert_eq!(
            (m.kind, m.column, m.row),
            (MouseEventKind::Down(MouseButton::Left), 9, 4)
        );
        assert_eq!(
            ev(b"\x1b[<0;10;5m").kind,
            MouseEventKind::Up(MouseButton::Left)
        );
        assert_eq!(ev(b"\x1b[<64;1;1M").kind, MouseEventKind::ScrollUp);
        assert_eq!(ev(b"\x1b[<65;1;1M").kind, MouseEventKind::ScrollDown);
        assert_eq!(
            ev(b"\x1b[<32;3;3M").kind,
            MouseEventKind::Drag(MouseButton::Left)
        );
        assert_eq!(ev(b"\x1b[<35;3;3M").kind, MouseEventKind::Moved);
        assert_eq!(
            ev(b"\x1b[<2;3;3M").kind,
            MouseEventKind::Down(MouseButton::Right)
        );
        assert!(ev(b"\x1b[<16;1;1M").modifiers.contains(C));
    }

    #[test]
    fn focus_and_colour_scheme_reports() {
        assert_eq!(feed(b"\x1b[I"), vec![Raw::Ct(CtEvent::FocusGained)]);
        assert_eq!(feed(b"\x1b[O"), vec![Raw::Ct(CtEvent::FocusLost)]);
        assert_eq!(feed(b"\x1b[?997;1n"), vec![Raw::Scheme(true)]);
        assert_eq!(feed(b"\x1b[?997;2n"), vec![Raw::Scheme(false)]);
        assert!(feed(b"\x1b[?997;9n").is_empty());
    }

    #[test]
    fn replies_to_our_queries_are_swallowed_not_typed() {
        // OSC 11 (both terminators), DA1, a mode report, XTVERSION, kitty flags.
        let replies: &[&[u8]] = &[
            b"\x1b]11;rgb:1414/1111/0f0f\x07",
            b"\x1b]11;rgb:1414/1111/0f0f\x1b\\",
            b"\x1b[?62;4c",
            b"\x1b[?2026;2$y",
            b"\x1bP>|foot 1.20\x1b\\",
            b"\x1b[?0u",
            b"\x1b[8;36;120t",
        ];
        for r in replies {
            assert!(feed(r).is_empty(), "{:?}", String::from_utf8_lossy(r));
        }
        // And what is typed around them survives.
        let evs = feed(b"a\x1b]11;rgb:0/0/0\x07b");
        assert_eq!(evs.len(), 2);
    }

    fn replies(s: &[u8]) -> (Vec<Raw>, Vec<Reply>) {
        let mut p = Parser::new();
        let evs = p.feed(s);
        (evs, p.take_replies())
    }

    fn color(target: ColorTarget, rgb: Option<(u8, u8, u8)>) -> Reply {
        Reply::Color { target, rgb }
    }

    #[test]
    fn osc_colour_replies_are_decoded_as_replies_not_keys() {
        use ColorTarget::*;
        // every terminator and every channel width Pi's parser accepts
        let (evs, got) = replies(
            b"\x1b]10;rgb:abab/b2b2/bfbf\x07\x1b]11;rgb:28/2c/34\x1b\\\x1b]4;7;#abb2bf\x07\x1b]4;15;rgb:f/f/f\x07",
        );
        assert!(evs.is_empty());
        assert_eq!(
            got,
            vec![
                color(Foreground, Some((0xab, 0xb2, 0xbf))),
                color(Background, Some((0x28, 0x2c, 0x34))),
                color(Palette(7), Some((0xab, 0xb2, 0xbf))),
                color(Palette(15), Some((255, 255, 255))),
            ]
        );
        // 12 digit hex is 16 bits a channel, scaled and rounded like Pi does
        let (_, got) = replies(b"\x1b]11;#282cff340000\x07");
        assert_eq!(got, vec![color(Background, Some((40, 254, 0)))]);
        // a reply whose colour cannot be read is still a reply
        let (_, got) = replies(b"\x1b]11;nonsense\x07\x1b]4;3;rgb:zz/0/0\x07");
        assert_eq!(got, vec![color(Background, None), color(Palette(3), None)]);
        // other OSC strings are skipped without a reply
        let (evs, got) = replies(b"\x1b]52;c;aGk=\x07\x1b]4;x;rgb:0/0/0\x07\x1b]12;rgb:0/0/0\x07");
        assert!(evs.is_empty() && got.is_empty());
    }

    #[test]
    fn device_attributes_end_a_batch_and_stay_out_of_the_keys() {
        let (evs, got) = replies(b"\x1b[?62;4;22c");
        assert!(evs.is_empty());
        assert_eq!(got, vec![Reply::DeviceAttributes]);
        // kitty flags and mode reports are not device attributes
        let (_, got) = replies(b"\x1b[?0u\x1b[?2026;2$y\x1b[?997;1n");
        assert!(got.is_empty());
    }

    #[test]
    fn a_colour_reply_split_across_reads_or_wedged_between_keys_leaves_no_stray_bytes() {
        let mut p = Parser::new();
        let mut keys = p.feed(b"a\x1b]11;rgb:28");
        keys.extend(p.feed(b"/2c/34"));
        assert_eq!(keys, vec![key(KeyCode::Char('a'), N)]);
        assert!(p.take_replies().is_empty());
        keys = p.feed(b"\x07b\x1b[?6");
        keys.extend(p.feed(b"2c"));
        keys.extend(p.feed(b"c"));
        assert_eq!(
            keys,
            vec![key(KeyCode::Char('b'), N), key(KeyCode::Char('c'), N)]
        );
        assert_eq!(
            p.take_replies(),
            vec![
                color(ColorTarget::Background, Some((0x28, 0x2c, 0x34))),
                Reply::DeviceAttributes
            ]
        );
    }

    #[test]
    fn bs_is_backspace_and_a_modified_ctrl_h_stays_ctrl_h() {
        assert_eq!(one(b"\x08"), k(KeyCode::Backspace, N));
        assert_eq!(one(b"\x7f"), k(KeyCode::Backspace, N));
        assert_eq!(one(b"\x1b\x08"), k(KeyCode::Backspace, A));
        // a terminal that tells them apart (kitty `CSI u`, modifyOtherKeys) says so
        assert_eq!(one(b"\x1b[104;5u"), k(KeyCode::Char('h'), C));
        assert_eq!(one(b"\x1b[27;5;104~"), k(KeyCode::Char('h'), C));
    }

    #[test]
    fn a_sequence_split_between_reads_is_decoded_whole() {
        let mut p = Parser::new();
        assert!(p.feed(b"\x1b").is_empty());
        assert!(p.feed(b"[1;").is_empty());
        let evs = p.feed(b"5D");
        assert_eq!(evs, vec![key(KeyCode::Left, C)]);
        // Same for a multi-byte character.
        let bytes = "é".as_bytes();
        assert!(p.feed(&bytes[..1]).is_empty());
        assert_eq!(p.feed(&bytes[1..]), vec![key(KeyCode::Char('é'), N)]);
    }

    #[test]
    fn a_lone_escape_is_the_escape_key_after_the_timeout() {
        let mut p = Parser::new();
        assert!(p.feed(b"\x1b").is_empty());
        assert!(p.pending());
        assert_eq!(p.timeout(), vec![key(KeyCode::Esc, N)]);
        assert!(!p.pending());
        // An unfinished sequence is dropped instead of typing its tail.
        p.feed(b"\x1b[1;");
        assert!(p.timeout().is_empty());
        assert!(!p.pending());
    }

    #[test]
    fn escape_then_a_key_typed_fast_is_alt() {
        // Both bytes in one read: it is alt+x, not escape then x.
        assert_eq!(one(b"\x1bx"), k(KeyCode::Char('x'), A));
    }

    #[test]
    fn garbage_does_not_wedge_the_parser() {
        let mut p = Parser::new();
        let evs = p.feed(b"\xff\xfe\x1b[\x01zok");
        // The bad bytes go, the keys after them survive.
        assert!(evs.iter().any(|e| *e == key(KeyCode::Char('o'), N)));
        assert!(evs.iter().any(|e| *e == key(KeyCode::Char('k'), N)));
        assert!(!p.pending());
    }

    #[test]
    fn typed_text_with_several_keys_in_one_read() {
        let evs = feed(b"ab\x1b[A\r");
        assert_eq!(
            evs,
            vec![
                key(KeyCode::Char('a'), N),
                key(KeyCode::Char('b'), N),
                key(KeyCode::Up, N),
                key(KeyCode::Enter, N),
            ]
        );
    }

    #[test]
    fn the_end_marker_is_found_wherever_a_read_cuts_it() {
        // Searching only the new bytes must not miss a marker that straddles two reads.
        let whole = b"x\x1b[200~ab\x1b[1;2Ac\x1b[201~y".to_vec();
        for cut1 in 0..whole.len() {
            for cut2 in cut1..whole.len() {
                let mut p = Parser::new();
                let mut evs = Vec::new();
                for part in [&whole[..cut1], &whole[cut1..cut2], &whole[cut2..]] {
                    evs.extend(p.feed(part));
                }
                let pastes: Vec<_> = evs
                    .iter()
                    .filter_map(|e| match e {
                        Raw::Ct(CtEvent::Paste(s)) => Some(s.clone()),
                        _ => None,
                    })
                    .collect();
                assert_eq!(pastes, ["ab\x1b[1;2Ac"], "cuts {cut1} {cut2}: {evs:?}");
                assert_eq!(evs.len(), 3, "x, the paste, y: cuts {cut1} {cut2}");
            }
        }
    }

    #[test]
    fn a_big_paste_in_small_reads_is_linear() {
        // Finding 15: each 4 KB read rescanned the whole paste, 1.3 s for 4 MB (release).
        let n = 8 << 20;
        let mut data = b"\x1b[200~".to_vec();
        data.extend(std::iter::repeat_n(b'x', n));
        data.extend(b"\x1b[201~");
        let mut p = Parser::new();
        let t = std::time::Instant::now();
        let mut got = None;
        for c in data.chunks(4096) {
            for e in p.feed(c) {
                if let Raw::Ct(CtEvent::Paste(s)) = e {
                    got = Some(s.len());
                }
            }
        }
        assert_eq!(got, Some(n));
        assert!(
            t.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            t.elapsed()
        );
    }

    #[test]
    fn a_huge_mouse_coordinate_saturates_instead_of_wrapping() {
        let evs = feed(b"\x1b[<0;70000;65537M");
        match evs.as_slice() {
            [Raw::Ct(CtEvent::Mouse(m))] => assert_eq!((m.column, m.row), (u16::MAX, u16::MAX)),
            o => panic!("{o:?}"),
        }
    }
}
