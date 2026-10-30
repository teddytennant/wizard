use serde_json::{Value, json};

use super::*;
use crate::agent::DoneReason;
use crate::tools::ToolOutput;

fn info(partial: bool) -> RunInfo {
    RunInfo {
        session_id: "sess-1".into(),
        cwd: "/work".into(),
        model: "grok-test".into(),
        permission_mode: "default".into(),
        tools: vec!["Bash".into()],
        partial,
        price: Price {
            usd_per_mtok_in: Some(1.0),
            usd_per_mtok_out: Some(2.0),
            ..Price::default()
        },
        ..RunInfo::default()
    }
}

/// The events one `echo hi` run produces, in the order the agent loop sends them.
fn echo_run() -> Vec<AgentEvent> {
    vec![
        AgentEvent::Usage {
            prompt_tokens: 120,
            completion_tokens: 20,
        },
        AgentEvent::ToolStarted {
            name: "execute".into(),
            args: json!({"command": "echo hi"}),
        },
        AgentEvent::ToolFinished {
            name: "execute".into(),
            output: ToolOutput::ok("hi"),
        },
        AgentEvent::StepCompleted { step: 1 },
        AgentEvent::TextDelta("do".into()),
        AgentEvent::TextDelta("ne".into()),
        AgentEvent::Usage {
            prompt_tokens: 120,
            completion_tokens: 20,
        },
        AgentEvent::StepCompleted { step: 2 },
        AgentEvent::Done {
            reason: DoneReason::Completed,
        },
    ]
}

fn frames(run: &mut ClaudeRun, events: &[AgentEvent]) -> Vec<Value> {
    events.iter().flat_map(|e| run.on_event(e)).collect()
}

fn kinds(frames: &[Value]) -> Vec<String> {
    frames
        .iter()
        .map(|f| match f["type"].as_str().unwrap() {
            "stream_event" => format!("stream:{}", f["event"]["type"].as_str().unwrap()),
            "system" => format!("system:{}", f["subtype"].as_str().unwrap()),
            other => other.to_string(),
        })
        .collect()
}

#[test]
fn init_carries_the_keys_a_claude_consumer_reads() {
    let run = ClaudeRun::new(info(false));
    let init = run.init();
    for key in [
        "type",
        "subtype",
        "cwd",
        "session_id",
        "tools",
        "mcp_servers",
        "model",
        "permissionMode",
        "slash_commands",
        "apiKeySource",
        "claude_code_version",
        "output_style",
        "agents",
        "skills",
        "plugins",
        "uuid",
    ] {
        assert!(init.get(key).is_some(), "init lacks {key}");
    }
    assert_eq!(init["subtype"], "init");
    assert_eq!(init["session_id"], "sess-1");
}

#[test]
fn a_tool_run_is_assistant_user_assistant_with_matching_ids() {
    let mut run = ClaudeRun::new(info(false));
    let out = frames(&mut run, &echo_run());
    assert_eq!(kinds(&out), ["assistant", "user", "assistant"]);

    let tool_use = &out[0]["message"]["content"][0];
    assert_eq!(tool_use["type"], "tool_use");
    assert_eq!(tool_use["name"], "Bash");
    assert_eq!(tool_use["input"], json!({"command": "echo hi"}));
    assert!(tool_use["id"].as_str().unwrap().starts_with("toolu_"));

    let result = &out[1]["message"]["content"][0];
    assert_eq!(result["type"], "tool_result");
    assert_eq!(result["tool_use_id"], tool_use["id"]);
    assert_eq!(result["content"], "hi");
    assert_eq!(result["is_error"], false);
    assert_eq!(out[1]["message"]["role"], "user");

    assert_eq!(out[2]["message"]["content"][0]["text"], "done");
    for frame in &out {
        assert_eq!(frame["session_id"], "sess-1");
        assert!(frame["uuid"].as_str().is_some());
        assert!(frame["parent_tool_use_id"].is_null());
    }
}

