//! The selectors against the real Pi's captures (`reference/pi/120x36/*.txt`) and through the app:
//! key in, request out. Reference rows that wizard cannot back (the catalogue refresh status, the
//! `automatic` theme, the delete and rename hints) are dropped from the reference before comparing.

use std::path::PathBuf;
use std::time::Duration;

use agent_core::{Config, Event, HistoryItem, ModelOption, Request, SessionInfo};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use piw::app::{App, AppOpts};
use piw::ui;
use ratatui::style::Color;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tuikit::testing::TestTerminal;

struct H {
    app: App,
    reqs: UnboundedReceiver<Request>,
    _msgs: UnboundedReceiver<piw::app::Msg>,
}

fn mo(id: &str, name: &str, p: &str) -> ModelOption {
    ModelOption {
        id: id.into(),
        name: name.into(),
        provider: p.into(),
    }
}

impl H {
    fn new(models: Vec<ModelOption>) -> H {
        let (tx, reqs) = unbounded_channel();
        let (mtx, msgs) = unbounded_channel();
        let mut app = App::new(
            AppOpts {
                cwd: PathBuf::from("/home/me/demo"),
                mock: true,
                theme: Some("dark".into()),
                ..Default::default()
            },
            tx,
            mtx,
        );
        app.size = (120, 35);
        app.frozen = Some(Duration::ZERO);
        app.branch = Some("main".into());
        let cfg = Config {
            model: models.first().map(|m| m.id.clone()).unwrap_or_default(),
            models,
            effort: "medium".into(),
            efforts: ["low", "medium", "high", "xhigh"]
                .map(String::from)
                .to_vec(),
            cwd: "/home/me/demo".into(),
            ..Default::default()
        };
        let mut h = H {
            app,
            reqs,
            _msgs: msgs,
        };
        h.app.on_event(Event::Ready {
            session_id: "s1".into(),
            config: cfg,
        });
        h
    }

    fn key(&mut self, c: KeyCode) {
        self.app.on_key(KeyEvent::new(c, KeyModifiers::NONE));
    }

    fn ctrl(&mut self, c: char) {
        self.app
            .on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    }

    fn typ(&mut self, s: &str) {
        for c in s.chars() {
            self.key(KeyCode::Char(c));
        }
    }

    fn cmd(&mut self, s: &str) {
        self.typ(s);
        self.key(KeyCode::Enter);
    }

    fn sent(&mut self) -> Vec<Request> {
        let mut v = Vec::new();
        while let Ok(r) = self.reqs.try_recv() {
            v.push(r);
        }
        v
    }

    fn screen(&mut self) -> TestTerminal {
        let (w, h) = self.app.size;
        let mut t = TestTerminal::new(w, h);
        t.draw(|buf, _| ui::draw(buf, &mut self.app));
        t
    }

    /// Rows from the selector's top rule to its bottom rule.
    fn block(&mut self) -> Vec<String> {
        block_of(&self.screen().plain())
    }
}

fn block_of(text: &str) -> Vec<String> {
    let rows: Vec<&str> = text.lines().collect();
    let rules: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.starts_with(&"─".repeat(60)))
        .map(|(i, _)| i)
        .collect();
    let (a, b) = (rules[rules.len() - 2], rules[rules.len() - 1]);
    rows[a..=b]
        .iter()
        .map(|r| r.trim_end().to_string())
        .collect()
}

fn reference(name: &str) -> Vec<String> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../reference/pi/120x36")
        .join(format!("{name}.txt"));
    block_of(&std::fs::read_to_string(p).expect("reference capture"))
}

fn drop_rows(mut v: Vec<String>, at: &[usize]) -> Vec<String> {
    for i in at.iter().rev() {
        v.remove(*i);
    }
    v
}

fn three() -> Vec<ModelOption> {
    vec![
        mo("fake/fake-model", "fake-model", "fake"),
        mo("other/small-model", "small-model", "other"),
        mo("other/big-model", "big-model", "other"),
    ]
}

