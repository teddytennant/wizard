//! Cell snapshots ported from Codex's `history_cell/tests.rs` and `snapshots/`, plus the
//! behaviours the spec calls out for streaming and reasoning.

use super::*;
use crate::style::{ColorLevel, Palette, set_palette};
use ratatui::style::{Color, Modifier, Style};
use serde_json::json;

/// A log without the once-per-session notes, which the capture rows do not have.
fn quiet_log() -> ChatLog {
    let mut log = ChatLog::new();
    log.notes = false;
    log
}

fn plain(l: &Line<'_>) -> String {
    l.spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect::<String>()
}

fn render(cell: &dyn HistoryCell, width: u16) -> Vec<String> {
    cell.display_lines(width).iter().map(plain).collect()
}

fn dark() {
    set_palette(Palette::new(
        Some((230, 230, 230)),
        Some((0, 0, 0)),
        ColorLevel::TrueColor,
    ));
}

fn todo(text: &str, status: TodoStatus) -> Todo {
    Todo {
        text: text.into(),
        status,
    }
}

// ---- plan -------------------------------------------------------------------------------------

#[test]
fn plan_update_with_note_and_wrapping() {
    let cell = PlanUpdateCell {
        explanation: Some(
            "I’ll update Grafana call error handling by adding retries and clearer messages when the backend is unreachable."
                .into(),
        ),
        steps: vec![
            todo("Investigate existing error paths and logging around HTTP timeouts", TodoStatus::Completed),
            todo("Harden Grafana client error handling with retry/backoff and user‑friendly messages", TodoStatus::InProgress),
            todo("Add tests for transient failure scenarios and surfacing to the UI", TodoStatus::Pending),
        ],
    };
    assert_eq!(
        render(&cell, 32),
        vec![
            "• Updated Plan",
            "  └ I’ll update Grafana call",
            "    error handling by adding",
            "    retries and clearer messages",
            "    when the backend is",
            "    unreachable.",
            "    ✔ Investigate existing error",
            "      paths and logging around",
            "      HTTP timeouts",
            "    □ Harden Grafana client",
            "      error handling with retry/",
            "      backoff and user‑friendly",
            "      messages",
            "    □ Add tests for transient",
            "      failure scenarios and",
            "      surfacing to the UI",
        ]
    );
}

#[test]
fn plan_update_without_note() {
    let cell = PlanUpdateCell {
        explanation: None,
        steps: vec![
            todo("Define error taxonomy", TodoStatus::InProgress),
            todo("Implement mapping to user messages", TodoStatus::Pending),
        ],
    };
    assert_eq!(
        render(&cell, 80),
        vec![
            "• Updated Plan",
            "  └ □ Define error taxonomy",
            "    □ Implement mapping to user messages",
        ]
    );
}

#[test]
fn plan_step_markers_are_plain_and_the_text_carries_the_style() {
    let cell = PlanUpdateCell {
        explanation: None,
        steps: vec![
            todo("done", TodoStatus::Completed),
            todo("doing", TodoStatus::InProgress),
            todo("later", TodoStatus::Pending),
        ],
    };
    let rows = cell.display_lines(80);
    // `✔ ` unstyled, text crossed out and dim.
    assert_eq!(rows[1].spans[1].style, Style::default());
    let done = rows[1].spans[2].style;
    assert!(
        done.add_modifier
            .contains(Modifier::CROSSED_OUT | Modifier::DIM)
    );
    let doing = rows[2].spans[1].style;
    assert_eq!(doing.fg, None);
    assert_eq!(rows[2].spans[2].style.fg, Some(Color::Cyan));
    assert!(rows[2].spans[2].style.add_modifier.contains(Modifier::BOLD));
    assert!(rows[3].spans[2].style.add_modifier.contains(Modifier::DIM));
}

#[test]
fn empty_plan_says_so() {
    let cell = PlanUpdateCell {
        explanation: None,
        steps: vec![],
    };
    assert_eq!(
        render(&cell, 80),
        vec!["• Updated Plan", "  └ (no steps provided)"]
    );
}

// ---- web search -------------------------------------------------------------------------------

#[test]
fn web_search_wraps_with_a_two_space_hang() {
    let mut cell = WebSearchCell::new(
        "c",
        "example search query with several generic words to exercise wrapping",
    );
    cell.completed = true;
    assert_eq!(
        render(&cell, 64),
        vec![
            "• Searched the web for example search query with several generic",
            "  words to exercise wrapping",
        ]
    );
}

