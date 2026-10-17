//! Recorded `wizard acp 3.7.1` transcripts replayed through the core.

mod common;

use agent_core::*;
use common::replay;

fn texts(events: &[Event]) -> String {
    events
        .iter()
        .filter_map(|e| {
            if let Event::TextDelta(t) = e {
                Some(t.as_str())
            } else {
                None
            }
        })
        .collect()
}

#[test]
fn new_session_builds_ready_and_commands() {
    let run = replay("new_session", "/work/proj", None);
    let Event::Ready { session_id, config } = &run.events[0] else {
        panic!("{:?}", run.events[0])
    };
    assert_eq!(session_id, "2026-10-05T17-21-17");
    assert_eq!(config.model, "xai-oauth/grok-4.7");
    assert_eq!(config.models.len(), 17);
    assert_eq!(
        config.models[0],
        ModelOption {
            id: "xai-oauth/grok-4.7".into(),
            name: "grok-4.7".into(),
            provider: "xai-oauth".into()
        }
    );
    assert!(config
        .models
        .iter()
        .any(|m| m.id == "sgapi/panel" && m.name == "panel" && m.provider == "sgapi"));
    assert_eq!(
        config.efforts,
        ["default", "low", "medium", "high", "xhigh"]
    );
    assert_eq!(config.effort, "default");
    assert_eq!(config.modes, ["genie", "sovereign", "chat"]);
    assert_eq!(config.mode, "genie");
    assert_eq!(config.cwd, "/work/proj");
    assert_eq!(config.backend, "wizard 3.7.1");
    let Event::Commands(cmds) = &run.events[1] else {
        panic!()
    };
    assert_eq!(cmds.len(), 32);
    let model = cmds.iter().find(|c| c.name == "model").unwrap();
    assert_eq!(model.input_hint, "[tag]");
    assert_eq!(
        cmds.iter().find(|c| c.name == "diff").unwrap().input_hint,
        ""
    );
    // Handshake order on the wire.
    let methods: Vec<_> = run
        .sent
        .iter()
        .map(|m| m["method"].as_str().unwrap())
        .collect();
    assert_eq!(methods, ["initialize", "session/new"]);
    assert_eq!(run.sent[1]["params"]["cwd"], "/work/proj");
}

#[test]
fn backend_string_falls_back_to_initialize_answer() {
    let mut core = backend_wizard::core::Core::new("/w".into(), None, String::new());
    core.start();
    core.on_line(r#"{"jsonrpc":"2.0","id":1,"result":{"agentInfo":{"version":"3.7.1"}}}"#);
    core.on_line(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"a","configOptions":[]}}"#);
    assert_eq!(core.config().backend, "wizard 3.7.1");
}

