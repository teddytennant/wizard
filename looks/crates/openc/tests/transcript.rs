//! Transcript behaviour through the whole app: mouse selection and copy, hyperlinks, search,
//! folding and scroll anchors.

mod common;

use agent_core::{Event, StopReason};
use common::Harness;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

fn started(w: u16, h: u16) -> Harness {
    let mut hr = Harness::new(w, h);
    hr.ready();
    hr
}

fn turn(h: &mut Harness, user: &str, answer: &str) {
    h.type_str(user);
    h.key(KeyCode::Enter);
    h.event(Event::TurnStart, 10);
    h.event(Event::TextDelta(answer.into()), 10);
    h.event(Event::TurnEnd(StopReason::EndTurn), 10);
}

fn mouse(h: &mut Harness, kind: MouseEventKind, col: u16, row: u16) {
    h.t += 30;
    let now = h.now();
    h.app.on_mouse(
        MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        },
        now,
    );
}

fn down(h: &mut Harness, c: u16, r: u16) {
    mouse(h, MouseEventKind::Down(MouseButton::Left), c, r);
}
fn drag(h: &mut Harness, c: u16, r: u16) {
    mouse(h, MouseEventKind::Drag(MouseButton::Left), c, r);
}
fn up(h: &mut Harness, c: u16, r: u16) {
    mouse(h, MouseEventKind::Up(MouseButton::Left), c, r);
}

/// Screen position of the first occurrence of `needle`.
fn find(h: &mut Harness, needle: &str) -> (u16, u16) {
    let s = h.screen();
    for (y, line) in s.lines().enumerate() {
        if let Some(i) = line.find(needle) {
            return (line[..i].chars().count() as u16, y as u16);
        }
    }
    panic!("{needle:?} not on screen:\n{s}");
}

fn last_copy() -> Option<String> {
    openc::clipboard::captured().pop()
}

#[test]
fn dragging_across_text_copies_exactly_the_selected_cells_and_says_so() {
    let mut h = started(100, 30);
    turn(&mut h, "hi", "alpha beta gamma\n\nsecond paragraph here");
    let (x, y) = find(&mut h, "alpha beta gamma");
    down(&mut h, x + 6, y);
    drag(&mut h, x + 9, y);
    up(&mut h, x + 9, y);
    assert_eq!(last_copy().as_deref(), Some("beta"));
    // The status line carries the toast and the cells are tinted.
    let s = h.screen();
    assert!(s.contains("copied 4 chars"), "{s}");
    let buf = h.term.backend().buffer();
    assert_eq!(buf[(x + 6, y)].bg, h.app.p.sel_bg);
    assert_eq!(buf[(x + 9, y)].bg, h.app.p.sel_bg);
    assert_ne!(buf[(x + 10, y)].bg, h.app.p.sel_bg);
    assert_ne!(buf[(x + 5, y)].bg, h.app.p.sel_bg);
    // Any key drops the highlight.
    h.key(KeyCode::Char('x'));
    h.draw();
    assert_ne!(h.term.backend().buffer()[(x + 6, y)].bg, h.app.p.sel_bg);
}

#[test]
fn a_drag_over_two_paragraphs_copies_whole_middle_rows_and_strips_the_indent() {
    let mut h = started(100, 30);
    turn(&mut h, "hi", "alpha beta gamma\n\nsecond paragraph here");
    let (x, y) = find(&mut h, "alpha beta gamma");
    let (_, y2) = find(&mut h, "second paragraph here");
    down(&mut h, x + 6, y);
    drag(&mut h, x + 5, y2);
    up(&mut h, x + 5, y2);
    assert_eq!(
        last_copy().as_deref(),
        Some("beta gamma\n\nsecond"),
        "the first row from the press to its end, the blank row, the last row to the pointer"
    );
}

#[test]
fn double_click_selects_a_word_triple_click_the_block() {
    let mut h = started(100, 30);
    turn(&mut h, "hi", "alpha beta gamma\n\nsecond paragraph here");
    let (x, y) = find(&mut h, "alpha beta gamma");
    down(&mut h, x + 8, y);
    up(&mut h, x + 8, y);
    down(&mut h, x + 8, y);
    up(&mut h, x + 8, y);
    assert_eq!(last_copy().as_deref(), Some("beta"));
    down(&mut h, x + 8, y);
    up(&mut h, x + 8, y);
    assert_eq!(
        last_copy().as_deref(),
        Some("alpha beta gamma\n\nsecond paragraph here")
    );
}

