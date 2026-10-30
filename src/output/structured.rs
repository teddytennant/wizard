//! The two structured sinks, `json` and `stream-json`.
//!
//! Both follow Claude Code's headless shapes (see [`crate::claude_stream`]):
//! `json` is the closing `result` object alone, `stream-json` is the `system`
//! init line, the `assistant` / `user` messages and then the `result`. Wizard's
//! own fields ride along on the `result` object, and the events Claude Code
//! has no message for (plans, todos, tasks, subagent bookkeeping, notices) are
//! still written, as extra lines with their own `type`.

use std::collections::HashMap;
use std::io::Write;

use serde_json::{Map, Value, json};

use super::{EventSink, image_json, reason_str};
use crate::agent::{AgentEvent, CancelHandle, DoneReason, PlanVerdict};
use crate::claude_stream::{ClaudeRun, RunInfo};
use crate::images::ImageRef;

/// Per-tool aggregation for the final `json` summary.
#[derive(Debug)]
struct ToolCallsEntry {
    name: String,
    calls: u64,
    errors: u64,
}

/// Buffers the whole run and emits one final JSON object: Claude Code's
/// `result` keys plus `reason`, `turns`, `steps`, `text`, `tool_calls`,
/// `errors` and `images`.
pub struct JsonSink<W: Write + Send> {
    out: W,
    run: ClaudeRun,
    annotations: Map<String, Value>,
    tool_calls: Vec<ToolCallsEntry>,
    /// Images the run produced, in order, where they were written.
    images: Vec<ImageRef>,
    /// Run id -> subagent name, so a subagent's tool calls aggregate under
    /// `<name> ▸ <tool>`.
    subagents: HashMap<u64, String>,
}

impl JsonSink<std::io::Stdout> {
    pub fn stdout() -> Self {
        Self::new(std::io::stdout())
    }
}

impl<W: Write + Send> JsonSink<W> {
    pub fn new(out: W) -> Self {
        Self {
            out,
            run: ClaudeRun::new(RunInfo::default()),
            annotations: Map::new(),
            tool_calls: Vec::new(),
            images: Vec::new(),
            subagents: HashMap::new(),
        }
    }

    /// Session, model and price details for the `result` object.
    pub fn with_info(mut self, info: RunInfo, cancel: Option<CancelHandle>) -> Self {
        self.run = ClaudeRun::new(info);
        if let Some(cancel) = cancel {
            self.run.set_cancel(cancel);
        }
        self
    }

    fn count(&mut self, name: String, error: bool, started: bool) {
        match self.tool_calls.iter_mut().find(|entry| entry.name == name) {
            Some(entry) => {
                entry.calls += u64::from(started);
                entry.errors += u64::from(error);
            }
            None if started => self.tool_calls.push(ToolCallsEntry {
                name,
                calls: 1,
                errors: 0,
            }),
            None => {}
        }
    }

    fn subagent_label(&self, run: u64, tool: &str) -> String {
        let who = self
            .subagents
            .get(&run)
            .map(String::as_str)
            .unwrap_or("subagent");
        format!("{who} ▸ {tool}")
    }
}

impl<W: Write + Send> EventSink for JsonSink<W> {
    fn event(&mut self, event: AgentEvent) {
        self.run.on_event(&event);
        match event {
            AgentEvent::ToolStarted { name, .. } => self.count(name, false, true),
            AgentEvent::ToolFinished { name, output } => self.count(name, output.is_error, false),
            AgentEvent::SubagentRunStarted { run, name, .. } => {
                self.subagents.insert(run, name);
            }
            AgentEvent::SubagentRunToolStarted { run, name, .. } => {
                let label = self.subagent_label(run, &name);
                self.count(label, false, true);
            }
            AgentEvent::SubagentRunToolFinished { run, name, output } => {
                let label = self.subagent_label(run, &name);
                self.count(label, output.is_error, false);
            }
            AgentEvent::SubagentRunDone { run, .. } => {
                self.subagents.remove(&run);
            }
            AgentEvent::PlanReady { gate, .. } => {
                // No human in the loop: approve so the turn executes.
                gate.answer(PlanVerdict::approve());
            }
            AgentEvent::PermissionRequest { gate, .. } => {
                // Nobody here to ask; the call is refused.
                gate.answer(crate::agent::PermissionAnswer::Unavailable);
            }
            AgentEvent::Interview { gate, .. } => {
                // No interactive user: decline so the model uses its judgment.
                gate.decline();
            }
            AgentEvent::Images { images, .. } | AgentEvent::SubagentRunImages { images, .. } => {
                self.images.extend(images);
            }
            // The summary is the result and what it cost; everything else is
            // either folded into the run state above or has no field here.
            _ => {}
        }
    }

