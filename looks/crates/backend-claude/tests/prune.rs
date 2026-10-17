//! The mapper forgets a tool call once its result is in (finding 22): a long session used to
//! keep every call and its output for as long as it lived.
use backend_claude::mapper::Mapper;
use serde_json::json;
use std::path::PathBuf;

fn call(i: usize) -> serde_json::Value {
    json!({"type": "assistant", "message": {"id": format!("m{i}"), "model": "claude-x",
        "content": [{"type": "tool_use", "id": format!("t{i}"), "name": "Bash",
                     "input": {"command": "echo hi"}}]}})
}

fn result(i: usize, text: &str) -> serde_json::Value {
    json!({"type": "user", "message": {"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": format!("t{i}"), "content": text}]},
        "tool_use_result": {"stdout": text, "stderr": "", "interrupted": false}})
}

#[test]
fn finished_tool_calls_are_not_kept() {
    let mut m = Mapper::new(PathBuf::from("/work"));
    let big = "x".repeat(20_000);
    for i in 0..3000 {
        let evs = m.feed(&call(i));
        assert!(evs.iter().any(|e| matches!(e, agent_core::Event::Tool(_))));
        let evs = m.feed(&result(i, &big));
        let done = evs.iter().any(|e| matches!(e, agent_core::Event::Tool(t) if t.status == agent_core::ToolStatus::Completed));
        assert!(done, "call {i} did not complete: {evs:?}");
    }
    assert_eq!(m.tracked_tools(), 0, "every call has finished");
}

#[test]
fn a_call_still_waiting_for_its_result_is_kept_and_a_background_agent_stays_for_its_notice() {
    let mut m = Mapper::new(PathBuf::from("/work"));
    m.feed(&call(1));
    assert_eq!(m.tracked_tools(), 1);
    m.feed(&result(1, "done"));
    assert_eq!(m.tracked_tools(), 0);
    // an Agent call whose result only says it was launched
    m.feed(
        &json!({"type": "assistant", "message": {"id": "ma", "model": "x", "content": [
        {"type": "tool_use", "id": "tA", "name": "Agent", "input": {"prompt": "go"}}]}}),
    );
    m.feed(
        &json!({"type": "user", "message": {"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "tA", "content": "launched"}]},
        "tool_use_result": {"isAsync": true, "status": "async_launched"}}),
    );
    assert_eq!(
        m.tracked_tools(),
        1,
        "a background agent waits for its notification"
    );
    let evs = m.feed(&json!({"type": "system", "subtype": "task_notification",
        "tool_use_id": "tA", "status": "completed", "summary": "all done"}));
    assert!(
        evs.iter().any(
            |e| matches!(e, agent_core::Event::Tool(t) if t.output.as_deref() == Some("all done"))
        ),
        "{evs:?}"
    );
    assert_eq!(m.tracked_tools(), 0);
}
