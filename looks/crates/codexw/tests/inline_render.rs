//! The inline renderer against the headless terminal: history reaches the scrollback in order,
//! the viewport stays on the last rows once the screen is full, a resize replays everything at
//! the new width, and Ctrl+L starts over.

mod common;

use agent_core::{Config, Event};
use codexw::ui::history_cells::InfoCell;
use common::*;

fn started(cols: u16, rows: u16) -> Harness {
    let mut h = harness(cols, rows, 7);
    h.draw();
    h.app.on_backend(Event::Ready {
        session_id: "s".into(),
        config: Config {
            model: "gpt-5.5".into(),
            effort: "default".into(),
            backend: "wizard 3.8.12".into(),
            ..Default::default()
        },
    });
    h.draw();
    h
}

fn info(h: &mut Harness, text: &str) {
    h.app.push_cell(Box::new(InfoCell {
        text: text.into(),
        hint: None,
    }));
    h.draw();
}

#[test]
fn history_scrolls_into_the_terminal_scrollback_in_order() {
    let mut h = started(120, 36);
    for i in 0..60 {
        info(&mut h, &format!("line {i:02}"));
    }
    let full = h.screen.full_text();
    let at = |n: usize| {
        full.find(&format!("line {n:02}"))
            .unwrap_or_else(|| panic!("line {n} missing:\n{full}"))
    };
    for n in 1..60 {
        assert!(at(n - 1) < at(n), "order broken at {n}");
    }
    assert_eq!(full.matches("Wizard (v3.8.12)").count(), 1);
    // Once the screen is full the viewport sits on the last five rows.
    let rows = h.screen.rows();
    assert!(rows[35].starts_with("  gpt-5.5 default"), "{rows:?}");
    assert!(rows[33].starts_with("› "), "{rows:?}");
    assert!(
        rows[30].contains("line 59") || rows[31].contains("line 59"),
        "{rows:?}"
    );
    assert_eq!(h.screen.alt_screen_entered, 0);
}

#[test]
fn separators_are_one_blank_row_between_cells() {
    let mut h = started(120, 36);
    info(&mut h, "first");
    info(&mut h, "second");
    let rows = h.screen.rows();
    let i = rows.iter().position(|r| r == "• first").unwrap();
    assert_eq!(rows[i + 1], "");
    assert_eq!(rows[i + 2], "• second");
}

#[test]
fn resize_replays_history_at_the_new_width() {
    let mut h = started(120, 36);
    for i in 0..5 {
        info(&mut h, &format!("line {i}"));
    }
    h.resize(80, 24);
    let full = h.screen.full_text();
    assert!(full.lines().all(|l| l.chars().count() <= 80), "{full}");
    assert_eq!(full.matches("Wizard (v3.8.12)").count(), 1);
    assert!(full.contains("• line 4"));
    // Tip wraps to two rows at 80 columns.
    let tip = full
        .lines()
        .skip_while(|l| !l.contains("Tip:"))
        .take_while(|l| l.starts_with("  "))
        .count();
    assert_eq!(tip, 2, "{full}");
    assert!(h.screen.scrollback_clears >= 1);
}

#[test]
fn ctrl_l_wipes_screen_and_scrollback_and_keeps_only_the_header() {
    let mut h = started(120, 36);
    for i in 0..50 {
        info(&mut h, &format!("line {i}"));
    }
    h.ctrl('l');
    h.draw();
    let full = h.screen.full_text();
    assert!(!full.contains("line 3"), "history survived Ctrl+L:\n{full}");
    assert!(full.contains("Wizard"));
    assert!(!full.contains("Tip:"), "Ctrl+L shows no tip");
}

#[test]
fn composer_grows_the_viewport_and_shrinks_it_back() {
    let mut h = started(120, 36);
    let top = |h: &Harness| {
        h.screen
            .rows()
            .iter()
            .position(|r| r.starts_with("› "))
            .unwrap()
    };
    let before = top(&h);
    h.app.on_paste("a\nb\nc");
    h.draw();
    let rows = h.screen.rows();
    assert!(rows.iter().any(|r| r == "  c"), "{rows:?}");
    assert_eq!(
        top(&h),
        before,
        "growing downward must not move the first text row"
    );
    h.ctrl('c');
    h.draw();
    assert!(!h.screen.rows().iter().any(|r| r == "  c"));
}
