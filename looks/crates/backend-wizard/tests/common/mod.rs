//! Replays a recorded `wizard acp` transcript through `Core` with no process.
//!
//! Fixture lines are `{"dir": "out"|"in"|"err", "t": secs, "msg": ...}`. `out`
//! lines are translated back into `Request`s (the core picks its own ids, so
//! responses are re-keyed); `in` lines are fed as they came. The hidden
//! `/status` probe has no recording, so the harness answers it itself.

#![allow(dead_code)]

use std::collections::HashMap;

use agent_core::{Event, Request};
use backend_wizard::core::{Action, Core};
use serde_json::{json, Value};

pub struct Rec {
    pub dir: String,
    pub msg: Value,
}

pub fn load(name: &str) -> Vec<Rec> {
    let path = format!("{}/tests/fixtures/{name}.jsonl", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{path}: {e}"))
        .lines()
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            Rec {
                dir: v["dir"].as_str().unwrap().to_string(),
                msg: v["msg"].clone(),
            }
        })
        .collect()
}

pub const PROBE_REPLY: &str =
    "model: grok-4.7\nusage: 100 prompt + 5 completion tokens\ncontext: 77 tokens";

pub struct Run {
    pub core: Core,
    pub events: Vec<Event>,
    /// Every JSON-RPC message the core wrote, in order.
    pub sent: Vec<Value>,
    idmap: HashMap<u64, u64>,
    unclaimed: Vec<(u64, String)>,
}

impl Run {
    pub fn new(cwd: &str, resume: Option<&str>) -> Self {
        let core = Core::new(
            cwd.to_string(),
            resume.map(str::to_string),
            "wizard 3.7.1".into(),
        );
        Run {
            core,
            events: vec![],
            sent: vec![],
            idmap: HashMap::new(),
            unclaimed: vec![],
        }
    }

    fn absorb(&mut self, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::Emit(e) => self.events.push(e),
                Action::Send(v) => {
                    if let (Some(id), Some(m)) = (
                        v.get("id").and_then(Value::as_u64),
                        v.get("method").and_then(Value::as_str),
                    ) {
                        if Some(id) != self.core.probe_request_id() {
                            self.unclaimed.push((id, m.to_string()));
                        }
                    }
                    self.sent.push(v);
                }
                Action::CloseStdin => {}
            }
        }
    }

    pub fn request(&mut self, r: Request) {
        let a = self.core.on_request(r);
        self.absorb(a);
    }

    pub fn line(&mut self, v: &Value) {
        let a = self.core.on_line(&v.to_string());
        self.absorb(a);
        if let Some(id) = self.core.probe_request_id() {
            let sid = self.core.session_id().unwrap_or("").to_string();
            let chunk = json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":sid,
                "update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":PROBE_REPLY}}}});
            let a = self.core.on_line(&chunk.to_string());
            self.absorb(a);
            let resp = json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}});
            let a = self.core.on_line(&resp.to_string());
            self.absorb(a);
        }
    }

    fn claim(&mut self, fixture_id: u64, method: &str) {
        if let Some(i) = self.unclaimed.iter().position(|(_, m)| m == method) {
            let (id, _) = self.unclaimed.remove(i);
            self.idmap.insert(fixture_id, id);
        }
    }

    pub fn out(&mut self, msg: &Value) {
        let method = msg["method"].as_str().unwrap_or("");
        let fid = msg.get("id").and_then(Value::as_u64);
        let p = &msg["params"];
        match method {
            "initialize" => {
                let a = self.core.start();
                self.absorb(a);
            }
            "session/new" => {}
            "session/prompt" => {
                let text = p["prompt"][0]["text"].as_str().unwrap().to_string();
                self.request(Request::Prompt(text));
            }
            "session/cancel" => self.request(Request::Cancel),
            "session/list" => self.request(Request::ListSessions),
            "session/load" => self.request(Request::LoadSession(
                p["sessionId"].as_str().unwrap().to_string(),
            )),
            "session/set_config_option" => {
                let v = p["value"].as_str().unwrap().to_string();
                self.request(match p["configId"].as_str().unwrap() {
                    "model" => Request::SetModel(v),
                    "thought_level" => Request::SetEffort(v),
                    "wizard_mode" => Request::SetMode(v),
                    other => panic!("config {other}"),
                });
            }
            other => panic!("unhandled out method {other}"),
        }
        if let Some(fid) = fid {
            self.claim(fid, method);
        }
    }

    pub fn inbound(&mut self, msg: &Value) {
        let mut m = msg.clone();
        if m.get("method").is_none() {
            if let Some(fid) = m.get("id").and_then(Value::as_u64) {
                match self.idmap.get(&fid) {
                    Some(id) => m["id"] = json!(id),
                    // A response to a request the core chose not to send (an
                    // option change mid-turn, a prompt it queued).
                    None => return,
                }
            }
        }
        self.line(&m);
    }
}

pub fn replay(name: &str, cwd: &str, resume: Option<&str>) -> Run {
    let mut run = Run::new(cwd, resume);
    for rec in load(name) {
        match rec.dir.as_str() {
            "out" => run.out(&rec.msg),
            "in" => run.inbound(&rec.msg),
            _ => {}
        }
    }
    run
}

/// A core that has finished the handshake: session `s1`, a few options, and
/// commands `model`, `status`, `todos`.
pub fn ready() -> Run {
    let mut run = Run::new("/work/proj", None);
    run.out(&json!({"id": 1, "method": "initialize"}));
    run.inbound(
        &json!({"jsonrpc":"2.0","id":1,"result":{"agentInfo":{"name":"wizard","version":"9.9.9"}}}),
    );
    run.out(&json!({"id": 2, "method": "session/new"}));
    run.inbound(&json!({"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1","configOptions":[
        {"id":"model","category":"model","currentValue":"openrouter/anthropic/claude-sonnet-5","options":[
            {"value":"openrouter/anthropic/claude-sonnet-5","name":"x"},{"value":"xai/grok-4.6","name":"y"},{"value":"xai-oauth/grok-4.6","name":"z"}]},
        {"id":"thought_level","category":"thought_level","currentValue":"low","options":[{"value":"low"},{"value":"high"}]},
        {"id":"wizard_mode","currentValue":"genie","options":[{"value":"genie"},{"value":"sovereign"}]}]}}));
    run.inbound(&json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{
        "sessionUpdate":"available_commands_update","availableCommands":[
            {"name":"model","description":"d","input":{"hint":"[tag]"}},{"name":"status","description":"d"},{"name":"todos","description":"d"}]}}}));
    run
}

pub fn chunk(sid: &str, kind: &str, text: &str) -> Value {
    json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":sid,"update":{"sessionUpdate":kind,"content":{"type":"text","text":text}}}})
}

/// Id of the last request the core sent with this method.
pub fn last_id(run: &Run, method: &str) -> u64 {
    run.sent
        .iter()
        .rev()
        .find(|m| m["method"] == method)
        .and_then(|m| m["id"].as_u64())
        .unwrap()
}