#[test]
fn a_user_block_copies_without_its_bar() {
    let mut h = started(100, 30);
    turn(&mut h, "hello there", "ok");
    let (x, y) = find(&mut h, "hello there");
    down(&mut h, x - 2, y);
    drag(&mut h, x + 10, y);
    up(&mut h, x + 10, y);
    assert_eq!(last_copy().as_deref(), Some("hello there"));
}

#[test]
fn a_plain_click_on_a_tool_row_still_folds_it_and_copies_nothing() {
    use agent_core::{ToolCall, ToolKind, ToolStatus};
    let mut h = started(100, 30);
    h.event(Event::TurnStart, 10);
    h.event(
        Event::Tool(ToolCall {
            id: "a".into(),
            name: "Bash".into(),
            kind: ToolKind::Execute,
            title: "ls".into(),
            status: ToolStatus::Completed,
            output: Some("one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten".into()),
            ..Default::default()
        }),
        10,
    );
    h.event(Event::TurnEnd(StopReason::EndTurn), 10);
    let before = h.screen();
    let (x, y) = find(&mut h, "Bash");
    down(&mut h, x, y);
    up(&mut h, x, y);
    let after = h.screen();
    assert_ne!(before, after, "the row unfolded");
    assert_eq!(last_copy(), None);
}

fn nav(h: &mut Harness) {
    h.key(KeyCode::Esc);
    h.advance(400);
}

fn search(h: &mut Harness, q: &str) {
    h.key(KeyCode::Char('/'));
    h.type_str(q);
    h.key(KeyCode::Enter);
}

#[test]
fn search_counts_every_occurrence_steps_with_n_and_marks_the_current_one() {
    let mut h = started(100, 30);
    turn(&mut h, "hi", "a needle here and another needle there");
    turn(&mut h, "more", "third Needle");
    nav(&mut h);
    search(&mut h, "needle");
    let s = h.screen();
    assert!(s.contains("match 1/3"), "{s}");
    let (x, y) = find(&mut h, "needle here");
    let buf = h.term.backend().buffer();
    assert_eq!(buf[(x, y)].bg, h.app.p.cur_bg, "current hit");
    let (x2, _) = find(&mut h, "needle there");
    assert_eq!(h.term.backend().buffer()[(x2, y)].bg, h.app.p.accent_bg);
    h.key(KeyCode::Char('n'));
    assert!(h.screen().contains("match 2/3"));
    h.key(KeyCode::Char('n'));
    assert!(h.screen().contains("match 3/3"));
    h.key(KeyCode::Char('n'));
    assert!(h.screen().contains("match 1/3"), "wraps");
    h.key(KeyCode::Char('N'));
    assert!(h.screen().contains("match 3/3"), "N goes back, wrapping");
    // Smart case: a capital makes it exact.
    search(&mut h, "Needle");
    assert!(h.screen().contains("match 1/1"));
    search(&mut h, "zebra");
    assert!(h.screen().contains("no match"));
}

#[test]
fn search_finds_text_hidden_in_a_folded_tool_and_opens_it() {
    use agent_core::{ToolCall, ToolKind, ToolStatus};
    let mut h = started(100, 30);
    h.event(Event::TurnStart, 10);
    let out: String =
        (0..30).map(|i| format!("line {i}\n")).collect::<String>() + "the haystack needle\n";
    h.event(
        Event::Tool(ToolCall {
            id: "a".into(),
            name: "Bash".into(),
            kind: ToolKind::Execute,
            title: "cargo test".into(),
            status: ToolStatus::Completed,
            output: Some(out),
            ..Default::default()
        }),
        10,
    );
    h.event(Event::TextDelta("done".into()), 10);
    h.event(Event::TurnEnd(StopReason::EndTurn), 10);
    assert!(!h.screen().contains("haystack"));
    nav(&mut h);
    search(&mut h, "haystack");
    let s = h.screen();
    assert!(
        s.contains("haystack needle") && s.contains("match 1/1"),
        "{s}"
    );
}

#[test]
fn typing_a_search_follows_the_first_hit_before_enter() {
    let mut h = started(100, 20);
    for i in 0..12 {
        turn(
            &mut h,
            &format!("q{i}"),
            &format!("paragraph number {i}\n\nmore text {i}"),
        );
    }
    turn(&mut h, "last", "the zebra is here");
    h.draw();
    nav(&mut h);
    h.draw();
    for _ in 0..8 {
        h.key(KeyCode::Char('k'));
    }
    h.key(KeyCode::Char('/'));
    h.type_str("zebra");
    let s = h.screen();
    assert!(s.contains("the zebra is here"), "{s}");
    assert!(s.contains("1/1"), "count while typing: {s}");
}

