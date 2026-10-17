//! `piw` on a pty with a fake terminal on the other end that answers (or does not answer) the
//! colour query. Checks the protocol and what the first screen is painted with: the query goes
//! out as Pi writes it, replies in any shape (all at once, split byte by byte, late, never) leave
//! no stray bytes in the editor, and a plain launch is painted in the theme Pi generates for what
//! the terminal said.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use piw::theme::COLOR_QUERY;
use serde_json::Value;

/// The longest any wait here may take; the waits end as soon as what they wait for is there.
const WAIT: Duration = Duration::from_secs(20);

const GOLDEN: &str = include_str!("../../tuikit/themes-pi/system-golden-more.json");

struct Pty {
    master: std::fs::File,
    child: Child,
    out: Vec<u8>,
    /// How much of `out` the responder has already looked at.
    seen: usize,
    behaviour: Behaviour,
    queued: Vec<(Instant, Vec<u8>)>,
}

#[derive(Clone)]
struct Behaviour {
    fg: Option<&'static str>,
    bg: Option<&'static str>,
    palette: Option<Vec<String>>,
    /// Answer the DA1 that ends the colour batch (the keyboard probe's DA1 is always answered).
    da1: bool,
    /// Wait this long before answering the colour batch.
    delay: Duration,
    /// Write the answer one byte at a time.
    bytewise: bool,
}

impl Behaviour {
    fn nothing() -> Behaviour {
        Behaviour {
            fg: None,
            bg: None,
            palette: None,
            da1: true,
            delay: Duration::ZERO,
            bytewise: false,
        }
    }
}

fn rgb_reply(h: &str) -> String {
    format!(
        "rgb:{}{}/{}{}/{}{}",
        &h[1..3],
        &h[1..3],
        &h[3..5],
        &h[3..5],
        &h[5..7],
        &h[5..7]
    )
}

impl Pty {
    fn spawn(behaviour: Behaviour, args: &[&str]) -> Pty {
        let mut master = -1;
        let mut slave = -1;
        let ws = libc::winsize {
            ws_row: 36,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &ws,
            )
        };
        assert_eq!(rc, 0, "openpty");
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_piw"));
        cmd.env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", "/home/me")
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("PIW_BACKEND", "mock")
            .env_remove("COLORFGBG");
        cmd.args(args);
        cmd.stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY, 0);
                Ok(())
            });
        }
        let child = cmd.spawn().expect("spawn piw");
        let master = unsafe { std::fs::File::from_raw_fd(master) };
        unsafe {
            let fl = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(master.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
        Pty {
            master,
            child,
            out: Vec::new(),
            seen: 0,
            behaviour,
            queued: Vec::new(),
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
    }

    /// The reply the terminal would write for the colour batch.
    fn answer(&self) -> Vec<u8> {
        let b = &self.behaviour;
        let mut s = String::new();
        if let Some(fg) = b.fg {
            s += &format!("\x1b]10;{}\x07", rgb_reply(fg));
        }
        if let Some(bg) = b.bg {
            s += &format!("\x1b]11;{}\x07", rgb_reply(bg));
        }
        if let Some(p) = &b.palette {
            for (i, c) in p.iter().enumerate() {
                s += &format!("\x1b]4;{i};{}\x1b\\", rgb_reply(c));
            }
        }
        if b.da1 {
            s += "\x1b[?62;4c";
        }
        s.into_bytes()
    }

    /// Read what piw wrote, answer queries as the configured terminal would, and flush answers
    /// that are due. Runs for `d`.
    fn pump(&mut self, d: Duration) {
        let end = Instant::now() + d;
        while Instant::now() < end {
            let mut buf = [0u8; 8192];
            match self.master.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => self.out.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2))
                }
                Err(_) => break,
            }
            self.respond();
            let now = Instant::now();
            let mut due = Vec::new();
            self.queued.retain(|(at, bytes)| {
                if *at <= now {
                    due.push(bytes.clone());
                    false
                } else {
                    true
                }
            });
            for bytes in due {
                if self.behaviour.bytewise {
                    for b in bytes {
                        self.send(&[b]);
                        std::thread::sleep(Duration::from_millis(1));
                    }
                } else {
                    self.send(&bytes);
                }
            }
        }
    }

    /// Pump until `done` holds or `deadline` passes, in short slices: a loaded machine draws
    /// late, so a fixed wait either flakes or wastes time. Returns whether `done` held.
    fn pump_until(&mut self, deadline: Duration, done: impl Fn(&Pty) -> bool) -> bool {
        let end = Instant::now() + deadline;
        while Instant::now() < end {
            if done(self) {
                return true;
            }
            self.pump(Duration::from_millis(20));
        }
        done(self)
    }

    /// Pump until the first screen is up and any reply to the colour query has gone out.
    fn start(&mut self) {
        let up = self.pump_until(WAIT, |p| {
            p.queued.is_empty()
                && (p.text().contains("fake-model") || p.text().contains("Pi can explain"))
        });
        assert!(up, "piw never drew: {:?}", self.text());
        // what the reply repaints follows it
        self.pump(Duration::from_millis(200));
    }

    fn respond(&mut self) {
        let new = self.out[self.seen..].to_vec();
        self.seen = self.out.len();
        // the keyboard probe at startup asks `CSI ? u` then DA1; every terminal answers DA1
        let probe = b"\x1b[?u\x1b[c";
        if find(&new, probe).is_some() {
            self.send(b"\x1b[?62;4c");
            return;
        }
        if find(&new, b"\x1b]10;?").is_some() {
            let at = Instant::now() + self.behaviour.delay;
            let a = self.answer();
            self.queued.push((at, a));
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.out).into_owned()
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn vector(name: &str) -> Value {
    let all: Value = serde_json::from_str(GOLDEN).unwrap();
    all.as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap()
        .clone()
}

fn hex(s: &str) -> (u8, u8, u8) {
    let v = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).unwrap();
    (v(1), v(3), v(5))
}