#[test]
fn web_search_without_detail_is_the_header_alone() {
    let mut cell = WebSearchCell::new("c", "");
    cell.completed = true;
    assert_eq!(render(&cell, 80), vec!["• Searched the web"]);
}

#[test]
fn a_running_search_shows_only_the_header() {
    let mut cell = WebSearchCell::new("c", "rust");
    cell.animations = false;
    assert_eq!(render(&cell, 80), vec!["• Searching the web"]);
}

// ---- mcp --------------------------------------------------------------------------------------

fn find_docs() -> McpCell {
    let mut c = McpCell::new(
        "c",
        "search",
        "find_docs",
        json!({"query": "ratatui styling", "limit": 3}),
    );
    c.animations = false;
    c
}

#[test]
fn active_mcp_call() {
    assert_eq!(
        render(&find_docs(), 80),
        vec![r#"• Calling search.find_docs({"query":"ratatui styling","limit":3})"#]
    );
}

#[test]
fn completed_mcp_call() {
    let mut c = find_docs();
    c.complete(Ok("Found styling guidance in styles.md".into()));
    assert_eq!(
        render(&c, 80),
        vec![
            r#"• Called search.find_docs({"query":"ratatui styling","limit":3})"#,
            "  └ Found styling guidance in styles.md",
        ]
    );
}

#[test]
fn mcp_call_with_an_error_result() {
    let mut c = find_docs();
    c.complete(Err("network timeout".into()));
    assert_eq!(
        render(&c, 80),
        vec![
            r#"• Called search.find_docs({"query":"ratatui styling","limit":3})"#,
            "  └ Error: network timeout",
        ]
    );
    // red bullet
    assert_eq!(c.display_lines(80)[0].spans[0].style.fg, Some(Color::Red));
}

#[test]
fn mcp_multiline_result() {
    let mut c = McpCell::new(
        "c",
        "metrics",
        "summary",
        json!({"metric": "trace.latency", "window": "15m"}),
    );
    c.animations = false;
    c.complete(Ok(
        "Latency summary: p50=120ms, p95=480ms.\nNo anomalies detected.".into(),
    ));
    assert_eq!(
        render(&c, 80),
        vec![
            r#"• Called metrics.summary({"metric":"trace.latency","window":"15m"})"#,
            "  └ Latency summary: p50=120ms, p95=480ms.",
            "    No anomalies detected.",
        ]
    );
}

#[test]
fn mcp_invocation_too_wide_for_the_header_stacks() {
    // spec B.8.1, 60x24/turns-10-mcp-done
    let mut c = McpCell::new(
        "c",
        "fake",
        "echo",
        json!({"message": "hello from the model", "times": 2}),
    );
    c.animations = false;
    c.complete(Ok("hello from the model\nhello from the model".into()));
    assert_eq!(
        render(&c, 60),
        vec![
            "• Called",
            r#"  └ fake.echo({"message":"hello from the model","times":2})"#,
            "    hello from the model",
            "    hello from the model",
        ]
    );
    // 40 columns: the invocation wraps with a hang of 8, results stay at 4
    assert_eq!(
        render(&c, 40),
        vec![
            "• Called",
            r#"  └ fake.echo({"message":"hello from the"#,
            r#"        model","times":2})"#,
            "    hello from the model",
            "    hello from the model",
        ]
    );
}

#[test]
fn mcp_result_is_cut_to_five_rows_worth_of_graphemes() {
    let mut c = McpCell::new("c", "fake", "echo", json!({}));
    c.animations = false;
    c.complete(Ok("y".repeat(800)));
    let rows = render(&c, 120);
    let total: usize = rows[1..]
        .iter()
        .map(|r| r.trim_start_matches([' ', '└']).len())
        .sum();
    assert!((572 + 3..=575 + 8).contains(&total), "{rows:?}");
    assert!(rows.last().unwrap().ends_with("..."));
}

#[test]
fn mcp_names_split_into_server_and_tool() {
    assert_eq!(
        split_mcp_name("mcp__fake__echo"),
        Some(("fake".into(), "echo".into()))
    );
    assert_eq!(
        split_mcp_name("fake__echo"),
        Some(("fake".into(), "echo".into()))
    );
    assert_eq!(split_mcp_name("echo"), None);
}

#[test]
fn interrupted_mcp_call_is_a_red_called_with_an_error_row() {
    let mut c = McpCell::new("c", "fake", "sleep", json!({}));
    c.animations = false;
    c.mark_failed();
    assert_eq!(
        render(&c, 120),
        vec!["• Called fake.sleep({})", "  └ Error: interrupted"]
    );
}

// ---- separators and notices -------------------------------------------------------------------

#[test]
fn plain_rule_is_the_terminal_width_and_dim() {
    for w in [30u16, 80, 120, 200] {
        let rows = FinalMessageSeparator {
            elapsed_seconds: Some(12),
        }
        .display_lines(w);
        assert_eq!(rows.len(), 1);
        assert_eq!(plain(&rows[0]), "─".repeat(w as usize));
        assert!(rows[0].spans[0].style.add_modifier.contains(Modifier::DIM));
    }
}

#[test]
fn rule_after_a_long_turn_carries_the_elapsed_time() {
    let row = &render(
        &FinalMessageSeparator {
            elapsed_seconds: Some(68),
        },
        120,
    )[0];
    assert!(row.starts_with("─ Worked for 1m 08s ─────"), "{row}");
    assert_eq!(crate::width::display_width(row), 120);
    // exactly 60 s is not "over a minute"
    assert_eq!(
        render(
            &FinalMessageSeparator {
                elapsed_seconds: Some(60)
            },
            20
        )[0],
        "─".repeat(20)
    );
}

#[test]
fn info_error_and_warning_rows() {
    assert_eq!(
        render(
            &InfoCell {
                text: "Copied last message to clipboard".into(),
                hint: None
            },
            120
        ),
        vec!["• Copied last message to clipboard"]
    );
    let info = InfoCell {
        text: "Goal active".into(),
        hint: Some("Objective: ship it".into()),
    }
    .display_lines(120);
    assert_eq!(plain(&info[0]), "• Goal active Objective: ship it");
    assert_eq!(info[0].spans[3].style.fg, Some(Color::DarkGray));
    let warn = render(
        &WarningCell {
            text: "Heads up: Long threads and multiple compactions can cause the model to be less accurate. Start a new thread when possible to keep threads small and targeted.".into(),
        },
        120,
    );
    assert_eq!(
        warn,
        vec![
            "⚠ Heads up: Long threads and multiple compactions can cause the model to be less accurate. Start a new thread when",
            "  possible to keep threads small and targeted.",
        ]
    );
}

#[test]
fn error_with_a_url_keeps_the_url_whole_and_does_not_indent() {
    let rows = render(
        &ErrorCell {
            text: "You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again later.".into(),
        },
        120,
    );
    assert_eq!(
        rows,
        vec![
            "■ You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), visit",
            "https://chatgpt.com/codex/settings/usage to purchase more credits or try again later.",
        ]
    );
}

// ---- user message -----------------------------------------------------------------------------

#[test]
fn user_message_wraps_and_prefixes_each_line() {
    dark();
    // spec B.1.2: the wrap width is the cell width minus 3
    let rows = render(
        &UserCell {
            text: "_count_rows".into(),
        },
        12,
    );
    assert_eq!(rows, vec!["", "› _count_ro", "  ws", ""]);
}

#[test]
fn user_message_rows_are_tinted_to_the_edge() {
    dark();
    let rows = UserCell { text: "hi".into() }.display_lines(40);
    for r in &rows {
        assert_eq!(r.style.bg, Some(Color::Rgb(30, 30, 30)));
    }
    assert!(
        rows[1].spans[0]
            .style
            .add_modifier
            .contains(Modifier::BOLD | Modifier::DIM)
    );
}

#[test]
fn user_text_is_sanitised() {
    assert_eq!(
        sanitize_user_text("a\u{1b}[31mred\u{1b}[0m\u{7}b\tc\nd"),
        "aredb\tc\nd"
    );
}

#[test]
fn empty_user_message_has_no_rows() {
    assert!(UserCell { text: "\n".into() }.display_lines(80).is_empty());
}

// ---- reasoning --------------------------------------------------------------------------------

#[test]
fn reasoning_with_a_title_and_body_hides_the_title_inline() {
    let cell = ReasoningCell::new(
        "**Reading the config**\n\nI will open lib.rs first and then compare it with main.rs.\n\nThe signature decides the fix.",
    );
    let rows: Vec<String> = render(&cell, 120)
        .iter()
        .map(|r| r.trim_end().to_string())
        .collect();
    assert_eq!(
        rows,
        vec![
            "• I will open lib.rs first and then compare it with main.rs.",
            "",
            "  The signature decides the fix.",
        ]
    );
    let first = cell.display_lines(120)[0].clone();
    assert!(first.spans[0].style.add_modifier.contains(Modifier::DIM));
    assert!(
        first.spans[1]
            .style
            .add_modifier
            .contains(Modifier::DIM | Modifier::ITALIC)
    );
}

#[test]
fn a_title_only_summary_shows_inline_with_its_title() {
    let cell = ReasoningCell::new("**Just a title**");
    assert_eq!(render(&cell, 120), vec!["• Just a title"]);
    assert!(
        cell.display_lines(120)[0].spans[1]
            .style
            .add_modifier
            .contains(Modifier::BOLD)
    );
}

#[test]
fn an_unstructured_summary_is_only_in_the_transcript() {
    let cell = ReasoningCell::new("plain thought with no bold title");
    assert!(cell.display_lines(120).is_empty());
    assert_eq!(
        render_transcript(&cell, 120),
        vec!["• plain thought with no bold title"]
    );
}

#[test]
fn a_title_and_text_on_one_line_is_only_in_the_transcript() {
    let cell = ReasoningCell::new(
        "**Planning the change** I will read lib.rs, patch it, then check the toolchain.",
    );
    assert!(cell.display_lines(120).is_empty());
    assert_eq!(
        render_transcript(&cell, 120),
        vec!["• Planning the change I will read lib.rs, patch it, then check the toolchain."]
    );
}

fn render_transcript(cell: &dyn HistoryCell, width: u16) -> Vec<String> {
    cell.transcript_lines(width).iter().map(plain).collect()
}

#[test]
fn first_bold_of_a_summary_is_the_status_header() {
    assert_eq!(
        extract_first_bold("**Exploring the repo** List files"),
        Some("Exploring the repo".into())
    );
    assert_eq!(extract_first_bold("no bold here"), None);
    assert_eq!(extract_first_bold("**unfinished"), None);
    assert_eq!(extract_first_bold("**  **"), None);
}

// ---- assistant text and streaming ---------------------------------------------------------------

fn stream_all(log: &mut ChatLog, deltas: &[&str]) -> Vec<BoxedCell> {
    let mut out = Vec::new();
    for d in deltas {
        out.extend(log.apply(&Event::TextDelta((*d).into())));
    }
    out.extend(log.apply(&Event::TurnEnd(StopReason::EndTurn)));
    out
}

fn first_calls(cells: &[BoxedCell], width: u16) -> Vec<String> {
    // the way the app does it: each cell is asked once as it is appended
    cells
        .iter()
        .flat_map(|c| c.display_lines(width))
        .map(|l| plain(&l))
        .collect()
}

#[test]
fn streamed_list_item_keeps_its_hanging_indent() {
    let mut log = quiet_log();
    log.set_context(60, std::path::Path::new("/tmp"));
    let cells = stream_all(
        &mut log,
        &[
            "1. Correctness issue: server tool-search completions are rejected.\n\n   In next_prompt_suggestion.rs, ToolSearchCall records its call id, but a paired output is ignored and suppresses suggestions.\n",
        ],
    );
    // The streamed rows and the finished message agree: the markdown renderer wraps the item
    // at the content width and the continuation keeps the two gutter columns plus the list
    // indent (Codex's `streamed_agent_list_paragraph_preserves_item_indent_when_wrapped`, where
    // the unwrapped source line is wrapped by the cell instead, keeps the 3 column indent of
    // the paragraph the same way).
    let streamed = first_calls(&cells, 60);
    assert_eq!(
        streamed,
        vec![
            "• 1. Correctness issue: server tool-search completions are",
            "     rejected.",
            "",
            "     In next_prompt_suggestion.rs, ToolSearchCall records",
            "     its call id, but a paired output is ignored and",
            "     suppresses suggestions.",
        ]
    );
    let finished: Vec<String> = cells
        .iter()
        .flat_map(|c| c.display_lines(60))
        .map(|l| plain(&l))
        .collect();
    assert_eq!(finished, streamed);
}

#[test]
fn only_the_first_chunk_has_the_bullet_and_the_rest_continue_it() {
    let mut log = quiet_log();
    log.set_context(80, std::path::Path::new("/tmp"));
    let cells = stream_all(&mut log, &["one\n", "two\n", "three"]);
    assert!(!cells[0].is_stream_continuation());
    assert!(cells[1..].iter().all(|c| c.is_stream_continuation()));
    assert_eq!(first_calls(&cells, 80), vec!["• one", "  two", "  three"]);
}

#[test]
fn a_finished_answer_rerenders_at_the_new_width_from_the_first_chunk() {
    let mut log = quiet_log();
    log.set_context(80, std::path::Path::new("/tmp"));
    let cells = stream_all(
        &mut log,
        &[
            "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu\n",
            "second line\n",
        ],
    );
    let first_pass = first_calls(&cells, 80);
    assert_eq!(first_pass.len(), 2);
    // a reflow at 30 columns: the first cell carries the whole message, the others nothing
    let reflow: Vec<String> = cells
        .iter()
        .flat_map(|c| c.display_lines(30))
        .map(|l| plain(&l))
        .collect();
    assert_eq!(
        reflow,
        vec![
            "• alpha beta gamma delta",
            "  epsilon zeta eta theta iota",
            "  kappa lambda mu",
            "  second line",
        ]
    );
}

#[test]
fn text_without_a_newline_stays_out_of_the_history_and_keeps_the_status_row() {
    let mut log = quiet_log();
    log.apply(&Event::TurnStart);
    assert!(
        log.apply(&Event::TextDelta("Streaming a long paragraph".into()))
            .is_empty()
    );
    assert!(!log.text_streaming());
    log.apply(&Event::TextDelta(" more\n".into()));
    assert!(log.text_streaming());
}

#[test]
fn a_table_is_the_live_tail_and_asks_for_a_reflow_when_the_answer_ends() {
    let mut log = quiet_log();
    log.set_context(80, std::path::Path::new("/tmp"));
    log.apply(&Event::TurnStart);
    let cells = log.apply(&Event::TextDelta(
        "Intro\n\n| a | b |\n|---|---|\n| 1 | 2 |\n".into(),
    ));
    assert_eq!(first_calls(&cells, 80), vec!["• Intro"]);
    let tail: Vec<String> = log.active_lines(80).iter().map(plain).collect();
    assert_eq!(tail[0], "", "{tail:?}");
    assert!(tail.iter().any(|l| l.contains("━")), "{tail:?}");
    assert!(log.text_streaming());
    log.apply(&Event::TurnEnd(StopReason::EndTurn));
    assert!(log.take_reflow());
    assert!(!log.take_reflow());
}

#[test]
fn the_tail_is_drawn_with_the_two_space_gutter_when_the_answer_has_started() {
    let mut log = quiet_log();
    log.set_context(80, std::path::Path::new("/tmp"));
    log.apply(&Event::TextDelta("| a | b |\n|---|---|\n".into()));
    let tail: Vec<String> = log.active_lines(80).iter().map(plain).collect();
    assert!(tail[0].starts_with("• "), "{tail:?}");
}

// ---- turns --------------------------------------------------------------------------------------

fn exec_call(id: &str, command: &str, status: ToolStatus, output: Option<&str>) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "execute".into(),
        kind: ToolKind::Execute,
        title: command.into(),
        input: json!({ "command": command }),
        status,
        output: output.map(str::to_string),
        ..Default::default()
    }
}

