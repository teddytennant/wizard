//! Snapshots and behaviour for the prompt, popups, transcript keys and hint row. Snapshot files
//! are `tests/snapshots/s2-*.txt`; update with `UPDATE_SNAPSHOTS=1`.

mod common;

use agent_core::{Event, NoticeLevel, StopReason};
use common::*;
use crossterm::event::{KeyCode, KeyModifiers};

const NONE: KeyModifiers = KeyModifiers::NONE;

fn bottom(h: &mut Harness, rows: usize) -> String {
    let t = h.text();
    let lines: Vec<&str> = t.lines().collect();
    lines[lines.len().saturating_sub(rows)..].join("\n")
}

#[test]
fn retry_notice_goes_in_the_hint_row_not_a_toast() {
    let mut h = Harness::new(120, 36);
    h.app.send_prompt("hello".into());
    h.event(Event::TurnStart);
    h.event(Event::Notice {
        level: NoticeLevel::Warn,
        text: "the response stream dropped; it restarts below".into(),
    });
    let t = h.text();
    assert!(!t.contains("Warning"), "no toast expected:\n{t}");
    assert_snapshot("s2-retry-120x36", &bottom(&mut h, 4));
    // usage and commands are hidden while it shows, esc stays
    assert!(!t.contains("commands"));
    assert!(t.contains("esc interrupt"));
    // the next content clears it
    h.event(Event::TextDelta("ok".into()));
    assert!(h.text().contains("ctrl+p commands"));
    h.event(Event::TurnEnd(StopReason::EndTurn));
}

#[test]
fn long_retry_messages_are_cut_at_80_and_hint_at_120() {
    use openw::ui::footer::retry_text;
    let long = "x".repeat(130);
    assert_eq!(
        retry_text(&long),
        format!("{}… (click to expand)", "x".repeat(80))
    );
    assert_eq!(retry_text(&"y".repeat(90)), format!("{}…", "y".repeat(80)));
    assert_eq!(retry_text("short"), "short");
}

#[test]
fn warn_while_idle_stays_a_toast() {
    let mut h = Harness::new(120, 36);
    h.event(Event::Notice {
        level: NoticeLevel::Warn,
        text: "No session yet.".into(),
    });
    assert!(h.text().contains("No session yet."));
    let _ = (KeyCode::Esc, NONE);
}

fn user_turn_with_meta(h: &mut Harness, shown: &str, meta: openw::app::MsgMeta, answer: &str) {
    h.app
        .send_prompt_with(shown.to_string(), shown.to_string(), meta);
    h.event(Event::TurnStart);
    h.event(Event::TextDelta(answer.into()));
    h.event(Event::TurnEnd(StopReason::EndTurn));
    h.fix_durations();
}

#[test]
fn user_blocks_carry_chips_and_optional_timestamps() {
    use openw::app::{Chip, MsgMeta};
    let mut h = Harness::new(120, 36);
    let meta = MsgMeta {
        files: vec![
            Chip {
                directory: false,
                name: "README.md".into(),
            },
            Chip {
                directory: true,
                name: "src/".into(),
            },
        ],
        ..Default::default()
    };
    user_turn_with_meta(&mut h, "@README.md @src/ what is this?", meta, "A demo.");
    assert_snapshot("s2-chips-120x36", &h.text());
    // timestamps add a row; the time itself depends on the clock, so only check its shape
    h.app.flags.timestamps = true;
    let t = h.text();
    assert!(
        t.lines().any(|l| l.trim_start().starts_with("┃  ")
            && (l.trim_end().ends_with("AM") || l.trim_end().ends_with("PM"))),
        "{t}"
    );
    h.app.flags.timestamps = false;
    assert!(!h.text().contains(" AM") && !h.text().contains(" PM"));
}

