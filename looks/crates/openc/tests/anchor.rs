//! Where the transcript and the composer sit while a short session grows (visual critique,
//! round 2, finding 1). The transcript starts at the top row and grows downward; the composer,
//! dock and status line are docked to the bottom edge from the first frame. So nothing drawn
//! moves when you send, and nothing moves while an answer streams until the screen is full.

mod common;

use agent_core::{Event, StopReason};
use common::Harness;
use crossterm::event::KeyCode;

fn rows_of(h: &mut Harness) -> Vec<String> {
    h.screen().lines().map(str::to_string).collect()
}

/// The main-area text of one screen row, so the rail at 160 columns is not part of the diff.
fn main_area(h: &Harness, row: &str) -> String {
    let a = h.app.geo.a as usize;
    row.chars().take(a).collect()
}

const SIZES: [(u16, u16); 5] = [(60, 24), (80, 24), (120, 36), (160, 45), (240, 60)];

#[test]
fn the_first_send_moves_nothing_that_was_already_drawn() {
    for (w, hh) in SIZES {
        let mut h = Harness::new(w, hh);
        h.ready();
        let before = rows_of(&mut h);
        let comp = h.app.geo.composer;
        let status = h.app.geo.status;
        let welcome_rows: Vec<usize> = before
            .iter()
            .enumerate()
            .filter(|(y, r)| *y < comp.y as usize && !main_area(&h, r).trim().is_empty())
            .map(|(y, _)| y)
            .collect();
        assert!(
            welcome_rows.first().is_some_and(|y| *y <= 2),
            "{w}x{hh}: the welcome starts at the top, got {welcome_rows:?}"
        );
        h.type_str("question");
        h.key(KeyCode::Enter);
        h.event(Event::TurnStart, 10);
        h.event(Event::TextDelta("A first line.\n\n".into()), 10);
        let after = rows_of(&mut h);
        assert_eq!(h.app.geo.composer, comp, "{w}x{hh}: composer moved on send");
        assert_eq!(h.app.geo.status, status, "{w}x{hh}: status moved on send");
        for y in welcome_rows {
            assert_eq!(
                main_area(&h, &before[y]),
                main_area(&h, &after[y]),
                "{w}x{hh}: welcome row {y} moved on send"
            );
        }
        assert!(
            after.iter().any(|r| r.contains("A first line.")),
            "{w}x{hh}"
        );
    }
}

#[test]
fn no_row_moves_while_a_short_answer_streams_line_by_line() {
    for (w, hh) in SIZES {
        let mut h = Harness::new(w, hh);
        h.ready();
        h.type_str("question");
        h.key(KeyCode::Enter);
        h.event(Event::TurnStart, 10);
        let mut prev = rows_of(&mut h);
        let comp = h.app.geo.composer;
        let vh = h.app.geo.transcript.height as usize;
        let mut full = false;
        // One line at a time, each its own paragraph, until the transcript is full and a few
        // more. While it fits, every row above the newest one must be what it was.
        for i in 0..(vh + 4) {
            h.event(Event::TextDelta(format!("Line number {i}.\n\n")), 20);
            let now = rows_of(&mut h);
            assert_eq!(
                h.app.geo.composer, comp,
                "{w}x{hh} line {i}: composer moved"
            );
            if !full {
                let last_prev = (0..vh)
                    .rev()
                    .find(|y| !main_area(&h, &prev[*y]).trim().is_empty());
                let last_now = (0..vh)
                    .rev()
                    .find(|y| !main_area(&h, &now[*y]).trim().is_empty());
                // Once the newest row reaches the last transcript row the screen is full and
                // the page has to scroll; that is the one place rows move.
                if last_now == Some(vh - 1) {
                    full = true;
                } else if let Some(lp) = last_prev {
                    for y in 0..lp {
                        assert_eq!(
                            main_area(&h, &prev[y]),
                            main_area(&h, &now[y]),
                            "{w}x{hh} line {i}: row {y} moved while the answer fits"
                        );
                    }
                }
            }
            prev = now;
        }
        assert!(full, "{w}x{hh}: the test never filled the screen");
        h.event(Event::TurnEnd(StopReason::EndTurn), 10);
        assert_eq!(h.app.geo.composer, comp);
    }
}

