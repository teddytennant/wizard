//! The composer: chips and their previews, the history panel, the `@` picker and the stash, at
//! the three sizes the captures use. `GROKW_DUMP_DIR` writes `.ansi` files for
//! `tools/grokw-cmp.py`; `UPDATE_SNAPSHOTS=1` rewrites the text snapshots.

mod common;

use common::*;
use crossterm::event::{KeyCode, KeyModifiers};

const SIZES: [(u16, u16); 3] = [(120, 36), (150, 42), (80, 24)];

fn each(name: &str, f: impl Fn(&mut Harness)) {
    for (w, h) in SIZES {
        let mut hn = Harness::new(w, h);
        hn.app.tr.usage.context_tokens = 1_475;
        f(&mut hn);
        hn.dump(&format!("{name}-{w}x{h}"));
        assert_snapshot(&format!("{name}-{w}x{h}"), &hn.text());
    }
}

fn lines(n: usize) -> String {
    (1..=n).map(|i| format!("pasted line {i}\n")).collect()
}

#[test]
fn paste_chip() {
    each("paste-chip", |h| h.app.on_paste(&lines(40)));
}

#[test]
fn paste_chip_with_long_lines() {
    each("paste-chip-big", |h| {
        let mut t = String::from("Reply with exactly the markdown between BEGIN and END, character for character, with whatever follows it on the same line\n\nBEGIN\n");
        for i in 0..52 {
            t.push_str(&format!("line {i}\n"));
        }
        t.push_str("echo \"hi\" | grep -v 'x' && ls -la\n```\nEND");
        h.app.on_paste(&t);
    });
}

#[test]
fn short_paste_stays_inline() {
    let mut h = Harness::new(120, 36);
    h.app.on_paste("one\ntwo\nthree");
    assert_eq!(h.app.ed.text(), "one\ntwo\nthree");
    assert!(h.app.inp.chips.is_empty());
}

#[test]
fn chip_expands_into_the_message_and_enter_on_it_inlines() {
    let mut h = Harness::new(120, 36);
    h.type_str("see ");
    h.app.on_paste(&lines(5));
    assert_eq!(h.app.ed.text(), "see [Pasted: 5 lines]");
    // Left lands on the chip's first cell; Enter inlines it
    h.press(KeyCode::Left);
    h.press(KeyCode::Enter);
    assert_eq!(h.app.ed.text(), format!("see {}", lines(5)));
    assert!(h.app.inp.chips.is_empty());
}

#[test]
fn send_replaces_chips_with_their_content() {
    let mut h = Harness::new(120, 36);
    h.app.on_paste(&lines(5));
    h.type_str(" ok");
    h.press(KeyCode::Enter);
    let sent = h.sent();
    let text = sent
        .iter()
        .find_map(|r| match r {
            agent_core::Request::Prompt(t) => Some(t.clone()),
            _ => None,
        })
        .expect("a prompt went out");
    assert_eq!(text, format!("{} ok", lines(5)));
    assert!(text.starts_with("pasted line 1\n"));
}

#[test]
fn backspace_takes_a_whole_chip_and_same_size_chips_stay_apart() {
    let mut h = Harness::new(120, 36);
    h.app.on_paste(&lines(5));
    h.type_str(" ");
    h.app.on_paste(&format!("x{}", lines(5)));
    assert_eq!(h.app.inp.chips.len(), 2);
    assert_ne!(h.app.inp.chips[0].content, h.app.inp.chips[1].content);
    h.press(KeyCode::Backspace);
    assert_eq!(h.app.ed.text(), "[Pasted: 5 lines] ");
    assert_eq!(h.app.inp.chips.len(), 1);
    assert!(h.app.inp.chips[0].content.starts_with("pasted"));
}