#[test]
fn compaction_is_a_divider_and_the_reply_is_labelled() {
    let mut h = Harness::new(120, 36);
    h.turn("write a story", "Once upon a time.");
    h.sent();
    h.app.run_action(openw::keys::Action::Compact);
    assert_eq!(
        h.sent(),
        vec![agent_core::Request::Prompt("/compact".into())]
    );
    h.event(Event::TurnStart);
    h.event(Event::TextDelta("## Objective\n\nTell a story.".into()));
    h.event(Event::TurnEnd(StopReason::EndTurn));
    h.fix_durations();
    let t = h.text();
    assert!(
        t.contains(&format!("{} Compaction {}", "─".repeat(52), "─".repeat(52))),
        "{t}"
    );
    assert!(t.contains("▣  Compaction · grok-4.6"), "{t}");
    assert_snapshot("s2-compaction-120x36", &t);
}

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

fn mouse(h: &mut Harness, kind: MouseEventKind, col: u16, row: u16) {
    h.app.on_mouse(MouseEvent {
        kind,
        column: col,
        row,
        modifiers: NONE,
    });
}

#[test]
fn dragging_over_text_selects_and_copies_it() {
    let mut h = Harness::new(120, 36);
    h.turn("hi", "alpha beta gamma\n\ndelta epsilon");
    h.text();
    // find the screen rows of the two paragraphs
    let t = h.text();
    let row_of = |needle: &str| t.lines().position(|l| l.contains(needle)).unwrap() as u16;
    let (r1, r2) = (row_of("alpha"), row_of("delta"));
    mouse(&mut h, MouseEventKind::Down(MouseButton::Left), 11, r1);
    mouse(&mut h, MouseEventKind::Drag(MouseButton::Left), 20, r2);
    // selected cells are painted inverted
    let term = h.render();
    let c = term.cell(12, r1).unwrap();
    assert_eq!(c.bg, tuikit::Theme::default_theme(tuikit::Mode::Dark).text);
    mouse(&mut h, MouseEventKind::Up(MouseButton::Left), 20, r2);
    h.text();
    let copied: Vec<&openw::app::Deferred> = h.app.pending.iter().collect();
    assert_eq!(
        copied,
        vec![&openw::app::Deferred::Copy(
            "beta gamma\n\ndelta epsilon".into()
        )]
    );
    assert!(h.text().contains("Copied to clipboard"));
    assert!(h.app.view.sel.is_none());
}

#[test]
fn a_plain_click_does_not_copy() {
    let mut h = Harness::new(120, 36);
    h.turn("hi", "alpha beta");
    let t = h.text();
    let r = t.lines().position(|l| l.contains("alpha")).unwrap() as u16;
    mouse(&mut h, MouseEventKind::Down(MouseButton::Left), 8, r);
    mouse(&mut h, MouseEventKind::Up(MouseButton::Left), 8, r);
    h.text();
    assert!(h.app.pending.is_empty());
}

#[test]
fn dragging_the_scrollbar_thumb_scrolls() {
    let mut h = Harness::new(120, 36);
    for i in 0..6 {
        h.turn(&format!("question {i}"), &"a line of text\n\n".repeat(8));
    }
    h.app.flags.scrollbar = true;
    h.text();
    let before = h.app.scroll.offset();
    assert!(before > 0, "sticky bottom");
    let sb = openw::ui::session::scrollbar_col(120);
    // grab the thumb near the bottom of the track and pull it up
    let (start, len) = h
        .app
        .scroll
        .thumb_halves(h.app.view.area.height as usize)
        .unwrap();
    let row = (start + len / 2) as u16 / 2;
    mouse(&mut h, MouseEventKind::Down(MouseButton::Left), sb, row);
    mouse(
        &mut h,
        MouseEventKind::Drag(MouseButton::Left),
        sb,
        row.saturating_sub(8),
    );
    assert!(
        h.app.scroll.offset() < before,
        "{} < {before}",
        h.app.scroll.offset()
    );
    mouse(
        &mut h,
        MouseEventKind::Up(MouseButton::Left),
        sb,
        row.saturating_sub(8),
    );
    assert!(h.app.view.sb_grab.is_none());
}

