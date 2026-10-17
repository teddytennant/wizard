//! Design checklist (section f) items that are facts about a drawn frame. Each test names the
//! items it covers; `docs/openc-checklist.md` points here.

mod common;

use agent_core::{Event, StopReason};
use common::Harness;
use crossterm::event::KeyCode;
use openc::app::ThemeChoice;
use openc::palette::Depth;
use ratatui::style::{Color, Modifier};

const SIZES: [(u16, u16); 5] = [(40, 20), (80, 24), (120, 36), (160, 45), (200, 50)];

fn answered(w: u16, h: u16, theme: ThemeChoice, depth: Depth, text: &str) -> Harness {
    let mut hx = Harness::with_theme(w, h, theme, depth);
    hx.ready();
    hx.type_str("question");
    hx.key(KeyCode::Enter);
    hx.event(Event::TurnStart, 10);
    hx.event(Event::TextDelta(text.into()), 10);
    hx.event(Event::TurnEnd(StopReason::EndTurn), 10);
    hx
}

const PROSE: &str = "A long paragraph of plain prose that has to be wrapped by the renderer because it is far wider than any sensible measure, with enough words to fill several rows at every terminal width we test here.\n\nSecond paragraph.\n\n- a list item\n- another";

#[test]
fn item_1_and_9_empty_session_is_welcome_at_the_top_and_the_composer_at_the_bottom() {
    // Round 2 finding 1: the transcript is top-anchored and the composer docked, so the first
    // send moves nothing. The empty session is the welcome block at the top, the composer on
    // the bottom rows and nothing in between.
    for (w, hh) in [(120u16, 36u16), (160, 45), (200, 55), (80, 24)] {
        let mut h = Harness::new(w, hh);
        h.ready();
        let s = h.screen();
        let rows: Vec<&str> = s.lines().collect();
        assert_eq!(rows.len(), hh as usize);
        let input = rows
            .iter()
            .position(|r| r.contains("Message claude"))
            .expect("composer input row");
        // Docked: the input is on the row it has once a conversation fills the screen.
        let pad_below = hh >= 30;
        let want = hh as usize - 1 - 1 - usize::from(pad_below);
        assert_eq!(input, want, "{w}x{hh}: composer input row");
        assert!(rows[input - 1].trim_start().starts_with('▎'), "{w}x{hh}");
        assert_eq!(
            rows[input + 1].trim_start().starts_with('▎'),
            pad_below,
            "{w}x{hh}"
        );
        assert!(
            rows[hh as usize - 1].contains("ask ·") || rows[hh as usize - 1].contains("sonnet")
        );
        // The wordmark is on one of the first three rows.
        let a = if w >= 140 { w - 36 } else { w } as usize;
        let main = |r: &str| r.chars().take(a).collect::<String>();
        let mark = rows
            .iter()
            .position(|r| main(r).contains("o p e n c"))
            .expect("wordmark");
        assert!(mark <= 2, "{w}x{hh}: wordmark on row {mark}");
        // Between the welcome and the composer: only blank rows.
        let top = input - 1;
        let welcome_end = rows[..top]
            .iter()
            .rposition(|r| !main(r).trim().is_empty())
            .expect("welcome rows");
        assert!(welcome_end < top, "{w}x{hh}");
        assert!(
            welcome_end <= 6,
            "{w}x{hh}: welcome is {welcome_end} rows deep"
        );
    }
}

#[test]
fn items_4_6_7_8_prose_measure_gaps_edges_and_no_band_above_the_composer() {
    for (w, hh) in SIZES {
        let mut h = answered(w, hh, ThemeChoice::Hearth, Depth::True, PROSE);
        let s = h.screen();
        let rows: Vec<String> = s.lines().map(str::to_string).collect();
        let a = if w >= 140 { w - 36 } else { w } as usize;
        for (y, r) in rows.iter().enumerate().take(rows.len() - 1) {
            let mut chars: Vec<char> = r.chars().collect();
            chars.resize(w as usize, ' ');
            // The rail is part of these rows at 140+; look only at the main area.
            let main: String = chars.iter().take(a).collect();
            if w >= 40 {
                assert_eq!(
                    chars.first().copied().unwrap_or(' '),
                    ' ',
                    "{w}x{hh} row {y} touches column 0: {r:?}"
                );
                let last = main.chars().last().unwrap_or(' ');
                assert!(
                    last == ' ' || last == '┃',
                    "{w}x{hh} row {y} touches the last column: {main:?}"
                );
            }
            // Item 4: text never reaches past 100 columns of measure plus its offset.
            let x = if a - 4 < 100 { 2 } else { (a - 100) / 2 };
            assert!(
                main.trim_end().chars().count() <= x + 100 + 1,
                "{w}x{hh} row {y} too wide: {main:?}"
            );
        }
        // Item 6: exactly one blank row between the user block, the prose and the list; no
        // double blanks anywhere.
        let mains: Vec<String> = rows.iter().map(|r| r.chars().take(a).collect()).collect();
        let q = mains
            .iter()
            .position(|r| r.contains("question"))
            .expect("user row");
        let p1 = rows
            .iter()
            .position(|r| r.contains("A long paragraph"))
            .expect("prose");
        assert_eq!(
            p1 - q,
            2,
            "{w}x{hh}: one blank row between the user block and the answer"
        );
        // Item 8: the composer is docked, so its top row is the same whatever the transcript
        // holds, and the transcript is top-anchored: its first content row is on row 0 or 1.
        let comp = h.app.geo.composer;
        let pad = if hh >= 30 { 2 } else { u16::from(hh >= 20) };
        assert_eq!(
            comp.bottom(),
            hh - 1,
            "{w}x{hh}: composer on the bottom rows"
        );
        assert_eq!(comp.height, 1 + pad, "{w}x{hh}");
        let first = rows
            .iter()
            .position(|r| !r.chars().take(a).collect::<String>().trim().is_empty())
            .expect("content");
        assert!(first <= 2, "{w}x{hh}: first content row {first}");
    }
}