fn texts(cells: &[BoxedCell], width: u16) -> Vec<Vec<String>> {
    cells.iter().map(|c| render(c.as_ref(), width)).collect()
}

#[test]
fn a_finished_command_is_one_cell_and_stays_in_flight_until_then() {
    let mut log = quiet_log();
    log.apply(&Event::TurnStart);
    assert!(
        log.apply(&Event::Tool(exec_call(
            "1",
            "echo hi",
            ToolStatus::Running,
            None
        )))
        .is_empty()
    );
    let live: Vec<String> = log.active_lines(80).iter().map(plain).collect();
    assert_eq!(live, vec!["• Running echo hi"]);
    let cells = log.apply(&Event::Tool(exec_call(
        "1",
        "echo hi",
        ToolStatus::Completed,
        Some("hi"),
    )));
    assert_eq!(
        texts(&cells, 80),
        vec![vec!["• Ran echo hi".to_string(), "  └ hi".into()]]
    );
    assert!(log.active_lines(80).is_empty());
}

#[test]
fn explore_calls_share_one_cell_until_something_else_arrives() {
    let mut log = quiet_log();
    log.apply(&Event::TurnStart);
    for (id, cmd, out) in [("1", "ls -la", "a"), ("2", "cat src/lib.rs", "b")] {
        assert!(
            log.apply(&Event::Tool(exec_call(id, cmd, ToolStatus::Running, None)))
                .is_empty()
        );
        assert!(
            log.apply(&Event::Tool(exec_call(
                id,
                cmd,
                ToolStatus::Completed,
                Some(out)
            )))
            .is_empty()
        );
    }
    let live: Vec<String> = log.active_lines(80).iter().map(plain).collect();
    assert_eq!(
        live,
        vec!["• Explored", "  └ List ls -la", "    Read lib.rs"]
    );
    let cells = log.apply(&Event::TextDelta("done\n".into()));
    // the rule goes in after the explored cell, before the answer
    assert_eq!(
        texts(&cells, 80),
        vec![
            vec![
                "• Explored".to_string(),
                "  └ List ls -la".into(),
                "    Read lib.rs".into()
            ],
            vec!["─".repeat(80)],
            vec!["• done".to_string()],
        ]
    );
}

