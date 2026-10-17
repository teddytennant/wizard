//! Numbers off the wire are not trusted: a debug build must not panic on them and a release
//! build must not wrap them into small ones.
use backend_claude::mapper::Mapper;
use serde_json::json;
use std::path::PathBuf;

#[test]
fn a_u64_max_token_count_saturates_instead_of_overflowing() {
    let mut m = Mapper::new(PathBuf::from("/work/proj"));
    let evs = m.feed(&json!({
        "type": "stream_event",
        "parent_tool_use_id": null,
        "event": {"type": "message_delta",
                  "usage": {"input_tokens": u64::MAX, "cache_read_input_tokens": 2, "output_tokens": 1}}
    }));
    let usage = evs
        .iter()
        .find_map(|e| match e {
            agent_core::Event::Usage(u) => Some(u.clone()),
            _ => None,
        })
        .expect("a usage event");
    assert_eq!(usage.context_tokens, u64::MAX);
}
