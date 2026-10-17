//! Behaviors a recording cannot show: queueing, errors, odd chunks.

mod common;

use agent_core::*;
use backend_wizard::wire;
use common::{chunk, last_id, ready};
use serde_json::json;

fn drain(run: &mut common::Run) -> Vec<Event> {
    std::mem::take(&mut run.events)
}

#[test]
fn prompt_during_a_turn_waits_for_it() {
    let mut run = ready();
    drain(&mut run);
    run.request(Request::Prompt("first".into()));
    run.request(Request::Prompt("second".into()));
    let prompts = |run: &common::Run| {
        run.sent
            .iter()
            .filter(|m| m["method"] == "session/prompt")
            .count()
    };
    assert_eq!(prompts(&run), 1);
    let id = last_id(&run, "session/prompt");
    run.line(&json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}}));
    // The hidden /status runs before the queued prompt goes out (the harness
    // answers the probe inside `line`, which releases the queue).
    let texts: Vec<_> = run
        .sent
        .iter()
        .filter(|m| m["method"] == "session/prompt")
        .map(|m| {
            m["params"]["prompt"][0]["text"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(texts, ["first", "/status", "second"]);
    let ends = run
        .events
        .iter()
        .filter(|e| matches!(e, Event::TurnEnd(_)))
        .count();
    let starts = run
        .events
        .iter()
        .filter(|e| **e == Event::TurnStart)
        .count();
    assert_eq!((starts, ends), (2, 1));
}

#[test]
fn cancel_drops_queued_prompts() {
    let mut run = ready();
    run.request(Request::Prompt("first".into()));
    run.request(Request::Prompt("second".into()));
    run.request(Request::Cancel);
    let id = last_id(&run, "session/prompt");
    run.line(&json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"cancelled"}}));
    let texts: Vec<_> = run
        .sent
        .iter()
        .filter(|m| m["method"] == "session/prompt")
        .map(|m| {
            m["params"]["prompt"][0]["text"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(texts, ["first", "/status"]);
}

#[test]
fn prompts_sent_before_ready_are_held() {
    let mut run = common::Run::new("/work/proj", None);
    run.request(Request::Prompt("early".into()));
    assert!(run.sent.is_empty());
    run.out(&json!({"id": 1, "method": "initialize"}));
    run.inbound(&json!({"jsonrpc":"2.0","id":1,"result":{}}));
    run.out(&json!({"id": 2, "method": "session/new"}));
    run.inbound(&json!({"jsonrpc":"2.0","id":2,"result":{"sessionId":"s9","configOptions":[]}}));
    let last = run.sent.last().unwrap();
    assert_eq!(last["method"], "session/prompt");
    assert_eq!(last["params"]["sessionId"], "s9");
}

#[test]
fn prompt_error_becomes_notice_and_error_turn_end() {
    let mut run = ready();
    drain(&mut run);
    run.request(Request::Prompt("hi".into()));
    let id = last_id(&run, "session/prompt");
    run.line(&json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":"Internal error","data":"403 spending-limit"}}));
    assert_eq!(
        run.events,
        [
            Event::TurnStart,
            Event::Notice {
                level: NoticeLevel::Error,
                text: "Internal error: 403 spending-limit".into()
            },
            Event::TurnEnd(StopReason::Error),
        ]
    );
}

#[test]
fn option_change_rejected_by_the_server_is_a_warning() {
    let mut run = ready();
    drain(&mut run);
    run.request(Request::SetEffort("high".into()));
    let id = last_id(&run, "session/set_config_option");
    run.line(&json!({"jsonrpc":"2.0","id":id,"error":{"code":-32600,"message":"Invalid request","data":"a turn is running; set options between turns"}}));
    assert_eq!(
        run.events,
        [Event::Notice {
            level: NoticeLevel::Warn,
            text: "Options can only be changed between turns.".into()
        }]
    );
}

#[test]
fn bare_model_tag_prefers_the_current_provider() {
    let mut run = ready();
    run.request(Request::SetModel("grok-4.6".into()));
    // Two providers carry grok-4.6 and neither is current: sent as typed.
    assert_eq!(run.sent.last().unwrap()["params"]["value"], "grok-4.6");
    run.request(Request::SetModel("xai/grok-4.6".into()));
    assert_eq!(run.sent.last().unwrap()["params"]["value"], "xai/grok-4.6");
}

#[test]
fn model_ids_split_at_the_first_slash_only() {
    let run = ready();
    let Some(Event::Ready { config, .. }) = run
        .events
        .iter()
        .find(|e| matches!(e, Event::Ready { .. }))
        .cloned()
    else {
        panic!()
    };
    assert_eq!(
        config.models[0],
        ModelOption {
            id: "openrouter/anthropic/claude-sonnet-5".into(),
            name: "anthropic/claude-sonnet-5".into(),
            provider: "openrouter".into()
        }
    );
    assert_eq!(config.backend, "wizard 3.7.1");
    assert_eq!(wire::split_model("panel"), ("", "panel"));
}

#[test]
fn wizard_injected_lines_are_warnings_not_text() {
    let mut run = ready();
    drain(&mut run);
    run.request(Request::Prompt("hi".into()));
    run.line(&chunk(
        "s1",
        "agent_message_chunk",
        "\n[wizard] the response stream dropped; it restarts below\n",
    ));
    run.line(&chunk("s1", "agent_message_chunk", "[x] normal"));
    assert_eq!(
        run.events[1],
        Event::Notice {
            level: NoticeLevel::Warn,
            text: "the response stream dropped; it restarts below".into()
        }
    );
    assert_eq!(run.events[2], Event::TextDelta("[x] normal".into()));
}

#[test]
fn updates_for_other_sessions_are_ignored() {
    let mut run = ready();
    drain(&mut run);
    run.line(&chunk("someone-else", "agent_message_chunk", "nope"));
    assert!(run.events.is_empty());
}

#[test]
fn session_switch_during_a_turn_is_refused() {
    let mut run = ready();
    drain(&mut run);
    run.request(Request::Prompt("hi".into()));
    let before = run.sent.len();
    run.request(Request::NewSession);
    run.request(Request::LoadSession("old".into()));
    assert_eq!(run.sent.len(), before);
    assert_eq!(
        run.events
            .iter()
            .filter(|e| matches!(
                e,
                Event::Notice {
                    level: NoticeLevel::Warn,
                    ..
                }
            ))
            .count(),
        2
    );
}

#[test]
fn new_session_reemits_ready() {
    let mut run = ready();
    drain(&mut run);
    run.request(Request::NewSession);
    let id = last_id(&run, "session/new");
    run.line(&json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":"s2","configOptions":[]}}));
    assert!(matches!(&run.events[0], Event::Ready { session_id, .. } if session_id == "s2"));
    run.request(Request::Prompt("x".into()));
    assert_eq!(run.sent.last().unwrap()["params"]["sessionId"], "s2");
}

#[test]
fn failed_resume_falls_back_to_a_new_session() {
    let mut run = common::Run::new("/work/proj", Some("gone"));
    run.out(&json!({"id": 1, "method": "initialize"}));
    run.inbound(&json!({"jsonrpc":"2.0","id":1,"result":{}}));
    let id = last_id(&run, "session/load");
    run.line(
        &json!({"jsonrpc":"2.0","id":id,"error":{"code":-32002,"message":"Resource not found"}}),
    );
    assert!(
        matches!(&run.events[0], Event::Notice { level: NoticeLevel::Warn, text } if text.contains("gone"))
    );
    assert_eq!(run.sent.last().unwrap()["method"], "session/new");
}

#[test]
fn startup_failure_is_fatal() {
    let mut run = common::Run::new("/work/proj", None);
    run.out(&json!({"id": 1, "method": "initialize"}));
    run.inbound(&json!({"jsonrpc":"2.0","id":1,"result":{}}));
    let id = last_id(&run, "session/new");
    run.line(&json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":"Authentication required"}}));
    assert!(
        matches!(&run.events[0], Event::ConfigChanged(c) if c.backend.starts_with("wizard")),
        "the version is known before the failure"
    );
    assert!(matches!(&run.events[1], Event::Fatal(t) if t.contains("Authentication required")));
}

#[test]
fn agent_requests_get_answers() {
    let mut run = ready();
    run.line(&json!({"jsonrpc":"2.0","id":77,"method":"fs/read_text_file","params":{}}));
    let r = run.sent.last().unwrap();
    assert_eq!(
        (r["id"].as_u64(), r["error"]["code"].as_i64()),
        (Some(77), Some(-32601))
    );
    run.line(&json!({"jsonrpc":"2.0","id":78,"method":"session/request_permission","params":{"options":[{"optionId":"no","kind":"reject_once"},{"optionId":"yes","kind":"allow_once"}]}}));
    assert_eq!(
        run.sent.last().unwrap()["result"]["outcome"]["optionId"],
        "yes"
    );
}

#[test]
fn acp_diff_blocks_fill_file_diff() {
    let mut run = ready();
    drain(&mut run);
    run.line(&json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{
        "sessionUpdate":"tool_call","toolCallId":"c1","title":"edit_file: a.rs","kind":"edit","status":"in_progress","rawInput":{"path":"a.rs"}}}}));
    run.line(&json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{
        "sessionUpdate":"tool_call_update","toolCallId":"c1","status":"completed",
        "content":[{"type":"diff","path":"a.rs","oldText":"a","newText":"b"},{"type":"content","content":{"type":"text","text":"ok"}}]}}}));
    let Event::Tool(t) = run.events.last().unwrap() else {
        panic!()
    };
    assert_eq!(
        t.diff,
        Some(FileDiff {
            path: "a.rs".into(),
            old: Some("a".into()),
            new: "b".into()
        })
    );
    assert_eq!(t.output.as_deref(), Some("ok"));
    assert_eq!(t.title, "a.rs");
}