#[test]
fn wizard_execute_results_are_split_into_output_and_exit_code() {
    let (out, code) = split_exec_result("running\nstderr:\nboom\nexit code: 101", true);
    assert_eq!((out.as_str(), code), ("running\nboom", 101));
    let (out, code) = split_exec_result("(command succeeded with no output)", false);
    assert_eq!((out.as_str(), code), ("", 0));
    let (out, code) = split_exec_result("exit code: 2", true);
    assert_eq!((out.as_str(), code), ("", 2));
    let (out, code) = split_exec_result("stderr:\nonly errors", true);
    assert_eq!((out.as_str(), code), ("only errors", 1));
}

#[test]
fn a_turn_that_did_work_ends_with_a_rule_and_a_chat_turn_does_not() {
    let mut log = quiet_log();
    log.apply(&Event::TurnStart);
    log.apply(&Event::Tool(exec_call(
        "1",
        "echo hi",
        ToolStatus::Running,
        None,
    )));
    log.apply(&Event::Tool(exec_call(
        "1",
        "echo hi",
        ToolStatus::Completed,
        Some("hi"),
    )));
    let end = log.apply(&Event::TurnEnd(StopReason::EndTurn));
    assert_eq!(texts(&end, 40), vec![vec!["─".repeat(40)]]);

    log.apply(&Event::TurnStart);
    log.apply(&Event::TextDelta("hello\n".into()));
    let end = log.apply(&Event::TurnEnd(StopReason::EndTurn));
    assert!(
        end.iter()
            .all(|c| !render(c.as_ref(), 40).iter().any(|r| r.starts_with('─')))
    );
}

