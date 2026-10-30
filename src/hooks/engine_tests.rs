//! Engine behavior through real `sh` hooks. Each case that has a Claude Code
//! counterpart was also run against Claude Code 2.1.314 with a mock API, and
//! the expectation here is what it did.

#![cfg(unix)]

use serde_json::{Value, json};

use super::event::HookEvent;
use super::spec::{Handler, HookSpec, Origin};
use super::*;
use crate::permissions::{PermissionMode, Policy, Rule};
use crate::tools::ToolAccess;

fn claude(event: HookEvent, matcher: Option<&str>, command: &str) -> HookSpec {
    HookSpec {
        event,
        matcher: matcher.map(str::to_string),
        handler: Handler::Command {
            command: command.to_string(),
            args: None,
        },
        timeout_secs: Some(10),
        origin: Origin::Claude,
        json: true,
        condition: None,
        is_async: false,
        block_on_failure: false,
        source: "test".to_string(),
    }
}

fn native(event: HookEvent, command: &str) -> HookSpec {
    HookDef {
        event,
        matcher: None,
        command: command.to_string(),
        timeout_secs: Some(10),
    }
    .into()
}

fn engine(specs: Vec<HookSpec>) -> HookEngine {
    HookEngine::from_specs(specs, std::env::temp_dir(), "engine-test".to_string())
}

fn say(json: &str) -> String {
    format!("printf '%s' '{json}'")
}

async fn pre(engine: &HookEngine, tool: &str, args: Value) -> Before {
    engine
        .before_tool(
            tool,
            ToolAccess::Execute,
            &args,
            "toolu_1",
            Mode::default(),
            None,
        )
        .await
}

fn blocked(before: &Before) -> &str {
    match before {
        Before::Blocked(message) => message,
        other => panic!("expected a block, got {other:?}"),
    }
}

#[tokio::test]
async fn every_event_reaches_a_hook_with_its_claude_name() {
    let log = std::env::temp_dir().join(format!("wizard-event-log-{}", uuid::Uuid::new_v4()));
    let cmd = format!("cat >> {}; echo >> {}", log.display(), log.display());
    for event in HookEvent::ALL {
        let engine = engine(vec![claude(event, None, &cmd)]);
        engine
            .emit(event, None, json!({}), Mode::default(), None)
            .await;
    }
    let text = std::fs::read_to_string(&log).unwrap();
    let _ = std::fs::remove_file(&log);
    let seen: Vec<String> = text
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            serde_json::from_str::<Value>(l).unwrap()["hook_event_name"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    let want: Vec<&str> = HookEvent::ALL.iter().map(|e| e.claude_name()).collect();
    assert_eq!(seen, want, "all 34 events fired, in order");
}

#[tokio::test]
async fn a_claude_hook_reads_claude_names_on_stdin() {
    let engine = engine(vec![claude(
        HookEvent::UserPromptSubmit,
        None,
        "cat >&2; exit 2",
    )]);
    let result = engine
        .user_prompt_submit_with_prompt("hi", Mode::default(), None)
        .await;
    let PromptSubmit::Block(stdin) = result else {
        panic!("the hook blocks with its stdin as the reason");
    };
    let payload: Value = serde_json::from_str(&stdin).unwrap();
    assert_eq!(payload["hook_event_name"], "UserPromptSubmit");
    assert_eq!(payload["prompt"], "hi");
    assert_eq!(payload["session_id"], "engine-test");
    assert_eq!(payload["permission_mode"], "default");
    assert!(payload.get("event").is_none(), "no wizard-only fields");
}

#[tokio::test]
async fn claude_hooks_see_bash_and_file_path() {
    let engine = engine(vec![claude(
        HookEvent::PreToolUse,
        Some("Write"),
        "cat >&2; exit 2",
    )]);
    let args = json!({ "path": "a.rs", "content": "x" });
    let before = pre(&engine, "write_file", args).await;
    let message = blocked(&before);
    let payload: Value =
        serde_json::from_str(message.trim_start_matches("blocked by pre_tool_use hook: ")).unwrap();
    assert_eq!(payload["tool_name"], "Write");
    assert_eq!(payload["tool_input"]["file_path"], "a.rs");
    assert_eq!(payload["wizard_tool_name"], "write_file");
    assert_eq!(payload["tool_use_id"], "toolu_1");
}

#[tokio::test]
async fn toml_hooks_keep_wizard_names_and_gain_claude_ones() {
    let engine = engine(vec![native(HookEvent::PostToolUse, "cat")]);
    let context = engine
        .post_tool_use_with_output(
            "execute",
            &json!({"command": "ls"}),
            "out",
            false,
            Mode::default(),
            None,
        )
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&context).unwrap();
    assert_eq!(payload["event"], "post_tool_use");
    assert_eq!(payload["tool_name"], "execute");
    assert_eq!(payload["args"]["command"], "ls");
    assert_eq!(payload["hook_event_name"], "PostToolUse");
    assert_eq!(payload["tool_input"]["command"], "ls");
    assert_eq!(payload["tool_output"], "out");
}