#[test]
fn a_growing_draft_does_not_move_the_welcome() {
    // Round 2 finding 11: the welcome used to re-centre as the draft grew by two rows.
    for (w, hh) in [(80, 24), (120, 36)] {
        let mut h = Harness::new(w, hh);
        h.ready();
        let before = rows_of(&mut h);
        let at = before
            .iter()
            .position(|r| r.contains("o p e n c"))
            .expect("welcome");
        for i in 0..3 {
            h.type_str(&format!("draft {i}"));
            h.ctrl('j');
        }
        let after = rows_of(&mut h);
        assert_eq!(
            after.iter().position(|r| r.contains("o p e n c")),
            Some(at),
            "{w}x{hh}"
        );
    }
}

fn long_session(w: u16, hh: u16, turns: usize) -> Harness {
    let mut h = Harness::new(w, hh);
    h.ready();
    for i in 0..turns {
        h.type_str(&format!("turn {i}"));
        h.key(KeyCode::Enter);
        h.event(Event::TurnStart, 10);
        h.event(Event::TextDelta(format!("Done with turn {i}.")), 10);
        h.event(Event::TurnEnd(StopReason::EndTurn), 10);
    }
    h
}

#[test]
fn a_user_message_over_the_composer_is_set_apart_by_an_edge() {
    // Round 2 finding 15: `turn 164` and the composer read as one slab at 80x24.
    let mut h = long_session(80, 24, 12);
    h.type_str("the last one");
    h.key(KeyCode::Enter);
    h.event(Event::TurnStart, 10);
    h.draw();
    let g = h.app.geo;
    let buf = h.term.backend().buffer().clone();
    let x = g.x + g.c - 1;
    let last = &buf[(x, g.transcript.bottom() - 1)];
    assert_eq!(
        last.bg, h.app.p.raised,
        "the user row is the last transcript row"
    );
    let edge = &buf[(x, g.composer.y)];
    assert_eq!(edge.symbol(), "▄");
    assert_eq!(edge.bg, h.app.p.bg, "half a row of page between the two");
    assert_eq!(edge.fg, h.app.p.raised);
    // Nothing is lost: the composer has the rows it always has.
    assert_eq!(g.composer.bottom(), 23);
}

#[test]
fn the_scrollbar_thumb_stays_while_the_view_is_off_the_end() {
    // Round 2 finding 15: the thumb faded in 1.5 s, so reading page 12 of 200 showed no place.
    let mut h = long_session(100, 30, 40);
    h.draw();
    h.key(KeyCode::PageUp);
    h.advance(10_000);
    h.draw();
    let g = h.app.geo;
    let buf = h.term.backend().buffer().clone();
    let x = g.a - 1;
    let thumb = (g.transcript.y..g.transcript.bottom())
        .filter(|y| buf[(x, *y)].symbol() == "┃")
        .count();
    assert!(thumb > 0, "no thumb ten seconds after the scroll");
    // At the end of the transcript, at rest, there is none.
    h.key(KeyCode::End);
    h.advance(10_000);
    h.draw();
    let buf = h.term.backend().buffer().clone();
    assert!(
        (g.transcript.y..g.transcript.bottom()).all(|y| buf[(x, y)].symbol() != "┃"),
        "a thumb while following the end"
    );
}

#[test]
fn the_composer_bar_is_the_users_colour() {
    // Round 2 finding 18: teal while typing, lavender once sent read as two speakers.
    let mut h = Harness::new(100, 30);
    h.ready();
    h.draw();
    let g = h.app.geo;
    let buf = h.term.backend().buffer().clone();
    assert_eq!(buf[(g.composer.x, g.composer.y + 1)].fg, h.app.p.user);
}