#[test]
fn interrupting_prints_the_notice_and_no_rule() {
    let mut log = quiet_log();
    log.apply(&Event::TurnStart);
    log.apply(&Event::Tool(exec_call(
        "1",
        "sleep 9",
        ToolStatus::Running,
        None,
    )));
    let end = log.apply(&Event::TurnEnd(StopReason::Cancelled));
    let rows = texts(&end, 120);
    assert_eq!(rows[0][0], "• Ran sleep 9");
    assert!(rows[1][0].starts_with("■ Conversation interrupted - tell the model"));
    assert_eq!(rows[1][1], "issue.");
    assert_eq!(end.len(), 2);
}

#[test]
fn a_failed_edit_prints_the_failure_row() {
    let mut log = quiet_log();
    log.apply(&Event::TurnStart);
    let call = ToolCall {
        id: "e".into(),
        name: "edit_file".into(),
        kind: ToolKind::Edit,
        input: json!({"path": "missing.rs"}),
        status: ToolStatus::Failed,
        output: Some("File not found".into()),
        diff: Some(FileDiff {
            path: "missing.rs".into(),
            old: Some("x".into()),
            new: "y".into(),
        }),
        ..Default::default()
    };
    let cells = log.apply(&Event::Tool(call));
    // the reason goes under it, as codex prints a patch's stderr
    assert_eq!(
        texts(&cells, 80),
        vec![vec![
            "✘ Failed to apply patch".to_string(),
            "  └ File not found".to_string()
        ]]
    );
}