#[test]
fn update_for_unknown_tool_still_yields_a_call() {
    let mut run = ready();
    drain(&mut run);
    run.line(
        &json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{
        "sessionUpdate":"tool_call_update","toolCallId":"zz","status":"failed"}}}),
    );
    assert!(
        matches!(&run.events[0], Event::Tool(t) if t.id == "zz" && t.status == ToolStatus::Failed)
    );
}

#[test]
fn refresh_replays_cached_commands_and_config() {
    let mut run = ready();
    drain(&mut run);
    run.request(Request::Refresh);
    assert!(matches!(&run.events[0], Event::Commands(c) if c.len() == 3));
    assert!(
        matches!(&run.events[1], Event::ConfigChanged(c) if c.effort == "low" && c.mode == "genie")
    );
}

#[test]
fn todo_read_output_is_parsed() {
    let todos = wire::parse_todo_lines("✓ a\n▸ b\n☐ c\nnoise");
    assert_eq!(todos.len(), 3);
    assert_eq!(
        todos[1],
        Todo {
            text: "b".into(),
            status: TodoStatus::InProgress
        }
    );
}

#[test]
fn rfc3339_parses_offsets_and_garbage() {
    assert_eq!(
        wire::parse_rfc3339("2026-10-05T17:21:40.665160287+00:00"),
        1791220900
    );
    assert_eq!(wire::parse_rfc3339("2026-10-05T19:21:40Z"), 1791228100);
    assert_eq!(wire::parse_rfc3339("2026-10-05T19:21:40+02:00"), 1791220900);
    assert_eq!(wire::parse_rfc3339("nope"), 0);
    assert_eq!(wire::parse_rfc3339(""), 0);
}

