//! `--minimal`: the live rows against the last ten rows of `120x36/120-minimal-resumed`,
//! `121-minimal-slash` and `122-minimal-typed`, and the scrollback hand-off.

mod common;

use common::cells::{assert_cells, Skip};
use common::*;
use grokw::term::ScreenMode;
use grokw::theme::Theme;
use tuikit::testing::TestTerminal;

/// The markdown session in minimal mode: no welcome, the terminal's own colours.
fn minimal() -> Harness {
    let mut h = Harness::new(120, 36);
    h.app.opts.screen = ScreenMode::Minimal;
    h.app.theme = Theme::terminal();
    h.markdown_turn();
    h
}

/// Draw the live area into `rows` rows and return it with what scrolled out above it.
fn live(h: &mut Harness, rows: u16) -> (TestTerminal, Option<ratatui::buffer::Buffer>) {
    let mut t = TestTerminal::new(120, rows);
    let mut commit = None;
    t.draw(|buf, _| commit = grokw::ui::minimal::draw(buf, &mut h.app));
    (t, commit)
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn the_live_rows_are_the_last_ten_of_the_capture() {
    let mut h = minimal();
    let (t, commit) = live(&mut h, 10);
    assert!(
        commit.is_some(),
        "the history above the window is handed over"
    );
    h.dump("minimal-resumed-live");
    assert_snapshot("minimal-resumed-live", &t.plain());
    assert_cells(
        "120x36/120-minimal-resumed",
        &t,
        &Skip {
            offset: 26,
            ..Default::default()
        },
    );
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn typing_shows_in_the_bare_prompt() {
    let mut h = minimal();
    h.type_str("hello");
    let (t, _) = live(&mut h, 10);
    assert_cells(
        "120x36/122-minimal-typed",
        &t,
        &Skip {
            offset: 17,
            ..Default::default()
        },
    );
}

#[test]
fn the_slash_popup_is_a_rule_eight_rows_and_a_footer() {
    let mut h = minimal();
    h.type_str("/");
    let (t, _) = live(&mut h, 12);
    h.dump("minimal-slash-live");
    assert_snapshot("minimal-slash-live", &t.plain());
    let text = t.plain();
    assert!(
        text.contains("↑/↓ navigate · enter confirm · esc cancel"),
        "{text}"
    );
    assert!(text.contains("❯ /memory"), "{text}");
}

#[test]
fn rows_are_committed_once() {
    let mut h = minimal();
    let (_, first) = live(&mut h, 10);
    let first = first.expect("history scrolled out");
    assert!(first.area.height > 20);
    // nothing new, nothing more to write
    let (_, again) = live(&mut h, 10);
    assert!(again.is_none());
    // a new turn pushes the rows that no longer fit
    h.turn("one more", "short answer");
    h.took(1.0);
    let (_, more) = live(&mut h, 10);
    let more = more.expect("the rows the new turn displaced");
    assert!(more.area.height >= 1);
    // what was written is flush left, in order
    let first_row: String = (0..first.area.width)
        .map(|x| first[(x, 0)].symbol().to_string())
        .collect::<String>()
        .trim_end()
        .to_string();
    assert!(first_row.starts_with("❯ "), "{first_row:?}");
}

#[test]
fn minimal_has_its_own_exit_hint_and_no_summary() {
    let mut h = minimal();
    let tail = h.app.epilogue(120);
    assert!(tail.contains("wizard-ui-grok --minimal --resume"), "{tail}");
    assert!(!tail.contains("> "), "{tail}");
    h.app.opts.screen = ScreenMode::Fullscreen;
    assert!(!h.app.epilogue(120).contains("--minimal"));
}

#[test]
fn theme_is_refused_in_minimal() {
    let mut h = minimal();
    h.app.dispatch("/theme grokday".into());
    assert_eq!(h.app.theme.kind, grokw::theme::Kind::Terminal);
}