    fn finish(&mut self, reason: DoneReason) {
        let mut extra = Map::new();
        extra.insert("turns".into(), json!(self.run.turns));
        extra.insert("steps".into(), json!(self.run.steps));
        extra.insert("text".into(), json!(self.run.all_text()));
        extra.insert(
            "tool_calls".into(),
            json!(
                self.tool_calls
                    .iter()
                    .map(|entry| json!({
                        "name": entry.name,
                        "calls": entry.calls,
                        "errors": entry.errors,
                    }))
                    .collect::<Vec<_>>()
            ),
        );
        extra.insert("images".into(), json!(self.images));
        extra.extend(std::mem::take(&mut self.annotations));
        let summary = self.run.result(reason_str(reason), extra);
        let _ = writeln!(self.out, "{summary}");
        let _ = self.out.flush();
    }

    fn annotate(&mut self, key: &str, value: Value) {
        self.annotations.insert(key.to_string(), value);
    }

    fn budget_exceeded(&self) -> bool {
        self.run.over_budget()
    }
}

/// Emits one JSON object per line as events arrive: the `system` init line,
/// Claude Code's `assistant` / `user` messages, and a closing `result`.
pub struct StreamJsonSink<W: Write + Send> {
    out: W,
    run: ClaudeRun,
    annotations: Map<String, Value>,
    started: bool,
}

impl StreamJsonSink<std::io::Stdout> {
    pub fn stdout() -> Self {
        Self::new(std::io::stdout())
    }
}

impl<W: Write + Send> StreamJsonSink<W> {
    pub fn new(out: W) -> Self {
        Self {
            out,
            run: ClaudeRun::new(RunInfo::default()),
            annotations: Map::new(),
            started: false,
        }
    }

    /// Session, model and price details for the init and `result` lines.
    pub fn with_info(mut self, info: RunInfo, cancel: Option<CancelHandle>) -> Self {
        self.run = ClaudeRun::new(info);
        if let Some(cancel) = cancel {
            self.run.set_cancel(cancel);
        }
        self
    }

    /// Write a line of the caller's own (a `control_response`, a replayed
    /// user message) between the run's.
    pub fn emit(&mut self, value: Value) {
        let _ = writeln!(self.out, "{value}");
        let _ = self.out.flush();
    }

    /// The `session_id` lines carry.
    pub fn session_id(&self) -> &str {
        &self.run.info().session_id
    }

    /// Close one turn of a multi-turn run: its `result`, then a clean slate
    /// (and a fresh `init`) for the next user message.
    pub fn finish_turn(&mut self, reason: DoneReason) {
        self.finish(reason);
        self.run.reset_turn();
        self.started = false;
    }

    fn ensure_started(&mut self) {
        if !self.started {
            self.started = true;
            let init = self.run.init();
            self.emit(init);
        }
    }