#[test]
fn home_and_end_scroll_even_with_text_in_the_box() {
    let mut h = Harness::new(120, 36);
    for i in 0..6 {
        h.turn(&format!("question {i}"), &"a line of text\n\n".repeat(8));
    }
    h.text();
    h.type_str("draft");
    h.key(KeyCode::Home, NONE);
    h.text();
    assert_eq!(h.app.scroll.offset(), 0);
    assert_eq!(h.app.prompt.editor.cursor(), 5, "cursor stays");
    h.key(KeyCode::End, NONE);
    h.text();
    assert!(h.app.scroll.at_bottom());
}

#[test]
fn every_newline_chord_inserts_a_newline_and_plain_enter_submits() {
    for (code, mods) in [
        (KeyCode::Enter, KeyModifiers::SHIFT),
        (KeyCode::Enter, KeyModifiers::ALT),
        (KeyCode::Enter, KeyModifiers::CONTROL),
        (KeyCode::Char('j'), KeyModifiers::CONTROL),
    ] {
        let mut h = Harness::new(120, 36);
        h.sent();
        h.type_str("a");
        h.key(code, mods);
        h.type_str("b");
        assert_eq!(h.app.prompt.text(), "a\nb", "{code:?} {mods:?}");
        assert!(h.sent().is_empty());
    }
    let mut h = Harness::new(120, 36);
    h.type_str("a");
    h.sent();
    h.key(KeyCode::Enter, NONE);
    assert_eq!(h.sent(), vec![agent_core::Request::Prompt("a".into())]);
}

#[test]
fn the_box_grows_with_the_text_up_to_a_third_of_the_screen() {
    let mut h = Harness::new(120, 36);
    h.type_str("one");
    let one = h.text();
    for _ in 0..3 {
        h.key(KeyCode::Enter, KeyModifiers::SHIFT);
        h.type_str("more");
    }
    let four = h.text();
    let bars = |t: &str| {
        t.lines()
            .filter(|l| l.trim_start().starts_with('┃'))
            .count()
    };
    assert_eq!(bars(&four), bars(&one) + 3);
    for _ in 0..30 {
        h.key(KeyCode::Enter, KeyModifiers::SHIFT);
        h.type_str("x");
    }
    // 36 / 3 = 12 text rows at most, plus 3 rows of frame inside the bar column
    assert_eq!(bars(&h.text()), 12 + 3);
}

#[test]
fn undo_lists_rewinds_reloads_and_puts_the_message_back() {
    use agent_core::Request;
    let mut h = Harness::new(120, 36);
    h.turn("first question", "first answer");
    h.turn("fix the flaky parser test please", "done");
    h.sent();
    h.app.run_action(openw::keys::Action::Undo);
    assert_eq!(h.sent(), vec![Request::Prompt("/rewind".into())]);
    // the listing never reaches the transcript
    let before = h.app.transcript.messages.len();
    h.event(Event::TurnStart);
    h.event(Event::Notice {
        level: NoticeLevel::Info,
        text: "rewind to before turn:\n4 — fix the flaky parser test please · parser.rs\n3 — first question\n\n/rewind <turn> restores the files and truncates the conversation.".into(),
    });
    h.event(Event::TurnEnd(StopReason::EndTurn));
    assert_eq!(h.app.transcript.messages.len(), before);
    assert_eq!(h.sent(), vec![Request::Prompt("/rewind 4".into())]);
    h.event(Event::TurnStart);
    h.event(Event::Notice {
        level: NoticeLevel::Info,
        text: "rewound to before turn 4: restored 1 file(s); conversation truncated".into(),
    });
    h.event(Event::TurnEnd(StopReason::EndTurn));
    assert_eq!(
        h.sent(),
        vec![Request::LoadSession("ses_1234567890".into())]
    );
    assert_eq!(h.app.prompt.text(), "fix the flaky parser test please");
    assert!(h.text().contains("rewound to before turn 4"));
    // wizard answers the load with the truncated history
    h.event(Event::History {
        session_id: "ses_1234567890".into(),
        items: vec![
            agent_core::HistoryItem::User("first question".into()),
            agent_core::HistoryItem::Assistant("first answer".into()),
        ],
    });
    let t = h.text();
    // the reverted message is gone from the transcript and sits in the prompt box
    assert!(t.contains("first answer"));
    assert_eq!(t.matches("flaky parser test please").count(), 1, "{t}");
}