#[test]
fn assistant_messages_carry_the_step_usage_and_a_stable_message_id() {
    let mut run = ClaudeRun::new(info(false));
    let out = frames(&mut run, &echo_run());
    let usage = &out[0]["message"]["usage"];
    assert_eq!(usage["input_tokens"], 120);
    assert_eq!(usage["output_tokens"], 20);
    assert!(usage.get("cache_read_input_tokens").is_some());
    assert_ne!(out[0]["message"]["id"], out[2]["message"]["id"]);
}

#[test]
fn the_result_has_claudes_keys_and_wizards() {
    let mut run = ClaudeRun::new(info(false));
    frames(&mut run, &echo_run());
    let result = run.result("completed", Map::new());
    for key in [
        "type",
        "subtype",
        "is_error",
        "duration_ms",
        "duration_api_ms",
        "num_turns",
        "result",
        "stop_reason",
        "session_id",
        "total_cost_usd",
        "usage",
        "modelUsage",
        "permission_denials",
        "terminal_reason",
        "uuid",
        "reason",
    ] {
        assert!(result.get(key).is_some(), "result lacks {key}");
    }
    assert_eq!(result["type"], "result");
    assert_eq!(result["subtype"], "success");
    assert_eq!(result["is_error"], false);
    assert_eq!(result["result"], "done");
    assert_eq!(result["num_turns"], 2);
    assert_eq!(result["usage"]["input_tokens"], 240);
    assert_eq!(result["usage"]["output_tokens"], 40);
    assert_eq!(result["usage"]["prompt_tokens"], 240);
    assert_eq!(result["reason"], "completed");
    assert_eq!(result["terminal_reason"], "completed");
    // 240 in at $1/M plus 40 out at $2/M.
    let cost = result["total_cost_usd"].as_f64().unwrap();
    assert!((cost - 0.00032).abs() < 1e-9, "{cost}");
    assert!(result["modelUsage"]["grok-test"]["costUSD"].is_number());
}

#[test]
fn run_outcomes_map_to_claudes_subtypes() {
    let run = ClaudeRun::new(info(false));
    for (reason, subtype, is_error) in [
        ("completed", "success", false),
        ("max_steps", "error_max_turns", true),
        ("circuit_breaker", "error_during_execution", true),
        ("time_limit", "error_during_execution", true),
    ] {
        let result = run.result(reason, Map::new());
        assert_eq!(result["subtype"], subtype, "{reason}");
        assert_eq!(result["is_error"], is_error, "{reason}");
    }
}

#[test]
fn partial_messages_bracket_each_block_like_claude() {
    let mut run = ClaudeRun::new(info(true));
    let out = frames(&mut run, &echo_run());
    assert_eq!(
        kinds(&out),
        [
            "system:status",
            "stream:message_start",
            "stream:content_block_start",
            "stream:content_block_delta",
            "assistant",
            "stream:content_block_stop",
            "stream:message_delta",
            "stream:message_stop",
            "user",
            "system:status",
            "stream:message_start",
            "stream:content_block_start",
            "stream:content_block_delta",
            "stream:content_block_delta",
            "assistant",
            "stream:content_block_stop",
            "stream:message_delta",
            "stream:message_stop",
        ]
    );
    assert_eq!(out[2]["event"]["content_block"]["type"], "tool_use");
    assert_eq!(out[3]["event"]["delta"]["type"], "input_json_delta");
    assert_eq!(
        out[3]["event"]["delta"]["partial_json"],
        json!({"command": "echo hi"}).to_string()
    );
    assert_eq!(out[6]["event"]["delta"]["stop_reason"], "tool_use");
    assert_eq!(out[12]["event"]["delta"]["text"], "do");
    assert_eq!(out[16]["event"]["delta"]["stop_reason"], "end_turn");
    assert!(out[2]["api_message_id"].as_str().is_some());
}