#[test]
fn an_edit_is_a_patch_cell_with_a_relative_path() {
    let mut log = quiet_log();
    log.set_context(80, std::path::Path::new("/tmp/proj"));
    log.apply(&Event::TurnStart);
    let call = ToolCall {
        id: "e".into(),
        name: "edit_file".into(),
        kind: ToolKind::Edit,
        input: json!({"path": "/tmp/proj/src/lib.rs"}),
        status: ToolStatus::Completed,
        diff: Some(FileDiff {
            path: "/tmp/proj/src/lib.rs".into(),
            old: Some("a + b".into()),
            new: "a.wrapping_add(b)".into(),
        }),
        ..Default::default()
    };
    let cells = log.apply(&Event::Tool(call));
    assert_eq!(
        texts(&cells, 80),
        vec![vec![
            "• Edited src/lib.rs (+1 -1)".to_string(),
            "    1 -a + b".into(),
            "    1 +a.wrapping_add(b)".into()
        ]]
    );
}

#[test]
fn a_todo_write_is_a_plan_cell_and_a_read_is_nothing() {
    let mut log = quiet_log();
    log.apply(&Event::TurnStart);
    let write = ToolCall {
        id: "t".into(),
        name: "todo".into(),
        input: json!({"action": "write", "items": [{"content": "Read", "status": "in_progress"}]}),
        status: ToolStatus::Completed,
        ..Default::default()
    };
    let cells = log.apply(&Event::Tool(write));
    assert_eq!(
        texts(&cells, 80),
        vec![vec!["• Updated Plan".to_string(), "  └ □ Read".into()]]
    );
    let read = ToolCall {
        id: "t2".into(),
        name: "todo".into(),
        input: json!({"action": "read"}),
        status: ToolStatus::Completed,
        ..Default::default()
    };
    assert!(log.apply(&Event::Tool(read)).is_empty());
}