#[test]
fn undo_refuses_when_wizards_newest_turn_is_not_ours() {
    use agent_core::Request;
    let mut h = Harness::new(120, 36);
    h.turn("my message", "ok");
    h.sent();
    h.app.run_action(openw::keys::Action::Undo);
    h.sent();
    h.event(Event::TurnStart);
    h.event(Event::Notice {
        level: NoticeLevel::Info,
        text: "rewind to before turn:\n9 — something else entirely".into(),
    });
    h.event(Event::TurnEnd(StopReason::EndTurn));
    assert!(h.sent().is_empty());
    let t = h.text();
    assert!(t.contains("nothing undone"), "{t}");
    let _ = Request::Cancel;
}

#[test]
fn redo_says_wizard_cannot() {
    let mut h = Harness::new(120, 36);
    h.turn("a", "b");
    h.app.run_action(openw::keys::Action::Redo);
    assert!(h.text().contains("nothing to redo"));
}

fn files(h: &mut Harness) {
    h.app.files = vec![
        "README.md".into(),
        "notes.md".into(),
        "src/main.rs".into(),
        "src/lib.rs".into(),
        "docs/guide.md".into(),
    ];
}

#[test]
fn at_popup_lists_directories_and_tab_goes_into_them() {
    let mut h = Harness::new(120, 36);
    files(&mut h);
    h.type_str("look at @sr");
    let ac = h.app.prompt.ac.as_ref().expect("popup open");
    let shown: Vec<&str> = ac.items.iter().map(|i| i.display.as_str()).collect();
    assert_eq!(shown[0], "src/", "{shown:?}");
    assert!(shown.contains(&"src/main.rs"));
    // tab on a directory continues inside it and keeps the popup
    h.key(KeyCode::Tab, NONE);
    assert_eq!(h.app.prompt.text(), "look at @src/");
    let ac = h.app.prompt.ac.as_ref().expect("popup still open");
    assert!(ac.items.iter().any(|i| i.display == "src/main.rs"));
    // enter on a file mentions it and adds the space
    h.type_str("ma");
    h.key(KeyCode::Enter, NONE);
    assert_eq!(h.app.prompt.text(), "look at @src/main.rs ");
    assert!(h.app.prompt.ac.is_none());
    assert_eq!(h.app.prompt.outgoing().files.len(), 1);
}

#[test]
fn enter_on_a_directory_mentions_it_as_a_directory() {
    let mut h = Harness::new(120, 36);
    files(&mut h);
    h.type_str("@doc");
    h.key(KeyCode::Enter, NONE);
    assert_eq!(h.app.prompt.text(), "@docs/ ");
    let out = h.app.prompt.outgoing();
    assert_eq!(out.files.len(), 1);
    assert!(out.files[0].directory);
}

#[test]
fn line_ranges_stay_on_the_mention() {
    let mut h = Harness::new(120, 36);
    files(&mut h);
    h.type_str("@main#12-30");
    assert_eq!(
        h.app.prompt.ac.as_ref().unwrap().items[0].display,
        "src/main.rs"
    );
    h.key(KeyCode::Enter, NONE);
    assert_eq!(h.app.prompt.text(), "@src/main.rs#12-30 ");
}

#[test]
fn esc_closes_the_popup_until_the_text_changes_and_drops_a_half_typed_command() {
    let mut h = Harness::new(120, 36);
    files(&mut h);
    h.type_str("see @R");
    h.key(KeyCode::Esc, NONE);
    assert!(h.app.prompt.ac.is_none());
    assert_eq!(h.app.prompt.text(), "see @R");
    h.type_str("E");
    assert!(h.app.prompt.ac.is_some(), "typing reopens it");
    let mut h = Harness::new(120, 36);
    h.type_str("/mo");
    h.key(KeyCode::Esc, NONE);
    assert_eq!(
        h.app.prompt.text(),
        "",
        "the half-typed command goes with the popup"
    );
}