    /// A line for an event Claude Code has no message for.
    fn extra(&mut self, event: AgentEvent) {
        let line = match event {
            AgentEvent::Images { source, images } => image_json(None, &source, &images),
            AgentEvent::SubagentRunImages {
                run,
                source,
                images,
            } => image_json(Some(run), &source, &images),
            AgentEvent::Error(message) => json!({"type": "error", "message": message}),
            AgentEvent::Notice(message) => json!({"type": "notice", "message": message}),
            AgentEvent::StreamRetrying => json!({"type": "stream_retrying"}),
            AgentEvent::HookFired {
                event,
                command,
                outcome,
            } if !self.run.info().hook_events => json!({
                "type": "hook",
                "event": event,
                "command": command,
                "outcome": outcome.to_string(),
            }),
            AgentEvent::PlanReady { plan, gate } => {
                // No human in the loop: report the plan and approve it.
                gate.answer(PlanVerdict::approve());
                json!({"type": "plan", "plan": plan, "approved": true})
            }
            AgentEvent::PermissionRequest { gate, .. } => {
                // Nobody here to ask; the call is refused.
                gate.answer(crate::agent::PermissionAnswer::Unavailable);
                json!({"type": "permission_request", "answered": false})
            }
            AgentEvent::Interview { questions, gate } => {
                // No interactive user: report the questions, then decline so
                // the model proceeds with its best judgment.
                let asked: Vec<&str> = questions.iter().map(|q| q.question.as_str()).collect();
                gate.decline();
                json!({"type": "interview", "questions": asked, "answered": false})
            }
            AgentEvent::OmakaseProceeding { plan } => {
                json!({"type": "plan", "plan": plan, "omakase": true, "approved": true})
            }
            AgentEvent::TodoUpdated(items) => json!({
                "type": "todo",
                "items": serde_json::to_value(&items).unwrap_or_default(),
            }),
            AgentEvent::TaskStarted { id, command } => {
                json!({"type": "task_started", "id": id, "command": command})
            }
            AgentEvent::TaskFinished {
                id,
                command,
                status,
            } => json!({
                "type": "task_finished",
                "id": id,
                "command": command,
                "status": status.describe(),
            }),
            AgentEvent::SubagentStarted { id, name, task } => {
                json!({"type": "subagent_started", "id": id, "name": name, "task": task})
            }
            AgentEvent::SubagentFinished {
                id,
                name,
                task,
                completed,
                output,
            } => json!({
                "type": "subagent_finished",
                "id": id,
                "name": name,
                "task": task,
                "completed": completed,
                "output": output,
            }),
            AgentEvent::CommandRequested(line) => {
                json!({"type": "command_requested", "command": line})
            }
            AgentEvent::UltraGuidance { label, guidance } => {
                json!({"type": "ultra_guidance", "roster": label, "guidance": guidance})
            }
            // A console's gate rides as its bare ticket number, which is all
            // it can honestly be off-process.
            AgentEvent::ConsoleOpened { command, gate } => {
                json!({"type": "console_opened", "command": command, "gate": gate})
            }
            AgentEvent::ConsoleWaiting { gate } => {
                json!({"type": "console_waiting", "gate": gate})
            }
            AgentEvent::ConsoleOutput { gate, chunk } => {
                json!({"type": "console_output", "gate": gate, "chunk": chunk})
            }
            AgentEvent::ConsoleClosed { gate } => json!({"type": "console_closed", "gate": gate}),
            _ => return,
        };
        self.emit(line);
    }
}

impl<W: Write + Send> EventSink for StreamJsonSink<W> {
    fn start(&mut self) {
        self.ensure_started();
    }

    fn event(&mut self, event: AgentEvent) {
        self.ensure_started();
        for frame in self.run.on_event(&event) {
            self.emit(frame);
        }
        self.extra(event);
    }

    fn finish(&mut self, reason: DoneReason) {
        self.ensure_started();
        let mut extra = Map::new();
        extra.insert("turns".into(), json!(self.run.turns));
        extra.insert("steps".into(), json!(self.run.steps));
        extra.extend(std::mem::take(&mut self.annotations));
        let result = self.run.result(reason_str(reason), extra);
        self.emit(result);
    }

    fn annotate(&mut self, key: &str, value: Value) {
        self.annotations.insert(key.to_string(), value);
    }

    fn budget_exceeded(&self) -> bool {
        self.run.over_budget()
    }
}
