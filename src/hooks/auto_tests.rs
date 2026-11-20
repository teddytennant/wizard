//! Auto mode through the engine: the judge sits between the policy and the
//! tool, with a fake model behind it.

use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};

use super::engine::{Before, HookEngine};
use crate::config::Mode;
use crate::permissions::judge_tests::Fake;
use crate::permissions::{PermissionMode, Policy};
use crate::tools::ToolAccess;

fn engine_with(reply: &str) -> (HookEngine, Arc<Fake>) {
    let engine = HookEngine::from_specs(Vec::new(), std::env::temp_dir(), "auto-test".to_string());
    let mut policy = Policy::new(Path::new("/p").to_path_buf());
    policy.mode = PermissionMode::Auto;
    policy.claude_defaults = true;
    engine.set_policy(policy);
    let fake = Arc::new(Fake::says(reply));
    engine.set_backend(fake.clone());
    (engine, fake)
}

async fn asked(engine: &HookEngine, request: &str) {
    engine
        .user_prompt_submit_with_prompt(request, Mode::default(), None)
        .await;
}

async fn call(engine: &HookEngine, tool: &str, access: ToolAccess, args: Value) -> Before {
    engine
        .before_tool(tool, access, &args, "toolu_1", Mode::default(), None)
        .await
}

fn blocked(before: &Before) -> &str {
    match before {
        Before::Blocked(message) => message,
        other => panic!("expected a block, got {other:?}"),
    }
}

#[tokio::test]
async fn an_allowed_call_runs() {
    let (engine, fake) = engine_with("ALLOW");
    asked(&engine, "format the code").await;
    let before = call(
        &engine,
        "execute",
        ToolAccess::Execute,
        json!({ "command": "cargo fmt" }),
    )
    .await;
    assert!(matches!(before, Before::Proceed { .. }), "{before:?}");
    let seen = fake.seen.lock().unwrap();
    assert!(seen[0].0.contains("format the code"));
    assert!(seen[0].0.contains("cargo fmt"));
}

#[tokio::test]
async fn a_denied_call_comes_back_to_the_model_with_the_reason() {
    let (engine, _) = engine_with("DENY: nobody asked for a deploy");
    asked(&engine, "fix the typo in the readme").await;
    let before = call(
        &engine,
        "execute",
        ToolAccess::Execute,
        json!({ "command": "./deploy.sh" }),
    )
    .await;
    let text = blocked(&before);
    assert!(text.contains("nobody asked for a deploy"), "{text}");
    assert!(text.contains("./deploy.sh"), "{text}");
}

#[tokio::test]
async fn a_judge_that_cannot_answer_fails_closed_to_ask() {
    for reply in ["no idea", ""] {
        let (engine, fake) = engine_with(reply);
        asked(&engine, "tidy up").await;
        let before = call(
            &engine,
            "execute",
            ToolAccess::Execute,
            json!({ "command": "make clean" }),
        )
        .await;
        // with nobody to approve, asking is a refusal in Claude Code's words
        assert_eq!(
            blocked(&before),
            "This command requires approval",
            "{reply:?}"
        );
        assert_eq!(fake.calls(), 1);
    }
}

#[tokio::test]
async fn an_erroring_model_fails_closed_to_ask() {
    let (engine, _) = engine_with("");
    let broken = Arc::new(Fake {
        reply: Err("model call failed: 503".into()),
        ..Fake::says("")
    });
    engine.set_backend(broken);
    asked(&engine, "tidy up").await;
    let before = call(
        &engine,
        "write_file",
        ToolAccess::Edit,
        json!({ "path": "/p/a.txt", "content": "x" }),
    )
    .await;
    assert!(blocked(&before).contains("haven't granted it yet"));
}

#[tokio::test]
async fn no_request_on_record_fails_closed_to_ask() {
    let (engine, fake) = engine_with("ALLOW");
    let before = call(
        &engine,
        "execute",
        ToolAccess::Execute,
        json!({ "command": "make clean" }),
    )
    .await;
    assert!(matches!(before, Before::Blocked(_)));
    assert_eq!(fake.calls(), 0);
}

#[tokio::test]
async fn read_only_calls_never_reach_the_judge() {
    let (engine, fake) = engine_with("DENY: no");
    asked(&engine, "look around").await;
    for (tool, access, args) in [
        ("read_file", ToolAccess::ReadOnly, json!({ "path": "a.rs" })),
        (
            "execute",
            ToolAccess::Execute,
            json!({ "command": "git status" }),
        ),
        ("todo", ToolAccess::Execute, json!({ "todos": [] })),
    ] {
        let before = call(&engine, tool, access, args).await;
        assert!(
            matches!(before, Before::Proceed { .. }),
            "{tool}: {before:?}"
        );
    }
    assert_eq!(fake.calls(), 0);
}

#[tokio::test]
async fn the_newest_request_is_the_one_judged() {
    let (engine, fake) = engine_with("ALLOW");
    asked(&engine, "first thing").await;
    asked(&engine, "second thing").await;
    call(
        &engine,
        "execute",
        ToolAccess::Execute,
        json!({ "command": "make" }),
    )
    .await;
    let seen = fake.seen.lock().unwrap();
    assert!(seen[0].0.contains("second thing"));
    assert!(!seen[0].0.contains("first thing"));
}

#[tokio::test]
async fn the_other_modes_never_call_the_judge() {
    let (engine, fake) = engine_with("DENY: no");
    let mut policy = engine.policy();
    policy.mode = PermissionMode::BypassPermissions;
    engine.set_policy(policy);
    asked(&engine, "anything").await;
    let before = call(
        &engine,
        "execute",
        ToolAccess::Execute,
        json!({ "command": "make" }),
    )
    .await;
    assert!(matches!(before, Before::Proceed { .. }));
    assert_eq!(fake.calls(), 0);
}