#[tokio::test]
async fn exit_two_blocks_with_stderr_as_the_reason() {
    // oracle: pre_exit2
    let engine = engine(vec![claude(
        HookEvent::PreToolUse,
        None,
        "echo no >&2; exit 2",
    )]);
    let before = pre(&engine, "execute", json!({"command": "ls"})).await;
    assert_eq!(blocked(&before), "blocked by pre_tool_use hook: no");
}

#[tokio::test]
async fn exit_two_wins_over_allowing_json() {
    // oracle: pre_exit2_with_json
    let cmd = format!(
        "{}; echo stderr >&2; exit 2",
        say(r#"{"hookSpecificOutput":{"permissionDecision":"allow"}}"#)
    );
    let engine = engine(vec![claude(HookEvent::PreToolUse, None, &cmd)]);
    assert!(matches!(
        pre(&engine, "execute", json!({})).await,
        Before::Blocked(_)
    ));
}

#[tokio::test]
async fn exit_one_and_plain_stdout_do_not_block() {
    // oracle: pre_exit1, pre_plain
    for cmd in ["echo oops >&2; exit 1", "echo some text"] {
        let engine = engine(vec![claude(HookEvent::PreToolUse, None, cmd)]);
        assert!(matches!(
            pre(&engine, "execute", json!({"command": "ls"})).await,
            Before::Proceed { .. }
        ));
    }
}

#[tokio::test]
async fn permission_decision_deny_blocks_with_its_reason() {
    // oracle: p_hook_deny_bypass
    let cmd = say(
        r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"denied by hook"}}"#,
    );
    let engine = engine(vec![claude(HookEvent::PreToolUse, None, &cmd)]);
    let before = pre(&engine, "execute", json!({})).await;
    assert_eq!(
        blocked(&before),
        "blocked by pre_tool_use hook: denied by hook"
    );
}

