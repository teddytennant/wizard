//! Renders scenes in process and compares them with captures of the real Claude Code
//! (`tests/fixtures/ref`), cell by cell: character, colours and attributes. The live
//! comparison in a terminal is `tools/compare.sh`; these keep the scenes that match from
//! drifting.

mod common;

use common::{assert_matches, Harness};
use crossterm::event::{KeyCode, KeyModifiers};

const SIZES: [(u16, u16); 3] = [(80, 24), (120, 40), (200, 50)];
const P_BASH: &str = "Run the bash command `echo hi` and then reply with exactly: done";
const P_EDIT: &str = "Read main.rs, then change hello to hi in it, then reply with exactly: done";
const LONG: &str = "this is a long line of input that keeps going so that it has to wrap at the right edge of the prompt box more than once at eighty columns and at least once at one hundred and twenty";

fn each(mut f: impl FnMut(u16, u16, String)) {
    for (w, h) in SIZES {
        f(w, h, format!("{w}x{h}"));
    }
}

#[test]
fn home() {
    each(|w, h, s| {
        let mut hn = Harness::new(&format!("static-{s}.jsonl"), w, h);
        assert_matches(&mut hn, &format!("home-{s}"), &[]);
    });
}

#[test]
fn multiline_input_and_ctrl_c() {
    each(|w, h, s| {
        let mut hn = Harness::new(&format!("static-{s}.jsonl"), w, h);
        hn.type_str("line one\\");
        hn.press(KeyCode::Enter);
        hn.type_str("line two");
        assert_matches(&mut hn, &format!("multiline-{s}"), &[]);
        hn.ctrl('c');
        assert_matches(&mut hn, &format!("ctrlc-hint-{s}"), &[]);
        hn.advance(2_500);
        assert_matches(&mut hn, &format!("ctrlc-after-{s}"), &[]);
    });
}

#[test]
fn bash_mode() {
    each(|w, h, s| {
        let mut hn = Harness::new(&format!("static-{s}.jsonl"), w, h);
        hn.type_str("!");
        assert_matches(&mut hn, &format!("bash-mode-{s}"), &[]);
        hn.type_str("ls");
        assert_matches(&mut hn, &format!("bash-mode-typed-{s}"), &[]);
    });
}

#[test]
fn mode_cycle() {
    each(|w, h, s| {
        let mut hn = Harness::new(&format!("static-{s}.jsonl"), w, h);
        // wizard confirms each change as the replay would
        let confirm = |hn: &mut Harness| {
            for r in hn.sent() {
                match r {
                    agent_core::Request::SetMode(m) => {
                        let mut cfg = hn.app.cfg.clone();
                        cfg.mode = m;
                        hn.event(agent_core::Event::ConfigChanged(cfg));
                    }
                    agent_core::Request::Prompt(p) if p == "/plan" => {
                        hn.event(agent_core::Event::TurnStart);
                        hn.event(agent_core::Event::Notice {
                            level: agent_core::NoticeLevel::Info,
                            text: "plan mode on".into(),
                        });
                        hn.event(agent_core::Event::TurnEnd(agent_core::StopReason::EndTurn));
                    }
                    _ => {}
                }
            }
        };
        hn.press(KeyCode::BackTab);
        confirm(&mut hn);
        assert_matches(&mut hn, &format!("modes-1-{s}"), &[]);
        hn.press(KeyCode::BackTab);
        confirm(&mut hn);
        assert_matches(&mut hn, &format!("modes-2-{s}"), &[]);
    });
}

#[test]
fn slash_menu() {
    each(|w, h, s| {
        let mut hn = Harness::new(&format!("static-{s}.jsonl"), w, h);
        hn.type_str("/");
        // the replayed description has the product name as the comparer expects it
        let named = [r"\((?:Claude|Wizard), Anthropic"];
        assert_matches(&mut hn, &format!("slash-{s}"), &named);
        hn.press(KeyCode::Down);
        hn.press(KeyCode::Down);
        assert_matches(&mut hn, &format!("slash-down-{s}"), &named);
        hn.type_str("mo");
        assert_matches(&mut hn, &format!("slash-filter-{s}"), &[]);
        hn.type_str("zzz");
        assert_matches(&mut hn, &format!("slash-none-{s}"), &[]);
        hn.ctrl('c');
        hn.advance(2_000);
        hn.type_str("/help");
        assert_matches(&mut hn, &format!("help-typed-{s}"), &[]);
    });
}

