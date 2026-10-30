//! Claude Code's headless message shapes, built from Wizard's agent events.
//!
//! `claude -p --output-format stream-json --verbose` prints one JSON object
//! per line: a `system` init line, `assistant` and `user` messages whose
//! `content` is a list of blocks (`text`, `thinking`, `tool_use`,
//! `tool_result`), and a closing `result` line with cost, duration and usage.
//! `--output-format json` prints just that last line. The shapes here were
//! captured from Claude Code 2.1.314 against a fake Messages server and are
//! pinned in the tests below.
//!
//! [`ClaudeRun`] is the state machine both Wizard sinks share. It turns the
//! agent's events into frames and keeps the run's totals (steps, tokens, cost,
//! last answer), so `json` and `stream-json` cannot disagree about a number.
//!
//! The agent loop reports a step as text deltas, then one `Usage`, then
//! `ToolStarted` / `ToolFinished` pairs, then `StepCompleted`. Like Claude
//! Code, each content block becomes its own `assistant` message, and every
//! block of one step shares the step's message id.

pub mod info;
pub mod meter;
pub mod schema;

use std::collections::HashMap;
use std::time::Instant;

use serde_json::{Map, Value, json};

use crate::agent::AgentEvent;
use crate::hooks::toolmap;
use meter::Meter;

/// The Claude Code release whose message shapes these follow.
pub const CLAUDE_CODE_VERSION: &str = "2.1.314";

/// How a run is priced, taken from the active provider.
#[derive(Debug, Clone, Default)]
pub struct Price {
    pub endpoint: String,
    pub usd_per_mtok_in: Option<f64>,
    pub usd_per_mtok_out: Option<f64>,
    pub self_hosted: bool,
}

/// What the `system` init line says about the session, and the knobs that
/// change what the stream carries.
#[derive(Debug, Clone, Default)]
pub struct RunInfo {
    pub session_id: String,
    pub cwd: String,
    pub model: String,
    pub permission_mode: String,
    pub output_style: String,
    pub tools: Vec<String>,
    pub slash_commands: Vec<String>,
    pub skills: Vec<String>,
    pub agents: Vec<String>,
    pub plugins: Vec<Value>,
    pub mcp_servers: Vec<Value>,
    pub price: Price,
    /// `--include-partial-messages`: `stream_event` lines for each block.
    pub partial: bool,
    /// `--include-hook-events`: hook activity as `system` lines.
    pub hook_events: bool,
    /// `--max-budget-usd`: stop the run once the estimated cost passes this.
    pub max_budget_usd: Option<f64>,
    /// `--max-turns`, for the message a run that hit it reports.
    pub max_turns: Option<u32>,
    /// `--add-dir`.
    pub additional_directories: Vec<String>,
}

/// Why a run ended, in Claude Code's words.
fn subtype_for(reason: &str, over_budget: bool) -> &'static str {
    if over_budget {
        return "error_max_budget_usd";
    }
    match reason {
        "completed" | "stopped" => "success",
        "max_steps" => "error_max_turns",
        _ => "error_during_execution",
    }
}

/// `terminal_reason` as Claude Code spells it.
fn terminal_reason(reason: &str, over_budget: bool) -> &'static str {
    if over_budget {
        return "budget_exhausted";
    }
    match reason {
        "completed" => "completed",
        "stopped" => "aborted_streaming",
        "max_steps" => "max_turns",
        "time_limit" => "time_limit",
        "circuit_breaker" => "circuit_breaker",
        _ => "error",
    }
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn short_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

#[derive(Debug, Clone, Copy, Default)]
struct StepUsage {
    input: u64,
    output: u64,
}

/// One content block waiting to become an `assistant` message.
enum Block {
    Thinking(String),
    Text(String),
}

/// The run's message state machine.
pub struct ClaudeRun {
    info: RunInfo,
    started: Instant,
    step_started: Instant,
    api_ms: u128,
    /// Message id shared by the blocks of the current step.
    message_id: String,
    message_open: bool,
    block_index: usize,
    text: String,
    thinking: String,
    step_usage: StepUsage,
    /// `(tool name, tool_use id, parent)` of calls started and not yet answered.
    pending: Vec<(String, String, Option<String>)>,
    subagent_parent: HashMap<u64, Option<String>>,
    last_text: String,
    all_text: String,
    meter: Meter,
    pub steps: u64,
    pub turns: u64,
    /// Model calls, which is what Claude Code counts as `num_turns`.
    calls: u64,
    denials: Vec<Value>,
    errors: Vec<String>,
}