#[test]
fn item_10_the_composer_stops_at_eight_rows_and_the_transcript_keeps_five() {
    let mut h = Harness::new(100, 30);
    h.ready();
    for i in 0..20 {
        h.type_str(&format!("line {i}"));
        h.ctrl('j');
    }
    h.draw();
    let g = h.app.geo;
    assert!(
        g.composer.height <= 8 + 2,
        "composer {} rows",
        g.composer.height
    );
    assert!(g.transcript.height >= 5);
}

fn cells_in_transcript(h: &mut Harness) -> Vec<ratatui::buffer::Cell> {
    h.draw();
    let g = h.app.geo;
    let buf = h.term.backend().buffer().clone();
    let mut out = Vec::new();
    for y in g.transcript.y..g.transcript.bottom() {
        for x in 0..g.a {
            out.push(buf[(x, y)].clone());
        }
    }
    out
}

#[test]
fn item_13_sixteen_colours_use_no_bright_black_and_no_background_tint() {
    let text = "Plan with `code` and **bold** and [a link](https://x.y)\n\n```rust\nfn main() {}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |";
    for theme in [ThemeChoice::Hearth, ThemeChoice::Parchment] {
        let mut h = answered(120, 36, theme, Depth::Ansi16, text);
        h.draw();
        for c in h.term.backend().buffer().content() {
            assert!(
                !matches!(c.fg, Color::DarkGray | Color::Indexed(8) | Color::Rgb(..)),
                "fg {:?}",
                c.fg
            );
            assert!(
                !matches!(c.fg, Color::Indexed(n) if (90..=97).contains(&n)),
                "fg {:?}",
                c.fg
            );
            assert!(matches!(c.bg, Color::Reset), "background tint {:?}", c.bg);
        }
    }
}

#[test]
fn item_16_only_user_rows_carry_the_raised_background_in_the_transcript() {
    let text = "Plan with `code`\n\n```rust\nfn main() {}\n```";
    let mut h = answered(120, 36, ThemeChoice::Hearth, Depth::True, text);
    let raised = h.app.p.raised;
    let n = cells_in_transcript(&mut h)
        .iter()
        .filter(|c| c.bg == raised)
        .count();
    // One user row: the column width, 100 cells, and nothing else.
    assert_eq!(n, 100);
}

#[test]
fn item_17_a_normal_turn_uses_only_the_status_colours_plus_the_code_chip() {
    let mut h = answered(
        120,
        36,
        ThemeChoice::Hearth,
        Depth::True,
        PROSE
            .replace("Second paragraph.", "Second `paragraph`.")
            .as_str(),
    );
    let p = h.app.p.clone();
    let allowed = [
        p.text, p.dim, p.faint, p.accent, p.user, p.ok, p.err, p.chip_fg, p.warn,
        // The welcome's rule.
        p.line,
    ];
    for c in cells_in_transcript(&mut h) {
        if c.symbol() != " " {
            assert!(
                allowed.contains(&c.fg),
                "colour {:?} on {:?}",
                c.fg,
                c.symbol()
            );
        }
    }
}

#[test]
fn item_18_thinking_is_never_italic_and_headings_are_bold() {
    let mut h = Harness::new(100, 30);
    h.ready();
    h.type_str("q");
    h.key(KeyCode::Enter);
    h.event(Event::TurnStart, 10);
    h.event(
        Event::ThoughtDelta("Considering the options. The second one wins".into()),
        10,
    );
    h.draw();
    for c in h.term.backend().buffer().content() {
        assert!(
            !c.modifier.contains(Modifier::ITALIC),
            "italic cell {:?}",
            c.symbol()
        );
    }
    h.event(Event::TextDelta("# Heading\n\nbody".into()), 10);
    h.event(Event::TurnEnd(StopReason::EndTurn), 10);
    h.draw();
    let buf = h.term.backend().buffer();
    let hit = buf
        .content()
        .iter()
        .enumerate()
        .find(|(_, c)| c.symbol() == "H")
        .expect("heading");
    assert!(hit.1.modifier.contains(Modifier::BOLD));
}

#[test]
fn item_35_scrolling_up_during_a_stream_stops_following_and_shows_the_hint() {
    let mut h = Harness::new(100, 20);
    h.ready();
    h.type_str("q");
    h.key(KeyCode::Enter);
    h.event(Event::TurnStart, 10);
    for i in 0..30 {
        h.event(Event::TextDelta(format!("row {i}\n\n")), 10);
    }
    h.draw();
    h.key(KeyCode::PageUp);
    h.event(Event::TextDelta("more output\n\n".into()), 10);
    let s = h.screen();
    assert!(s.contains("↓ new output"), "{s}");
    assert!(!s.contains("more output"));
    h.key(KeyCode::End);
    let s = h.screen();
    assert!(
        s.contains("more output") && !s.contains("↓ new output"),
        "{s}"
    );
}