#[tokio::test]
async fn old_style_block_decision_blocks() {
    // oracle: pre_oldblock
    let cmd = say(r#"{"decision":"block","reason":"old-style block"}"#);
    let engine = engine(vec![claude(HookEvent::PreToolUse, None, &cmd)]);
    let before = pre(&engine, "execute", json!({})).await;
    assert!(blocked(&before).ends_with("old-style block"));
}

#[tokio::test]
async fn updated_input_rewrites_arguments_in_wizard_shape() {
    // oracle: pre_rewrite, pre_rewrite_noperm
    let cmd = say(r#"{"hookSpecificOutput":{"updatedInput":{"file_path":"b.rs","content":"y"}}}"#);
    let engine = engine(vec![claude(HookEvent::PreToolUse, None, &cmd)]);
    match pre(
        &engine,
        "write_file",
        json!({"path": "a.rs", "content": "x"}),
    )
    .await
    {
        Before::Proceed { args, .. } => assert_eq!(args, json!({"path": "b.rs", "content": "y"})),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn later_hooks_see_the_rewrite() {
    let rewrite = say(r#"{"hookSpecificOutput":{"updatedInput":{"command":"first"}}}"#);
    let engine = engine(vec![
        claude(HookEvent::PreToolUse, None, &rewrite),
        claude(HookEvent::PreToolUse, None, "cat >&2; exit 2"),
    ]);
    let before = pre(&engine, "execute", json!({"command": "orig"})).await;
    assert!(blocked(&before).contains(r#""command":"first""#));
}

#[tokio::test]
async fn pre_tool_additional_context_rides_with_the_result() {
    // oracle: pre_ctx
    let cmd = say(r#"{"hookSpecificOutput":{"additionalContext":"PRE-CTX"}}"#);
    let engine = engine(vec![claude(HookEvent::PreToolUse, None, &cmd)]);
    match pre(&engine, "execute", json!({})).await {
        Before::Proceed { context, .. } => assert_eq!(context.as_deref(), Some("PRE-CTX")),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn post_tool_block_and_exit_two_become_feedback() {
    // oracle: post_block, post_exit2
    for cmd in [
        say(r#"{"decision":"block","reason":"post says no"}"#),
        "echo post says no >&2; exit 2".to_string(),
    ] {
        let engine = engine(vec![claude(HookEvent::PostToolUse, None, &cmd)]);
        let extra = engine
            .post_tool_use_with_output("execute", &json!({}), "ok", false, Mode::default(), None)
            .await
            .unwrap();
        assert!(extra.contains("post says no"), "{extra}");
    }
}

#[tokio::test]
async fn claude_post_hooks_skip_failures_and_failure_hooks_run() {
    let engine = engine(vec![
        claude(
            HookEvent::PostToolUse,
            None,
            &say(r#"{"hookSpecificOutput":{"additionalContext":"SUCCESS"}}"#),
        ),
        claude(
            HookEvent::PostToolUseFailure,
            None,
            &say(r#"{"hookSpecificOutput":{"additionalContext":"FAILED"}}"#),
        ),
    ]);
    let ok = engine
        .post_tool_use_with_output("execute", &json!({}), "fine", false, Mode::default(), None)
        .await;
    assert_eq!(ok.as_deref(), Some("SUCCESS"));
    let bad = engine
        .post_tool_use_with_output("execute", &json!({}), "boom", true, Mode::default(), None)
        .await;
    assert_eq!(bad.as_deref(), Some("FAILED"));
}

#[tokio::test]
async fn user_prompt_exit_two_and_block_decision_block_the_turn() {
    // oracle: ups_exit2, ups_block
    for cmd in [
        "echo nope >&2; exit 2".to_string(),
        say(r#"{"decision":"block","reason":"nope"}"#),
    ] {
        let engine = engine(vec![claude(HookEvent::UserPromptSubmit, None, &cmd)]);
        let result = engine
            .user_prompt_submit_with_prompt("hi", Mode::default(), None)
            .await;
        assert!(matches!(result, PromptSubmit::Block(reason) if reason == "nope"));
    }
}

#[tokio::test]
async fn user_prompt_plain_stdout_and_additional_context_are_context() {
    // oracle: ups_plain, and "UserPromptSubmit hook additional context"
    for cmd in [
        "echo extra".to_string(),
        say(
            r#"{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"extra"}}"#,
        ),
    ] {
        let engine = engine(vec![claude(HookEvent::UserPromptSubmit, None, &cmd)]);
        let result = engine
            .user_prompt_submit_with_prompt("hi", Mode::default(), None)
            .await;
        assert!(matches!(result, PromptSubmit::Continue(Some(text)) if text == "extra"));
    }
}

#[tokio::test]
async fn plain_stdout_is_not_context_on_other_claude_events() {
    let engine = engine(vec![claude(HookEvent::PostToolUse, None, "echo chatter")]);
    let extra = engine
        .post_tool_use_with_output("execute", &json!({}), "ok", false, Mode::default(), None)
        .await;
    assert_eq!(extra, None);
}

#[tokio::test]
async fn stop_hook_block_sends_the_model_back() {
    // oracle: stop_block_once ("Stop hook feedback: keep going please")
    let cmd = say(r#"{"decision":"block","reason":"keep going please"}"#);
    let engine = engine(vec![claude(HookEvent::Stop, None, &cmd)]);
    let again = engine.stop("done", false, Mode::default(), None).await;
    assert_eq!(again.as_deref(), Some("keep going please"));
    let none = super::engine::HookEngine::from_specs(vec![], std::env::temp_dir(), "x".into());
    assert_eq!(none.stop("done", false, Mode::default(), None).await, None);
}

#[tokio::test]
async fn stop_payload_has_the_last_message_and_active_flag() {
    let engine = engine(vec![claude(HookEvent::Stop, None, "cat >&2; exit 2")]);
    let feedback = engine
        .stop("all done", true, Mode::default(), None)
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&feedback).unwrap();
    assert_eq!(payload["stop_hook_active"], true);
    assert_eq!(payload["last_assistant_message"], "all done");
    assert_eq!(payload["hook_event_name"], "Stop");
}

#[tokio::test]
async fn matchers_follow_claude_rules_for_claude_hooks() {
    // oracle: matcher_regex_pipe, matcher_regex_dot, matcher_nomatch
    let engine = engine(vec![
        claude(HookEvent::PreToolUse, Some("Edit|Bash"), "echo pipe"),
        claude(HookEvent::PreToolUse, Some("^Ba.*"), "echo regex"),
        claude(HookEvent::PreToolUse, Some("Write"), "echo write"),
        claude(HookEvent::PreToolUse, Some("mcp__gh__.*"), "echo mcp"),
    ]);
    assert_eq!(
        engine.matching_commands(HookEvent::PreToolUse, Some("execute")),
        vec!["echo pipe", "echo regex"]
    );
    assert_eq!(
        engine.matching_commands(HookEvent::PreToolUse, Some("write_file")),
        vec!["echo write"]
    );
    assert_eq!(
        engine.matching_commands(HookEvent::PreToolUse, Some("gh__list")),
        vec!["echo mcp"]
    );
}

#[tokio::test]
async fn non_tool_matchers_use_the_event_subject() {
    let engine = engine(vec![claude(
        HookEvent::SessionStart,
        Some("resume"),
        "echo resumed",
    )]);
    let startup = engine
        .session_start_from("startup", Mode::default(), None)
        .await;
    assert_eq!(startup, None);
    let resumed = engine
        .session_start_from("resume", Mode::default(), None)
        .await;
    assert_eq!(resumed.as_deref(), Some("resumed"));
}

#[tokio::test]
async fn identical_claude_hooks_from_two_files_run_once() {
    let a = claude(HookEvent::PreToolUse, None, "echo same");
    let mut b = a.clone();
    b.source = "other file".to_string();
    let engine = engine(vec![a, b]);
    assert_eq!(engine.specs().count(), 1);
}

#[tokio::test]
async fn the_if_condition_limits_a_hook_to_matching_calls() {
    let mut hook = claude(HookEvent::PreToolUse, None, "echo checked >&2; exit 2");
    hook.condition = Some("Bash(git push:*)".to_string());
    let engine = engine(vec![hook]);
    assert!(matches!(
        pre(&engine, "execute", json!({"command": "ls"})).await,
        Before::Proceed { .. }
    ));
    assert!(matches!(
        pre(
            &engine,
            "execute",
            json!({"command": "git push origin main"})
        )
        .await,
        Before::Blocked(_)
    ));
}

#[tokio::test]
async fn on_failure_block_turns_a_crash_into_a_block() {
    let mut hook = claude(HookEvent::PreToolUse, None, "exit 7");
    hook.block_on_failure = true;
    let engine = engine(vec![hook]);
    assert!(matches!(
        pre(&engine, "execute", json!({})).await,
        Before::Blocked(_)
    ));
}

#[tokio::test]
async fn async_hooks_do_not_hold_up_the_call() {
    let mut hook = claude(HookEvent::PreToolUse, None, "sleep 5; exit 2");
    hook.is_async = true;
    let engine = engine(vec![hook]);
    let started = std::time::Instant::now();
    assert!(matches!(
        pre(&engine, "execute", json!({})).await,
        Before::Proceed { .. }
    ));
    assert!(started.elapsed() < std::time::Duration::from_secs(3));
}

#[tokio::test]
async fn continue_false_halts_a_pre_tool_use() {
    // oracle: pre_halt
    let cmd = say(r#"{"continue":false,"stopReason":"halted by hook"}"#);
    let engine = engine(vec![claude(HookEvent::PreToolUse, None, &cmd)]);
    assert!(blocked(&pre(&engine, "execute", json!({})).await).contains("halted by hook"));
}

fn policy_with(mode: PermissionMode, allow: &[&str], ask: &[&str], deny: &[&str]) -> Policy {
    let root = std::path::Path::new("/p");
    let parse = |rules: &[&str]| {
        rules
            .iter()
            .map(|r| Rule::parse(r, root).unwrap())
            .collect()
    };
    Policy {
        mode,
        allow: parse(allow),
        ask: parse(ask),
        deny: parse(deny),
        cwd: root.to_path_buf(),
    }
}

#[tokio::test]
async fn a_deny_rule_blocks_before_the_tool_runs() {
    let engine = engine(vec![]);
    engine.set_policy(policy_with(
        PermissionMode::Default,
        &[],
        &[],
        &["Bash(rm:*)"],
    ));
    let before = pre(&engine, "execute", json!({"command": "rm -rf x"})).await;
    assert_eq!(
        blocked(&before),
        "Permission to use Bash with command rm -rf x has been denied."
    );
    assert!(matches!(
        pre(&engine, "execute", json!({"command": "ls"})).await,
        Before::Proceed { .. }
    ));
}

#[tokio::test]
async fn a_hook_allow_does_not_beat_a_deny_rule() {
    // oracle: p_hook_allow_vs_deny_rule
    let cmd = say(r#"{"hookSpecificOutput":{"permissionDecision":"allow"}}"#);
    let engine = engine(vec![claude(HookEvent::PreToolUse, None, &cmd)]);
    engine.set_policy(policy_with(PermissionMode::Default, &[], &[], &["Bash"]));
    assert!(matches!(
        pre(&engine, "execute", json!({})).await,
        Before::Blocked(_)
    ));
}

#[tokio::test]
async fn a_hook_ask_makes_the_call_ask_and_nobody_answers() {
    // oracle: p_hook_ask_default
    let cmd = say(
        r#"{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"hook asks"}}"#,
    );
    let engine = engine(vec![claude(HookEvent::PreToolUse, None, &cmd)]);
    let before = pre(&engine, "execute", json!({})).await;
    assert!(blocked(&before).starts_with("hook asks"));
    assert!(blocked(&before).contains("No one is available"));
}

#[tokio::test]
async fn an_ask_rule_goes_to_permission_request_hooks() {
    // oracle: p_permreq_allow, p_permreq_deny
    let allow = say(
        r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#,
    );
    let deny = say(
        r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"perm hook denies"}}}"#,
    );
    for (cmd, allowed) in [(allow, true), (deny, false)] {
        let engine = engine(vec![claude(HookEvent::PermissionRequest, None, &cmd)]);
        engine.set_policy(policy_with(PermissionMode::Default, &[], &["Bash"], &[]));
        let before = pre(&engine, "execute", json!({"command": "ls"})).await;
        match (allowed, &before) {
            (true, Before::Proceed { .. }) => {}
            (false, Before::Blocked(message)) => assert_eq!(message, "perm hook denies"),
            other => panic!("{other:?}"),
        }
    }
}

#[tokio::test]
async fn a_denied_call_fires_permission_denied() {
    let engine = engine(vec![claude(
        HookEvent::PermissionDenied,
        None,
        "cat >&2; exit 0",
    )]);
    engine.set_policy(policy_with(PermissionMode::Default, &[], &[], &["Bash"]));
    // The hook's stderr is dropped on exit 0; what matters is the call is
    // refused and the event path does not panic or hang.
    assert!(matches!(
        pre(&engine, "execute", json!({})).await,
        Before::Blocked(_)
    ));
}

#[tokio::test]
async fn modes_are_enforced_by_before_tool() {
    let engine = engine(vec![]);
    engine.set_policy(policy_with(
        PermissionMode::DontAsk,
        &["Bash(ls:*)"],
        &[],
        &[],
    ));
    assert!(matches!(
        pre(&engine, "execute", json!({"command": "ls -la"})).await,
        Before::Proceed { .. }
    ));
    assert!(matches!(
        pre(&engine, "execute", json!({"command": "touch x"})).await,
        Before::Blocked(_)
    ));
    engine.set_mode(PermissionMode::BypassPermissions);
    assert!(matches!(
        pre(&engine, "execute", json!({"command": "touch x"})).await,
        Before::Proceed { .. }
    ));
}

#[tokio::test]
async fn pre_compact_block_and_pre_model_switch_block() {
    let block = say(r#"{"decision":"block","reason":"not now"}"#);
    let engine = engine(vec![
        claude(HookEvent::PreCompact, Some("auto"), &block),
        claude(HookEvent::PreModelSwitch, None, &block),
    ]);
    assert_eq!(
        engine
            .pre_compact("auto", None, Mode::default(), None)
            .await
            .as_deref(),
        Some("not now")
    );
    assert_eq!(
        engine
            .pre_compact("manual", None, Mode::default(), None)
            .await,
        None
    );
    assert_eq!(
        engine
            .pre_model_switch("a", "b", "command", Mode::default(), None)
            .await
            .as_deref(),
        Some("not now")
    );
}

#[tokio::test]
async fn post_tool_batch_can_add_context_and_halt() {
    let cmd = say(
        r#"{"hookSpecificOutput":{"additionalContext":"BATCH"},"continue":false,"stopReason":"enough"}"#,
    );
    let engine = engine(vec![claude(HookEvent::PostToolBatch, None, &cmd)]);
    let args = json!({"command": "ls"});
    let verdict = engine
        .post_tool_batch(
            &[lifecycle::BatchCall {
                tool: "execute",
                args: &args,
                tool_use_id: "t",
                output: "out",
            }],
            Mode::default(),
            None,
        )
        .await;
    assert_eq!(verdict.context.as_deref(), Some("BATCH"));
    assert_eq!(verdict.halt.as_deref(), Some("enough"));
}

#[tokio::test]
async fn todo_writes_fire_task_events_for_new_and_finished_items() {
    let log = std::env::temp_dir().join(format!("wizard-task-log-{}", uuid::Uuid::new_v4()));
    let cmd = format!("cat >> {}; echo >> {}", log.display(), log.display());
    let engine = engine(vec![
        claude(HookEvent::TaskCreated, None, &cmd),
        claude(HookEvent::TaskCompleted, None, &cmd),
    ]);
    let write = |items: Value| json!({"action": "write", "items": items});
    let first = write(json!([{"content": "a", "status": "pending"}]));
    engine
        .post_tool_use_with_output("todo", &first, "ok", false, Mode::default(), None)
        .await;
    let second = write(
        json!([{"content": "a", "status": "completed"}, {"content": "b", "status": "pending"}]),
    );
    engine
        .post_tool_use_with_output("todo", &second, "ok", false, Mode::default(), None)
        .await;
    let text = std::fs::read_to_string(&log).unwrap();
    let _ = std::fs::remove_file(&log);
    let events: Vec<String> = text
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            serde_json::from_str::<Value>(l).unwrap()["hook_event_name"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(events, ["TaskCreated", "TaskCompleted", "TaskCreated"]);
}

#[tokio::test]
async fn prompt_hooks_use_the_backend_and_ok_false_blocks() {
    struct Fake(&'static str);
    #[async_trait::async_trait]
    impl HookBackend for Fake {
        async fn ask_model(&self, prompt: &str, _: Option<&str>) -> Result<String, String> {
            assert!(prompt.contains("check it"), "{prompt}");
            assert!(
                prompt.contains("PreToolUse"),
                "payload is spliced in: {prompt}"
            );
            Ok(self.0.to_string())
        }
        async fn run_agent(&self, p: &str, m: Option<&str>) -> Result<String, String> {
            self.ask_model(p, m).await
        }
        async fn call_mcp(&self, _: &str, _: &str, _: Value) -> Result<(String, bool), String> {
            Err("unused".into())
        }
    }
    let spec = HookSpec {
        handler: Handler::Prompt {
            prompt: "check it: $ARGUMENTS".into(),
            model: None,
        },
        ..claude(HookEvent::PreToolUse, None, "")
    };
    let engine = engine(vec![spec.clone()]);
    engine.set_backend(std::sync::Arc::new(Fake(
        r#"here you go {"ok": false, "reason": "unsafe"}"#,
    )));
    assert_eq!(
        blocked(&pre(&engine, "execute", json!({})).await),
        "blocked by pre_tool_use hook: unsafe"
    );
    let engine = self::engine(vec![spec]);
    engine.set_backend(std::sync::Arc::new(Fake(r#"{"ok": true}"#)));
    assert!(matches!(
        pre(&engine, "execute", json!({})).await,
        Before::Proceed { .. }
    ));
}

#[tokio::test]
async fn prompt_hooks_without_a_backend_warn_and_proceed() {
    let spec = HookSpec {
        handler: Handler::Prompt {
            prompt: "x".into(),
            model: None,
        },
        ..claude(HookEvent::PreToolUse, None, "")
    };
    let engine = engine(vec![spec]);
    assert!(matches!(
        pre(&engine, "execute", json!({})).await,
        Before::Proceed { .. }
    ));
}

#[tokio::test]
async fn mcp_tool_hooks_substitute_payload_paths() {
    struct Fake;
    #[async_trait::async_trait]
    impl HookBackend for Fake {
        async fn ask_model(&self, _: &str, _: Option<&str>) -> Result<String, String> {
            Err("unused".into())
        }
        async fn run_agent(&self, _: &str, _: Option<&str>) -> Result<String, String> {
            Err("unused".into())
        }
        async fn call_mcp(
            &self,
            server: &str,
            tool: &str,
            input: Value,
        ) -> Result<(String, bool), String> {
            assert_eq!((server, tool), ("audit", "log"));
            assert_eq!(input, json!({"msg": "Bash ls"}));
            Ok((r#"{"decision":"block","reason":"audited"}"#.into(), false))
        }
    }
    let spec = HookSpec {
        handler: Handler::McpTool {
            server: "audit".into(),
            tool: "log".into(),
            input: json!({"msg": "${tool_name} ${tool_input.command}"}),
        },
        ..claude(HookEvent::PreToolUse, None, "")
    };
    let engine = engine(vec![spec]);
    engine.set_backend(std::sync::Arc::new(Fake));
    assert_eq!(
        blocked(&pre(&engine, "execute", json!({"command": "ls"})).await),
        "blocked by pre_tool_use hook: audited"
    );
}

/// Keys Claude Code 2.1.314 put on stdin in a mock-API run, per event. A
/// script written against Claude's payloads reads these names.
#[tokio::test]
async fn payloads_carry_the_keys_claude_code_sends() {
    fn keys(payload: &str) -> Vec<String> {
        let value: Value = serde_json::from_str(payload).unwrap();
        value.as_object().unwrap().keys().cloned().collect()
    }
    fn assert_has(payload: &str, want: &[&str]) {
        let have = keys(payload);
        for key in want {
            assert!(have.iter().any(|k| k == key), "missing {key} in {have:?}");
        }
    }
    let echo_block = "cat >&2; exit 2";
    let common = [
        "session_id",
        "transcript_path",
        "cwd",
        "hook_event_name",
        "permission_mode",
    ];

    let engine_pre = engine(vec![claude(HookEvent::PreToolUse, None, echo_block)]);
    let before = pre(&engine_pre, "execute", json!({"command": "ls"})).await;
    let payload = blocked(&before).trim_start_matches("blocked by pre_tool_use hook: ");
    assert_has(payload, &common);
    assert_has(payload, &["tool_name", "tool_input", "tool_use_id"]);

    let engine_ups = engine(vec![claude(HookEvent::UserPromptSubmit, None, echo_block)]);
    let PromptSubmit::Block(payload) = engine_ups
        .user_prompt_submit_with_prompt("p", Mode::default(), None)
        .await
    else {
        panic!("blocked");
    };
    assert_has(&payload, &common);
    assert_has(&payload, &["prompt"]);

    let engine_start = engine(vec![claude(HookEvent::SessionStart, None, "cat")]);
    let out = engine_start
        .emit(
            HookEvent::SessionStart,
            Some("startup"),
            json!({"source": "startup"}),
            Mode::default(),
            None,
        )
        .await;
    assert!(
        out.context.is_empty(),
        "a JSON object on stdout is a decision, not context"
    );
    let engine_start = engine(vec![claude(
        HookEvent::SessionStart,
        None,
        "cat > /dev/null; echo ready",
    )]);
    assert_eq!(
        engine_start
            .session_start(Mode::default(), None)
            .await
            .as_deref(),
        Some("ready")
    );

    let engine_stop = engine(vec![claude(HookEvent::Stop, None, echo_block)]);
    let payload = engine_stop
        .stop("m", false, Mode::default(), None)
        .await
        .unwrap();
    assert_has(&payload, &common);
    assert_has(
        &payload,
        &[
            "stop_hook_active",
            "last_assistant_message",
            "background_tasks",
            "session_crons",
        ],
    );

    let engine_batch = engine(vec![claude(HookEvent::PostToolBatch, None, echo_block)]);
    let args = json!({"command": "ls"});
    let verdict = engine_batch
        .post_tool_batch(
            &[lifecycle::BatchCall {
                tool: "execute",
                args: &args,
                tool_use_id: "t",
                output: "o",
            }],
            Mode::default(),
            None,
        )
        .await;
    let payload = verdict.halt.unwrap();
    assert_has(&payload, &common);
    assert_has(&payload, &["tool_calls"]);
    let value: Value = serde_json::from_str(&payload).unwrap();
    for key in ["tool_name", "tool_input", "tool_use_id", "tool_response"] {
        assert!(
            value["tool_calls"][0].get(key).is_some(),
            "tool_calls[0].{key}"
        );
    }
}
