//! The dock must not read as the end of the block above it (round 2 finding 2). The edge row
//! over the tray is checked at every depth in both themes, against a transcript whose last
//! rows are a code block, the case the critic drew.

mod common;

use agent_core::{Event, Todo, TodoStatus};
use common::Harness;
use crossterm::event::KeyCode;
use openc::app::ThemeChoice;
use openc::palette::Depth;
use ratatui::style::Color;

const ANSWER: &str = "Here is the plan.\n\n```rust\nfn main() {\n    let name = std::env::args().nth(1).unwrap_or_else(|| \"world\".into());\n    println!(\"hello, {name}\");\n    println!(\"again\");\n    println!(\"and again\");\n    println!(\"and again\");\n    println!(\"and again\");\n    println!(\"and again\");\n    println!(\"and again\");\n    println!(\"and again\");\n    println!(\"and again\");\n    println!(\"and again\");\n    println!(\"and again\");\n    println!(\"and again\");\n    println!(\"and again\");\n    println!(\"and again\");\n    println!(\"and again\");\n}\n```\n";

fn scene(w: u16, hh: u16, theme: ThemeChoice, depth: Depth) -> Harness {
    let mut h = Harness::with_theme(w, hh, theme, depth);
    h.ready();
    h.type_str("go");
    h.key(KeyCode::Enter);
    h.event(Event::TurnStart, 10);
    h.event(Event::TextDelta(ANSWER.into()), 10);
    h.event(
        Event::Todos(vec![
            Todo {
                text: "Read the code".into(),
                status: TodoStatus::Completed,
            },
            Todo {
                text: "Fix greeting".into(),
                status: TodoStatus::InProgress,
            },
            Todo {
                text: "Add tests".into(),
                status: TodoStatus::Pending,
            },
        ]),
        10,
    );
    h
}

#[test]
fn the_dock_has_an_edge_and_its_own_surface_at_dark_light_256_and_16_colours() {
    for theme in [ThemeChoice::Hearth, ThemeChoice::Parchment] {
        for depth in [Depth::True, Depth::Ansi256, Depth::Ansi16, Depth::Mono] {
            let mut h = scene(100, 30, theme, depth);
            h.draw();
            let g = h.app.geo;
            assert!(g.dock.height >= 2, "{theme:?} {depth:?}: dock {:?}", g.dock);
            let buf = h.term.backend().buffer().clone();
            let x = g.x + 8;
            let edge = &buf[(x, g.dock.y)];
            let body = &buf[(x, g.dock.y + 1)];
            let tinted = matches!(depth, Depth::True | Depth::Ansi256);
            if tinted {
                // The row between the content and the tray is the page colour with a half
                // block: the tray's colour is only the foreground there.
                assert_eq!(edge.symbol(), "▄", "{theme:?} {depth:?}");
                assert_ne!(
                    edge.bg, body.bg,
                    "{theme:?} {depth:?}: edge row is part of the tray"
                );
                assert_eq!(
                    edge.fg, body.bg,
                    "{theme:?} {depth:?}: half block is the tray colour"
                );
                // The tray is not the code block's colour, and not the composer's.
                let code = (0..g.dock.y)
                    .rev()
                    .map(|y| buf[(x, y)].bg)
                    .find(|bg| *bg != edge.bg)
                    .expect("a tinted row above the dock");
                assert_ne!(
                    body.bg, code,
                    "{theme:?} {depth:?}: tray has the code colour"
                );
                let comp = buf[(x, g.composer.y)].bg;
                assert_ne!(
                    body.bg, comp,
                    "{theme:?} {depth:?}: tray has the composer colour"
                );
            } else {
                // No backgrounds exist: the edge is a rule.
                assert_eq!(edge.symbol(), "─", "{theme:?} {depth:?}");
                assert_eq!(body.bg, Color::Reset, "{theme:?} {depth:?}");
            }
        }
    }
}

#[test]
fn a_dock_costs_one_extra_row_for_its_edge_and_none_when_empty() {
    let mut h = Harness::new(100, 30);
    h.ready();
    h.draw();
    assert_eq!(h.app.geo.dock.height, 0);
    let mut s = scene(100, 30, ThemeChoice::Hearth, Depth::True);
    s.draw();
    // One edge row, a todo header and the two other items (the list is open from 30 rows).
    assert_eq!(s.app.geo.dock.height, 1 + 4);
}
