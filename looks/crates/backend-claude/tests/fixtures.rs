//! Replays recorded `claude -p` stream-json transcripts (tests/fixtures/*.jsonl, recorded
//! on claude 2.1.289 against haiku, paths and ids sanitized) through the mapper. No process.

use agent_core::*;
use backend_claude::{history, mapper::Mapper};
use std::path::Path;

fn lines(name: &str) -> Vec<serde_json::Value> {
    let p = format!("{}/tests/fixtures/{name}.jsonl", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("{p}: {e}"))
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn replay(name: &str) -> Vec<Event> {
    let mut m = Mapper::new("/work/proj".into());
    lines(name).iter().flat_map(|v| m.feed(v)).collect()
}

fn text(evs: &[Event]) -> String {
    evs.iter()
        .filter_map(|e| {
            if let Event::TextDelta(t) = e {
                Some(t.as_str())
            } else {
                None
            }
        })
        .collect()
}

fn thought(evs: &[Event]) -> String {
    evs.iter()
        .filter_map(|e| {
            if let Event::ThoughtDelta(t) = e {
                Some(t.as_str())
            } else {
                None
            }
        })
        .collect()
}

/// Last version of each tool call, in order of first appearance.
fn calls(evs: &[Event]) -> Vec<ToolCall> {
    let mut out: Vec<ToolCall> = Vec::new();
    for e in evs {
        if let Event::Tool(t) = e {
            match out.iter_mut().find(|o| o.id == t.id) {
                Some(o) => *o = t.clone(),
                None => out.push(t.clone()),
            }
        }
    }
    out
}

fn notices(evs: &[Event]) -> Vec<(NoticeLevel, String)> {
    evs.iter()
        .filter_map(|e| {
            if let Event::Notice { level, text } = e {
                Some((*level, text.clone()))
            } else {
                None
            }
        })
        .collect()
}

fn ends(evs: &[Event]) -> Vec<StopReason> {
    evs.iter()
        .filter_map(|e| {
            if let Event::TurnEnd(r) = e {
                Some(*r)
            } else {
                None
            }
        })
        .collect()
}

fn starts(evs: &[Event]) -> usize {
    evs.iter().filter(|e| matches!(e, Event::TurnStart)).count()
}

fn last_usage(evs: &[Event]) -> Usage {
    evs.iter()
        .rev()
        .find_map(|e| {
            if let Event::Usage(u) = e {
                Some(u.clone())
            } else {
                None
            }
        })
        .expect("no usage")
}

#[test]
fn plain_answer_streams_text_and_reports_usage() {
    let evs = replay("plain");
    assert_eq!(evs.first(), Some(&Event::TurnStart));
    assert_eq!(text(&evs), "pong");
    assert_eq!(
        thought(&evs),
        "",
        "thinking is empty unless summaries are on"
    );
    assert_eq!(ends(&evs), vec![StopReason::EndTurn]);
    assert_eq!(starts(&evs), 1);
    let u = last_usage(&evs);
    assert_eq!(u.cost_usd, Some(0.0180703));
    assert_eq!(u.input_tokens, 10);
    assert_eq!(u.output_tokens, 46);
    assert_eq!(u.cached_tokens, 13803);
    // input + cache read + cache creation of the last API call
    assert_eq!(u.context_tokens, 10 + 13803 + 8225);
    assert_eq!(u.context_window, 200_000);
}

#[test]
fn whole_assistant_messages_do_not_repeat_streamed_text() {
    // The fixture has deltas "p", "ong" and then an assistant message with "pong".
    assert_eq!(text(&replay("plain")).matches("pong").count(), 1);
}

#[test]
fn thinking_summaries_become_thought_deltas() {
    let evs = replay("thinking");
    assert!(thought(&evs).contains("= 391"), "{}", thought(&evs));
    assert!(text(&evs).contains("391"));
    let first_thought = evs
        .iter()
        .position(|e| matches!(e, Event::ThoughtDelta(_)))
        .unwrap();
    let first_text = evs
        .iter()
        .position(|e| matches!(e, Event::TextDelta(_)))
        .unwrap();
    assert!(first_thought < first_text);
}

#[test]
fn tool_turn_maps_kinds_titles_diffs_and_outputs() {
    let evs = replay("tools");
    let c = calls(&evs);
    let by = |name: &str| c.iter().filter(|t| t.name == name).collect::<Vec<_>>();
    assert!(
        c.iter()
            .all(|t| t.status == ToolStatus::Completed || t.status == ToolStatus::Failed),
        "{c:?}"
    );

    let bash = by("Bash");
    assert_eq!(bash[0].kind, ToolKind::Execute);
    assert_eq!(bash[0].title, "find . -name \"*.py\" -type f");
    assert_eq!(bash[0].output.as_deref(), Some("./b.py\n./a.py"));
    assert_eq!(
        bash.last().unwrap().output.as_deref(),
        Some("a.py\nb.py\nnew.txt\nnotes.txt")
    );

    let read = &by("Read")[0];
    assert_eq!(read.kind, ToolKind::Read);
    assert_eq!(
        read.title, "notes.txt",
        "paths under the cwd are shown relative"
    );
    assert!(read.output.as_deref().unwrap().contains("2\tbeta"));

    let edit = &by("Edit")[0];
    assert_eq!(edit.kind, ToolKind::Edit);
    assert_eq!(
        edit.diff,
        Some(FileDiff {
            path: "/work/proj/notes.txt".into(),
            old: Some("beta".into()),
            new: "BETA".into()
        })
    );

    let write = &by("Write")[0];
    assert_eq!(
        write.diff,
        Some(FileDiff {
            path: "/work/proj/new.txt".into(),
            old: None,
            new: "hi".into()
        })
    );
    assert!(write
        .output
        .as_deref()
        .unwrap()
        .starts_with("File created successfully"));

    assert_eq!(ends(&evs), vec![StopReason::EndTurn]);
}

#[test]
fn tool_is_announced_pending_then_running_then_done() {
    let evs = replay("tools");
    let first = calls(&evs)[0].id.clone();
    let seq: Vec<ToolStatus> = evs
        .iter()
        .filter_map(|e| {
            if let Event::Tool(t) = e {
                Some(t).filter(|t| t.id == first)
            } else {
                None
            }
        })
        .map(|t| t.status)
        .collect();
    assert_eq!(
        seq,
        vec![
            ToolStatus::Pending,
            ToolStatus::Running,
            ToolStatus::Completed
        ]
    );
}

#[test]
fn task_create_and_update_build_the_todo_list() {
    let evs = replay("todo_tasks");
    let lists: Vec<&Vec<Todo>> = evs
        .iter()
        .filter_map(|e| {
            if let Event::Todos(t) = e {
                Some(t)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        lists[0],
        &vec![Todo {
            text: "Read notes.txt".into(),
            status: TodoStatus::Pending
        }]
    );
    let last = lists.last().unwrap();
    assert_eq!(last.len(), 3);
    assert!(
        last.iter().all(|t| t.status == TodoStatus::Completed),
        "{last:?}"
    );
    assert_eq!(last[1].text, "Count lines");
}

#[test]
fn todowrite_replaces_the_list_and_ignores_invalid_calls() {
    let evs = replay("todo_write");
    let c = calls(&evs);
    let failed: Vec<_> = c
        .iter()
        .filter(|t| t.name == "TodoWrite" && t.status == ToolStatus::Failed)
        .collect();
    assert_eq!(failed.len(), 2, "the first two calls had the wrong schema");
    assert!(failed[0]
        .output
        .as_deref()
        .unwrap()
        .starts_with("InputValidationError"));
    let lists: Vec<&Vec<Todo>> = evs
        .iter()
        .filter_map(|e| {
            if let Event::Todos(t) = e {
                Some(t)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(lists.len(), 2);
    assert_eq!(
        lists[0].iter().map(|t| t.status).collect::<Vec<_>>(),
        vec![TodoStatus::Pending; 3]
    );
    assert!(lists[1].iter().all(|t| t.status == TodoStatus::Completed));
}

#[test]
fn foreground_subagent_nests_its_calls_and_returns_the_report() {
    let evs = replay("subagent_fg");
    let c = calls(&evs);
    let agent = c.iter().find(|t| t.name == "Agent").unwrap();
    assert_eq!(agent.kind, ToolKind::Think);
    assert_eq!(agent.title, "Read notes.txt and count its lines");
    assert_eq!(agent.parent_id, None);
    assert_eq!(agent.status, ToolStatus::Completed);
    let out = agent.output.as_deref().unwrap();
    assert!(out.starts_with("## Results"), "{out}");
    assert!(!out.contains("Subagent hand-back"));
    let inner: Vec<_> = c
        .iter()
        .filter(|t| t.parent_id.as_deref() == Some(agent.id.as_str()))
        .collect();
    assert_eq!(
        inner.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        vec!["Read", "Bash"]
    );
    assert_eq!(starts(&evs), 1);
    assert_eq!(ends(&evs), vec![StopReason::EndTurn]);
}

#[test]
fn background_subagent_completes_after_the_turn_that_launched_it() {
    let evs = replay("subagent_bg");
    // launch turn, then the CLI starts a second turn on its own for the notification
    assert_eq!(starts(&evs), 2);
    assert_eq!(ends(&evs), vec![StopReason::EndTurn, StopReason::EndTurn]);
    let first_end = evs
        .iter()
        .position(|e| matches!(e, Event::TurnEnd(_)))
        .unwrap();
    let agent_states: Vec<(usize, ToolStatus)> = evs
        .iter()
        .enumerate()
        .filter_map(|(i, e)| {
            if let Event::Tool(t) = e {
                Some(t).filter(|t| t.name == "Agent").map(|t| (i, t.status))
            } else {
                None
            }
        })
        .collect();
    assert!(agent_states
        .iter()
        .any(|(i, s)| *i < first_end && *s == ToolStatus::Running));
    assert!(
        agent_states
            .iter()
            .all(|(i, s)| !(*i < first_end && *s == ToolStatus::Completed)),
        "the launch result must not complete the call"
    );
    let agent = calls(&evs).into_iter().find(|t| t.name == "Agent").unwrap();
    assert_eq!(agent.status, ToolStatus::Completed);
    assert!(agent.output.as_deref().unwrap().contains("three lines"));
    // inner calls arrive after the first TurnEnd and still carry the parent
    let inner_at = evs
        .iter()
        .position(|e| matches!(e, Event::Tool(t) if t.name == "Read"))
        .unwrap();
    assert!(inner_at > first_end);
}

#[test]
fn denied_tools_fail_with_the_cli_message() {
    let evs = replay("denied");
    let c = calls(&evs);
    assert_eq!(c.len(), 2);
    for t in &c {
        assert_eq!(t.status, ToolStatus::Failed);
        assert!(
            t.output
                .as_deref()
                .unwrap()
                .starts_with("Permission for this tool use was denied"),
            "{:?}",
            t.output
        );
    }
    assert!(notices(&evs).is_empty(), "a denial is not a turn error");
    assert_eq!(ends(&evs), vec![StopReason::EndTurn]);
}

#[test]
fn denial_with_a_reason_keeps_the_reason() {
    let evs = replay("permission_denied");
    let t = &calls(&evs)[0];
    assert_eq!(t.status, ToolStatus::Failed);
    assert!(t.output.as_deref().unwrap().contains("needs approval"));
}

#[test]
fn interrupting_a_tool_cancels_the_turn_and_the_next_prompt_works() {
    let evs = replay("interrupt_tool");
    assert_eq!(ends(&evs), vec![StopReason::Cancelled, StopReason::EndTurn]);
    assert_eq!(starts(&evs), 2);
    let c = calls(&evs);
    assert_eq!(c[0].status, ToolStatus::Failed);
    assert!(c[0]
        .output
        .as_deref()
        .unwrap()
        .contains("doesn't want to proceed"));
    assert!(notices(&evs).is_empty(), "a cancel is not an error");
}

#[test]
fn interrupting_a_stream_keeps_the_text_once() {
    let evs = replay("interrupt_stream");
    assert_eq!(ends(&evs), vec![StopReason::Cancelled, StopReason::EndTurn]);
    assert!(notices(&evs).is_empty());
    let t = text(&evs);
    assert!(t.starts_with("1\n2\n3"), "{t:?}");
    assert!(t.ends_with("after"));
    assert_eq!(
        t.matches("1\n2\n3").count(),
        1,
        "the partial assistant message must not be re-emitted"
    );
}

#[test]
fn local_slash_commands_become_notices_without_text() {
    let evs = replay("slash");
    let n = notices(&evs);
    assert!(n[0].1.starts_with("## Context Usage"));
    assert_eq!(n[0].0, NoticeLevel::Info);
    assert!(n
        .iter()
        .any(|(_, t)| t.starts_with("Set effort level to high")));
    assert!(n
        .iter()
        .any(|(_, t)| t.contains("Available: sonnet, opus, haiku")));
    assert!(n.iter().any(|(_, t)| t.contains("MCP server")));
    assert_eq!(
        starts(&evs),
        ends(&evs).len(),
        "every turn that starts also ends"
    );
}

#[test]
fn clear_switches_session_id_via_empty_history() {
    let evs = replay("slash");
    let h = evs
        .iter()
        .find_map(|e| {
            if let Event::History { session_id, items } = e {
                Some((session_id.clone(), items.len()))
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(h, ("75350cfc-1285-4b08-9418-81c9bdff36cd".to_string(), 0));
}

#[test]
fn unknown_slash_command_is_just_a_model_turn() {
    let evs = replay("slash");
    assert!(text(&evs).contains("doesn't exist"));
}

#[test]
fn compaction_is_reported_and_resets_context() {
    let evs = replay("compact");
    let n = notices(&evs);
    assert!(n.iter().any(|(_, t)| t == "Compacting context..."));
    assert!(n
        .iter()
        .any(|(_, t)| t == "Context compacted: 22.4k -> 2.3k tokens"));
    assert_eq!(last_usage(&evs).context_tokens, 2275);
    assert_eq!(ends(&evs).len(), 2);
}

#[test]
fn api_retries_then_error_result() {
    let evs = replay("retry_529");
    let n = notices(&evs);
    assert_eq!(
        n[0],
        (
            NoticeLevel::Warn,
            "API 529 (overloaded), retry 1/2 in 0.5s".to_string()
        )
    );
    assert_eq!(n[1].0, NoticeLevel::Warn);
    assert_eq!(n[2].0, NoticeLevel::Error);
    assert!(n[2].1.starts_with("API Error: 529"));
    assert_eq!(ends(&evs), vec![StopReason::Error]);
    assert_eq!(
        text(&evs),
        "",
        "the synthetic error message is not assistant text"
    );
}

#[test]
fn rate_limit_result_with_exit_zero_is_an_error_notice() {
    let evs = replay("rate_limit_429");
    let n = notices(&evs);
    assert_eq!(n.last().unwrap().0, NoticeLevel::Error);
    assert!(n.last().unwrap().1.contains("429"));
    assert_eq!(ends(&evs), vec![StopReason::Error]);
}

#[test]
fn plan_limit_text_is_an_error_even_when_is_error_is_false() {
    let mut m = Mapper::new("/w".into());
    let r = serde_json::json!({"type":"result","subtype":"success","is_error":false,"num_turns":1,"result":"You've hit your session limit · resets 5pm","total_cost_usd":0.0});
    let evs = m.feed(&r);
    assert!(
        matches!(&evs[1], Event::Notice { level: NoticeLevel::Error, text } if text.starts_with("You've hit your")),
        "{evs:?}"
    );
    assert_eq!(evs.last(), Some(&Event::TurnEnd(StopReason::Error)));
}

#[test]
fn max_turns_result_maps_to_max_turns() {
    let mut m = Mapper::new("/w".into());
    let r = serde_json::json!({"type":"result","subtype":"error_max_turns","is_error":true,"num_turns":9,"result":"","errors":[]});
    assert_eq!(
        m.feed(&r).last(),
        Some(&Event::TurnEnd(StopReason::MaxTurns))
    );
}

#[test]
fn resumed_session_keeps_its_id() {
    let mut m = Mapper::new("/work/proj".into());
    let evs: Vec<Event> = lines("resume").iter().flat_map(|v| m.feed(v)).collect();
    assert_eq!(m.session_id, "ac56c04b-f76f-4057-8b7e-3c78fc53ce54");
    assert_eq!(text(&evs), "pong");
    assert!(
        !evs.iter().any(|e| matches!(e, Event::History { .. })),
        "same id, no history event"
    );
}

#[test]
fn restart_cost_offset_keeps_cost_monotonic() {
    let mut m = Mapper::new("/w".into());
    m.set_cost_offset(0.5);
    let r = serde_json::json!({"type":"result","subtype":"success","is_error":false,"num_turns":1,"result":"ok","total_cost_usd":0.25,"usage":{"input_tokens":1,"output_tokens":1},"modelUsage":{"m":{"contextWindow":1000000}}});
    let u = m
        .feed(&r)
        .into_iter()
        .find_map(|e| {
            if let Event::Usage(u) = e {
                Some(u)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(u.cost_usd, Some(0.75));
    assert_eq!(u.context_window, 1_000_000, "window comes from modelUsage");
}

#[test]
fn killed_mid_turn_fails_running_tools_and_ends_the_turn() {
    let mut m = Mapper::new("/work/proj".into());
    let all = lines("interrupt_tool");
    for v in &all[..all.iter().position(|v| v["type"] == "user").unwrap()] {
        m.feed(v);
    }
    let evs = m.abort_turn(StopReason::Error, "claude exited");
    assert!(matches!(&evs[0], Event::Tool(t) if t.status == ToolStatus::Failed));
    assert_eq!(evs.last(), Some(&Event::TurnEnd(StopReason::Error)));
    assert!(m.abort_turn(StopReason::Error, "x").is_empty());
}

#[test]
fn transcript_replay_folds_results_into_tool_calls() {
    let f = std::fs::File::open(format!(
        "{}/tests/fixtures/session_tools.jsonl",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let items = history::parse(std::io::BufReader::new(f), Path::new("/work/proj"));
    assert!(matches!(&items[0], HistoryItem::User(t) if t.starts_with("In this directory")));
    let tools: Vec<&ToolCall> = items
        .iter()
        .filter_map(|i| {
            if let HistoryItem::Tool(t) = i {
                Some(t)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(tools.len(), 7);
    assert!(tools.iter().all(|t| t.output.is_some()));
    let edit = tools.iter().find(|t| t.name == "Edit").unwrap();
    assert_eq!(edit.diff.as_ref().unwrap().new, "BETA");
    assert_eq!(edit.status, ToolStatus::Completed);
    assert!(matches!(items.last().unwrap(), HistoryItem::Assistant(_)));
    // no raw tool_result blobs leak in as user prompts
    assert_eq!(
        items
            .iter()
            .filter(|i| matches!(i, HistoryItem::User(_)))
            .count(),
        1
    );
}

#[test]
fn sessions_listing_uses_title_then_first_prompt_and_skips_empty_files() {
    let root = std::env::temp_dir().join(format!("backend-claude-test-{}", std::process::id()));
    let cwd = Path::new("/work/some.proj");
    let dir = root.join(history::slug(cwd));
    std::fs::create_dir_all(&dir).unwrap();
    let user = |t: &str| {
        format!("{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"{t}\"}}}}\n")
    };
    std::fs::write(dir.join("aaa.jsonl"), user("first prompt here")).unwrap();
    std::fs::write(
        dir.join("bbb.jsonl"),
        format!(
            "{}{{\"type\":\"ai-title\",\"aiTitle\":\"Nice title\"}}\n",
            user("other")
        ),
    )
    .unwrap();
    std::fs::write(dir.join("ccc.jsonl"), "{\"type\":\"queue-operation\"}\n").unwrap();
    std::fs::write(dir.join("notes.txt"), "x").unwrap();
    let list = history::list_sessions_in(&root, cwd, 10);
    let mut got: Vec<(String, String)> = list
        .iter()
        .map(|s| (s.id.clone(), s.title.clone()))
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            ("aaa".into(), "first prompt here".into()),
            ("bbb".into(), "Nice title".into())
        ]
    );
    assert!(list
        .iter()
        .all(|s| s.updated > 0 && s.cwd == "/work/some.proj"));
    std::fs::remove_dir_all(&root).ok();
}