#[test]
fn pasting_the_chip_text_again_expands_it() {
    let mut h = Harness::new(120, 36);
    h.app.on_paste(&lines(6));
    h.app.on_paste(&lines(6));
    assert_eq!(
        h.app.ed.text(),
        lines(6).trim_end_matches('\n').to_string() + "\n"
    );
}

#[test]
fn image_chip_sends_the_path() {
    let dir = std::env::temp_dir().join(format!("grokw-img-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("shot.png");
    // a 16x16 PNG header is enough for the size check
    let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    b.extend_from_slice(&16u32.to_be_bytes());
    b.extend_from_slice(&16u32.to_be_bytes());
    b.extend_from_slice(&[8, 6, 0, 0, 0]);
    std::fs::write(&png, &b).unwrap();
    let mut h = Harness::new(120, 36);
    h.type_str("what is in ");
    h.app.on_paste(&png.display().to_string());
    assert_eq!(h.app.ed.text(), "what is in [Image #1] ");
    h.dump("image-chip-120x36");
    // the temp path's length varies (TMPDIR, the pid's digits): it is swapped for a fixed one
    // and the row padded back, so the panel's right edge stays where it was drawn
    let real = dir.display().to_string();
    let shown: Vec<String> = h
        .text()
        .lines()
        .map(|l| match l.rfind('│') {
            Some(edge) if l.contains(&real) => {
                let pad = real.chars().count().saturating_sub("/tmp/IMG".len());
                format!(
                    "{}{}{}",
                    l[..edge].replace(&real, "/tmp/IMG"),
                    " ".repeat(pad),
                    &l[edge..]
                )
            }
            _ => l.to_string(),
        })
        .collect();
    assert_snapshot("image-chip-120x36", &shown.join("\n"));
    h.press(KeyCode::Enter);
    let sent = h.sent();
    let text = sent
        .iter()
        .find_map(|r| match r {
            agent_core::Request::Prompt(t) => Some(t.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(text, format!("what is in @{}", png.display()));
    std::fs::remove_dir_all(&dir).ok();
}

fn history(h: &mut Harness) {
    h.app.ed.history_push("Do these in order, tersely. 1) Make a todo list of these steps. 2) Run ls -la in the shell. 3) Read README.md. 4) Edit README.md: change hello to hello world. 5) Grep for println in src. 6) Web search ratatui and give one sentence. 7) Finish with a markdown reply with a heading, a bold phrase, a bullet list of 3 items, a 2-row table, and a rust code block of 3 lines.");
}

#[test]
fn history_panel() {
    each("history-panel", |h| {
        history(h);
        h.press(KeyCode::Up);
    });
}

#[test]
fn history_browse_walks_and_restores() {
    let mut h = Harness::new(120, 36);
    for t in ["one", "two", "three"] {
        h.app.ed.history_push(t);
    }
    h.press(KeyCode::Up);
    assert_eq!(h.app.ed.text(), "three");
    h.press(KeyCode::Up);
    assert_eq!(h.app.ed.text(), "two");
    h.press(KeyCode::Down);
    assert_eq!(h.app.ed.text(), "three");
    // Down on the newest backs out and leaves the composer as it was
    h.press(KeyCode::Down);
    assert!(h.app.inp.hist.is_none());
    assert_eq!(h.app.ed.text(), "");
    h.press(KeyCode::Up);
    h.press(KeyCode::Up);
    h.press(KeyCode::Esc);
    assert_eq!(h.app.ed.text(), "");
    // Enter keeps the entry
    h.press(KeyCode::Up);
    h.press(KeyCode::Up);
    h.press(KeyCode::Enter);
    assert!(h.app.inp.hist.is_none());
    assert_eq!(h.app.ed.text(), "two");
}

#[test]
fn history_search_filters_by_the_composer_text() {
    let mut h = Harness::new(120, 36);
    for t in ["fix the bug", "write a test", "fix the test"] {
        h.app.ed.history_push(t);
    }
    h.type_str("/history");
    h.press(KeyCode::Enter);
    assert!(h.app.inp.hist.is_some());
    h.type_str("bug");
    let p = h.app.inp.hist.as_ref().unwrap();
    assert_eq!(p.items.len(), 1);
    h.dump("history-search-120x36");
    assert_snapshot("history-search-120x36", &h.text());
    h.press(KeyCode::Enter);
    assert_eq!(h.app.ed.text(), "fix the bug");
}

fn project(h: &mut Harness) {
    h.app.files = [
        "README.md",
        "src/",
        "src/main.rs",
        "src/ui/",
        "src/ui/view.rs",
        "docs/",
        "docs/guide.md",
    ]
    .map(String::from)
    .to_vec();
}

#[test]
fn at_picker_lists_the_top_level_first() {
    each("at-top", |h| {
        project(h);
        h.type_str("look at @");
    });
}

#[test]
fn at_picker_filters_and_accepts() {
    let mut h = Harness::new(120, 36);
    project(&mut h);
    h.type_str("@vw");
    h.dump("at-query-120x36");
    assert_snapshot("at-query-120x36", &h.text());
    h.press(KeyCode::Tab);
    assert_eq!(h.app.ed.text(), "@src/ui/view.rs ");
    // a directory is taken without the space
    let mut h = Harness::new(120, 36);
    project(&mut h);
    h.type_str("@docs");
    h.press(KeyCode::Tab);
    assert_eq!(h.app.ed.text(), "@docs");
    // Enter never sends while the picker is open
    let mut h = Harness::new(120, 36);
    project(&mut h);
    h.type_str("@mai");
    h.press(KeyCode::Enter);
    assert!(h.sent().is_empty());
    assert_eq!(h.app.ed.text(), "@src/main.rs ");
}

#[test]
fn at_picker_directory_mode_and_right() {
    let mut h = Harness::new(120, 36);
    project(&mut h);
    h.type_str("@s/");
    // only directories, shown with a slash
    let p = h.app.popup_open_state().unwrap();
    assert!(p.items.iter().all(|i| i.dir));
    assert!(p.items[0].label.ends_with('/'));
    h.press(KeyCode::Tab);
    assert_eq!(h.app.ed.text(), "@src/");
    // Right on a directory fills its path and keeps the picker open
    let mut h = Harness::new(120, 36);
    project(&mut h);
    h.type_str("@do");
    h.press(KeyCode::Right);
    assert_eq!(h.app.ed.text(), "@docs");
    assert!(h.app.popup_open_state().is_some());
}

#[test]
fn at_picker_stays_closed_after_a_word_and_on_no_match() {
    let mut h = Harness::new(120, 36);
    project(&mut h);
    h.type_str("mail me@src");
    assert!(h.app.popup_open_state().is_none());
    h.type_str(" @zzzzzz");
    assert!(h.app.popup_open_state().is_none());
}

#[test]
fn at_picker_window_scrolls_and_pages_by_four() {
    let mut h = Harness::new(120, 36);
    h.app.files = (0..30).map(|i| format!("f{i:02}.rs")).collect();
    h.type_str("@f");
    for _ in 0..3 {
        h.press(KeyCode::PageDown);
    }
    assert_eq!(h.app.popup_sel, 12);
    assert_eq!(h.app.inp.at_scroll, 5);
    h.dump("at-scrolled-120x36");
    assert_snapshot("at-scrolled-120x36", &h.text());
}

#[test]
fn stash_caption_and_auto_return() {
    each("stash-title", |h| {
        h.app
            .titles
            .insert(h.app.tr.session_id.clone(), "Demo title".into());
        h.type_str("draft");
        h.ctrl('s');
    });
    let mut h = Harness::new(120, 36);
    h.type_str("draft");
    h.ctrl('s');
    assert!(h.app.ed.is_empty());
    h.type_str("hello");
    h.press(KeyCode::Enter);
    // a chord stash comes back after the next send
    assert_eq!(h.app.ed.text(), "draft");
    assert!(h.app.stash.is_none());
    // a double Esc stash only on the chord
    h.app.tr.busy = false;
    h.press(KeyCode::Esc);
    h.press(KeyCode::Esc);
    assert!(h.app.stash.is_some());
    h.type_str("next");
    h.press(KeyCode::Enter);
    assert!(h.app.ed.is_empty());
    h.ctrl('s');
    assert_eq!(h.app.ed.text(), "draft");
}

#[test]
fn composer_grows_to_half_the_screen_and_scrolls() {
    let mut h = Harness::new(120, 36);
    for i in 0..30 {
        h.type_str(&format!("row {i}"));
        h.key(KeyCode::Enter, KeyModifiers::SHIFT);
    }
    h.dump("grown-120x36");
    assert_snapshot("grown-120x36", &h.text());
}

#[test]
fn unfocused_box_collapses_to_three_rows() {
    let mut h = Harness::new(120, 36);
    for i in 0..6 {
        h.type_str(&format!("row {i}"));
        h.key(KeyCode::Enter, KeyModifiers::SHIFT);
    }
    h.press(KeyCode::Tab);
    h.dump("unfocused-grown-120x36");
    assert_snapshot("unfocused-grown-120x36", &h.text());
}

#[test]
fn external_editor_is_requested_and_its_text_becomes_the_draft() {
    use grokw::ui::composer::input::edit_done;
    let mut h = Harness::new(120, 36);
    h.type_str("/edit-prompt");
    h.press(KeyCode::Enter);
    // an empty draft goes to the editor
    assert_eq!(h.app.inp.edit_request.take(), Some(String::new()));
    edit_done(&mut h.app, Ok("written in vi\n".into()));
    assert_eq!(h.app.ed.text(), "written in vi");
    // the palette keeps what is typed
    h.ctrl('p');
    h.type_str("external");
    h.press(KeyCode::Enter);
    assert_eq!(h.app.inp.edit_request.take(), Some("written in vi".into()));
    // an editor that fails leaves the draft and says so
    edit_done(
        &mut h.app,
        Err("The editor exited with exit status: 1".into()),
    );
    assert_eq!(h.app.ed.text(), "written in vi");
    assert!(h.app.toast.is_some());
    // an empty file leaves the draft alone
    edit_done(&mut h.app, Ok("\n".into()));
    assert_eq!(h.app.ed.text(), "written in vi");
}

fn mouse(h: &mut Harness, kind: crossterm::event::MouseEventKind, col: u16, row: u16) {
    h.app.on_mouse(crossterm::event::MouseEvent {
        kind,
        column: col,
        row,
        modifiers: KeyModifiers::NONE,
    });
}

#[test]
fn popup_rows_hover_and_click() {
    use crossterm::event::{MouseButton, MouseEventKind};
    let mut h = Harness::new(120, 36);
    h.type_str("/mo");
    // rows start under the rule at 20: the third is a few lines down
    mouse(&mut h, MouseEventKind::Moved, 30, 22);
    assert_eq!(h.app.inp.popup_hover, Some(1));
    h.dump("popup-hover-120x36");
    assert_snapshot("popup-hover-120x36", &h.text());
    mouse(&mut h, MouseEventKind::Down(MouseButton::Left), 30, 21);
    // the first row is /model, a command without arguments left: it ran
    assert!(h.app.ed.is_empty() || h.app.ed.text().starts_with("/model"));
}

#[test]
fn click_puts_the_cursor_and_double_click_expands_a_chip() {
    use crossterm::event::{MouseButton, MouseEventKind};
    let mut h = Harness::new(120, 36);
    h.type_str("hello world");
    // the editor learns its width when it is drawn
    h.render();
    mouse(&mut h, MouseEventKind::Down(MouseButton::Left), 11, 31);
    assert_eq!(h.app.ed.cursor(), 5);
    h.app.ed.clear();
    h.app.on_paste(&lines(5));
    h.render();
    // two clicks on the chip inside half a second
    mouse(&mut h, MouseEventKind::Down(MouseButton::Left), 8, 31);
    assert!(!h.app.inp.chips.is_empty());
    mouse(&mut h, MouseEventKind::Down(MouseButton::Left), 8, 31);
    assert!(h.app.inp.chips.is_empty());
    assert!(h.app.ed.text().starts_with("pasted line 1"));
}

fn bar(h: &mut Harness) -> String {
    let rows = h.text();
    let h_ = h.app.size.1 as usize;
    rows.lines().nth(h_ - 2).unwrap_or("").trim().to_string()
}

#[test]
fn the_bar_follows_the_state() {
    let mut h = Harness::new(120, 36);
    assert_eq!(bar(&mut h), "Shift+Tab:mode  │  Ctrl+.:shortcuts");
    h.type_str("hi");
    assert_eq!(
        bar(&mut h),
        "Enter:send  │  Shift+Tab:mode  │  Ctrl+.:shortcuts"
    );
    // multiline moves the send chord
    h.app.multiline = true;
    assert_eq!(
        bar(&mut h),
        "Shift+Enter:send  │  Shift+Tab:mode  │  Ctrl+.:shortcuts"
    );
    h.app.multiline = false;
    // a paste chip under the cursor offers to expand
    h.app.ed.clear();
    h.app.on_paste(&lines(5));
    h.press(KeyCode::Left);
    assert_eq!(
        bar(&mut h),
        "Enter:expand  │  Shift+Tab:mode  │  Ctrl+.:shortcuts"
    );
    h.app.ed.clear();
    h.app.inp.chips.clear();
    // a running turn
    h.app.tr.busy = true;
    assert_eq!(
        bar(&mut h),
        "Shift+Tab:mode  │  Ctrl+c:cancel  │  Ctrl+.:shortcuts"
    );
    h.type_str("more");
    assert_eq!(
        bar(&mut h),
        "Enter:queue  │  Shift+Tab:mode  │  Ctrl+c:cancel  │  Ctrl+Enter:send now  │  Ctrl+.:shortcuts"
    );
    h.app.ed.clear();
    h.app.queue.push("queued".into());
    assert_eq!(
        bar(&mut h),
        "Enter:send now  │  Shift+Tab:mode  │  Ctrl+c:cancel  │  Ctrl+;:queue  │  Ctrl+.:shortcuts"
    );
    // the history panel
    h.app.tr.busy = false;
    h.app.queue.clear();
    h.app.ed.history_push("one");
    h.press(KeyCode::Up);
    assert_eq!(
        bar(&mut h),
        "↑/↓:nav  │  PgUp/PgDn:page  │  Enter:select  │  Esc:cancel  │  Ctrl+.:shortcuts"
    );
    // ctrl+q arms the quit confirmation and replaces the whole bar
    h.press(KeyCode::Esc);
    h.ctrl('q');
    assert_eq!(bar(&mut h), "Ctrl+q:press again to quit");
}

#[test]
fn a_long_prompt_wraps_into_four_rows() {
    each("typed-long", |h| h.type_str(PROMPT));
}

#[test]
fn history_search_matches_the_real_panel_states() {
    let mut h = Harness::new(120, 36);
    h.type_str("/history");
    h.press(KeyCode::Enter);
    // the command itself is the one entry, as in the real session
    h.dump("hist-search-120x36");
    assert_snapshot("hist-search-open-120x36", &h.text());
    h.type_str("zz");
    h.dump("hist-search-none-120x36");
    assert_snapshot("hist-search-none-120x36", &h.text());
    h.press(KeyCode::Esc);
    assert!(h.app.inp.hist.is_none());
    assert_eq!(h.app.ed.text(), "");
}
