//! Test harness: an `App` with no backend process, fed events by hand, rendered through
//! `tuikit::testing::TestTerminal`. The clock is frozen and the config is the one the scripted
//! backend reports, which is what the reference captures were taken against.

#![allow(dead_code)]

use std::path::PathBuf;
use std::time::Duration;

pub mod cells;

use agent_core::{Event, Request, StopReason, Usage};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use jcodew::app::{App, AppOpts, Msg};
use jcodew::fake;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tuikit::testing::TestTerminal;

pub struct Harness {
    pub app: App,
    pub requests: UnboundedReceiver<Request>,
    _msgs: UnboundedReceiver<Msg>,
    turns: u64,
}

/// The directory the reference captures were taken in.
pub const CWD: &str = "/tmp/jcodew-cmp/proj";

impl Harness {
    pub fn new(w: u16, h: u16) -> Self {
        std::env::set_var("HOME", "/home/me");
        jcodew::theme::set_truecolor(true);
        let (tx, requests) = unbounded_channel();
        let (mtx, msgs) = unbounded_channel();
        let opts = AppOpts {
            cwd: PathBuf::from(CWD),
            mock: true,
            ..Default::default()
        };
        let mut app = App::new(opts, tx, mtx);
        app.size = (w, h);
        app.clock.freeze(Duration::from_millis(5_000));
        let mut hn = Harness {
            app,
            requests,
            _msgs: msgs,
            turns: 0,
        };
        hn.event(Event::Ready {
            session_id: "fake-session-1".into(),
            config: fake::config(std::path::Path::new(CWD)),
        });
        hn.event(Event::Commands(agent_core::mock::commands()));
        hn.sent();
        hn
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

    pub fn alt(&mut self, c: char) {
        self.key(KeyCode::Char(c), KeyModifiers::ALT);
    }

    pub fn type_str(&mut self, s: &str) {
        for c in s.chars() {
            self.key(KeyCode::Char(c), KeyModifiers::NONE);
        }
    }

    /// Type a prompt and press Enter.
    pub fn say(&mut self, s: &str) {
        self.type_str(s);
        self.press(KeyCode::Enter);
    }

    /// Requests the app sent so far.
    pub fn sent(&mut self) -> Vec<Request> {
        let mut v = Vec::new();
        while let Ok(r) = self.requests.try_recv() {
            v.push(r);
        }
        v
    }

    pub fn advance(&mut self, ms: u64) {
        let now = self.app.clock.elapsed();
        self.app.clock.freeze(now + Duration::from_millis(ms));
    }

    /// Send `prompt` and play the scripted turn for it to the end, as the backend would.
    pub fn turn(&mut self, prompt: &str) {
        self.say(prompt);
        self.begin();
        self.finish(prompt);
    }

    /// The backend opens the turn.
    pub fn begin(&mut self) {
        self.event(Event::TurnStart);
    }

    /// Everything the turn does, then its end, 5.6 seconds after it began.
    pub fn finish(&mut self, prompt: &str) {
        for step in fake::steps(std::path::Path::new(CWD), prompt) {
            for ev in fake::events(&step) {
                self.event(ev);
            }
        }
        self.advance(5_600);
        self.turns += 1;
        self.event(Event::Usage(Usage {
            input_tokens: 1200 * self.turns,
            output_tokens: 34 * self.turns,
            context_tokens: 1200,
            context_window: 200_000,
            ..Default::default()
        }));
        self.event(Event::TurnEnd(StopReason::EndTurn));
    }

    pub fn render(&mut self) -> TestTerminal {
        let (w, h) = self.app.size;
        let mut t = TestTerminal::new(w, h);
        t.draw(|buf, _| jcodew::ui::draw(buf, &mut self.app));
        t
    }

    pub fn text(&mut self) -> String {
        self.render().plain()
    }

    /// The screen row that starts with `prefix`, trailing blanks trimmed.
    pub fn row_starting(&mut self, prefix: &str) -> Option<(u16, String)> {
        let t = self.render();
        (0..self.app.size.1).find_map(|y| {
            let row = t.row(y);
            row.starts_with(prefix)
                .then(|| (y, row.trim_end().to_string()))
        })
    }
}

/// Compare with `tests/snapshots/<name>.txt`; `UPDATE_SNAPSHOTS=1` writes it instead.
pub fn assert_snapshot(name: &str, text: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(format!("{name}.txt"));
    let text: String = text
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&path, &text).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("snapshot {name}: {e}; run with UPDATE_SNAPSHOTS=1"));
    assert_eq!(text, want, "snapshot {name} differs");
}
