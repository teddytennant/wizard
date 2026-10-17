//! Whole-screen snapshots at 40, 80, 120 and 160 columns, driven by the scripted mock backend
//! on a fake clock. Review a changed snapshot like a diff of the UI; accept with
//! `UPDATE_SNAPSHOTS=1 cargo test -p openc --test screens`.

mod common;

use agent_core::Event;
use common::{assert_snapshot, hello, Harness, Mock};
use crossterm::event::KeyCode;

const SIZES: [(u16, u16); 4] = [(40, 20), (80, 24), (120, 36), (160, 45)];

/// Type the prompt, send it, feed the mock's events, return a drawn harness.
fn turn(w: u16, h: u16, prompt: &str, events: &[Event], dt: u64) -> Harness {
    let mut hx = Harness::new(w, h);
    hx.ready();
    hx.type_str(prompt);
    hx.key(KeyCode::Enter);
    hx.events(events.to_vec(), dt);
    hx
}

async fn scenario(name: &str, prompt: &str, dt: u64) {
    let mut m = Mock::start();
    hello(&mut m).await;
    let events = m.run(prompt).await;
    for (w, h) in SIZES {
        let mut hx = turn(w, h, prompt, &events, dt);
        assert_snapshot(&format!("{name}-{w}x{h}"), &hx.screen());
    }
}

#[test]
fn start_screen() {
    for (w, h) in SIZES {
        let mut hx = Harness::new(w, h);
        hx.ready();
        let s = hx.screen();
        assert_snapshot(&format!("start-{w}x{h}"), &s);
    }
}

#[tokio::test]
async fn tools_turn() {
    scenario("tools", "tools please", 120).await;
}

#[tokio::test]
async fn long_answer() {
    scenario("long", "long answer", 40).await;
}

#[tokio::test]
async fn thinking_turn() {
    scenario("think", "think about it", 400).await;
}

#[tokio::test]
async fn failing_tool_and_notice() {
    scenario("error", "error please", 300).await;
}

#[tokio::test]
async fn subagent_tree() {
    scenario("sub", "sub agent", 250).await;
}

#[tokio::test]
async fn permission_panel_edit_and_bash() {
    let mut m = Mock::start();
    hello(&mut m).await;
    for (name, prompt) in [("perm-edit", "perm edit"), ("perm-bash", "perm bash")] {
        let events = m.run(prompt).await;
        assert!(
            matches!(events.last(), Some(Event::Permission(_))),
            "{events:?}"
        );
        for (w, h) in [(80, 24), (120, 36)] {
            let mut hx = turn(w, h, prompt, &events, 100);
            assert_snapshot(&format!("{name}-{w}x{h}"), &hx.screen());
        }
        // Answer so the mock's turn ends before the next scenario.
        m.tx.send(agent_core::Request::Decide {
            id: "perm-1".into(),
            allow: true,
            scope: Default::default(),
            note: String::new(),
        })
        .unwrap();
        m.collect(|e| matches!(e, Event::TurnEnd(_))).await;
    }
}

#[tokio::test]
async fn busy_turn_with_a_running_tool_and_a_queued_message() {
    let mut m = Mock::start();
    hello(&mut m).await;
    let events = m.run_for("busy now", 400).await;
    for (w, h) in [(80, 24), (120, 36)] {
        let mut hx = turn(w, h, "busy now", &events, 200);
        hx.type_str("and then bump the changelog");
        hx.key(KeyCode::Enter);
        hx.advance(300);
        assert_snapshot(&format!("busy-{w}x{h}"), &hx.screen());
    }
}

#[test]
fn popups_and_dialogs() {
    let mut hx = Harness::new(120, 36);
    hx.ready();
    hx.app.files = vec![
        "src/main.rs".into(),
        "src/lib.rs".into(),
        "README.md".into(),
        "Cargo.toml".into(),
    ];
    hx.type_str("/mo");
    assert_snapshot("popup-slash-120x36", &hx.screen());
    hx.key(KeyCode::Esc);
    hx.ctrl('u');
    hx.type_str("see @ma");
    assert_snapshot("popup-at-120x36", &hx.screen());
    let mut hx = Harness::new(120, 36);
    hx.ready();
    hx.ctrl('p');
    assert_snapshot("dialog-palette-120x36", &hx.screen());
    let mut hx = Harness::new(120, 36);
    hx.ready();
    hx.ctrl('t');
    assert_snapshot("dialog-settings-120x36", &hx.screen());
    let mut hx = Harness::new(100, 40);
    hx.ready();
    hx.key(KeyCode::F(1));
    assert_snapshot("dialog-help-100x40", &hx.screen());
}

#[tokio::test]
async fn nav_mode_and_scrolled_view() {
    let mut m = Mock::start();
    hello(&mut m).await;
    let long = m.run("long answer").await;
    let tools = m.run("tools please").await;
    let mut hx = Harness::new(100, 24);
    hx.ready();
    for (p, evs) in [("long answer", &long), ("tools please", &tools)] {
        hx.type_str(p);
        hx.key(KeyCode::Enter);
        hx.events(evs.clone(), 60);
    }
    hx.draw();
    hx.key(KeyCode::Esc);
    hx.type_str("kk");
    assert_snapshot("nav-100x24", &hx.screen());
    // Scroll up far, then stream more: the view stays put and says so.
    hx.key(KeyCode::Esc);
    hx.key(KeyCode::PageUp);
    hx.key(KeyCode::PageUp);
    hx.event(Event::TurnStart, 50);
    hx.event(
        Event::TextDelta("new words arriving below the fold ".into()),
        50,
    );
    assert_snapshot("scrolled-100x24", &hx.screen());
}

#[tokio::test]
async fn light_theme_and_sixteen_colours_keep_the_same_layout() {
    use openc::app::ThemeChoice;
    use openc::palette::Depth;
    let screens: Vec<String> = [
        (ThemeChoice::Hearth, Depth::True),
        (ThemeChoice::Parchment, Depth::True),
        (ThemeChoice::Hearth, Depth::Ansi256),
        (ThemeChoice::Hearth, Depth::Ansi16),
        (ThemeChoice::Hearth, Depth::Mono),
    ]
    .iter()
    .map(|(t, d)| {
        let mut hx = Harness::with_theme(100, 30, *t, *d);
        hx.ready();
        hx.type_str("hello");
        hx.key(KeyCode::Enter);
        hx.event(Event::TurnStart, 10);
        hx.event(
            Event::TextDelta("Hi there. Here is `code` and **bold** text.".into()),
            10,
        );
        hx.event(Event::TurnEnd(agent_core::StopReason::EndTurn), 10);
        hx.screen()
    })
    .collect();
    for s in &screens[1..] {
        assert_eq!(
            s, &screens[0],
            "layout must not depend on theme or colour depth"
        );
    }
}