impl ClaudeRun {
    pub fn new(info: RunInfo) -> Self {
        let now = Instant::now();
        let meter = Meter::new(&info.model, info.price.clone(), info.max_budget_usd);
        Self {
            info,
            meter,
            started: now,
            step_started: now,
            api_ms: 0,
            message_id: short_id("msg"),
            message_open: false,
            block_index: 0,
            text: String::new(),
            thinking: String::new(),
            step_usage: StepUsage::default(),
            pending: Vec::new(),
            subagent_parent: HashMap::new(),
            last_text: String::new(),
            all_text: String::new(),
            steps: 0,
            turns: 0,
            calls: 0,
            denials: Vec::new(),
            errors: Vec::new(),
        }
    }

    /// Errors the run survived or ended on, in order.
    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    /// Cancel the turn in flight when `--max-budget-usd` is crossed.
    pub fn set_cancel(&mut self, cancel: crate::agent::CancelHandle) {
        self.meter.set_cancel(cancel);
    }

    pub fn info(&self) -> &RunInfo {
        &self.info
    }

    pub fn set_session_id(&mut self, id: String) {
        self.info.session_id = id;
    }

    pub fn over_budget(&self) -> bool {
        self.meter.exceeded()
    }

    /// The last step's text, the run's answer; the whole transcript when the
    /// last step said nothing.
    pub fn answer(&self) -> &str {
        if !self.text.is_empty() {
            &self.text
        } else if self.last_text.is_empty() {
            &self.all_text
        } else {
            &self.last_text
        }
    }

    /// Everything the assistant said, step after step.
    pub fn all_text(&self) -> String {
        format!("{}{}", self.all_text, self.text)
    }

    /// Estimated cost so far, in USD.
    pub fn cost_usd(&self) -> f64 {
        self.meter.cost_usd()
    }

    fn base(&self, kind: &str) -> Map<String, Value> {
        let mut map = Map::new();
        map.insert("type".into(), json!(kind));
        map
    }

    fn envelope(&self, mut map: Map<String, Value>, parent: Option<&str>) -> Value {
        map.insert("parent_tool_use_id".into(), json!(parent));
        map.insert("session_id".into(), json!(self.info.session_id));
        map.insert("uuid".into(), json!(new_uuid()));
        map.insert("timestamp".into(), json!(now_iso()));
        Value::Object(map)
    }

    /// The `system` init line.
    pub fn init(&self) -> Value {
        let info = &self.info;
        json!({
            "type": "system",
            "subtype": "init",
            "cwd": info.cwd,
            "session_id": info.session_id,
            "tools": info.tools,
            "mcp_servers": info.mcp_servers,
            "model": info.model,
            "permissionMode": info.permission_mode,
            "slash_commands": info.slash_commands,
            "apiKeySource": "none",
            "claude_code_version": CLAUDE_CODE_VERSION,
            "wizard_version": env!("CARGO_PKG_VERSION"),
            "output_style": info.output_style,
            "agents": info.agents,
            "skills": info.skills,
            "plugins": info.plugins,
            "uuid": new_uuid(),
            "original_cwd": info.cwd,
            "additional_directories": info.additional_directories,
            "terminal_slash_commands": [],
            "analytics_disabled": true,
            "product_feedback_disabled": true,
            "fast_mode_state": "off",
        })
    }

    fn assistant(&self, block: Value, parent: Option<&str>) -> Value {
        let mut map = self.base("assistant");
        map.insert(
            "message".into(),
            json!({
                "id": self.message_id,
                "type": "message",
                "role": "assistant",
                "model": self.info.model,
                "content": [block],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": self.usage_json(),
                "context_management": null,
            }),
        );
        self.envelope(map, parent)
    }

    fn usage_json(&self) -> Value {
        json!({
            "input_tokens": self.step_usage.input,
            "output_tokens": self.step_usage.output,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0,
        })
    }

    fn stream_event(&self, event: Value) -> Value {
        let mut map = self.base("stream_event");
        map.insert("event".into(), event);
        let mut value = self.envelope(map, None);
        if let Some(object) = value.as_object_mut() {
            object.remove("timestamp");
            object.insert("api_message_id".into(), json!(self.message_id));
        }
        value
    }

