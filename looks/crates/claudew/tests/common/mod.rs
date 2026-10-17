//! Test harness: an `App` with no backend process, fed the events of the replay scripts in
//! `tests/fixtures/replay`, driven by keys and rendered through `TestTerminal`; and a reader for
//! the reference captures in `tests/fixtures/ref` (`tmux capture-pane -e` output of the real
//! Claude Code) that compares them with a render cell by cell.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_core::{Event, Request};
use claudew::app::{App, AppOpts, Msg};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use regex::Regex;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tuikit::testing::TestTerminal;

/// The directory the reference captures were taken in (with the user name scrubbed).
pub const CWD: &str =
    "/tmp/claude-1000/-home-owner/00000000-0000-0000-0000-000000000000/scratchpad/look-claude/proj";

pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// One step of a replay script.
#[allow(clippy::large_enum_variant)]
pub enum Step {
    Event(Event),
    Await(String),
}

pub fn replay(name: &str) -> Vec<Step> {
    let text = std::fs::read_to_string(fixtures().join("replay").join(name)).expect("replay");
    let mut out = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line).expect("json");
        if let Some(e) = v.get("event") {
            out.push(Step::Event(
                serde_json::from_value(e.clone()).expect("event"),
            ));
        } else if let Some(w) = v.get("await") {
            out.push(Step::Await(w.as_str().unwrap_or("").to_string()));
        }
    }
    out
}

pub struct Harness {
    pub app: App,
    pub requests: UnboundedReceiver<Request>,
    _msgs: UnboundedReceiver<Msg>,
    steps: Vec<Step>,
    at: usize,
}

impl Harness {
    /// A started app at `w` x `h` with the head of `script` (config, commands) delivered.
    pub fn new(script: &str, w: u16, h: u16) -> Harness {
        std::env::set_var("HOME", "/home/someone");
        // the references were captured with EDITOR=nvim
        std::env::remove_var("VISUAL");
        std::env::set_var("EDITOR", "nvim");
        let (tx, requests) = unbounded_channel();
        let (mtx, msgs) = unbounded_channel();
        let opts = AppOpts {
            cwd: PathBuf::from(CWD),
            ..Default::default()
        };
        let mut app = App::new(opts, tx, mtx);
        app.size = (w, h);
        app.clock.freeze(Duration::from_millis(1_000));
        app.fixed_hm = Some((21, 41));
        app.now_unix = 1_792_000_000;
        app.files = ["main.rs", "notes.md", "src/", "src/lib.rs"]
            .map(String::from)
            .to_vec();
        let mut hn = Harness {
            app,
            requests,
            _msgs: msgs,
            steps: replay(script),
            at: 0,
        };
        hn.play();
        hn
    }

    /// Deliver events up to the next `await` and step past it.
    pub fn play(&mut self) {
        while self.at < self.steps.len() {
            let i = self.at;
            self.at += 1;
            match &self.steps[i] {
                Step::Event(e) => {
                    let e = e.clone();
                    self.app.on_event(e);
                }
                Step::Await(_) => return,
            }
        }
    }

    pub fn event(&mut self, ev: Event) {
        self.app.on_event(ev);
    }

    pub fn key(&mut self, code: KeyCode, mods: KeyModifiers) {
        self.app.on_key(KeyEvent::new(code, mods));
    }

    pub fn press(&mut self, code: KeyCode) {
        self.key(code, KeyModifiers::NONE);
    }

    pub fn ctrl(&mut self, c: char) {
        self.key(KeyCode::Char(c), KeyModifiers::CONTROL);
    }

    pub fn type_str(&mut self, s: &str) {
        for c in s.chars() {
            self.press(KeyCode::Char(c));
        }
    }

    pub fn advance(&mut self, ms: u64) {
        let now = self.app.clock.elapsed();
        self.app.clock.freeze(now + Duration::from_millis(ms));
        self.app.on_tick();
    }

    pub fn sent(&mut self) -> Vec<Request> {
        let mut v = Vec::new();
        while let Ok(r) = self.requests.try_recv() {
            v.push(r);
        }
        v
    }

    pub fn render(&mut self) -> TestTerminal {
        let (w, h) = self.app.size;
        let mut t = TestTerminal::new(w, h);
        t.draw(|buf, _| claudew::ui::draw(buf, &mut self.app));
        t
    }
}

/// A reference capture painted into a buffer of its size.
pub fn reference(name: &str) -> Buffer {
    let size = name.rsplit('-').next().unwrap();
    let (w, h) = size.split_once('x').unwrap();
    let (w, h): (u16, u16) = (w.parse().unwrap(), h.parse().unwrap());
    let text = std::fs::read_to_string(fixtures().join("ref").join(format!("{name}.ansi")))
        .unwrap_or_else(|e| panic!("{name}: {e}"));
    let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
    let mut style = Style::new();
    for (y, line) in text.split('\n').enumerate().take(h as usize) {
        let mut x = 0u16;
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
                chars.next();
                let mut params = String::new();
                for p in chars.by_ref() {
                    if p.is_ascii_alphabetic() {
                        if p == 'm' {
                            style = sgr(style, &params);
                        }
                        break;
                    }
                    params.push(p);
                }
                continue;
            }
            // an OSC sequence (a hyperlink) carries no cells: skip to its terminator
            if c == '\x1b' && chars.peek() == Some(&']') {
                while let Some(p) = chars.next() {
                    if p == '\x07' || (p == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
                continue;
            }
            let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) as u16;
            if w == 0 || x >= buf.area.width {
                continue;
            }
            if let Some(cell) = buf.cell_mut((x, y as u16)) {
                cell.set_symbol(&c.to_string());
                cell.set_style(style);
            }
            x += w;
        }
    }
    buf
}