#[test]
fn tool_turn_event_sequence() {
    let run = replay("todo_exec_slash", "/work/proj", None);
    let from = run
        .events
        .iter()
        .position(|e| *e == Event::TurnStart)
        .unwrap();
    let to = run
        .events
        .iter()
        .position(|e| matches!(e, Event::TurnEnd(_)))
        .unwrap();
    let kinds: Vec<String> = run.events[from..=to]
        .iter()
        .map(|e| match e {
            Event::TurnStart => "TurnStart".into(),
            Event::Tool(t) => format!("Tool({} {:?})", t.name, t.status),
            Event::Todos(t) => format!("Todos({})", t.len()),
            Event::TextDelta(_) => "TextDelta".into(),
            Event::TurnEnd(r) => format!("TurnEnd({r:?})"),
            other => format!("{other:?}"),
        })
        .collect();
    let mut dedup = kinds.clone();
    dedup.dedup();
    assert_eq!(
        dedup,
        [
            "TurnStart",
            "Tool(todo Running)",
            "Tool(todo Completed)",
            "Todos(2)",
            "Tool(execute Running)",
            "Tool(execute Completed)",
            "Tool(todo Running)",
            "Tool(todo Completed)",
            "Todos(2)",
            "TextDelta",
            "TurnEnd(EndTurn)",
        ]
    );
    assert_eq!(
        texts(&run.events),
        "There are **2** files/directories: `README.md` and `src`."
    );
    let exec = run
        .events
        .iter()
        .find_map(|e| match e {
            Event::Tool(t) if t.name == "execute" && t.status == ToolStatus::Completed => Some(t),
            _ => None,
        })
        .unwrap();
    assert_eq!(exec.kind, ToolKind::Execute);
    assert_eq!(exec.title, "ls -1 | wc -l && ls -1");
    assert_eq!(exec.output.as_deref(), Some("2\nREADME.md\nsrc"));
    assert_eq!(exec.input["command"], "ls -1 | wc -l && ls -1");
    let todos = run
        .events
        .iter()
        .rev()
        .find_map(|e| {
            if let Event::Todos(t) = e {
                Some(t)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(
        todos[1],
        Todo {
            text: "count them".into(),
            status: TodoStatus::Completed
        }
    );
    // `todo` has no kind on the wire.
    let todo = run
        .events
        .iter()
        .find_map(|e| match e {
            Event::Tool(t) if t.name == "todo" => Some(t),
            _ => None,
        })
        .unwrap();
    assert_eq!(todo.kind, ToolKind::Other);
}

#[test]
fn usage_comes_from_a_hidden_status_run() {
    let run = replay("todo_exec_slash", "/work/proj", None);
    // After the first real turn: the harness's canned /status answer.
    let first = run
        .events
        .iter()
        .find_map(|e| {
            if let Event::Usage(u) = e {
                Some(u)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(
        (
            first.input_tokens,
            first.output_tokens,
            first.context_tokens
        ),
        (100, 5, 77)
    );
    let probes = run
        .sent
        .iter()
        .filter(|m| {
            m["method"] == "session/prompt" && m["params"]["prompt"][0]["text"] == "/status"
        })
        .count();
    // One hidden after the todo/exec turn, one the transcript itself typed.
    assert_eq!(probes, 2);
    // The user's own /status answer carries the real numbers.
    assert!(run.events.iter().any(|e| matches!(e, Event::Usage(u) if u.input_tokens == 27821 && u.output_tokens == 196 && u.context_tokens == 9377)));
}

#[test]
fn slash_commands_arrive_as_one_notice() {
    let run = replay("todo_exec_slash", "/work/proj", None);
    let notices: Vec<&str> = run
        .events
        .iter()
        .filter_map(|e| {
            if let Event::Notice { text, .. } = e {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(notices.len(), 6, "{notices:?}");
    assert_eq!(notices[0], "✓ list the files\n✓ count them");
    assert!(notices[1].starts_with("model: grok-4.7\nprovider: xai-oauth"));
    assert_eq!(notices[2], "```diff\n(working tree clean)\n```");
    assert_eq!(
        notices[3],
        "usage: /model <tag> — or pick one from the model menu"
    );
    assert_eq!(notices[4], "reasoning effort: high");
    // Each command turn: TurnStart, Notice, TurnEnd, with no TextDelta in between.
    let i = run
        .events
        .iter()
        .position(|e| matches!(e, Event::Notice { text, .. } if text.starts_with("✓")))
        .unwrap();
    assert_eq!(run.events[i - 1], Event::TurnStart);
    assert_eq!(run.events[i + 1], Event::TurnEnd(StopReason::EndTurn));
}

#[test]
fn config_changes_follow_set_options_and_effort_command() {
    let run = replay("todo_exec_slash", "/work/proj", None);
    let efforts: Vec<&str> = run
        .events
        .iter()
        .filter_map(|e| {
            if let Event::ConfigChanged(c) = e {
                Some(c.effort.as_str())
            } else {
                None
            }
        })
        .collect();
    // set low, set sovereign, set genie, then `/effort high` pushes its own update.
    assert_eq!(efforts, ["low", "low", "low", "high"]);
    let modes: Vec<&str> = run
        .events
        .iter()
        .filter_map(|e| {
            if let Event::ConfigChanged(c) = e {
                Some(c.mode.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(modes[..3], ["genie", "sovereign", "genie"]);
    let sent: Vec<_> = run
        .sent
        .iter()
        .filter(|m| m["method"] == "session/set_config_option")
        .map(|m| {
            (
                m["params"]["configId"].as_str().unwrap(),
                m["params"]["value"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        sent,
        [
            ("thought_level", "low"),
            ("wizard_mode", "sovereign"),
            ("wizard_mode", "genie")
        ]
    );
}

#[test]
fn sessions_list_maps_fields() {
    let run = replay("todo_exec_slash", "/work/proj", None);
    let Some(Event::Sessions(s)) = run.events.last() else {
        panic!()
    };
    assert_eq!(s[0].id, "2026-10-05T17-21-34");
    assert_eq!(s[0].cwd, "/work/proj");
    assert_eq!(s[0].updated, 1791220900); // 2026-10-05T17:21:40Z
    assert!(s[0].title.starts_with("Make a todo list"));
    // The list is filtered by the project directory.
    let list = run
        .sent
        .iter()
        .find(|m| m["method"] == "session/list")
        .unwrap();
    assert_eq!(list["params"]["cwd"], "/work/proj");
}

#[test]
fn file_tools_map_kind_title_and_synthesized_diffs() {
    let run = replay("file_tools", "/work/proj", None);
    let done: Vec<&ToolCall> = run
        .events
        .iter()
        .filter_map(|e| match e {
            Event::Tool(t) if t.status == ToolStatus::Completed => Some(t),
            _ => None,
        })
        .collect();
    let summary: Vec<(&str, ToolKind, &str)> = done
        .iter()
        .map(|t| (t.name.as_str(), t.kind, t.title.as_str()))
        .collect();
    assert_eq!(
        summary,
        [
            ("write_file", ToolKind::Edit, "notes.txt"),
            ("edit_file", ToolKind::Edit, "notes.txt"),
            ("read_file", ToolKind::Read, "notes.txt"),
            ("search_files", ToolKind::Search, "hello"),
            ("list_files", ToolKind::Read, "."),
            ("web_fetch", ToolKind::Fetch, "https://example.com"),
            ("git_status", ToolKind::Read, ""),
        ]
    );
    assert_eq!(
        done[0].diff,
        Some(FileDiff {
            path: "notes.txt".into(),
            old: None,
            new: "hi there".into()
        })
    );
    assert_eq!(
        done[1].diff,
        Some(FileDiff {
            path: "notes.txt".into(),
            old: Some("hi".into()),
            new: "hello".into()
        })
    );
    assert!(done[2].diff.is_none());
    // /diff afterwards is a command result.
    assert!(run.events.iter().any(
        |e| matches!(e, Event::Notice { text, .. } if text.contains("diff --git a/notes.txt"))
    ));
}

#[test]
fn cancel_sends_notification_and_midturn_option_changes_only_warn() {
    let run = replay("cancel_midturn", "/work/proj", None);
    let cancel = run
        .sent
        .iter()
        .find(|m| m["method"] == "session/cancel")
        .unwrap();
    assert!(cancel.get("id").is_none(), "cancel is a notification");
    assert_eq!(cancel["params"]["sessionId"], "2026-10-05T17-22-30");
    // The mid-turn set_config_option never goes on the wire.
    let sets = run
        .sent
        .iter()
        .filter(|m| m["method"] == "session/set_config_option")
        .count();
    assert_eq!(sets, 1);
    let warns = run
        .events
        .iter()
        .filter(|e| {
            matches!(
                e,
                Event::Notice {
                    level: NoticeLevel::Warn,
                    ..
                }
            )
        })
        .count();
    // The recording also tried to switch model while the sleep was still running.
    assert_eq!(warns, 2);
    // wizard finishes the running tool, then ends the turn as cancelled.
    let end = run
        .events
        .iter()
        .position(|e| *e == Event::TurnEnd(StopReason::Cancelled))
        .unwrap();
    assert!(matches!(run.events[end - 1], Event::Tool(ref t) if t.status == ToolStatus::Completed));
    // The /status typed at t=37s was queued behind the turn, not sent into it.
    let status_ids: Vec<_> = run
        .sent
        .iter()
        .enumerate()
        .filter(|(_, m)| m["params"]["prompt"][0]["text"] == "/status")
        .map(|(i, _)| i)
        .collect();
    let cancel_at = run
        .sent
        .iter()
        .position(|m| m["method"] == "session/cancel")
        .unwrap();
    assert!(status_ids.iter().all(|i| *i > cancel_at));
}

#[test]
fn unadvertised_slash_word_streams_as_text() {
    let run = replay("unknown_command", "/work/proj", None);
    assert!(texts(&run.events).contains("failed: unknown command"));
    assert!(!run.events.iter().any(|e| matches!(e, Event::Notice { .. })));
    let failed = run
        .events
        .iter()
        .find_map(|e| match e {
            Event::Tool(t) if t.status == ToolStatus::Failed => Some(t),
            _ => None,
        })
        .unwrap();
    assert_eq!(failed.name, "run_command");
    assert_eq!(failed.kind, ToolKind::Other);
    assert_eq!(
        failed.output.as_deref(),
        Some("unknown command '/nonexistentcmd' — try /help")
    );
}

#[test]
fn thoughts_stream_and_load_replays_history() {
    let run = replay("thought_and_load", "/work/proj", None);
    let thoughts: String = run
        .events
        .iter()
        .filter_map(|e| {
            if let Event::ThoughtDelta(t) = e {
                Some(t.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(thoughts, "Calculating 17 times 23.");
    assert_eq!(texts(&run.events), "17 × 23 = 340 + 51 = 391.");

    let Some(Event::History { session_id, items }) = run
        .events
        .iter()
        .find(|e| matches!(e, Event::History { .. }))
    else {
        panic!()
    };
    assert_eq!(session_id, "2026-10-05T17-22-05");
    assert_eq!(items.len(), 1 + 1 + 7 + 1, "{items:#?}");
    assert!(matches!(&items[0], HistoryItem::User(t) if t.starts_with("Do exactly these steps")));
    assert_eq!(
        items[1],
        HistoryItem::Assistant("I'll follow those steps in order, one tool at a time.".into())
    );
    let HistoryItem::Tool(edit) = &items[3] else {
        panic!()
    };
    assert_eq!(
        (edit.name.as_str(), edit.status),
        ("edit_file", ToolStatus::Completed)
    );
    assert!(edit.output.as_deref().unwrap().starts_with("Edited "));
    assert_eq!(edit.diff.as_ref().unwrap().new, "hello");
    assert_eq!(items[9], HistoryItem::Assistant("done".into()));
    // History comes before Ready, and replayed chunks are not live events.
    let h = run
        .events
        .iter()
        .position(|e| matches!(e, Event::History { .. }))
        .unwrap();
    assert!(
        matches!(&run.events[h + 1], Event::Ready { session_id, .. } if session_id == "2026-10-05T17-22-05")
    );
    // A loaded session reports no usage of its own; the hidden /status after the load supplies
    // the context size, so the sidebar does not claim zero.
    let ready = h + 1;
    assert!(
        run.events[ready..]
            .iter()
            .any(|e| matches!(e, Event::Usage(u) if u.context_tokens > 0)),
        "no usage after the load: {:#?}",
        &run.events[ready..]
    );
    // The load went out with the cwd, and the failed load is an error notice.
    let load = run
        .sent
        .iter()
        .find(|m| m["method"] == "session/load")
        .unwrap();
    assert_eq!(load["params"]["sessionId"], "2026-10-05T17-22-05");
    assert_eq!(load["params"]["cwd"], "/work/proj");
    assert!(
        matches!(run.events.last(), Some(Event::Notice { level: NoticeLevel::Error, text }) if text.contains("does-not-exist"))
    );
}

#[test]
fn resume_at_spawn_loads_instead_of_creating() {
    let mut run = common::Run::new("/work/proj", Some("2026-10-05T17-22-05"));
    // Startup is initialize, then session/load, so no session/new is ever sent.
    run.out(&serde_json::json!({"id": 1, "method": "initialize"}));
    run.inbound(&serde_json::json!({"jsonrpc":"2.0","id":1,"result":{}}));
    let methods: Vec<_> = run
        .sent
        .iter()
        .map(|m| m["method"].as_str().unwrap())
        .collect();
    assert_eq!(methods, ["initialize", "session/load"]);
}