#[test]
fn an_mcp_tool_is_a_called_cell_and_a_search_a_pair_of_cells() {
    let mut log = quiet_log();
    log.apply(&Event::TurnStart);
    let mcp = |status, out: Option<&str>| ToolCall {
        id: "m".into(),
        name: "mcp__fake__echo".into(),
        kind: ToolKind::Other,
        input: json!({"message": "hi"}),
        status,
        output: out.map(str::to_string),
        ..Default::default()
    };
    assert!(
        log.apply(&Event::Tool(mcp(ToolStatus::Running, None)))
            .is_empty()
    );
    let live: Vec<String> = log.active_lines(80).iter().map(plain).collect();
    assert_eq!(live, vec![r#"• Calling fake.echo({"message":"hi"})"#]);
    let cells = log.apply(&Event::Tool(mcp(ToolStatus::Completed, Some("hi"))));
    assert_eq!(
        texts(&cells, 80),
        vec![vec![
            r#"• Called fake.echo({"message":"hi"})"#.to_string(),
            "  └ hi".into()
        ]]
    );
    let search = |status| ToolCall {
        id: "s".into(),
        name: "web_search".into(),
        kind: ToolKind::Search,
        input: json!({"query": "rust"}),
        status,
        ..Default::default()
    };
    log.apply(&Event::Tool(search(ToolStatus::Running)));
    let cells = log.apply(&Event::Tool(search(ToolStatus::Completed)));
    assert_eq!(
        texts(&cells, 80),
        vec![
            vec!["• Searching the web".to_string()],
            vec!["• Searched the web for rust".to_string()]
        ]
    );
}

#[test]
fn reasoning_title_becomes_the_status_header_for_the_rest_of_the_turn() {
    let mut log = quiet_log();
    log.apply(&Event::TurnStart);
    assert_eq!(log.status_header(), None);
    log.apply(&Event::ThoughtDelta(
        "**Exploring the repo** List files".into(),
    ));
    assert_eq!(log.status_header().as_deref(), Some("Exploring the repo"));
    log.apply(&Event::Tool(exec_call(
        "1",
        "ls",
        ToolStatus::Running,
        None,
    )));
    assert_eq!(log.status_header().as_deref(), Some("Exploring the repo"));
    log.apply(&Event::TurnEnd(StopReason::EndTurn));
    assert_eq!(log.status_header(), None);
}

#[test]
fn notices_and_fatal_errors_become_cells() {
    let mut log = quiet_log();
    let cells = log.apply(&Event::Notice {
        level: NoticeLevel::Warn,
        text: "careful".into(),
    });
    assert_eq!(texts(&cells, 80), vec![vec!["⚠ careful".to_string()]]);
    let cells = log.apply(&Event::Notice {
        level: NoticeLevel::Error,
        text: "bad".into(),
    });
    assert_eq!(texts(&cells, 80), vec![vec!["■ bad".to_string()]]);
    let cells = log.apply(&Event::Fatal("gone".into()));
    assert_eq!(texts(&cells, 80), vec![vec!["■ gone".to_string()]]);
}

#[test]
fn replay_makes_whole_source_backed_answers() {
    let mut log = ChatLog::new_replay();
    log.apply(&Event::TextDelta("## Heading\n\ntext".into()));
    let cells = log.apply(&Event::TurnEnd(StopReason::EndTurn));
    assert_eq!(
        texts(&cells, 40),
        vec![vec!["• ## Heading".to_string(), "".into(), "  text".into()]]
    );
}

// ---- palettes -----------------------------------------------------------------------------------

fn light() {
    set_palette(Palette::new(
        Some((26, 26, 26)),
        Some((255, 255, 255)),
        ColorLevel::TrueColor,
    ));
}

fn style_of_span(rows: &[Line<'static>], needle: &str) -> Style {
    rows.iter()
        .flat_map(|l| l.spans.iter())
        .find(|s| s.content.contains(needle))
        .unwrap_or_else(|| panic!("no span with {needle:?}"))
        .style
}

#[test]
fn table_header_and_rule_follow_the_terminal_theme() {
    let src = "| aa | bb |\n|---|---|\n| 1 | 2 |\n";
    dark();
    let rows = agent_markdown_lines(src, 40, None);
    // the header colour sits on the row's line style, the rule on its span
    let header = rows[0].style;
    assert_eq!(header.fg, Some(Color::Rgb(249, 226, 175)));
    assert!(header.add_modifier.contains(Modifier::BOLD));
    assert_eq!(style_of_span(&rows, "━").fg, Some(Color::Rgb(46, 46, 46)));

    light();
    let rows = agent_markdown_lines(src, 40, None);
    assert_eq!(rows[0].style.fg, Some(Color::Rgb(223, 142, 29)));
    assert_eq!(
        style_of_span(&rows, "━").fg,
        Some(Color::Rgb(209, 209, 209))
    );
    dark();
}

#[test]
fn code_blocks_use_latte_on_a_light_terminal_and_stay_rgb_at_256_colours() {
    light();
    let rows = agent_markdown_lines("```rust\nfn main() {}\n```\n", 40, None);
    let fn_span = rows[0]
        .spans
        .iter()
        .find(|s| s.content == "fn")
        .expect("fn");
    assert_eq!(fn_span.style.fg, Some(Color::Rgb(136, 57, 239)));
    set_palette(Palette::new(
        Some((230, 230, 230)),
        Some((0, 0, 0)),
        ColorLevel::Ansi256,
    ));
    let rows = agent_markdown_lines("```rust\nfn main() {}\n```\n", 40, None);
    let fn_span = rows[0]
        .spans
        .iter()
        .find(|s| s.content == "fn")
        .expect("fn");
    assert_eq!(fn_span.style.fg, Some(Color::Rgb(203, 166, 247)));
    dark();
}

#[test]
fn info_notice_with_line_breaks_gets_a_row_per_line() {
    let rows = render(
        &InfoCell {
            text: "usage: 10 prompt + 4 completion tokens\ncontext: 99 tokens\n".into(),
            hint: None,
        },
        80,
    );
    assert_eq!(
        rows,
        vec![
            "• usage: 10 prompt + 4 completion tokens",
            "  context: 99 tokens"
        ]
    );
}

#[test]
fn a_failed_edit_without_a_diff_still_prints_the_failure_row() {
    // wizard drops the diff of a failed call
    let mut log = quiet_log();
    log.apply(&Event::TurnStart);
    let call = ToolCall {
        id: "e".into(),
        name: "edit_file".into(),
        kind: ToolKind::Edit,
        input: json!({"path": "missing.rs", "old_string": "x", "new_string": "y"}),
        status: ToolStatus::Failed,
        output: Some("File not found".into()),
        diff: None,
        ..Default::default()
    };
    let cells = log.apply(&Event::Tool(call));
    // the reason goes under it, as codex prints a patch's stderr
    assert_eq!(
        texts(&cells, 80),
        vec![vec![
            "✘ Failed to apply patch".to_string(),
            "  └ File not found".to_string()
        ]]
    );
}

#[test]
fn notes_say_once_what_wizard_cannot_do() {
    let mut log = ChatLog::new();
    log.notes = true;
    log.apply(&Event::TurnStart);
    let run = |log: &mut ChatLog, id: &str| {
        log.apply(&Event::Tool(exec_call(
            id,
            "echo hi",
            ToolStatus::Running,
            None,
        )));
        log.apply(&Event::Tool(exec_call(
            id,
            "echo hi",
            ToolStatus::Completed,
            Some("hi"),
        )))
    };
    let first = run(&mut log, "1");
    assert_eq!(
        texts(&first, 120),
        vec![
            vec!["• Ran echo hi".to_string(), "  └ hi".into()],
            vec![EXEC_OUTPUT_NOTE.to_string()],
        ]
    );
    assert_eq!(run(&mut log, "2").len(), 1);

    let edit = |id: &str, old: Option<&str>| ToolCall {
        id: id.into(),
        name: "edit_file".into(),
        kind: ToolKind::Edit,
        input: json!({"path": "a.rs"}),
        status: ToolStatus::Completed,
        diff: Some(FileDiff {
            path: "a.rs".into(),
            old: old.map(str::to_string),
            new: "b".into(),
        }),
        ..Default::default()
    };
    let cells = log.apply(&Event::Tool(edit("e1", Some("a"))));
    assert_eq!(texts(&cells, 200)[1], vec![PATCH_SNIPPET_NOTE.to_string()]);
    assert_eq!(log.apply(&Event::Tool(edit("e2", Some("a")))).len(), 1);
}