#[test]
fn a_retried_stream_drops_the_partial_text() {
    let mut run = ClaudeRun::new(info(false));
    let out = frames(
        &mut run,
        &[
            AgentEvent::TextDelta("half".into()),
            AgentEvent::StreamRetrying,
            AgentEvent::TextDelta("whole".into()),
            AgentEvent::Usage {
                prompt_tokens: 1,
                completion_tokens: 1,
            },
            AgentEvent::StepCompleted { step: 1 },
        ],
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["message"]["content"][0]["text"], "whole");
}

#[test]
fn thinking_becomes_a_thinking_block_before_the_text() {
    let mut run = ClaudeRun::new(info(false));
    let out = frames(
        &mut run,
        &[
            AgentEvent::ThinkingDelta("hmm".into()),
            AgentEvent::TextDelta("ok".into()),
            AgentEvent::Usage {
                prompt_tokens: 1,
                completion_tokens: 1,
            },
        ],
    );
    assert_eq!(out[0]["message"]["content"][0]["type"], "thinking");
    assert_eq!(out[0]["message"]["content"][0]["thinking"], "hmm");
    assert_eq!(out[1]["message"]["content"][0]["type"], "text");
}

#[test]
fn the_budget_cancels_the_turn_once_the_estimate_passes_it() {
    let mut run = ClaudeRun::new(RunInfo {
        max_budget_usd: Some(0.0001),
        ..info(false)
    });
    let cancel = crate::agent::CancelHandle::default();
    run.set_cancel(cancel.clone());
    run.on_event(&AgentEvent::Usage {
        prompt_tokens: 10,
        completion_tokens: 10,
    });
    assert!(!cancel.is_cancelled(), "$0.00003 is under the limit");
    run.on_event(&AgentEvent::Usage {
        prompt_tokens: 100_000,
        completion_tokens: 0,
    });
    assert!(cancel.is_cancelled());
    let result = run.result("stopped", Map::new());
    assert_eq!(result["subtype"], "error_max_budget_usd");
    assert_eq!(result["is_error"], true);
}

#[test]
fn a_subagent_tool_call_names_the_spawning_call_as_its_parent() {
    let mut run = ClaudeRun::new(info(false));
    let out = frames(
        &mut run,
        &[
            AgentEvent::ToolStarted {
                name: "spawn_subagent".into(),
                args: json!({"task": "look"}),
            },
            AgentEvent::SubagentRunStarted {
                run: 7,
                bg: None,
                name: "worker".into(),
                task: "look".into(),
            },
            AgentEvent::SubagentRunToolStarted {
                run: 7,
                name: "read_file".into(),
                args: json!({"path": "a.rs"}),
            },
            AgentEvent::SubagentRunToolFinished {
                run: 7,
                name: "read_file".into(),
                output: ToolOutput::ok("x"),
            },
        ],
    );
    let spawn_id = out[0]["message"]["content"][0]["id"].clone();
    assert_eq!(out[0]["message"]["content"][0]["name"], "Agent");
    assert_eq!(out[1]["parent_tool_use_id"], spawn_id);
    assert_eq!(
        out[1]["message"]["content"][0]["input"]["file_path"],
        "a.rs"
    );
    assert_eq!(out[2]["parent_tool_use_id"], spawn_id);
}

#[test]
fn hook_events_appear_only_when_asked_for() {
    let hook = AgentEvent::HookFired {
        event: "pre_tool_use".into(),
        command: "./check.sh".into(),
        outcome: crate::hooks::HookOutcome::Blocked("no".into()),
    };
    let mut quiet = ClaudeRun::new(info(false));
    assert!(quiet.on_event(&hook).is_empty());
    let mut loud = ClaudeRun::new(RunInfo {
        hook_events: true,
        ..info(false)
    });
    let out = loud.on_event(&hook);
    assert_eq!(out[0]["subtype"], "hook_response");
    assert_eq!(out[0]["hook_name"], "./check.sh");
}