fn words(n: usize) -> String {
    (0..n)
        .map(|i| format!("w{i}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn top_row(h: &mut Harness) -> String {
    let s = h.screen();
    s.lines().next().unwrap_or("").trim().to_string()
}

#[test]
fn resizing_and_back_keeps_the_same_words_on_top() {
    let mut h = started(100, 20);
    turn(&mut h, "hi", &words(2400));
    h.draw();
    for _ in 0..2 {
        h.key(KeyCode::PageUp);
    }
    let before = top_row(&mut h);
    let first: String = before.split_whitespace().next().unwrap().into();
    assert!(first.starts_with('w'), "{before:?}");
    h.resize(60, 20);
    let narrow = top_row(&mut h);
    assert!(
        narrow.split_whitespace().any(|w| w == first),
        "at 60 columns {narrow:?} should still show {first}"
    );
    h.resize(30, 20);
    let narrower = top_row(&mut h);
    assert!(
        narrower.split_whitespace().any(|w| w == first),
        "{narrower:?}"
    );
    h.resize(100, 20);
    assert_eq!(
        top_row(&mut h),
        before,
        "back at the original width, the same row"
    );
}

#[test]
fn folding_a_tool_keeps_its_row_on_the_same_screen_line() {
    use agent_core::{ToolCall, ToolKind, ToolStatus};
    let mut h = started(100, 30);
    for i in 0..10 {
        turn(&mut h, &format!("filler {i}"), "some text\n\nand more text");
    }
    h.event(Event::TurnStart, 10);
    let out: String = (0..12).map(|i| format!("out {i}\n")).collect();
    for (id, cmd) in [("a", "one"), ("b", "two")] {
        h.event(
            Event::Tool(ToolCall {
                id: id.into(),
                name: "Bash".into(),
                kind: ToolKind::Execute,
                title: cmd.into(),
                status: ToolStatus::Completed,
                output: Some(out.clone()),
                ..Default::default()
            }),
            10,
        );
    }
    h.event(Event::TextDelta("after the tools".into()), 10);
    h.event(Event::TurnEnd(StopReason::EndTurn), 10);
    h.draw();
    let (_, y0) = find(&mut h, "Bash    one");
    // Nav mode: walk the cursor onto the first tool block and open it.
    nav(&mut h);
    h.draw();
    for _ in 0..2 {
        h.key(KeyCode::Char('k'));
        h.draw();
    }
    let (x, y_before) = find(&mut h, "Bash    one");
    h.key(KeyCode::Char('o'));
    let (x2, y_after) = find(&mut h, "Bash    one");
    assert_eq!(
        (x, y_before),
        (x2, y_after),
        "the folded row stays put (was at {y0})"
    );
    assert!(h.screen().contains("out 0"));
    h.key(KeyCode::Char('o'));
    let (_, y_back) = find(&mut h, "Bash    one");
    assert_eq!(y_back, y_before);
}

fn tool_turn(h: &mut Harness, user: &str, n: usize) {
    use agent_core::{ToolCall, ToolKind, ToolStatus};
    h.type_str(user);
    h.key(KeyCode::Enter);
    h.event(Event::TurnStart, 10);
    for i in 0..n {
        h.event(
            Event::Tool(ToolCall {
                id: format!("{user}{i}"),
                name: "Bash".into(),
                kind: ToolKind::Execute,
                title: format!("cmd {user} {i}"),
                status: ToolStatus::Completed,
                output: Some("o1\no2\no3\no4\no5\no6\no7\no8\no9".into()),
                ..Default::default()
            }),
            10,
        );
    }
    h.event(Event::TextDelta(format!("answer for {user}")), 10);
    h.event(Event::TurnEnd(StopReason::EndTurn), 10);
}

#[test]
fn turn_jumps_fold_everything_and_copy_block_work_in_nav_mode() {
    let mut h = started(100, 40);
    tool_turn(&mut h, "alpha", 2);
    tool_turn(&mut h, "beta", 2);
    h.draw();
    nav(&mut h);
    h.draw();
    // `[` goes to the user message of the turn you are in, `[` again to the one before it.
    h.key(KeyCode::Char('['));
    h.key(KeyCode::Char('['));
    h.key(KeyCode::Char('y'));
    assert_eq!(last_copy().as_deref(), Some("alpha"));
    h.key(KeyCode::Char(']'));
    h.key(KeyCode::Char('y'));
    assert_eq!(last_copy().as_deref(), Some("beta"));
    // `O` opens every tool body of the turn, and again closes them all.
    h.key(KeyCode::Char('O'));
    let s = h.screen();
    assert_eq!(s.matches("▾ Bash").count(), 2, "{s}");
    h.key(KeyCode::Char('O'));
    let s = h.screen();
    assert_eq!(s.matches("▸ Bash").count(), 4, "{s}");
    // `Y` copies the turn as markdown.
    h.key(KeyCode::Char('Y'));
    let t = last_copy().unwrap();
    assert!(
        t.starts_with("> beta") && t.contains("answer for beta"),
        "{t}"
    );
    // ctrl+o opens everything everywhere, and the status line says so.
    h.advance(5000);
    h.ctrl('o');
    let s = h.screen();
    assert!(s.contains("detail"), "{s}");
    assert!(s.matches("▾ Bash").count() >= 2, "{s}");
    h.ctrl('o');
    assert_eq!(h.screen().matches("▾ Bash").count(), 0);
}

/// `find` on the buffer cells, with the OSC 8 wrapping taken off each glyph.
fn find_cells(h: &mut Harness, needle: &str) -> (u16, u16) {
    h.draw();
    let buf = h.term.backend().buffer();
    let w = buf.area.width as usize;
    let glyph = |sym: &str| -> String {
        match sym.split_once("\x1b\\") {
            Some((_, rest)) => rest.split("\x1b]8;;").next().unwrap_or("").to_string(),
            None => sym.to_string(),
        }
    };
    for (y, row) in buf.content().chunks(w).enumerate() {
        let line: String = row.iter().map(|c| glyph(c.symbol())).collect();
        if let Some(i) = line.find(needle) {
            return (line[..i].chars().count() as u16, y as u16);
        }
    }
    panic!("{needle:?} not in the buffer");
}

#[test]
fn urls_and_existing_paths_become_osc8_cells_and_other_text_does_not() {
    openc::ui::links::init(true, env!("CARGO_MANIFEST_DIR"));
    let mut h = started(100, 30);
    turn(
        &mut h,
        "hi",
        "see https://example.com/docs and `Cargo.toml` or src/nothing.rs ok",
    );
    let (x, y) = find_cells(&mut h, "https://example.com");
    let buf = h.term.backend().buffer();
    let sym = buf[(x, y)].symbol().to_string();
    assert!(
        sym.starts_with("\x1b]8;id=")
            && sym.contains(";https://example.com/docs\x1b\\h\x1b]8;;\x1b\\"),
        "{sym:?}"
    );
    // The last cell of the URL is wrapped too, the one after it is plain.
    let n = "https://example.com/docs".len() as u16;
    assert!(buf[(x + n - 1, y)].symbol().contains("\x1b]8;id="));
    assert_eq!(buf[(x + n, y)].symbol(), " ");
    let (cx, cy) = find_cells(&mut h, "Cargo.toml");
    let sym = h.term.backend().buffer()[(cx, cy)].symbol().to_string();
    assert!(
        sym.contains(";file:///") && sym.contains("/Cargo.toml"),
        "{sym:?}"
    );
    let (nx, ny) = find_cells(&mut h, "src/nothing.rs");
    assert_eq!(h.term.backend().buffer()[(nx, ny)].symbol(), "s");
    // No stray underline colour is left for the terminal to draw.
    for c in h.term.backend().buffer().content() {
        assert_eq!(c.underline_color, ratatui::style::Color::Reset);
    }
}

#[test]
fn only_one_spinner_is_on_screen_even_with_two_running_tools() {
    use agent_core::{ToolCall, ToolKind, ToolStatus};
    let mut h = started(100, 30);
    h.type_str("go");
    h.key(KeyCode::Enter);
    h.event(Event::TurnStart, 10);
    for (id, name) in [("a", "Bash"), ("b", "Task")] {
        h.event(
            Event::Tool(ToolCall {
                id: id.into(),
                name: name.into(),
                kind: ToolKind::Execute,
                title: id.into(),
                status: ToolStatus::Running,
                ..Default::default()
            }),
            10,
        );
    }
    let s = h.screen();
    let spins = s.chars().filter(|c| "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(*c)).count();
    assert_eq!(spins, 1, "{s}");
    // The status line keeps a static busy mark instead of a second spinner.
    assert!(s.lines().last().unwrap().contains('⠿'), "{s}");
}