#[test]
fn closed_dialogs_leave_a_record_and_long_input_wraps() {
    each(|w, h, s| {
        let mut hn = Harness::new(&format!("static-{s}.jsonl"), w, h);
        hn.type_str("/model");
        hn.press(KeyCode::Enter);
        hn.press(KeyCode::Esc);
        assert_matches(&mut hn, &format!("model-kept-{s}"), &[]);
        hn.type_str("/resume");
        hn.press(KeyCode::Enter);
        hn.press(KeyCode::Esc);
        hn.type_str(LONG);
        assert_matches(&mut hn, &format!("wrap-{s}"), &[]);
    });
}

#[test]
fn a_turn_with_a_tool_call_an_edit_and_a_permission_prompt() {
    each(|w, h, s| {
        let mut hn = Harness::new(&format!("turn-{s}.jsonl"), w, h);
        hn.type_str(P_BASH);
        assert_matches(&mut hn, &format!("turn-typed-{s}"), &[]);
        hn.press(KeyCode::Enter);
        hn.play();
        assert_matches(&mut hn, &format!("turn-done-{s}"), &[]);
        hn.ctrl('o');
        assert_matches(&mut hn, &format!("turn-detail-{s}"), &[r"\d\d:\d\d [AP]M"]);
        hn.ctrl('o');
        hn.type_str(P_EDIT);
        hn.press(KeyCode::Enter);
        hn.play();
        let option2 = [
            r"(?:2\. )Yes, (?:and switch to accept edits|allow all edits).*$",
            r"^      commands\) for this session \(shift\+tab\)\s*$",
            r" · Tab to amend",
        ];
        // at 80 columns the reference's longer second option takes two rows and moves the
        // screen up by one; compare.py lines that up, this comparer does not
        if w > 80 {
            assert_matches(&mut hn, &format!("edit-permission-{s}"), &option2);
        }
        hn.press(KeyCode::Enter);
        hn.play();
        assert_matches(&mut hn, &format!("edit-done-{s}"), &[]);
        hn.key(KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_matches(&mut hn, &format!("edit-detail-{s}"), &[r"\d\d:\d\d [AP]M"]);
    });
}

const P_THINK: &str = "Think it over: add two and two";

/// A reply with a thinking block: the usual view says how long it took, ctrl+o shows it.
#[test]
fn thinking_is_one_line_until_the_transcript_is_opened() {
    for (w, h) in [(80, 24), (120, 40), (200, 50)] {
        let s = format!("{w}x{h}");
        let mut hn = Harness::new(&format!("think-{s}.jsonl"), w, h);
        hn.type_str(P_THINK);
        hn.press(KeyCode::Enter);
        hn.play();
        assert_matches(&mut hn, &format!("think-done-{s}"), &[]);
        hn.ctrl('o');
        assert_matches(&mut hn, &format!("think-detail-{s}"), &[]);
    }
}

const SL_A: [&str; 2] = [
    "\x1b[36mproj\x1b[0m on \x1b[32mmain\x1b[0m \x1b[2m+2 -1\x1b[0m",
    "plain row",
];

fn status_line(hn: &mut Harness, lines: &[&str], padding: usize) {
    hn.app.status_line.cfg = Some(claudew::statusline_run::Cfg {
        command: "true".into(),
        padding,
        refresh: None,
    });
    hn.app.status_line.lines = lines.iter().map(ToString::to_string).collect();
}

/// The status line against 2.1.314 (`tests/fixtures/ref-2.1.314`, a test command run by the real
/// program): at the start, with text typed, under the slash menu and after a turn.
#[test]
fn status_line_rows_and_the_mode_line_without_its_hint() {
    each(|w, h, s| {
        let mut hn = Harness::new(&format!("statusline-{s}.jsonl"), w, h);
        status_line(&mut hn, &SL_A, 0);
        assert_matches(&mut hn, &format!("sl-home-{s}"), &[]);
        hn.advance(12_000);
        hn.type_str("hello");
        assert_matches(&mut hn, &format!("sl-typed-{s}"), &[]);
        for _ in 0..5 {
            hn.press(KeyCode::Backspace);
        }
        hn.type_str("/");
        assert_matches(
            &mut hn,
            &format!("sl-slash-{s}"),
            &[r"\((?:Claude|Wizard), Anthropic"],
        );
        hn.ctrl('c');
        hn.advance(2_500);
        hn.type_str("what is 2+2");
        hn.press(KeyCode::Enter);
        hn.play();
        // the reference had been put in accept edits mode by an earlier scene
        assert_matches(
            &mut hn,
            &format!("sl-turn-{s}"),
            &[r"^  (?:⏵⏵ accept edits on \(shift\+tab to cycle\)|⏸ manual mode on)"],
        );
    });
}

/// `padding` indents both sides; a row past the margin ends in an ellipsis; wide characters count
/// two columns.
#[test]
fn status_line_padding_and_the_cut() {
    each(|w, h, s| {
        let mut hn = Harness::new(&format!("static-{s}.jsonl"), w, h);
        let long = format!("\x1b[31mred\x1b[0m {}", "x".repeat(150));
        status_line(&mut hn, &["first row", &long, "wide 世界世界 end"], 2);
        // the reference's cells after a wide character are blanks; the look's are its second half
        assert_matches(&mut hn, &format!("slp-home-{s}"), &[r"^\s+wide .*$"]);
    });
}

fn task(text: &str, status: agent_core::TodoStatus) -> agent_core::Todo {
    agent_core::Todo {
        text: text.into(),
        status,
        active: None,
    }
}

/// The task list against 2.1.314: under the spinner while the turn runs, above the prompt after
/// it, hidden and shown with ctrl+t.
#[test]
fn the_task_list_running_idle_hidden_and_shown() {
    use agent_core::{Event, StopReason, TodoStatus::*};
    let spinner = r"^[·✢*✳✶✻✽] .*$";
    each(|w, h, s| {
        let mut hn = Harness::new(&format!("static-{s}.jsonl"), w, h);
        hn.type_str("Make a task list");
        hn.press(KeyCode::Enter);
        hn.event(Event::TurnStart);
        hn.event(Event::Todos(vec![
            task("Read main.rs", Completed),
            task("Change hello to hi", InProgress),
            task("Run the tests", Pending),
        ]));
        hn.advance(2_000);
        assert_matches(
            &mut hn,
            &format!("todo-running-{s}"),
            &[spinner, r" · ← for agents"],
        );
        hn.event(Event::TextDelta("done".into()));
        hn.event(Event::TurnEnd(StopReason::EndTurn));
        hn.advance(12_000);
        assert_matches(&mut hn, &format!("todo-{s}"), &[]);
        hn.ctrl('t');
        assert_matches(&mut hn, &format!("todo-hidden-{s}"), &[]);
        hn.ctrl('t');
        assert_matches(&mut hn, &format!("todo-shown-{s}"), &[]);
    });
}

#[test]
fn a_long_task_list_shows_five_and_counts_the_rest() {
    use agent_core::{Event, StopReason, TodoStatus::*};
    let spinner = r"^[·✢*✳✶✻✽] .*$";
    each(|w, h, s| {
        let mut hn = Harness::new(&format!("static-{s}.jsonl"), w, h);
        hn.type_str("Make many tasks");
        hn.press(KeyCode::Enter);
        hn.event(Event::TurnStart);
        let many = (1..=14)
            .map(|n| {
                task(
                    &format!(
                        "Step {n} of the long job, with a name long enough to need cutting at narrow widths"
                    ),
                    match n {
                        1..=3 => Completed,
                        4 => InProgress,
                        _ => Pending,
                    },
                )
            })
            .collect();
        hn.event(Event::Todos(many));
        hn.advance(2_000);
        assert_matches(
            &mut hn,
            &format!("todo-many-running-{s}"),
            &[spinner, r" · ← for agents"],
        );
        hn.event(Event::TextDelta("done".into()));
        hn.event(Event::TurnEnd(StopReason::EndTurn));
        hn.advance(12_000);
        // at 24 rows the footer does not fit: 2.1.314 loses its mode line off the bottom and the
        // look scrolls instead (compare.py's `scroll-look`), so only the taller screens are cells
        if h > 24 {
            assert_matches(&mut hn, &format!("todo-many-{s}"), &[]);
        }
    });
}