#[test]
fn usage_parser_reads_status_and_cost_text() {
    let u = wire::parse_usage("usage: 27821 prompt + 196 completion tokens\ncontext: 9377 tokens")
        .unwrap();
    assert_eq!(
        (u.input_tokens, u.output_tokens, u.context_tokens),
        (27821, 196, 9377)
    );
    assert!(wire::parse_usage("model: x").is_none());
}

#[test]
fn frontend_only_list_matches_the_server_doc() {
    for c in [
        "clear",
        "resume",
        "login",
        "settings",
        "dashboard",
        "vim",
        "ui",
        "view",
        "quit",
        "exit",
    ] {
        assert!(backend_wizard::FRONTEND_ONLY.contains(&c), "{c}");
    }
}

#[test]
fn command_turn_is_recognized_even_when_the_prompt_beat_the_commands_list() {
    let mut run = common::Run::new("/work/proj", None);
    run.request(Request::Prompt("/status".into()));
    run.out(&json!({"id": 1, "method": "initialize"}));
    run.inbound(&json!({"jsonrpc":"2.0","id":1,"result":{}}));
    run.out(&json!({"id": 2, "method": "session/new"}));
    run.inbound(&json!({"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1","configOptions":[]}}));
    // The prompt is already out; the commands list arrives after it was sent.
    assert_eq!(run.sent.last().unwrap()["method"], "session/prompt");
    run.line(&json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{
        "sessionUpdate":"available_commands_update","availableCommands":[{"name":"status","description":"d"}]}}}));
    run.line(&chunk(
        "s1",
        "agent_message_chunk",
        "usage: 5 prompt + 6 completion tokens",
    ));
    let id = last_id(&run, "session/prompt");
    run.line(&json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}}));
    assert!(run
        .events
        .iter()
        .any(|e| matches!(e, Event::Notice { text, .. } if text.starts_with("usage: 5"))));
    assert!(!run.events.iter().any(|e| matches!(e, Event::TextDelta(_))));
}