fn sgr(mut st: Style, params: &str) -> Style {
    let p: Vec<u16> = if params.is_empty() {
        vec![0]
    } else {
        params
            .split([';', ':'])
            .map(|x| x.parse().unwrap_or(0))
            .collect()
    };
    let mut i = 0;
    while i < p.len() {
        match p[i] {
            0 => st = Style::new(),
            1 => st = st.add_modifier(Modifier::BOLD),
            2 => st = st.add_modifier(Modifier::DIM),
            3 => st = st.add_modifier(Modifier::ITALIC),
            4 => st = st.add_modifier(Modifier::UNDERLINED),
            7 => st = st.add_modifier(Modifier::REVERSED),
            9 => st = st.add_modifier(Modifier::CROSSED_OUT),
            22 => st = st.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => st = st.remove_modifier(Modifier::ITALIC),
            24 => st = st.remove_modifier(Modifier::UNDERLINED),
            27 => st = st.remove_modifier(Modifier::REVERSED),
            29 => st = st.remove_modifier(Modifier::CROSSED_OUT),
            n @ 30..=37 => st = st.fg(Color::Indexed((n - 30) as u8)),
            n @ 90..=97 => st = st.fg(Color::Indexed((n - 82) as u8)),
            39 => st = st.fg(Color::Reset),
            n @ 40..=47 => st = st.bg(Color::Indexed((n - 40) as u8)),
            49 => st = st.bg(Color::Reset),
            38 | 48 if p.get(i + 1) == Some(&5) => {
                let c = Color::Indexed(*p.get(i + 2).unwrap_or(&0) as u8);
                st = if p[i] == 38 { st.fg(c) } else { st.bg(c) };
                i += 2;
            }
            _ => {}
        }
        i += 1;
    }
    st
}

fn row_text(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width)
        .map(|x| buf[(x, y)].symbol().to_string())
        .map(|s| {
            if s.is_empty() {
                "\u{200b}".to_string()
            } else {
                s
            }
        })
        .collect()
}

/// What a cell shows: a blank shows only its background and the flags that draw on blanks.
fn shown(buf: &Buffer, x: u16, y: u16) -> (String, Color, Color, Modifier) {
    let c = &buf[(x, y)];
    let fg = c.fg;
    let bg = c.bg;
    let m = c.modifier;
    if c.symbol() == " " {
        let keep = m & (Modifier::REVERSED | Modifier::UNDERLINED | Modifier::CROSSED_OUT);
        return (" ".into(), Color::Reset, bg, keep);
    }
    (c.symbol().to_string(), fg, bg, m)
}

/// Cells where `look` differs from `reference`, leaving out the spans the patterns match in
/// either buffer's row text. Each mismatch is described as `row:col ref=.. look=..`.
pub fn mismatches(look: &Buffer, reference: &Buffer, masks: &[&str]) -> Vec<String> {
    let rx: Vec<Regex> = masks.iter().map(|m| Regex::new(m).unwrap()).collect();
    let (w, h) = (reference.area.width, reference.area.height);
    let mut out = Vec::new();
    for y in 0..h {
        let mut masked = vec![false; w as usize];
        for buf in [look, reference] {
            let text = row_text(buf, y);
            let cols: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
            let col_of = |byte: usize| cols.iter().position(|&b| b >= byte).unwrap_or(cols.len());
            for r in &rx {
                for m in r.find_iter(&text) {
                    let (a, b) = (
                        col_of(m.start()),
                        col_of(m.end()).max(col_of(m.start()) + 1),
                    );
                    for slot in masked.iter_mut().take(b.min(w as usize)).skip(a) {
                        *slot = true;
                    }
                }
            }
        }
        for x in 0..w {
            if !masked[x as usize] && shown(look, x, y) != shown(reference, x, y) {
                out.push(format!(
                    "{y}:{x} ref={:?} look={:?}\n  ref  |{}\n  look |{}",
                    shown(reference, x, y),
                    shown(look, x, y),
                    row_text(reference, y).trim_end(),
                    row_text(look, y).trim_end()
                ));
            }
        }
    }
    out
}

/// Spans that differ for a stated reason in every scene: the brand mark, the model line, the
/// transient effort notice and the random placeholder.
pub const COMMON: &[&str] = &[
    r"^[ ▐▛█▜▝▀▌▘▟▙]{9}",
    r"(?:Claude Code|Wizard) v[0-9.]+",
    r"^.{11}\S.* · (?:Claude Max|Fastest for quick answers)\s*$",
    r"[○◐●◉] \w+ · /effort",
    r"Try .*$",
    r" · ← \d+ agents?",
    r"You've used \d+% of your weekly limit.*$",
    r"^✻ \S+ for \d+s · done \d+:\d\d [AP]M",
    r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
];

/// The wizard version the replay scripts report: the one a recorded real wizard gave.
pub fn replay_version() -> String {
    std::fs::read_to_string(fixtures().join("replay/version.txt"))
        .expect("tests/fixtures/replay/version.txt")
        .trim()
        .to_string()
}

pub fn assert_matches(hn: &mut Harness, name: &str, extra: &[&str]) {
    let reference = reference(name);
    let t = hn.render();
    let header = t.row(1);
    if header.contains("Wizard v") {
        let want = format!("Wizard v{}", replay_version());
        assert!(
            header.trim_end().ends_with(&want),
            "{name}: header {header:?}"
        );
    }
    let masks: Vec<&str> = COMMON.iter().chain(extra).copied().collect();
    let bad = mismatches(t.buffer(), &reference, &masks);
    assert!(
        bad.is_empty(),
        "{name}: {} cells differ from the reference; first:\n{}",
        bad.len(),
        bad.iter().take(3).cloned().collect::<Vec<_>>().join("\n")
    );
}
