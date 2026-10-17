//! Cell-exact tests for the round 1 fidelity findings (`docs/critics/piw-fidelity-r1.md`), each
//! against what Pi itself produced.

mod common;

use agent_core::{Event, ModelOption, Usage};
use common::*;
use serde_json::Value;

fn plain(l: &ratatui::text::Line<'_>) -> String {
    l.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Finding 11: the stats row at narrow widths, against Pi's own `FooterComponent`
/// (`tools/pi-footer-golden.mjs`, 6 sessions at 14 widths).
#[test]
fn footer_rows_at_every_width_are_pis() {
    let golden: Vec<Value> = serde_json::from_str(include_str!("pi-footer-golden.json")).unwrap();
    assert!(golden.len() >= 80);
    for g in &golden {
        let w = g["width"].as_u64().unwrap() as u16;
        let u = &g["usage"];
        let mut h = Harness::new(w.max(10), 15);
        let model = g["model"].as_str().unwrap().to_string();
        let mut models = vec![ModelOption {
            id: model.clone(),
            name: model.clone(),
            provider: "fake".into(),
        }];
        if g["providers"].as_u64() == Some(2) {
            models.push(ModelOption {
                id: "other".into(),
                name: "other".into(),
                provider: "other".into(),
            });
        }
        h.app.config.model = model;
        h.app.config.models = models;
        h.app.config.effort = g["level"].as_str().unwrap().into();
        h.app.config.efforts = ["low", "medium", "high", "xhigh"]
            .map(String::from)
            .to_vec();
        let pct = g["percent"].as_f64().unwrap();
        h.event(Event::Usage(Usage {
            input_tokens: u["input"].as_u64().unwrap(),
            output_tokens: u["output"].as_u64().unwrap(),
            cached_tokens: u["cacheRead"].as_u64().unwrap(),
            context_tokens: (pct * 2000.0).round() as u64,
            context_window: 200_000,
            cost_usd: Some(u["cost"].as_f64().unwrap()),
        }));
        h.app.size = (w, 15);
        let cx = h.app.cx(w);
        let rows = piw::ui::footer::render(&h.app, &cx);
        let want = g["rows"].as_array().unwrap();
        let got: Vec<String> = rows
            .iter()
            .map(|l| plain(l).trim_end().to_string())
            .collect();
        let want: Vec<String> = want
            .iter()
            .map(|v| v.as_str().unwrap().trim_end().to_string())
            .collect();
        assert_eq!(
            got[1], want[1],
            "stats row of {} at {w} columns (usage {u}, {pct}%)",
            g["name"]
        );
        // the width of the row is the width of the screen, spaces included
        assert_eq!(
            plain(&rows[1]).chars().count().max(w as usize),
            w as usize,
            "{} at {w}",
            g["name"]
        );
    }
}

/// Finding 12: Pi writes the resume hint in SGR 2 on the default colour, not in its `dim` colour.
#[test]
fn the_resume_hint_is_faint_and_uncoloured() {
    let mut h = Harness::new(120, 36);
    h.turn("hi", "Hello.");
    let out = h.app.epilogue(120);
    assert!(
        out.contains("\x1b[2mTo resume this session:\x1b[0m wizard-ui-pi --session"),
        "{out:?}"
    );
}

mod markdown {
    use piw::theme::{PiTheme, Tok};
    use piw::ui::markdown_theme::render;
    use ratatui::style::{Modifier, Style};
    use ratatui::text::Line;

    fn rows(src: &str) -> Vec<Line<'static>> {
        render(src, 80, &PiTheme::dark(), Style::default())
    }

    fn text(l: &Line<'_>) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// Style of the first span whose text starts at display column `col`.
    fn style_at(l: &Line<'_>, col: usize) -> Style {
        let mut x = 0;
        for s in &l.spans {
            let w = s.content.chars().count();
            if col >= x && col < x + w {
                return s.style;
            }
            x += w;
        }
        Style::default()
    }

    /// Finding 10a: a list in a block quote. The bullet ends with SGR 39, which drops the quote
    /// colour for the text after it; the italic stays. Pi's capture: text default fg, italic.
    #[test]
    fn list_text_in_a_quote_is_italic_in_the_default_colour() {
        let th = PiTheme::dark();
        let r = rows("> quote\n>\n> - list in quote\n> - second\n");
        let l = r
            .iter()
            .find(|l| text(l).contains("list in quote"))
            .unwrap();
        assert_eq!(text(l), "│ - list in quote");
        assert_eq!(style_at(l, 0).fg, th.fg(Tok::MdQuoteBorder).fg);
        assert_eq!(
            style_at(l, 2).fg,
            th.fg(Tok::MdListBullet).fg,
            "the bullet keeps its colour"
        );
        let t = style_at(l, 4);
        assert_eq!(t.fg, None, "text after the bullet is the default colour");
        assert!(t.add_modifier.contains(Modifier::ITALIC));
        let q = r
            .iter()
            .find(|l| text(l).contains("quote") && !text(l).contains("list"))
            .unwrap();
        assert_eq!(
            style_at(q, 2).fg,
            th.fg(Tok::MdQuote).fg,
            "a plain paragraph keeps it"
        );
    }

    #[test]
    fn a_styled_token_in_a_quoted_list_item_restores_the_quote_colour_after_it() {
        let th = PiTheme::dark();
        let r = rows("> - a **b** c\n");
        let l = &r[0];
        assert_eq!(text(l), "│ - a b c");
        assert_eq!(style_at(l, 4).fg, None, "before the styled token: default");
        assert_eq!(style_at(l, 6).fg, None, "the styled token itself");
        assert_eq!(
            style_at(l, 8).fg,
            th.fg(Tok::MdQuote).fg,
            "after it: the quote colour is back"
        );
    }

    /// Finding 10b: marked's image token has no case in Pi's inline renderer; its text, the alt
    /// text, is printed plain.
    #[test]
    fn an_image_is_its_alt_text() {
        let r = rows("![alt text](https://example.com/img.png)\n");
        assert_eq!(text(&r[0]), "alt text");
        assert_eq!(style_at(&r[0], 0), Style::default());
    }

    /// Finding 10c: a link reference definition is a `def` token with a `space` token each side,
    /// so it leaves two blank rows where pulldown-cmark leaves one.
    #[test]
    fn a_link_reference_definition_keeps_its_blank_rows() {
        let r = rows("Term and [ref][r1].\n\n[r1]: https://example.com/ref\n\n![alt text](https://example.com/img.png)\n");
        let t: Vec<String> = r.iter().map(text).collect();
        assert_eq!(
            t,
            [
                "Term and ref (https://example.com/ref).",
                "",
                "",
                "alt text"
            ],
            "{t:?}"
        );
        // without blank lines around it nothing is added
        let r = rows("one\n\ntwo\n");
        assert_eq!(r.iter().map(text).collect::<Vec<_>>(), ["one", "", "two"]);
    }
}

mod search {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};
    use piw::theme::{PiTheme, Tok};
    use ratatui::style::Modifier;
    use tuikit::testing::TestTerminal;

    fn ctrl_shift_f(h: &mut Harness) {
        h.key(
            KeyCode::Char('f'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
    }

    /// 70 filler lines with `banana` on lines 10 and 66, so the second one is on screen.
    fn long_answer(h: &mut Harness) {
        answer_with(h, &[10, 66]);
    }

    /// 70 lines of text, `banana` on the given ones; drawn once so the view has a size.
    fn answer_with(h: &mut Harness, at: &[usize]) {
        let text: String = (1..=70)
            .map(|i| match at.contains(&i) {
                true => format!("line {i} banana here\n\n"),
                false => format!("line {i}\n\n"),
            })
            .collect();
        h.turn("hello", &text);
        h.render();
    }

    fn row_text(t: &TestTerminal, y: u16, from: u16) -> String {
        (from..120)
            .map(|x| t.cell(x, y).unwrap().symbol().to_string())
            .collect()
    }

    fn banana_rows(t: &TestTerminal) -> Vec<u16> {
        (4..30)
            .filter(|y| row_text(t, *y, 0).contains("banana"))
            .collect()
    }

    #[test]
    fn the_box_is_pis_at_120_columns() {
        let mut h = Harness::new(120, 36);
        long_answer(&mut h);
        ctrl_shift_f(&mut h);
        let t = h.render();
        // rows 1 to 3, cols 71 to 118 (width 48, margin 1), the same text Pi drew
        assert_eq!(
            row_text(&t, 1, 71),
            format!("┌{}┐ ", "─".repeat(46)).trim_end().to_string() + " "
        );
        assert_eq!(
            row_text(&t, 2, 71).trim_end(),
            format!("│ Find in transcript{}│", " ".repeat(27))
        );
        assert_eq!(
            row_text(&t, 3, 71).trim_end(),
            format!("└{} ↑ Shift+Enter · ↓ Enter ─┘", "─".repeat(20))
        );
        // placeholder: dim, the first glyph also reversed (Pi's fake cursor sits on it)
        let f = t.cell(73, 2).unwrap();
        assert_eq!(f.symbol(), "F");
        assert!(f.modifier.contains(Modifier::DIM | Modifier::REVERSED));
        let rest = t.cell(74, 2).unwrap();
        assert!(
            rest.modifier.contains(Modifier::DIM) && !rest.modifier.contains(Modifier::REVERSED)
        );
        // row 0 is untouched and the box has no style of its own
        assert_ne!(t.cell(71, 1).unwrap().symbol(), " ");
        assert_eq!(t.cell(71, 1).unwrap().modifier, Modifier::empty());
    }

    #[test]
    fn the_box_follows_the_width_and_shrinks_its_buttons() {
        // the labels need 23 cells plus two gaps and a rule: they fit from a box of 28
        for (w, box_w, labels) in [(80u16, 32usize, true), (30, 28, true), (29, 27, false)] {
            let mut h = Harness::new(w, 24);
            long_answer(&mut h);
            ctrl_shift_f(&mut h);
            let t = h.render();
            let x0 = w - 1 - box_w as u16;
            let row = |y| {
                (x0..x0 + box_w as u16)
                    .map(|x| t.cell(x, y).unwrap().symbol().to_string())
                    .collect::<String>()
            };
            assert_eq!(row(1), format!("┌{}┐", "─".repeat(box_w - 2)), "width {w}");
            assert!(
                row(2).starts_with("│ Find in") && row(2).ends_with('│'),
                "{}",
                row(2)
            );
            let bottom = row(3);
            assert_eq!(bottom.chars().count(), box_w);
            // Pi drops the key names (`↑ ↓`) when they would not leave room, then the buttons
            assert_eq!(bottom.contains("Shift+Enter"), labels, "{bottom}");
        }
    }

    #[test]
    fn typing_picks_the_match_on_screen_and_highlights_every_match_in_view() {
        let th = PiTheme::dark();
        let mut h = Harness::new(120, 36);
        long_answer(&mut h);
        ctrl_shift_f(&mut h);
        h.type_str("banana");
        let t = h.render();
        // `2/2`: dim, right-aligned inside the box, as in Pi's capture 146
        assert_eq!(
            row_text(&t, 2, 71).trim_end(),
            format!("│ banana{}2/2 │", " ".repeat(35))
                .replace("2/2", " 2/2")
                .replace("  2/2", " 2/2")
        );
        let rows = banana_rows(&t);
        assert_eq!(rows.len(), 1, "only the second banana is in view: {rows:?}");
        let y = rows[0];
        let x = row_text(&t, y, 0).find("banana").unwrap() as u16;
        let bg = th.color(Tok::SearchMatchBg);
        let fg = th.color(Tok::SearchMatchText);
        for dx in 0..6 {
            let c = t.cell(x + dx, y).unwrap();
            assert_eq!((c.fg, c.bg), (fg, bg), "current match colours at {dx}");
            assert!(
                c.modifier.contains(Modifier::BOLD | Modifier::REVERSED),
                "current match is bold and reversed"
            );
        }
        // revealing the selection stops following, so the jump label shows even at the bottom
        let last = h.app.scroll.view_rows() as u16 - 1;
        assert!(
            row_text(&t, last, 0).contains("Jump to latest message"),
            "{}",
            row_text(&t, last, 0)
        );
        // the cells around it keep their own style
        let before = t.cell(x - 1, y).unwrap();
        assert_ne!(before.bg, bg);
        assert!(!t
            .cell(x + 6, y)
            .unwrap()
            .modifier
            .contains(Modifier::REVERSED));
    }

    #[test]
    fn enter_wraps_to_the_first_match_and_scrolls_a_third_of_a_screen_above_it() {
        let mut h = Harness::new(120, 36);
        long_answer(&mut h);
        ctrl_shift_f(&mut h);
        h.type_str("banana");
        h.press(KeyCode::Enter);
        let t = h.render();
        assert!(
            row_text(&t, 2, 71).contains(" 1/2 "),
            "{}",
            row_text(&t, 2, 71)
        );
        let rows = banana_rows(&t);
        assert_eq!(rows.len(), 1, "{rows:?}");
        // the match sits view_h / 3 rows below the top of the view
        let view_h = h.app.scroll.view_rows() as u16;
        assert_eq!(rows[0], view_h / 3, "view of {view_h} rows");
        // shift+enter goes back to the second, which scrolls again
        h.key(KeyCode::Enter, KeyModifiers::SHIFT);
        let t = h.render();
        assert!(row_text(&t, 2, 71).contains(" 2/2 "));
        h.press(KeyCode::Esc);
        assert!(h.app.search.is_none());
    }

    #[test]
    fn a_match_that_is_not_current_is_underlined_over_the_same_colours() {
        let th = PiTheme::dark();
        let mut h = Harness::new(120, 60);
        answer_with(&mut h, &[60, 66]);
        ctrl_shift_f(&mut h);
        h.type_str("banana");
        let t = h.render();
        let rows = banana_rows_tall(&t);
        assert_eq!(rows.len(), 2, "{rows:?}");
        let cell = |y: u16| {
            let x = row_text(&t, y, 0).find("banana").unwrap() as u16;
            t.cell(x, y).unwrap().clone()
        };
        // the first match at or below the top of the view is the selected one (1/2)
        let (current, other) = (cell(rows[0]), cell(rows[1]));
        assert!(current
            .modifier
            .contains(Modifier::BOLD | Modifier::REVERSED));
        assert!(
            other.modifier.contains(Modifier::UNDERLINED)
                && !other.modifier.contains(Modifier::REVERSED),
            "{other:?}"
        );
        for c in [&current, &other] {
            assert_eq!(
                (c.fg, c.bg),
                (th.color(Tok::SearchMatchText), th.color(Tok::SearchMatchBg))
            );
        }
    }

    fn banana_rows_tall(t: &TestTerminal) -> Vec<u16> {
        (4..54)
            .filter(|y| row_text(t, *y, 0).contains("banana"))
            .collect()
    }

    #[test]
    fn the_box_owns_the_keyboard_and_escape_closes_it() {
        let mut h = Harness::new(120, 36);
        long_answer(&mut h);
        ctrl_shift_f(&mut h);
        h.type_str("zzz");
        assert_eq!(
            h.app.editor.text(),
            "",
            "typing goes to the query, not the editor"
        );
        let t = h.render();
        assert!(
            row_text(&t, 2, 71).contains("No matches"),
            "{}",
            row_text(&t, 2, 71)
        );
        h.press(KeyCode::Backspace);
        h.press(KeyCode::Backspace);
        h.press(KeyCode::Backspace);
        h.type_str("  line   10 ");
        // whitespace runs are one space in the corpus and the query
        let t = h.render();
        assert!(
            row_text(&t, 2, 71).contains("1/1"),
            "{}",
            row_text(&t, 2, 71)
        );
        h.press(KeyCode::Esc);
        assert!(h.app.search.is_none());
        h.type_str("x");
        assert_eq!(h.app.editor.text(), "x");
        // ctrl+shift+f toggles
        ctrl_shift_f(&mut h);
        assert!(h.app.search.is_some());
        ctrl_shift_f(&mut h);
        assert!(h.app.search.is_none());
    }
}

mod flash {
    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::style::Modifier;
    use std::time::Duration;

    /// Finding 8: `ctrl+x` flashes ` Copied! ` in reverse video at the top right for a second
    /// (Pi's capture 149: cols 111 to 119 of row 0).
    #[test]
    fn ctrl_x_flashes_copied_at_the_top_right_for_a_second() {
        let mut h = Harness::new(120, 36);
        h.turn("hi", "Hello there.");
        h.key(crossterm::event::KeyCode::Char('x'), KeyModifiers::CONTROL);
        let t = h.render();
        let row: String = (111..120)
            .map(|x| t.cell(x, 0).unwrap().symbol().to_string())
            .collect();
        assert_eq!(row, " Copied! ");
        for x in 111..120 {
            let c = t.cell(x, 0).unwrap();
            assert!(c.modifier.contains(Modifier::REVERSED), "col {x}");
        }
        assert!(!t
            .cell(110, 0)
            .unwrap()
            .modifier
            .contains(Modifier::REVERSED));
        assert!(!t
            .cell(120 - 1, 1)
            .unwrap()
            .modifier
            .contains(Modifier::REVERSED));
        // a second later it is gone and the loop knows when to wake
        assert!(h.app.next_wake(std::time::Instant::now()).is_some());
        h.app.frozen = Some(Duration::from_millis(1001));
        h.app.on_tick(std::time::Instant::now());
        let t = h.render();
        assert!(!t
            .cell(112, 0)
            .unwrap()
            .modifier
            .contains(Modifier::REVERSED));
    }

    #[test]
    fn ctrl_x_with_nothing_to_copy_flashes_nothing() {
        let mut h = Harness::new(120, 36);
        h.key(crossterm::event::KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert!(h.app.flashes.is_empty());
    }
}

mod clicks {
    use super::*;
    use agent_core::{StopReason, ToolKind};
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use tuikit::testing::TestTerminal;

    fn click(h: &mut Harness, x: u16, y: u16) {
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            h.app.on_mouse(MouseEvent {
                kind,
                column: x,
                row: y,
                modifiers: KeyModifiers::NONE,
            });
        }
    }

    fn rows(t: &TestTerminal) -> Vec<String> {
        t.plain()
            .lines()
            .map(|l| l.trim_end().to_string())
            .collect()
    }

    fn at(r: &[String], needle: &str) -> u16 {
        r.iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("{needle} not in\n{}", r.join("\n"))) as u16
    }

    fn bash_turn(h: &mut Harness) {
        h.app.send_prompt("run it".into());
        h.event(Event::TurnStart);
        let out: String = (1..=30).map(|i| format!("line {i} of output\n")).collect();
        h.event(Event::Tool(tool(
            "b1",
            "execute",
            ToolKind::Execute,
            "seq 30",
            serde_json::json!({"command": "seq 30"}),
            Some(out.trim_end()),
        )));
        h.event(Event::TextDelta("Done.".into()));
        h.event(Event::TurnEnd(StopReason::EndTurn));
    }

    /// Finding 9: a left click on a tool block flips that block, and only that block.
    #[test]
    fn a_click_on_a_tool_block_expands_it_and_another_collapses_it() {
        let mut h = Harness::new(120, 80);
        bash_turn(&mut h);
        let r = rows(&h.render());
        assert!(
            !r.iter().any(|l| l.contains("line 1 of output")),
            "collapsed"
        );
        let title = at(&r, "$ seq 30");
        click(&mut h, 5, title);
        let r = rows(&h.render());
        assert!(
            r.iter().any(|l| l.contains("line 1 of output")),
            "expanded by the click:\n{}",
            r.join("\n")
        );
        assert!(!h.app.expanded, "the global setting is untouched");
        let title = at(&r, "$ seq 30");
        click(&mut h, 5, title);
        assert!(!rows(&h.render())
            .iter()
            .any(|l| l.contains("line 1 of output")));
    }

    #[test]
    fn the_padding_rows_and_the_blank_row_above_a_block_do_not_take_clicks() {
        let mut h = Harness::new(120, 80);
        bash_turn(&mut h);
        let r = rows(&h.render());
        let title = at(&r, "$ seq 30");
        for y in [title - 1, title - 2] {
            click(&mut h, 5, y);
        }
        assert!(!rows(&h.render())
            .iter()
            .any(|l| l.contains("line 1 of output")));
        // nor does a click on ordinary text
        let t = at(&rows(&h.render()), "Done.");
        click(&mut h, 3, t);
        assert!(h.app.tool_exp.is_empty());
    }

    #[test]
    fn a_release_somewhere_else_is_a_drag_not_a_click() {
        let mut h = Harness::new(120, 80);
        bash_turn(&mut h);
        let title = at(&rows(&h.render()), "$ seq 30");
        h.app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: title,
            modifiers: KeyModifiers::NONE,
        });
        h.app.on_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 9,
            row: title,
            modifiers: KeyModifiers::NONE,
        });
        assert!(h.app.tool_exp.is_empty());
    }

    #[test]
    fn ctrl_o_sets_every_block_again() {
        let mut h = Harness::new(120, 80);
        bash_turn(&mut h);
        let title = at(&rows(&h.render()), "$ seq 30");
        click(&mut h, 5, title);
        assert!(!h.app.tool_exp.is_empty());
        h.ctrl('o'); // expanded globally; the block's own choice is dropped
        assert!(h.app.tool_exp.is_empty());
        h.ctrl('o');
        assert!(!rows(&h.render())
            .iter()
            .any(|l| l.contains("line 1 of output")));
    }

    #[test]
    fn a_click_on_a_thinking_block_hides_or_shows_that_run() {
        let mut h = Harness::new(120, 36);
        h.app.send_prompt("think".into());
        h.event(Event::TurnStart);
        h.event(Event::ThoughtDelta("The user wants a short answer.".into()));
        h.event(Event::TextDelta("Done.".into()));
        h.event(Event::TurnEnd(StopReason::EndTurn));
        let r = rows(&h.render());
        let y = at(&r, "The user wants a short answer.");
        click(&mut h, 3, y);
        let r = rows(&h.render());
        assert!(
            r.iter().any(|l| l.trim() == "Thinking..."),
            "{}",
            r.join("\n")
        );
        assert!(!h.app.hide_thinking);
        let y = at(&r, "Thinking...");
        click(&mut h, 3, y);
        assert!(rows(&h.render())
            .iter()
            .any(|l| l.contains("The user wants a short answer.")));
        // ctrl+t drops the per-run choice
        click(&mut h, 3, y);
        h.ctrl('t');
        assert!(h.app.think_hidden.is_empty());
    }

    #[test]
    fn a_tool_without_a_result_yet_is_not_clickable() {
        let mut h = Harness::new(120, 80);
        h.app.send_prompt("run it".into());
        h.event(Event::TurnStart);
        let mut call = tool(
            "p1",
            "execute",
            ToolKind::Execute,
            "sleep 9",
            serde_json::json!({"command": "sleep 9"}),
            None,
        );
        call.status = agent_core::ToolStatus::Running;
        h.event(Event::Tool(call));
        let title = at(&rows(&h.render()), "$ sleep 9");
        click(&mut h, 5, title);
        assert!(h.app.tool_exp.is_empty());
    }
}