    fn status_line(&self) -> Value {
        json!({
            "type": "system",
            "subtype": "status",
            "status": "requesting",
            "session_id": self.info.session_id,
            "uuid": new_uuid(),
        })
    }

    /// `message_start` once per step, in partial mode.
    fn open_message(&mut self, frames: &mut Vec<Value>) {
        if self.message_open || !self.info.partial {
            return;
        }
        self.message_open = true;
        frames.push(self.status_line());
        frames.push(self.stream_event(json!({
            "type": "message_start",
            "message": {
                "id": self.message_id,
                "type": "message",
                "role": "assistant",
                "model": self.info.model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": 0,
                    "output_tokens": 0,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0,
                },
            },
        })));
    }

    fn partial_start(&mut self, frames: &mut Vec<Value>, block: Value) {
        if !self.info.partial {
            return;
        }
        self.open_message(frames);
        frames.push(self.stream_event(json!({
            "type": "content_block_start",
            "index": self.block_index,
            "content_block": block,
        })));
    }

    fn partial_delta(&self, frames: &mut Vec<Value>, delta: Value) {
        if self.info.partial {
            frames.push(self.stream_event(json!({
                "type": "content_block_delta",
                "index": self.block_index,
                "delta": delta,
            })));
        }
    }

    fn partial_stop(&mut self, frames: &mut Vec<Value>) {
        if self.info.partial {
            frames.push(self.stream_event(json!({
                "type": "content_block_stop",
                "index": self.block_index,
            })));
        }
        self.block_index += 1;
    }

    fn close_message(&mut self, frames: &mut Vec<Value>, stop_reason: &str) {
        if self.info.partial && self.message_open {
            frames.push(self.stream_event(json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                "usage": self.usage_json(),
            })));
            frames.push(self.stream_event(json!({"type": "message_stop"})));
        }
        self.message_open = false;
        self.block_index = 0;
    }

    /// Turn buffered thinking and text into `assistant` messages.
    fn flush_blocks(&mut self, frames: &mut Vec<Value>) {
        let mut blocks = Vec::new();
        if !self.thinking.is_empty() {
            blocks.push(Block::Thinking(std::mem::take(&mut self.thinking)));
        }
        if !self.text.is_empty() {
            blocks.push(Block::Text(std::mem::take(&mut self.text)));
        }
        for block in blocks {
            let value = match block {
                Block::Thinking(text) => {
                    json!({"type": "thinking", "thinking": text, "signature": ""})
                }
                Block::Text(text) => json!({"type": "text", "text": text}),
            };
            frames.push(self.assistant(value, None));
            self.partial_stop(frames);
        }
    }

    fn end_step(&mut self, frames: &mut Vec<Value>, stop_reason: &str) {
        self.remember_text();
        self.flush_blocks(frames);
        self.close_message(frames, stop_reason);
        self.message_id = short_id("msg");
        self.step_usage = StepUsage::default();
        self.step_started = Instant::now();
    }

    /// Frames for one agent event, in output order.
    pub fn on_event(&mut self, event: &AgentEvent) -> Vec<Value> {
        let mut frames = Vec::new();
        match event {
            AgentEvent::TextDelta(delta) => {
                if self.text.is_empty() {
                    self.partial_start(&mut frames, json!({"type": "text", "text": ""}));
                }
                self.text.push_str(delta);
                self.partial_delta(&mut frames, json!({"type": "text_delta", "text": delta}));
            }
            AgentEvent::ThinkingDelta(delta) => {
                if self.thinking.is_empty() {
                    self.partial_start(
                        &mut frames,
                        json!({"type": "thinking", "thinking": "", "signature": ""}),
                    );
                }
                self.thinking.push_str(delta);
                self.partial_delta(
                    &mut frames,
                    json!({"type": "thinking_delta", "thinking": delta}),
                );
            }
            AgentEvent::Error(message) => self.errors.push(message.clone()),
            AgentEvent::StreamRetrying => {
                self.text.clear();
                self.thinking.clear();
                self.message_open = false;
                self.block_index = 0;
            }
            AgentEvent::Usage {
                prompt_tokens,
                completion_tokens,
            } => {
                self.calls += 1;
                self.meter.add(*prompt_tokens, *completion_tokens);
                self.api_ms += self.step_started.elapsed().as_millis();
                self.step_usage = StepUsage {
                    input: *prompt_tokens,
                    output: *completion_tokens,
                };
                self.remember_text();
                self.flush_blocks(&mut frames);
            }
            AgentEvent::ToolStarted { name, args } => {
                self.remember_text();
                self.flush_blocks(&mut frames);
                self.tool_use(&mut frames, name, args, None);
            }
            AgentEvent::ToolFinished { name, output } => {
                self.tool_result(&mut frames, name, &output.content, output.is_error, None);
            }
            AgentEvent::SubagentRunStarted { run, .. } => {
                let parent = self
                    .pending
                    .iter()
                    .rev()
                    .find(|(name, _, _)| name == "spawn_subagent")
                    .map(|(_, id, _)| id.clone());
                self.subagent_parent.insert(*run, parent);
            }
            AgentEvent::SubagentRunToolStarted { run, name, args } => {
                let parent = self.subagent_parent.get(run).cloned().flatten();
                self.tool_use(&mut frames, name, args, parent);
            }
            AgentEvent::SubagentRunToolFinished { run, name, output } => {
                let parent = self.subagent_parent.get(run).cloned().flatten();
                self.tool_result(&mut frames, name, &output.content, output.is_error, parent);
            }
            AgentEvent::SubagentRunDone { run, .. } => {
                self.subagent_parent.remove(run);
            }
            AgentEvent::StepCompleted { .. } => {
                self.steps += 1;
                self.end_step(&mut frames, "end_turn");
            }
            AgentEvent::Done { .. } => {
                self.turns += 1;
                self.end_step(&mut frames, "end_turn");
            }
            AgentEvent::HookFired {
                event,
                command,
                outcome,
            } if self.info.hook_events => {
                frames.push(json!({
                    "type": "system",
                    "subtype": "hook_response",
                    "hook_event": event,
                    "hook_name": command,
                    "outcome": outcome.to_string(),
                    "session_id": self.info.session_id,
                    "uuid": new_uuid(),
                }));
            }
            _ => {}
        }
        frames
    }

    /// Move the text of a finished model reply into the answer bookkeeping.
    fn remember_text(&mut self) {
        if self.text.is_empty() {
            return;
        }
        self.last_text = self.text.clone();
        self.all_text.push_str(&self.text);
    }

    fn tool_use(
        &mut self,
        frames: &mut Vec<Value>,
        name: &str,
        args: &Value,
        parent: Option<String>,
    ) {
        let id = short_id("toolu");
        let claude_name = toolmap::claude_name(name);
        let input = toolmap::to_claude_input(name, args);
        self.partial_start(
            frames,
            json!({"type": "tool_use", "id": id, "name": claude_name, "input": {}}),
        );
        self.partial_delta(
            frames,
            json!({"type": "input_json_delta", "partial_json": input.to_string()}),
        );
        frames.push(self.assistant(
            json!({"type": "tool_use", "id": id, "name": claude_name, "input": input}),
            parent.as_deref(),
        ));
        self.partial_stop(frames);
        self.close_message(frames, "tool_use");
        self.message_id = short_id("msg");
        self.pending.push((name.to_string(), id, parent));
    }

    fn tool_result(
        &mut self,
        frames: &mut Vec<Value>,
        name: &str,
        content: &str,
        is_error: bool,
        parent: Option<String>,
    ) {
        let at = self
            .pending
            .iter()
            .position(|(pending, _, p)| pending == name && *p == parent)
            .or_else(|| {
                self.pending
                    .iter()
                    .position(|(pending, _, _)| pending == name)
            });
        let id = match at {
            Some(at) => self.pending.remove(at).1,
            None => short_id("toolu"),
        };
        let mut map = self.base("user");
        map.insert(
            "message".into(),
            json!({
                "role": "user",
                "content": [{
                    "tool_use_id": id,
                    "type": "tool_result",
                    "content": content,
                    "is_error": is_error,
                }],
            }),
        );
        if name == "execute" {
            // Claude Code's structured Bash result. Wizard's `execute` returns
            // one merged text, so it all goes in `stdout`.
            map.insert(
                "tool_use_result".into(),
                json!({
                    "stdout": content,
                    "stderr": "",
                    "interrupted": false,
                    "isImage": false,
                    "noOutputExpected": content.is_empty(),
                }),
            );
        }
        frames.push(self.envelope(map, parent.as_deref()));
        self.step_started = Instant::now();
    }

    /// Forget the last turn's answer, step count and errors so the next
    /// user message gets its own `result`. Tokens and cost keep adding up
    /// across turns, the way Claude Code's do.
    pub fn reset_turn(&mut self) {
        self.started = Instant::now();
        self.api_ms = 0;
        self.steps = 0;
        self.turns = 0;
        self.calls = 0;
        self.last_text.clear();
        self.all_text.clear();
        self.errors.clear();
        self.denials.clear();
    }

    /// Record a tool refused by policy, for `permission_denials`.
    pub fn deny(&mut self, tool: &str, input: Value) {
        self.denials.push(json!({
            "tool_name": toolmap::claude_name(tool),
            "tool_use_id": short_id("toolu"),
            "tool_input": input,
        }));
    }

    /// The `errors` array: what went wrong during the run, then the sentence
    /// Claude Code gives for the limit that ended it.
    fn error_lines(&self, reason: &str) -> Vec<String> {
        let mut lines = self.errors.clone();
        if self.meter.exceeded() {
            lines.push(format!(
                "Reached maximum budget (${})",
                self.info.max_budget_usd.unwrap_or_default()
            ));
        } else if reason == "max_steps" {
            lines.push(match self.info.max_turns {
                Some(turns) => format!("Reached maximum number of turns ({turns})"),
                None => "Reached maximum number of turns".to_string(),
            });
        }
        lines
    }

    /// The closing `result` object. `extra` carries Wizard's own fields.
    pub fn result(&self, reason: &str, extra: Map<String, Value>) -> Value {
        let subtype = subtype_for(reason, self.meter.exceeded());
        let is_error = subtype != "success";
        let model_usage = json!({
            self.info.model.clone(): {
                "inputTokens": self.meter.prompt_tokens,
                "outputTokens": self.meter.completion_tokens,
                "cacheReadInputTokens": 0,
                "cacheCreationInputTokens": 0,
                "webSearchRequests": 0,
                "costUSD": self.cost_usd(),
                "contextWindow": 0,
                "maxOutputTokens": 0,
                "thinkingTokens": 0,
                "canonicalModel": self.info.model,
                "provider": "wizard",
                "costBasis": "estimate",
            },
        });
        let mut map = Map::new();
        map.insert("type".into(), json!("result"));
        map.insert("subtype".into(), json!(subtype));
        map.insert("is_error".into(), json!(is_error));
        map.insert("api_error_status".into(), Value::Null);
        map.insert(
            "duration_ms".into(),
            json!(self.started.elapsed().as_millis() as u64),
        );
        map.insert("duration_api_ms".into(), json!(self.api_ms as u64));
        map.insert(
            "num_turns".into(),
            json!(self.calls.max(self.steps).max(self.turns)),
        );
        map.insert("result".into(), json!(self.answer()));
        map.insert(
            "stop_reason".into(),
            json!(if is_error {
                Value::Null
            } else {
                json!("end_turn")
            }),
        );
        map.insert("session_id".into(), json!(self.info.session_id));
        map.insert("total_cost_usd".into(), json!(self.cost_usd()));
        map.insert(
            "usage".into(),
            json!({
                "input_tokens": self.meter.prompt_tokens,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0,
                "output_tokens": self.meter.completion_tokens,
                "output_tokens_details": {"thinking_tokens": 0},
                "server_tool_use": {"web_search_requests": 0, "web_fetch_requests": 0},
                "service_tier": "standard",
                "cache_creation": {"ephemeral_1h_input_tokens": 0, "ephemeral_5m_input_tokens": 0},
                "inference_geo": "",
                "iterations": [],
                "speed": "standard",
                "fallback_credit": null,
                "prompt_tokens": self.meter.prompt_tokens,
                "completion_tokens": self.meter.completion_tokens,
            }),
        );
        map.insert("model_usage_since_last_result".into(), model_usage.clone());
        map.insert("modelUsage".into(), model_usage);
        map.insert("fast_mode_state".into(), json!("off"));
        map.insert("permission_denials".into(), json!(self.denials));
        map.insert(
            "terminal_reason".into(),
            json!(terminal_reason(reason, self.meter.exceeded())),
        );
        map.insert("uuid".into(), json!(new_uuid()));
        map.insert("reason".into(), json!(reason));
        map.insert("errors".into(), json!(self.error_lines(reason)));
        for (key, value) in extra {
            map.insert(key, value);
        }
        Value::Object(map)
    }
}

#[cfg(test)]
mod tests;