#[test]
fn enter_with_nothing_listed_does_not_submit() {
    let mut h = Harness::new(120, 36);
    h.sent();
    h.type_str("/zzzz");
    assert!(h.text().contains("No matching items"));
    h.key(KeyCode::Enter, NONE);
    assert!(h.sent().is_empty());
    assert_eq!(h.app.prompt.text(), "/zzzz");
}

#[test]
fn slash_popup_follows_the_reference_ordering_for_mo() {
    let mut h = Harness::new(120, 36);
    h.type_str("/mo");
    let names: Vec<String> = h
        .app
        .prompt
        .ac
        .as_ref()
        .unwrap()
        .items
        .iter()
        .map(|i| i.display.trim().to_string())
        .collect();
    // the alias `/mo` makes /models an exact hit; reference 04-slash-filtered has it first
    assert_eq!(names[0], "/models", "{names:?}");
}

#[test]
fn a_pasted_image_path_becomes_a_chip_and_goes_out_as_an_at_path() {
    let dir = std::env::temp_dir().join(format!("openw-img-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("shot one.png");
    std::fs::write(&img, b"\x89PNG").unwrap();
    let mut h = Harness::new(120, 36);
    // terminals escape spaces in a dropped path
    h.paste(&img.display().to_string().replace(' ', "\\ "));
    assert_eq!(h.app.prompt.text(), "[Image 1] ");
    h.type_str("what is this");
    let out = h.app.prompt.outgoing();
    assert_eq!(out.shown, "[Image 1] what is this");
    assert_eq!(out.sent, format!("@{} what is this", img.display()));
    assert_eq!(out.files[0].name, "shot one.png");
    // a second one is numbered 2
    h.paste(&img.display().to_string());
    assert!(h.app.prompt.text().contains("[Image 2]"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_file_list_respects_gitignore() {
    let dir = std::env::temp_dir().join(format!("openw-ls-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("target")).unwrap();
    std::fs::write(dir.join(".gitignore"), "target/\n*.log\n").unwrap();
    std::fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
    std::fs::write(dir.join("target/junk.o"), "x").unwrap();
    std::fs::write(dir.join("a.log"), "x").unwrap();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .output()
            .unwrap()
    };
    git(&["init", "-q"]);
    let got = openw::app::list_files(&dir);
    assert!(got.contains(&"src/main.rs".to_string()), "{got:?}");
    assert!(got.contains(&".gitignore".to_string()));
    assert!(
        !got.iter()
            .any(|f| f.contains("target") || f.ends_with(".log")),
        "{got:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn stash_survives_a_restart() {
    use openw::app::{App, AppOpts};
    let dir = std::env::temp_dir().join(format!("openw-stash-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let opts = AppOpts {
        cwd: "/tmp".into(),
        mock: true,
        state_dir: Some(dir.clone()),
        ..Default::default()
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (mtx, _mrx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(opts.clone(), tx, mtx);
    app.prompt.set_text("half written thought");
    app.run_action(openw::keys::Action::StashPush);
    assert!(app.prompt.is_empty());
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (mtx, _mrx) = tokio::sync::mpsc::unbounded_channel();
    let mut again = App::new(opts, tx, mtx);
    assert_eq!(again.prompt.stash, vec!["half written thought".to_string()]);
    again.run_action(openw::keys::Action::StashPop);
    assert_eq!(again.prompt.text(), "half written thought");
    assert!(again.prompt.stash.is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_visible_scrollbar_narrows_the_text_by_two_columns() {
    let table = "| a | b |\n|---|---|\n| 1 | two |";
    let width_of = |h: &mut Harness| {
        let t = h.text();
        let l = t.lines().find(|l| l.contains('┌')).unwrap().to_string();
        let end = l.chars().rev().position(|c| c == '┐').unwrap();
        l.chars().count() - end - l.chars().position(|c| c == '┌').unwrap()
    };
    let mut h = Harness::new(120, 36);
    h.turn("t", table);
    let plain = width_of(&mut h);
    assert_eq!(plain, 113);
    h.app.flags.scrollbar = true;
    assert_eq!(width_of(&mut h), 111, "reference: session-markdown-full");
}

#[test]
fn a_wide_table_wraps_like_opencode() {
    // reference: the same markdown typed into opencode 1.18.34 at 150x42 (sidebar shown)
    let md = "| id | description | status |\n|---|---|---|\n| 1 | the quick brown fox jumps over the lazy dog and keeps running through the forest until the very end of the long path ahead of it all | ok |\n| 22 | short | a considerably longer status cell that goes on and on and on and on forever and ever |";
    let mut h = Harness::new(150, 42);
    h.turn("table", md);
    let t = h.text();
    let rows: Vec<&str> = t
        .lines()
        .filter(|l| l.contains('│') || l.contains('┌') || l.contains('├') || l.contains('└'))
        .collect();
    // the sidebar starts at column 108 and has its own text
    let rows: Vec<String> = rows
        .iter()
        .map(|l| l.chars().take(108).collect::<String>())
        .collect();
    // header row: id 2, description 53, status 42 columns wide
    let head = rows.iter().find(|l| l.contains("description")).unwrap();
    assert!(head.contains("│id│description"), "{head}");
    let lines: Vec<&str> = rows.iter().map(|l| l.trim_start().trim_end()).collect();
    assert_eq!(lines[3], "│1 │the quick brown fox jumps over the lazy dog and      │ok                                        │");
    assert_eq!(lines[4], "│  │keeps running through the forest until the very end  │                                          │");
    assert_eq!(lines[5], "│  │of the long path ahead of it all                     │                                          │");
    assert_eq!(lines[9], "│  │                                                     │ever                                      │");
}

#[test]
fn the_prompt_wraps_after_the_same_characters_opencode_does() {
    // measured in opencode 1.18.34: the home prompt text is 70 columns wide
    let x = "x".repeat(66);
    for (p, first_row) in [
        ('.', 69),
        ('/', 69),
        ('-', 69),
        ('(', 69),
        ('!', 70),
        ('_', 70),
        ('=', 70),
    ] {
        let mut h = Harness::new(120, 36);
        h.type_str(&format!("{x}ab{p}cdefghij"));
        let t = h.text();
        let row = t.lines().find(|l| l.contains("xxxxxxxx")).unwrap();
        let shown = row.trim_start().trim_start_matches('┃').trim();
        assert_eq!(shown.chars().count(), first_row, "{p}: {row}");
    }
}

#[test]
fn assistant_text_breaks_urls_after_slashes() {
    let mut h = Harness::new(120, 36);
    // 108 columns of text: put the URL so that it straddles the edge
    let lead = "a".repeat(80);
    h.turn(
        "q",
        &format!("{lead} (https://example.com/a/very/long/url/that/goes/on) end"),
    );
    let t = h.text();
    assert!(
        t.lines()
            .any(|l| l.trim_end().ends_with("(https://example.com/a/very/")),
        "{t}"
    );
}

/// opencode's scrollbox keeps one blank row of padding above the first block, also when the
/// content overflows and the view is scrolled all the way up (fidelity round 1, finding 3).
#[test]
fn scrolled_to_the_top_the_first_block_sits_one_row_below_the_edge() {
    let mut h = Harness::new(120, 36);
    for i in 0..6 {
        h.turn(&format!("question {i}"), &"a line of text\n\n".repeat(8));
    }
    h.text();
    h.key(KeyCode::Home, NONE);
    let rows: Vec<String> = h.text().lines().map(String::from).collect();
    assert_eq!(rows[0].trim(), "", "row 0 is padding");
    assert!(
        rows[1].contains('┃'),
        "user bar starts on row 1: {:?}",
        rows[1]
    );
    assert!(rows[2].contains("question 0"), "{:?}", rows[2]);
}