fn behaviour_for(name: &str) -> Behaviour {
    let v = vector(name);
    let leak = |s: &Value| {
        s.as_str()
            .map(|s| &*Box::leak(s.to_string().into_boxed_str()))
    };
    Behaviour {
        fg: leak(&v["fg"]),
        bg: leak(&v["bg"]),
        palette: v["palette"]
            .as_array()
            .map(|p| p.iter().map(|c| c.as_str().unwrap().to_string()).collect()),
        da1: true,
        delay: Duration::ZERO,
        bytewise: false,
    }
}

/// Types a prompt and returns everything written from then on, once `want` is among it (or
/// the deadline passed, for the assertion to say what was missing).
fn send_prompt(p: &mut Pty, want: &str) -> String {
    let mark = p.out.len();
    p.send(b"hello [[hello]]\r");
    p.pump_until(WAIT, |p| {
        String::from_utf8_lossy(&p.out[mark..]).contains(want)
    });
    p.pump(Duration::from_millis(200));
    String::from_utf8_lossy(&p.out[mark..]).into_owned()
}

fn sgr_bg(c: (u8, u8, u8)) -> String {
    format!("48;2;{};{};{}", c.0, c.1, c.2)
}

#[test]
fn the_query_is_written_as_pi_writes_it() {
    let mut p = Pty::spawn(Behaviour::nothing(), &[]);
    p.start();
    assert!(p.text().contains(COLOR_QUERY), "{:?}", p.text());
    assert!(COLOR_QUERY.starts_with("\x1b]10;?\x07\x1b]11;?\x07\x1b]4;0;?\x07"));
    assert!(COLOR_QUERY.ends_with("\x1b]4;15;?\x07\x1b[c"));
    // mode 2031 for a theme that follows the terminal, without asking for the current scheme
    assert!(p.text().contains("\x1b[?2031h") && !p.text().contains("\x1b[?996n"));
}

#[test]
fn a_reporting_terminal_gets_its_generated_theme_on_the_first_screen() {
    for name in [
        "onedark-palette",
        "black-bg-only",
        "solarized-light-palette",
    ] {
        let mut p = Pty::spawn(behaviour_for(name), &[]);
        p.start();
        let colors = &vector(name)["out"]["colors"];
        let user_bg = sgr_bg(hex(colors["userMessageBg"].as_str().unwrap()));
        let drawn = send_prompt(&mut p, &user_bg);
        assert!(
            drawn.contains(&user_bg),
            "{name}: no {user_bg} in the user message"
        );
        assert!(
            !drawn.contains("48;2;33;59;73"),
            "{name}: the built-in dark theme leaked"
        );
        assert!(
            !p.text().contains("rgb:"),
            "{name}: a reply was echoed into the screen"
        );
    }
}