#[test]
fn an_asking_core_puts_a_finished_plan_to_the_frontend() {
    let mut run = ready();
    run.core.set_asking(true);
    run.events.clear();
    run.line(&json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{
        "sessionUpdate":"tool_call","toolCallId":"call-3","title":"exit_plan","kind":"other","status":"in_progress","rawInput":{"plan":"1. edit"}}}}));
    let before = run.sent.len();
    run.line(&json!({"jsonrpc":"2.0","id":9,"method":"session/request_permission","params":{"sessionId":"s1",
        "toolCall":{"toolCallId":"call-3","title":"Ready to code?","kind":"switch_mode","rawInput":{"plan":"1. edit"}},
        "options":[{"optionId":"approve","name":"Yes, proceed","kind":"allow_once"},{"optionId":"keep-planning","name":"No, keep planning","kind":"reject_once"}]}}));
    assert_eq!(
        run.sent.len(),
        before,
        "nothing answers it but the frontend"
    );
    let Some(Event::Permission(req)) = run.events.last() else {
        panic!("{:?}", run.events)
    };
    assert_eq!((req.id.as_str(), req.tool.as_str()), ("9", "exit_plan"));
    assert_eq!(req.input["plan"], "1. edit");
    run.request(Request::Decide {
        id: "9".into(),
        allow: false,
        scope: DecideScope::Once,
        note: String::new(),
    });
    let r = run.sent.last().unwrap();
    assert_eq!(
        (
            r["id"].as_u64(),
            r["result"]["outcome"]["optionId"].as_str()
        ),
        (Some(9), Some("keep-planning"))
    );
}

#[test]
fn a_cancel_answers_an_open_question() {
    let mut run = ready();
    run.core.set_asking(true);
    run.line(&json!({"jsonrpc":"2.0","id":10,"method":"session/request_permission","params":{"sessionId":"s1",
        "toolCall":{"toolCallId":"x"},"options":[{"optionId":"approve","kind":"allow_once"}]}}));
    run.request(Request::Cancel);
    assert!(run
        .sent
        .iter()
        .any(|m| m["id"] == 10 && m["result"]["outcome"]["outcome"] == "cancelled"));
}

#[test]
fn two_user_messages_in_a_row_replay_as_two() {
    let mut run = common::Run::new("/work/proj", Some("s1"));
    run.out(&json!({"id": 1, "method": "initialize"}));
    run.inbound(&json!({"jsonrpc":"2.0","id":1,"result":{}}));
    let id = last_id(&run, "session/load");
    for text in ["please slow", "please write"] {
        run.line(
            &json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{
            "sessionUpdate":"user_message_chunk","content":{"type":"text","text":text},"_meta":{"isReplay":true}}}}),
        );
    }
    run.line(&json!({"jsonrpc":"2.0","id":id,"result":{"configOptions":[]}}));
    let Some(Event::History { items, .. }) = run
        .events
        .iter()
        .find(|e| matches!(e, Event::History { .. }))
    else {
        panic!("{:?}", run.events)
    };
    assert_eq!(
        items,
        &vec![
            HistoryItem::User("please slow".into()),
            HistoryItem::User("please write".into())
        ]
    );
}

#[test]
fn error_detail_is_never_raw_json() {
    let e = json!({"code": -32002, "message": "Resource not found", "data": {"uri": "s9", "message": "no session s9"}});
    assert_eq!(
        wire::rpc_error_text(&e),
        "Resource not found: no session s9"
    );
    let e = json!({"code": -32002, "message": "Resource not found", "data": {"uri": "s9"}});
    assert_eq!(wire::rpc_error_text(&e), "Resource not found");
}
