//! Test harness: an `App` with no backend process, fed events by hand, rendered through
//! `tuikit::testing::TestTerminal`, and a tiny snapshot comparer.

#![allow(dead_code)]

use std::path::PathBuf;
use std::time::Duration;

use agent_core::{Event, Request, StopReason, ToolCall, ToolKind, ToolStatus, Usage};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use piw::app::{App, AppOpts, Msg};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tuikit::testing::TestTerminal;

pub struct Harness {
    pub app: App,
    pub requests: UnboundedReceiver<Request>,
    pub msgs: UnboundedReceiver<Msg>,
}

impl Harness {
    /// A started app at `w` x `h` with the mock backend's config and commands already delivered,
    /// the clock frozen so spinners and elapsed times do not move.
    pub fn new(w: u16, h: u16) -> Self {
        Self::with_theme(w, h, Some("dark"))
    }

    /// Like [`new`](Self::new) with the theme setting chosen: the colour tests are written against
    /// `dark`, and `None` is Pi's own default, the `system` theme.
    pub fn with_theme(w: u16, h: u16, theme: Option<&str>) -> Self {
        std::env::set_var("HOME", "/home/me");
        let (tx, requests) = unbounded_channel();
        let (mtx, msgs) = unbounded_channel();
        let opts = AppOpts {
            cwd: PathBuf::from("/home/me/proj"),
            mock: true,
            theme: theme.map(String::from),
            ..Default::default()
        };
        let mut app = App::new(opts, tx, mtx);
        app.size = (w, h);
        app.frozen = Some(Duration::ZERO);
        app.branch = Some("main".into());
        let mut hn = Harness {
            app,
            requests,
            msgs,
        };
        let mut cfg = piw::mock::config();
        cfg.cwd = "/home/me/proj".into();
        hn.event(Event::Ready {
            session_id: "ses_1234567890".into(),
            config: cfg,
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

    pub fn type_str(&mut self, s: &str) {
        for c in s.chars() {
            self.key(KeyCode::Char(c), KeyModifiers::NONE);
        }
    }

    pub fn paste(&mut self, s: &str) {
        self.app.on_paste(s);
    }

    /// Apply the messages tasks have sent the app (file lists, shell output) and return how many.
    pub fn deliver(&mut self) -> usize {
        let mut n = 0;
        while let Ok(m) = self.msgs.try_recv() {
            self.app.update(m);
            n += 1;
        }
        n
    }

    /// Wait for the running `!cmd` to end and apply what it sent (at most 10 s).
    pub async fn settle_bash(&mut self) {
        for _ in 0..500 {
            self.deliver();
            if !self.app.bashes.last().is_some_and(|b| b.running) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the shell command did not end");
    }

    /// Requests the app sent so far.
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
        t.draw(|buf, _| piw::ui::draw(buf, &mut self.app));
        t
    }

    pub fn text(&mut self) -> String {
        self.render().plain()
    }

    /// Write the current screen as ANSI to `$PIW_DUMP_DIR/<name>.ansi` when that is set, so cell
    /// colours can be diffed against `reference/pi/*.ansi` (see tools/cmp-ansi.py).
    pub fn dump(&mut self, name: &str) {
        if let Some(dir) = std::env::var_os("PIW_DUMP_DIR") {
            let t = self.render();
            let _ = std::fs::create_dir_all(&dir);
            std::fs::write(
                std::path::Path::new(&dir).join(format!("{name}.ansi")),
                t.ansi(),
            )
            .expect("dump");
        }
    }

    /// A finished turn: the user prompt goes out as the app would send it, then the answer.
    pub fn turn(&mut self, prompt: &str, answer: &str) {
        self.app.send_prompt(prompt.to_string());
        self.event(Event::TurnStart);
        self.event(Event::TextDelta(answer.to_string()));
        self.event(Event::Usage(Usage {
            input_tokens: 1_200,
            output_tokens: 500,
            context_tokens: 1_700,
            context_window: 200_000,
            ..Default::default()
        }));
        self.event(Event::TurnEnd(StopReason::EndTurn));
    }
}

pub fn tool(
    id: &str,
    name: &str,
    kind: ToolKind,
    title: &str,
    input: serde_json::Value,
    output: Option<&str>,
) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        kind,
        title: title.into(),
        input,
        status: ToolStatus::Completed,
        output: output.map(Into::into),
        diff: None,
        parent_id: None,
    }
}

fn snap_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(format!("{name}.txt"))
}

/// Compare `actual` with `tests/snapshots/<name>.txt`. `UPDATE_SNAPSHOTS=1` rewrites it.
pub fn assert_snapshot(name: &str, actual: &str) {
    let path = snap_path(name);
    let actual: String = actual
        .lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() || !path.exists() {
        std::fs::write(&path, &actual).expect("write snapshot");
        return;
    }
    let want = std::fs::read_to_string(&path).expect("read snapshot");
    if want != actual {
        let a: Vec<&str> = want.lines().collect();
        let b: Vec<&str> = actual.lines().collect();
        for i in 0..a.len().max(b.len()) {
            if a.get(i) != b.get(i) {
                panic!(
                    "snapshot {name} differs at row {i}:\n  want {:?}\n  got  {:?}\n(UPDATE_SNAPSHOTS=1 rewrites it)",
                    a.get(i),
                    b.get(i)
                );
            }
        }
    }
}