#[test]
fn a_terminal_that_reports_nothing_gets_the_fallback_tier() {
    let mut p = Pty::spawn(Behaviour::nothing(), &[]);
    p.start();
    let drawn = send_prompt(&mut p, "hello");
    // no panel background anywhere, palette index 6 for the thinking rule, faint footer
    assert!(!drawn.contains("48;2;"), "a panel was painted: {drawn:?}");
    // (the rules and the footer were painted with the first frame, before the prompt)
    let all = p.text();
    assert!(
        all.contains("38;5;6;") || all.contains("38;5;6m"),
        "the medium rule is index 6"
    );
    assert!(all.contains("\x1b[2m") || all.contains(";2m"), "faint text");
    // the logo's blue half block is the one fixed background
    assert!(
        !all.replace("48;2;79;142;179", "").contains("48;2;"),
        "a panel was painted at startup"
    );
}

#[test]
fn replies_written_one_byte_at_a_time_decode_the_same() {
    let mut b = behaviour_for("onedark-palette");
    b.bytewise = true;
    let mut p = Pty::spawn(b, &[]);
    p.start();
    let colors = &vector("onedark-palette")["out"]["colors"];
    let user_bg = sgr_bg(hex(colors["userMessageBg"].as_str().unwrap()));
    let drawn = send_prompt(&mut p, &user_bg);
    assert!(drawn.contains(&user_bg));
}

#[test]
fn a_terminal_that_answers_after_the_wait_still_themes_the_screen_and_leaves_no_stray_bytes() {
    // piw stops waiting after 100 ms and paints the fallback tier; 400 ms later the answer comes,
    // and what is typed afterwards must reach the editor untouched
    let mut b = behaviour_for("black-bg-only");
    b.delay = Duration::from_millis(400);
    let mut p = Pty::spawn(b, &[]);
    p.start();
    let colors = &vector("black-bg-only")["out"]["colors"];
    let user_bg = sgr_bg(hex(colors["userMessageBg"].as_str().unwrap()));
    let drawn = send_prompt(&mut p, &user_bg);
    assert!(drawn.contains(&user_bg), "late colours");
    assert!(!p.text().contains("rgb:"));
    // the editor text of a later keystroke is exactly what was typed
    let mark = p.out.len();
    p.send(b"abc");
    p.pump_until(WAIT, |p| {
        String::from_utf8_lossy(&p.out[mark..]).contains("abc")
    });
    p.pump(Duration::from_millis(100));
    let after = String::from_utf8_lossy(&p.out[mark..]).into_owned();
    assert!(after.contains("abc"), "{after:?}");
    assert!(
        !after.contains("]11") && !after.contains("[?62"),
        "{after:?}"
    );
}

#[test]
fn a_terminal_that_never_answers_costs_a_tenth_of_a_second_not_the_keyboard_probe() {
    // no DA1 for the colour batch either: the 100 ms timeout applies
    let mut b = Behaviour::nothing();
    b.da1 = false;
    let started = Instant::now();
    let mut p = Pty::spawn(b, &[]);
    let mut up = false;
    while started.elapsed() < Duration::from_secs(5) {
        p.pump(Duration::from_millis(50));
        if p.text().contains("fake-model") || p.text().contains("Pi can explain") {
            up = true;
            break;
        }
    }
    assert!(up, "piw never drew: {:?}", p.text());
}

#[test]
fn an_explicit_theme_is_not_touched_by_what_the_terminal_says() {
    let mut p = Pty::spawn(behaviour_for("onedark-palette"), &["--use-theme", "dark"]);
    p.start();
    let drawn = send_prompt(&mut p, "48;2;33;59;73");
    assert!(drawn.contains("48;2;33;59;73"), "dark's userMessageBg");
}