fn fix_names(rows: Vec<String>) -> Vec<String> {
    rows.into_iter()
        .map(|r| {
            r.replace("Fake Model", "fake-model")
                .replace("Small Model", "small-model")
                .replace("Big Model", "big-model")
        })
        .collect()
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn scoped_models_block_is_the_capture() {
    let mut h = H::new(three());
    h.cmd("/scoped-models");
    assert_eq!(
        h.block(),
        fix_names(drop_rows(reference("105-scoped-models"), &[13]))
    );
    h.key(KeyCode::Down);
    h.key(KeyCode::Enter);
    assert_eq!(
        h.block(),
        fix_names(drop_rows(reference("106-scoped-models-toggled"), &[13]))
    );
    h.ctrl('a');
    assert_eq!(
        h.block(),
        fix_names(drop_rows(reference("106b-scoped-all"), &[13]))
    );
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn model_selector_matches_the_capture() {
    let mut h = H::new(three());
    h.cmd("/model");
    let got = h.block();
    // the model wizard started with is the one marked `· default`
    let want = fix_names(drop_rows(reference("103-model"), &[12, 13]));
    assert_eq!(got, want);
    h.typ("big");
    let want = fix_names(drop_rows(reference("104-model-filtered"), &[10, 11]));
    assert_eq!(h.block(), want);
    // Enter picks the model and says so
    h.key(KeyCode::Enter);
    assert!(h
        .sent()
        .contains(&Request::SetModel("other/big-model".into())));
}

#[test]
fn model_selector_colours() {
    let mut h = H::new(three());
    h.cmd("/model");
    let t = h.screen();
    let rows: Vec<String> = t.plain().lines().map(String::from).collect();
    let y = rows
        .iter()
        .position(|r| r.starts_with("→ ✓ fake-model"))
        .unwrap() as u16;
    let rgb = |x: u16, y: u16| t.cell(x, y).unwrap().fg;
    assert_eq!(rgb(0, y), Color::Rgb(0xa7, 0x98, 0xd7)); // → accent
    assert_eq!(rgb(4, y), Color::Rgb(0xa7, 0x98, 0xd7)); // selected id accent
    assert_eq!(rgb(16, y), Color::Rgb(0x9d, 0xa5, 0xa9)); // [fake] muted
    assert_eq!(rgb(24, y), Color::Rgb(0x9d, 0xa5, 0xa9)); // `· default` is part of the muted badge
    let top = rows
        .iter()
        .position(|r| r.starts_with(&"─".repeat(60)))
        .unwrap();
    let rules: Vec<u16> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.starts_with(&"─".repeat(60)))
        .map(|(i, _)| i as u16)
        .collect();
    let _ = top;
    assert_eq!(rgb(0, rules[rules.len() - 2]), Color::Rgb(0x5f, 0xa8, 0xcc)); // border
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn theme_submenu_matches_the_capture_without_automatic() {
    let mut h = H::new(three());
    h.cmd("/settings");
    for _ in 0..9 {
        h.key(KeyCode::Down);
    }
    h.key(KeyCode::Enter);
    // without `automatic` the label column is one narrower
    let want: Vec<String> = drop_rows(reference("118b-theme-submenu"), &[6])
        .into_iter()
        .map(|r| r.replace("system     Theme", "system    Theme"))
        .collect();
    assert_eq!(h.block(), want);
    // moving the highlight previews; escape puts the old theme back
    h.key(KeyCode::Down);
    assert_eq!(h.app.theme.name, "light");
    h.key(KeyCode::Esc);
    assert_eq!(h.app.theme.name, "dark");
}

#[test]
fn settings_rows_colours_and_live_changes() {
    let mut h = H::new(three());
    h.cmd("/settings");
    let rows = h.block();
    assert_eq!(rows[1], ">");
    assert!(rows[3].starts_with("→ Show hardware cursor"));
    assert_eq!(
        rows[rows.len() - 2],
        "  Type to search · Enter/Space to change · Esc to cancel"
    );
    let t = h.screen();
    let plain: Vec<String> = t.plain().lines().map(String::from).collect();
    let y = plain
        .iter()
        .position(|r| r.starts_with("→ Show hardware"))
        .unwrap() as u16;
    assert_eq!(t.cell(0, y).unwrap().fg, Color::Rgb(0xa7, 0x98, 0xd7));
    assert_eq!(t.cell(29, y).unwrap().fg, Color::Rgb(0xa7, 0x98, 0xd7)); // selected value
    assert_eq!(t.cell(2, y + 1).unwrap().fg, Color::Reset); // label default
    assert_eq!(t.cell(29, y + 1).unwrap().fg, Color::Rgb(0x9d, 0xa5, 0xa9)); // value muted
                                                                             // "Hide thinking" is the fifth row: enter flips it and the app follows
    for _ in 0..4 {
        h.key(KeyCode::Down);
    }
    assert!(!h.app.hide_thinking);
    h.key(KeyCode::Enter);
    assert!(h.app.hide_thinking);
    h.key(KeyCode::Esc);
    h.ctrl('t');
    assert!(!h.app.hide_thinking);
}

#[test]
fn thinking_selector_lists_the_backend_levels() {
    let mut h = H::new(three());
    h.cmd("/thinking");
    let rows = h.block();
    assert_eq!(rows[2], "Thinking Level");
    assert_eq!(rows[4], "Shift+Tab cycles thinking levels in-session");
    assert!(
        rows.contains(&"→ ✓ medium    Moderate reasoning (~8k tokens) · default".to_string()),
        "{rows:#?}"
    );
    assert!(rows.contains(&"    low       Light reasoning (~2k tokens)".to_string()));
    h.key(KeyCode::Down);
    h.key(KeyCode::Enter);
    assert!(h.sent().contains(&Request::SetEffort("high".into())));
}

fn sessions() -> Vec<SessionInfo> {
    vec![
        SessionInfo {
            id: "a".into(),
            title: "first [[hello]]".into(),
            cwd: "/home/me/demo".into(),
            updated: piw::app::FROZEN_NOW - 5,
        },
        SessionInfo {
            id: "b".into(),
            title: "elsewhere".into(),
            cwd: "/home/me/other".into(),
            updated: piw::app::FROZEN_NOW - 7200,
        },
    ]
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn resume_selector_header_rows_and_loading() {
    let mut h = H::new(three());
    h.app.on_event(Event::Sessions(sessions()));
    h.cmd("/resume");
    assert!(h.sent().contains(&Request::ListSessions));
    let got = h.block();
    let want = reference("110-resume");
    // header, hints (minus delete and rename), search and the capture's one row
    assert_eq!(got[2], want[2]);
    assert_eq!(got[3], want[3]);
    assert_eq!(got[4], "ctrl+s sort · ctrl+n named · ctrl+p path (off)");
    let left = "› first [[hello]]";
    assert_eq!(
        got[8],
        format!("{left}{}now", " ".repeat(120 - left.chars().count() - 3))
    );
    assert!(!got.iter().any(|r| r.contains("elsewhere")));
    // tab shows every folder, with its directory
    h.key(KeyCode::Tab);
    let all = h.block();
    assert!(all[2].starts_with("Resume Session (All)"));
    assert!(all
        .iter()
        .any(|r| r.contains("~/other") || r.contains("/home/me/other")));
    // enter loads it and the history that follows says it was resumed
    h.key(KeyCode::Enter);
    assert!(h.sent().contains(&Request::LoadSession("a".into())));
    h.app.on_event(Event::History {
        session_id: "a".into(),
        items: vec![HistoryItem::User("first".into())],
    });
    let text = h.screen().plain();
    assert!(text.contains("Resumed session"), "{text}");
}

#[test]
fn resume_colours() {
    let mut h = H::new(three());
    h.app.on_event(Event::Sessions(sessions()));
    h.cmd("/resume");
    let t = h.screen();
    let plain: Vec<String> = t.plain().lines().map(String::from).collect();
    let y = plain.iter().position(|r| r.starts_with("› first")).unwrap() as u16;
    let c = t.cell(0, y).unwrap();
    assert_eq!(c.fg, Color::Rgb(0xa7, 0x98, 0xd7));
    assert_eq!(c.bg, Color::Rgb(0x21, 0x3b, 0x49)); // selectedBg across the row
    assert_eq!(t.cell(119, y).unwrap().bg, Color::Rgb(0x21, 0x3b, 0x49));
    let rules: Vec<u16> = plain
        .iter()
        .enumerate()
        .filter(|(_, r)| r.starts_with(&"─".repeat(60)))
        .map(|(i, _)| i as u16)
        .collect();
    assert_eq!(
        t.cell(0, rules[0]).unwrap().fg,
        Color::Rgb(0xa7, 0x98, 0xd7)
    ); // accent frame
}

#[test]
fn scoping_models_limits_ctrl_p() {
    let mut h = H::new(three());
    h.cmd("/scoped-models");
    h.ctrl('x'); // clear all
    h.key(KeyCode::Enter); // fake-model back on
    h.key(KeyCode::Down);
    h.key(KeyCode::Enter); // small-model on
    h.key(KeyCode::Esc);
    h.sent();
    h.ctrl('p');
    assert!(h
        .sent()
        .contains(&Request::SetModel("other/small-model".into())));
    // the model selector now opens on the scope
    h.ctrl('l');
    let rows = h.block();
    assert!(
        rows.contains(&"Scope: all | scoped".to_string()),
        "{rows:#?}"
    );
}

#[test]
fn session_hotkeys_and_changelog_blocks() {
    let mut h = H::new(three());
    h.cmd("/session");
    h.cmd("/hotkeys");
    h.cmd("/changelog");
    let text = h.screen().plain();
    assert!(text.contains("What's New"), "{text}");
    h.app.size = (120, 120);
    let text = h.screen().plain();
    assert!(text.contains("Session Info"));
    assert!(text.contains(" Keyboard Shortcuts"));
    assert!(text.contains("│ Shift+Tab"));
}

#[test]
fn every_selector_fits_80_and_150_columns() {
    for (w, hgt) in [(80u16, 24u16), (150, 42)] {
        for cmd in [
            "/model",
            "/thinking",
            "/scoped-models",
            "/settings",
            "/resume",
        ] {
            let mut h = H::new(three());
            h.app.on_event(Event::Sessions(sessions()));
            h.app.size = (w, hgt);
            h.cmd(cmd);
            let t = h.screen();
            for (y, row) in t.plain().lines().enumerate() {
                assert!(
                    tuikit::width::display_width(row) <= w as usize,
                    "{cmd} at {w}x{hgt}, row {y}: {row:?}"
                );
            }
            let rows = block_of(&t.plain());
            assert!(rows.len() > 4, "{cmd}: {rows:?}");
        }
    }
}
