//! Pi's editing keys through the app, read off the screen: the cursor is the reverse-video cell,
//! so a word stop is a column, exactly what the fidelity harness diffs. The expected columns come
//! from Pi's own `findWordForward` / `findWordBackward` (`tools/pi-words-golden.mjs`).

mod common;

use common::*;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::style::Modifier;

const TEXT: &str = "foo.bar-baz/qux  (hello, world)  snake_case CamelCase 12.5";

/// (row, column) of the cursor cell between the editor's rules.
fn cursor(h: &mut Harness) -> (u16, u16) {
    let t = h.render();
    let (w, hgt) = h.app.size;
    let rules: Vec<u16> = (0..hgt)
        .filter(|y| t.row(*y).starts_with(&"─".repeat(40)))
        .collect();
    let (a, b) = (rules[rules.len() - 2], rules[rules.len() - 1]);
    for y in a + 1..b {
        for x in 0..w {
            if t.cell(x, y).unwrap().modifier.contains(Modifier::REVERSED) {
                return (y, x);
            }
        }
    }
    panic!("no cursor between the rules:\n{}", t.plain());
}

fn alt(h: &mut Harness, c: char) {
    h.key(KeyCode::Char(c), KeyModifiers::ALT);
}

fn editor_text(h: &mut Harness) -> String {
    h.app.editor.text().to_string()
}

#[test]
fn alt_f_and_alt_b_stop_where_pis_word_navigation_stops() {
    let mut h = Harness::new(120, 36);
    h.type_str(TEXT);
    h.ctrl('a');
    let mut cols = Vec::new();
    for _ in 0..17 {
        alt(&mut h, 'f');
        cols.push(cursor(&mut h).1);
    }
    assert_eq!(
        cols,
        [3, 4, 7, 8, 11, 12, 15, 18, 23, 24, 30, 31, 43, 53, 56, 57, 58]
    );
    let mut back = Vec::new();
    for _ in 0..17 {
        alt(&mut h, 'b');
        back.push(cursor(&mut h).1);
    }
    assert_eq!(
        back,
        [57, 56, 54, 44, 33, 30, 25, 23, 18, 17, 12, 11, 8, 7, 4, 3, 0]
    );
}

#[test]
fn ctrl_w_takes_a_punctuation_run_and_the_pieces_of_a_number_one_at_a_time() {
    let mut h = Harness::new(120, 36);
    h.type_str(TEXT);
    h.ctrl('w');
    assert!(
        editor_text(&mut h).ends_with("CamelCase 12."),
        "{}",
        editor_text(&mut h)
    );
    h.ctrl('w');
    assert!(editor_text(&mut h).ends_with("CamelCase 12"));
    h.ctrl('w');
    assert!(editor_text(&mut h).ends_with("CamelCase "));
    // the three kills came back as one entry
    h.ctrl('y');
    assert_eq!(editor_text(&mut h), TEXT);
}

#[test]
fn consecutive_kills_yank_back_as_one_piece_and_alt_d_accumulates_forward() {
    let mut h = Harness::new(120, 36);
    h.type_str("one two three");
    h.ctrl('w');
    h.ctrl('w');
    h.ctrl('y');
    assert_eq!(editor_text(&mut h), "one two three");
    h.ctrl('u');
    h.type_str("abc def");
    h.ctrl('a');
    alt(&mut h, 'd');
    alt(&mut h, 'd');
    h.ctrl('y');
    assert_eq!(editor_text(&mut h), "abc def");
}

#[test]
fn a_paste_marker_is_one_stop_for_word_motion_and_kills() {
    let mut h = Harness::new(120, 36);
    h.type_str("see ");
    let pasted: String = (1..=12).map(|i| format!("line {i}\n")).collect();
    h.paste(&pasted);
    h.type_str(" after");
    h.ctrl('a');
    alt(&mut h, 'f');
    alt(&mut h, 'f');
    let marker = "[paste #1 +13 lines]";
    assert_eq!(cursor(&mut h).1 as usize, "see ".len() + marker.len());
    alt(&mut h, 'b');
    assert_eq!(cursor(&mut h).1, 4);
}

#[test]
fn up_puts_the_cursor_at_the_start_and_down_at_the_end() {
    let mut h = Harness::new(120, 36);
    for p in ["one [[hello]]", "two [[hello]]", "three [[hello]]"] {
        h.type_str(p);
        h.press(KeyCode::Enter);
        h.sent();
    }
    h.press(KeyCode::Up);
    assert_eq!(editor_text(&mut h), "three [[hello]]");
    assert_eq!(
        cursor(&mut h).1,
        0,
        "recalled text starts with the cursor at column 0"
    );
    h.press(KeyCode::Up);
    h.press(KeyCode::Up);
    h.press(KeyCode::Up);
    assert_eq!(
        editor_text(&mut h),
        "one [[hello]]",
        "nothing older than the oldest"
    );
    h.press(KeyCode::Down);
    assert_eq!(editor_text(&mut h), "two [[hello]]");
    assert_eq!(
        cursor(&mut h).1 as usize,
        "two [[hello]]".len(),
        "going down ends at the end"
    );
    h.type_str(" edited");
    assert_eq!(editor_text(&mut h), "two [[hello]] edited");
}

#[test]
fn up_over_a_typed_draft_goes_to_the_line_start_first() {
    let mut h = Harness::new(120, 36);
    h.type_str("older");
    h.press(KeyCode::Enter);
    h.sent();
    h.type_str("draft");
    h.press(KeyCode::Up);
    assert_eq!(
        (editor_text(&mut h).as_str(), cursor(&mut h).1),
        ("draft", 0)
    );
    h.press(KeyCode::Up);
    assert_eq!(editor_text(&mut h), "older");
    h.press(KeyCode::Down);
    assert_eq!(editor_text(&mut h), "draft", "the draft is back");
}

#[test]
fn ctrl_h_does_nothing_and_backspace_deletes() {
    let mut h = Harness::new(120, 36);
    h.type_str("abc");
    h.ctrl('h');
    assert_eq!(editor_text(&mut h), "abc");
    h.press(KeyCode::Backspace);
    assert_eq!(editor_text(&mut h), "ab");
}
