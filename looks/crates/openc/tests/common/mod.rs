//! Test harness: an `App` on a fake clock drawn into a ratatui `TestBackend`, fed by the
//! scripted mock backend, plus a tiny snapshot helper (no snapshot crate).

#![allow(dead_code)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use agent_core::mock::MockBackend;
use agent_core::{Backend, BackendHandle, Event, Request};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use openc::app::{App, Msg, Opts, ThemeChoice};
use openc::palette::{Depth, UNICODE};
use openc::term::Caps;
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

static STATE: std::sync::Once = std::sync::Once::new();
static SESSION: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Keep prompt history and drafts out of the real home directory.
fn isolate_state() {
    STATE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("openc-test-state-{}", std::process::id()));
        openc::store::use_dir(dir);
    });
}

pub struct Harness {
    pub app: App,
    pub reqs: UnboundedReceiver<Request>,
    pub app_rx: UnboundedReceiver<Msg>,
    pub base: Instant,
    /// Milliseconds on the fake clock.
    pub t: u64,
    pub term: Terminal<TestBackend>,
}

impl Harness {
    pub fn new(w: u16, h: u16) -> Harness {
        Self::with_theme(w, h, ThemeChoice::Hearth, Depth::True)
    }

    pub fn with_theme(w: u16, h: u16, theme: ThemeChoice, depth: Depth) -> Harness {
        Self::with_glyphs(w, h, theme, depth, UNICODE)
    }

    pub fn with_glyphs(
        w: u16,
        h: u16,
        theme: ThemeChoice,
        depth: Depth,
        glyphs: openc::palette::Glyphs,
    ) -> Harness {
        isolate_state();
        openc::clipboard::capture();
        let (tx, reqs) = unbounded_channel();
        let (app_tx, app_rx) = unbounded_channel();
        let opts = Opts {
            cwd: PathBuf::from("/work/proj"),
            theme,
            prompt: None,
            mouse: true,
            spinner: true,
        };
        let mut app = App::new(opts, tx, app_tx, Caps::default(), depth, glyphs, (w, h));
        let base = Instant::now();
        app.start = base;
        let term = Terminal::new(TestBackend::new(w, h)).expect("test terminal");
        Harness {
            app,
            reqs,
            app_rx,
            base,
            t: 0,
            term,
        }
    }

    pub fn now(&self) -> Instant {
        self.base + Duration::from_millis(self.t)
    }

    pub fn advance(&mut self, ms: u64) {
        self.t += ms;
    }

    /// Feed one backend event, `dt` ms after the previous one.
    pub fn event(&mut self, ev: Event, dt: u64) {
        self.t += dt;
        let now = self.now();
        self.app.on_backend(ev, now);
    }

    pub fn events(&mut self, evs: Vec<Event>, dt: u64) {
        for e in evs {
            self.event(e, dt);
        }
    }

    pub fn key(&mut self, code: KeyCode) {
        self.key_mod(code, KeyModifiers::NONE);
    }

    pub fn key_mod(&mut self, code: KeyCode, m: KeyModifiers) {
        self.t += 40;
        let now = self.now();
        self.app.on_key(KeyEvent::new(code, m), now);
    }

    pub fn ctrl(&mut self, c: char) {
        self.key_mod(KeyCode::Char(c), KeyModifiers::CONTROL);
    }

    pub fn type_str(&mut self, s: &str) {
        for c in s.chars() {
            self.key(KeyCode::Char(c));
        }
    }

    pub fn resize(&mut self, w: u16, h: u16) {
        self.term = Terminal::new(TestBackend::new(w, h)).expect("test terminal");
        self.app.size = (w, h);
    }

    pub fn draw(&mut self) {
        let now = self.now();
        let app = &mut self.app;
        self.term
            .draw(|f| openc::ui::draw(app, f, now))
            .expect("draw");
    }

    /// Draw and return the screen as text, trailing spaces trimmed per row.
    pub fn screen(&mut self) -> String {
        self.draw();
        tuikit::testing::plain_text(self.term.backend().buffer())
    }

    pub fn ansi(&mut self) -> String {
        self.draw();
        tuikit::testing::ansi_dump(self.term.backend().buffer())
    }

