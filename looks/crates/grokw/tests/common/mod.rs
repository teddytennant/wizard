//! Test harness: an `App` with no backend process, fed events by hand, rendered through
//! `tuikit::testing::TestTerminal`, and a tiny snapshot comparer. The clock is frozen, the wall
//! clock pinned (5:42 PM), and the config is the one the reference captures were taken with.

#![allow(dead_code)]

use std::path::PathBuf;
use std::time::Duration;

pub mod cells;

use agent_core::{
    Config, Event, ModelOption, Request, SessionInfo, StopReason, ToolCall, ToolKind, ToolStatus,
    Usage,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use grokw::app::{App, AppOpts, Msg, Screen};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tuikit::testing::TestTerminal;

pub struct Harness {
    pub app: App,
    pub requests: UnboundedReceiver<Request>,
    _msgs: UnboundedReceiver<Msg>,
}

pub const CWD: &str = "/tmp/grok-harvest-000000/proj";

pub fn config() -> Config {
    let m = |id: &str, name: &str| ModelOption {
        id: id.into(),
        name: name.into(),
        provider: "xai".into(),
    };
    Config {
        model: "xai/grok-4.7".into(),
        models: vec![
            m("xai/grok-4.7", "Grok 4.7"),
            m("xai/grok-4.7-fast", "Grok 4.7 Fast"),
            m("xai/grok-4.6", "Grok 4.6"),
            m("xai/grok-4.5", "Grok 4.5"),
        ],
        effort: "high".into(),
        efforts: ["xhigh", "high", "medium", "low"]
            .map(String::from)
            .to_vec(),
        mode: "genie".into(),
        modes: vec!["genie".into(), "sovereign".into()],
        cwd: CWD.into(),
        backend: "mock 0.0".into(),
    }
}

impl Harness {
    /// A started app at `w` x `h` with the config and commands delivered and the session screen
    /// showing (`home` starts on the welcome screen instead).
    pub fn new(w: u16, h: u16) -> Self {
        Self::build(w, h, false)
    }

    pub fn home(w: u16, h: u16) -> Self {
        Self::build(w, h, true)
    }

    fn build(w: u16, h: u16, home: bool) -> Self {
        std::env::set_var("HOME", "/home/me");
        let (tx, requests) = unbounded_channel();
        let (mtx, msgs) = unbounded_channel();
        let opts = AppOpts {
            cwd: PathBuf::from(CWD),
            mock: true,
            ..Default::default()
        };
        let mut app = App::new(opts, tx, mtx);
        app.size = (w, h);
        app.clock.freeze(Duration::from_millis(5_850));
        app.fixed_hm = Some((17, 42));
        app.wall = Some(1_790_003_600);
        app.branch = Some("main".into());
        app.home.tip = "Use Shift+Tab to cycle between modes like Plan mode.".into();
        if !home {
            app.screen = Screen::Session;
        }
        let mut hn = Harness {
            app,
            requests,
            _msgs: msgs,
        };
        hn.event(Event::Ready {
            session_id: "0191b6f0-3c1a-7a52-9a0e-6d3e9b2f1c44".into(),
            config: config(),
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

    /// Requests the app sent so far.
    pub fn sent(&mut self) -> Vec<Request> {
        let mut v = Vec::new();
        while let Ok(r) = self.requests.try_recv() {
            v.push(r);
        }
        v
    }

    /// Advance the frozen clock.
    pub fn advance(&mut self, ms: u64) {
        let now = self.app.clock.elapsed();
        self.app.clock.freeze(now + Duration::from_millis(ms));
    }

    pub fn at(&mut self, ms: u64) {
        self.app.clock.freeze(Duration::from_millis(ms));
    }

    pub fn render(&mut self) -> TestTerminal {
        let (w, h) = self.app.size;
        let mut t = TestTerminal::new(w, h);
        t.draw(|buf, _| grokw::ui::draw(buf, &mut self.app));
        t
    }

    pub fn text(&mut self) -> String {
        self.render().plain()
    }

    /// Write the current screen as ANSI to `$GROKW_DUMP_DIR/<name>.ansi` when that is set, so
    /// cell colours can be diffed against `reference/grok/*.ansi` (tools/cmp-ansi.py).
    pub fn dump(&mut self, name: &str) {
        if let Some(dir) = std::env::var_os("GROKW_DUMP_DIR") {
            let t = self.render();
            let _ = std::fs::create_dir_all(&dir);
            std::fs::write(
                std::path::Path::new(&dir).join(format!("{name}.ansi")),
                t.ansi(),
            )
            .expect("dump");
        }
    }

    /// A finished turn: the prompt goes out as the app sends it, then the answer.
    pub fn turn(&mut self, prompt: &str, answer: &str) {
        self.app.send_prompt(prompt.to_string());
        self.event(Event::TurnStart);
        self.event(Event::TextDelta(answer.to_string()));
        self.event(Event::Usage(Usage {
            input_tokens: 1_200,
            output_tokens: 500,
            context_tokens: 1_700,
            context_window: 256_000,
            ..Default::default()
        }));
        self.event(Event::TurnEnd(StopReason::EndTurn));
    }

    /// Set the duration the last turn took, so `Worked for` is reproducible.
    pub fn took(&mut self, secs: f64) {
        for m in self.app.tr.messages.iter_mut().rev() {
            if m.took.is_some() {
                m.took = Some(Duration::from_secs_f64(secs));
                break;
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

pub fn session(id: &str, title: &str, updated: i64) -> SessionInfo {
    SessionInfo {
        id: id.into(),
        title: title.into(),
        cwd: CWD.into(),
        updated,
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

pub const PROMPT: &str = "Do these in order, tersely. 1) Make a todo list of these steps. 2) Run ls -la in the shell. 3) Read README.md. 4) Edit README.md: change hello to hello world. 5) Grep for println in src. 6) Web search ratatui and give one sentence. 7) Finish with a markdown reply with a heading, a bold phrase, a bullet list of 3 items, a 2-row table, and a rust code block of 3 lines.";

pub const DONE: &str = "## Done\n\nRatatui is a Rust crate for building terminal user interfaces, and the successor of tui-rs.\n\n**hello world**\n\n- Listed the workspace (`README.md`, `src`, `.git`)\n- Changed `README.md` from `hello` to `hello world`\n- Found `println!(\"hi\");` in `src/main.rs`\n\n| Step | Result |\n|------|--------|\n| Edit | hello world |\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n";

impl Harness {
    /// Stages of the long turn the reference captures show, up to and including `upto`:
    /// 0 prompt sent, 1 thinking, 2 first text, 3 shell + read + edit, 4 searches, 5 final reply.
    pub fn long_turn(&mut self, upto: u8) {
        use serde_json::json;
        self.app.send_prompt(PROMPT.to_string());
        self.render();
        if upto == 0 {
            return;
        }
        self.event(Event::TurnStart);
        self.event(Event::ThoughtDelta(
            "I will execute the requested steps in order.".into(),
        ));
        if upto == 1 {
            return;
        }
        self.event(Event::TextDelta(
            "I'll work through the seven steps in order, starting with the todo list.".into(),
        ));
        self.render();
        if upto == 2 {
            return;
        }
        let todo = tool(
            "t0",
            "todo",
            ToolKind::Other,
            "todo",
            json!({"items": []}),
            Some("ok"),
        );
        self.event(Event::Tool(todo));
        let mut ls = tool(
            "t1",
            "execute",
            ToolKind::Execute,
            "ls -la",
            json!({"command": "ls -la", "description": "List workspace files in detail"}),
            Some("README.md\nsrc\n"),
        );
        self.event(Event::Tool(ls.clone()));
        ls.status = ToolStatus::Completed;
        self.event(Event::Tool(ls));
        self.event(Event::Tool(tool(
            "t2",
            "read_file",
            ToolKind::Read,
            "README.md",
            json!({"path": "README.md"}),
            Some("# demo project\nhello\n"),
        )));
        let mut edit = tool(
            "t3",
            "edit_file",
            ToolKind::Edit,
            "README.md",
            json!({"path": "README.md"}),
            Some("ok"),
        );
        edit.diff = Some(agent_core::FileDiff {
            path: "README.md".into(),
            old: Some("# demo project\nhello\n".into()),
            new: "# demo project\nhello world\n".into(),
        });
        self.event(Event::Tool(edit));
        if upto == 3 {
            return;
        }
        self.event(Event::ThoughtDelta("Now the search.".into()));
        self.event(Event::Tool(tool(
            "t4",
            "search_files",
            ToolKind::Search,
            "println",
            json!({"pattern": "println", "path": "src"}),
            Some("src/main.rs:2: println!(\"hi\");"),
        )));
        self.event(Event::ThoughtDelta("Web search next.".into()));
        for (i, q) in ["ratatui", "ratatui crate", "ratatui widgets"]
            .iter()
            .enumerate()
        {
            self.event(Event::Tool(tool(
                &format!("w{i}"),
                "web_search",
                ToolKind::Search,
                q,
                json!({"query": q}),
                Some("results"),
            )));
        }
        self.event(Event::ThoughtDelta(
            "Preparing the final markdown reply.".into(),
        ));
        if upto == 4 {
            return;
        }
        self.app.fixed_hm = Some((17, 43));
        self.render();
        self.event(Event::TextDelta(DONE.to_string()));
        self.event(Event::Usage(Usage {
            context_tokens: 23_200,
            context_window: 256_000,
            ..Default::default()
        }));
        self.event(Event::TurnEnd(StopReason::EndTurn));
    }

    /// Pin the durations the reference shows: thoughts and the turn.
    pub fn pin_durations(&mut self, thoughts: &[f64], turn: f64) {
        use agent_core::transcript::Part;
        let mut it = thoughts.iter();
        for m in self.app.tr.messages.iter_mut() {
            for p in m.parts.iter_mut() {
                if let Part::Thought { took, .. } = p {
                    if let Some(s) = it.next() {
                        *took = Some(Duration::from_secs_f64(*s));
                    }
                }
            }
            if m.took.is_some() {
                m.took = Some(Duration::from_secs_f64(turn));
            }
        }
    }
}

pub const MD_PROMPT: &str = include_str!("markdown-prompt.txt");

/// The markdown between BEGIN and END of the showcase prompt, which the model echoes back.
pub fn md_reply() -> String {
    let t = MD_PROMPT;
    let a = t.find("BEGIN\n").unwrap() + "BEGIN\n".len();
    let b = t.rfind("\nEND").unwrap();
    t[a..b].to_string()
}

impl Harness {
    /// The markdown showcase turn: the prompt, then the reply echoed back.
    pub fn markdown_turn(&mut self) {
        self.app.send_prompt(MD_PROMPT.trim_end().to_string());
        self.render();
        self.event(Event::TurnStart);
        self.event(Event::TextDelta(md_reply()));
        self.event(Event::Usage(Usage {
            context_tokens: 1_800,
            context_window: 256_000,
            ..Default::default()
        }));
        self.event(Event::TurnEnd(StopReason::EndTurn));
        self.pin_durations(&[], 7.1);
    }
}

pub const MINI_PROMPT: &str =
    "Edit README.md: change hello to hello there. Then reply with one short sentence.";

impl Harness {
    /// The short edit turn the 80x24 and 150x42 captures use: `sent` stops while the model
    /// waits after a search, `done` finishes with the edit and a one-line reply.
    pub fn mini_turn(&mut self, done: bool) {
        use serde_json::json;
        self.app.fixed_hm = Some((16, 53));
        self.app.tr.usage.context_tokens = 19_300;
        self.app.send_prompt(MINI_PROMPT.to_string());
        self.render();
        self.event(Event::TurnStart);
        self.event(Event::TextDelta("I'll update README.md so \"hello\" becomes \"hello there\", then reply in one short sentence.".into()));
        self.render();
        self.event(Event::Tool(tool(
            "g1",
            "search_files",
            ToolKind::Search,
            "hello",
            json!({"pattern": "hello"}),
            Some("README.md:2: hello"),
        )));
        if !done {
            return;
        }
        let mut edit = tool(
            "e1",
            "edit_file",
            ToolKind::Edit,
            "README.md",
            json!({"path": "README.md"}),
            Some("ok"),
        );
        edit.diff = Some(agent_core::FileDiff {
            path: "README.md".into(),
            old: Some("# demo project\nhello\n".into()),
            new: "# demo project\nhello there\n".into(),
        });
        self.event(Event::Tool(edit));
        self.render();
        self.event(Event::TextDelta(
            "Updated README.md so it now says \"hello there\".".into(),
        ));
        self.event(Event::Usage(Usage {
            context_tokens: 19_400,
            context_window: 256_000,
            ..Default::default()
        }));
        self.event(Event::TurnEnd(StopReason::EndTurn));
        self.pin_durations(&[], 6.3);
    }
}
