//! Test harness: an `App` with no backend process, fed events by hand, rendered through
//! `tuikit::testing::TestTerminal`, and a tiny snapshot comparer.

#![allow(dead_code)]

use std::path::PathBuf;
use std::time::Duration;

use agent_core::mock;
use agent_core::{Event, Request, SessionInfo, StopReason, ToolCall, ToolKind, ToolStatus, Usage};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use openw::app::{App, AppOpts, Msg};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tuikit::testing::TestTerminal;

pub struct Harness {
    pub app: App,
    pub requests: UnboundedReceiver<Request>,
    _msgs: UnboundedReceiver<Msg>,
}

impl Harness {
    /// Home screen at `w` x `h` with the mock backend's config already delivered. The tip and
    /// the placeholder example are pinned so snapshots do not move.
    pub fn new(w: u16, h: u16) -> Self {
        Self::with(
            w,
            h,
            AppOpts {
                cwd: PathBuf::from("/home/me/proj"),
                mock: true,
                seed: 2,
                ..Default::default()
            },
        )
    }

    pub fn with(w: u16, h: u16, opts: AppOpts) -> Self {
        let (tx, requests) = unbounded_channel();
        let (mtx, msgs) = unbounded_channel();
        let mut app = App::new(opts, tx, mtx);
        app.size = (w, h);
        app.frozen = Some(Duration::ZERO);
        app.branch = Some("main".into());
        let mut h = Harness {
            app,
            requests,
            _msgs: msgs,
        };
        let mut cfg = mock::config();
        cfg.cwd = "/home/me/proj".into();
        h.event(Event::Ready {
            session_id: "ses_1234567890".into(),
            config: cfg,
        });
        h.event(Event::Commands(mock::commands()));
        h
    }

    pub fn event(&mut self, ev: Event) {
        self.app.on_event(ev);
    }

    pub fn key(&mut self, code: KeyCode, mods: KeyModifiers) {
        self.app.on_key(KeyEvent::new(code, mods));
    }

    pub fn type_str(&mut self, s: &str) {
        for c in s.chars() {
            self.key(KeyCode::Char(c), KeyModifiers::NONE);
        }
    }

    pub fn paste(&mut self, s: &str) {
        self.app.on_paste(s);
    }

    pub fn sessions(&mut self, n: usize) {
        let v = (0..n)
            .map(|i| SessionInfo {
                id: format!("s{i}"),
                title: format!("Session {i}"),
                cwd: "/home/me/proj".into(),
                updated: 1_790_000_000 - i as i64 * 1000,
            })
            .collect();
        self.event(Event::Sessions(v));
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
        t.draw(|buf, _| {
            openw::ui::draw(buf, &mut self.app);
        });
        t
    }

    pub fn text(&mut self) -> String {
        self.render().plain()
    }

    /// Write the current screen as ANSI to `$OPENW_DUMP_DIR/<name>.ansi` when that is set, so
    /// cell colors can be diffed against `reference/*.ansi` (see tools/cmp-ansi.py).
    pub fn dump(&mut self, name: &str) {
        if let Some(dir) = std::env::var_os("OPENW_DUMP_DIR") {
            let t = self.render();
            let _ = std::fs::create_dir_all(&dir);
            std::fs::write(
                std::path::Path::new(&dir).join(format!("{name}.ansi")),
                t.ansi(),
            )
            .expect("dump");
        }
    }

    /// A finished turn: user prompt, some assistant text, and a fixed duration.
    pub fn turn(&mut self, prompt: &str, answer: &str) {
        self.app.send_prompt(prompt.to_string());
        self.event(Event::TurnStart);
        self.event(Event::TextDelta(answer.to_string()));
        self.event(Event::Usage(Usage {
            input_tokens: 11_200,
            output_tokens: 500,
            context_tokens: 11_700,
            context_window: 200_000,
            ..Default::default()
        }));
        self.event(Event::TurnEnd(StopReason::EndTurn));
        self.fix_durations();
    }

    /// Wall-clock durations are not reproducible; pin them.
    pub fn fix_durations(&mut self) {
        for m in &mut self.app.transcript.messages {
            if m.took.is_some() {
                m.took = Some(Duration::from_millis(6500));
            }
            for p in &mut m.parts {
                if let agent_core::transcript::Part::Thought { took, .. } = p {
                    if took.is_some() {
                        *took = Some(Duration::from_millis(237));
                    }
                }
            }
        }
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

/// Compare `actual` with `tests/snapshots/<name>.txt`. `UPDATE_SNAPSHOTS=1` rewrites it, and is
/// the only way a snapshot gets created: a missing file used to be written and pass, so deleting
/// a `.txt` turned its test green.
pub fn assert_snapshot(name: &str, actual: &str) {
    let path = snap_path(name);
    let actual: String = actual
        .lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&path, &actual).expect("write snapshot");
        return;
    }
    assert!(
        path.exists(),
        "snapshot {name} is missing; run with UPDATE_SNAPSHOTS=1 to create it"
    );
    let want = std::fs::read_to_string(&path).expect("read snapshot");
    if want != actual {
        let mut msg = format!("snapshot {name} differs (UPDATE_SNAPSHOTS=1 to accept)\n");
        let (w, a): (Vec<&str>, Vec<&str>) = (want.lines().collect(), actual.lines().collect());
        for i in 0..w.len().max(a.len()) {
            let (l, r) = (
                w.get(i).copied().unwrap_or("<none>"),
                a.get(i).copied().unwrap_or("<none>"),
            );
            if l != r {
                msg.push_str(&format!("{i:>3} want |{l}\n    got  |{r}\n"));
            }
        }
        panic!("{msg}");
    }
}