    /// Requests the app has sent so far.
    pub fn sent(&mut self) -> Vec<Request> {
        let mut v = Vec::new();
        while let Ok(r) = self.reqs.try_recv() {
            v.push(r);
        }
        v
    }

    /// Seed the app as if the backend had said hello.
    pub fn ready(&mut self) {
        let cfg = agent_core::Config {
            model: "sonnet".into(),
            models: vec![
                agent_core::ModelOption {
                    id: "sonnet".into(),
                    name: "Sonnet 5.5".into(),
                    provider: "anthropic".into(),
                },
                agent_core::ModelOption {
                    id: "opus".into(),
                    name: "Opus 5.5".into(),
                    provider: "anthropic".into(),
                },
                agent_core::ModelOption {
                    id: "haiku".into(),
                    name: "Haiku 4.5".into(),
                    provider: "anthropic".into(),
                },
            ],
            effort: "high".into(),
            efforts: ["low", "medium", "high"].map(String::from).to_vec(),
            mode: "default".into(),
            modes: ["default", "acceptEdits", "plan", "bypassPermissions"]
                .map(String::from)
                .to_vec(),
            cwd: "/work/proj".into(),
            backend: "claude 2.1.289".into(),
        };
        self.event(
            Event::Ready {
                session_id: format!(
                    "sess-{}",
                    SESSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ),
                config: cfg,
            },
            10,
        );
        self.app.git = openc::app::GitInfo {
            branch: "main".into(),
            dirty: true,
        };
        self.app.refresh_welcome();
        let _ = self.sent();
    }
}

/// A live mock backend. `run` sends a prompt and returns the events up to `TurnEnd` or a
/// permission request, whichever comes first.
pub struct Mock {
    pub tx: UnboundedSender<Request>,
    rx: UnboundedReceiver<Event>,
}

impl Mock {
    pub fn start() -> Mock {
        let BackendHandle { tx, rx } =
            MockBackend::spawn(PathBuf::from("/work/proj"), None).expect("mock");
        Mock { tx, rx }
    }

    pub async fn collect(&mut self, stop_on: impl Fn(&Event) -> bool) -> Vec<Event> {
        let mut out = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(15), self.rx.recv()).await {
                Ok(Some(ev)) => {
                    let stop = stop_on(&ev);
                    out.push(ev);
                    if stop {
                        return out;
                    }
                }
                _ => return out,
            }
        }
    }

    pub async fn run(&mut self, prompt: &str) -> Vec<Event> {
        self.tx.send(Request::Prompt(prompt.into())).expect("send");
        self.collect(|e| matches!(e, Event::TurnEnd(_) | Event::Permission(_)))
            .await
    }

    /// Everything the mock says in the first `ms` milliseconds.
    pub async fn run_for(&mut self, prompt: &str, ms: u64) -> Vec<Event> {
        self.tx.send(Request::Prompt(prompt.into())).expect("send");
        let mut out = Vec::new();
        let end = tokio::time::Instant::now() + Duration::from_millis(ms);
        while let Ok(Some(ev)) = tokio::time::timeout_at(end, self.rx.recv()).await {
            out.push(ev);
        }
        out
    }
}

/// Startup events the mock sends before any prompt.
pub async fn hello(m: &mut Mock) -> Vec<Event> {
    m.collect(|e| matches!(e, Event::Commands(_))).await
}

fn snap_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(format!("{name}.txt"))
}

/// Compare `actual` with `tests/snapshots/<name>.txt`. `UPDATE_SNAPSHOTS=1` rewrites it.
pub fn assert_snapshot(name: &str, actual: &str) {
    let path = snap_path(name);
    let actual = format!("{}\n", actual.trim_end());
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::create_dir_all(path.parent().expect("dir")).expect("mkdir");
        std::fs::write(&path, &actual).expect("write snapshot");
        return;
    }
    match std::fs::read_to_string(&path) {
        Ok(want) if want == actual => {}
        Ok(want) => panic!("snapshot {name} differs\n--- want\n{want}\n--- got\n{actual}\nrun with UPDATE_SNAPSHOTS=1 to accept"),
        Err(_) => panic!("no snapshot for {name}; run with UPDATE_SNAPSHOTS=1 to create it\n{actual}"),
    }
}
