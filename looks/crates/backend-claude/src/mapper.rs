//! Pure translation of Claude Code stream-json lines into [`Event`]s.
//!
//! No IO and no clock except for rate-limit reset text, so every fixture under
//! `tests/fixtures/` can be replayed through [`Mapper::feed`] in a unit test.
//!
//! Turn boundaries are not one per prompt. The CLI emits `system/init` at the
//! start of every turn and one `result` at the end, but it also starts turns on
//! its own (a background subagent finishing, a local slash command), so
//! `TurnStart` is raised by the first main-thread message after a `result`,
//! not only by [`Mapper::begin_turn`].

use crate::tools::{self, apply_result, cap, flatten_content, new_call};
use agent_core::{Event, NoticeLevel, StopReason, Todo, TodoStatus, ToolCall, ToolStatus, Usage};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Result strings the CLI uses for plan/usage limits. They arrive as a normal
/// `result` with exit code 0, sometimes without `is_error` set.
const LIMIT_PREFIXES: &[&str] = &[
    "You've hit your",
    "You've reached your",
    "You're out of usage credits",
    "Your org is out of usage",
    "Your seat type doesn't include usage",
    "Your usage allocation has been disabled",
];

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

fn u(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}

/// A failed request that claude is going to send again. It travels as a [`NoticeLevel::Warn`]
/// notice so the shared event type stays as it is; a frontend that knows the shape (openc)
/// folds the attempts into one row with a countdown, the rest show the text as it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryNotice {
    pub reason: String,
    pub attempt: u32,
    pub max: u32,
    pub delay_ms: u64,
}

impl RetryNotice {
    /// The line other frontends show as it is: `API 529 (overloaded), retry 1/2 in 0.5s`.
    pub fn text(&self) -> String {
        format!(
            "API {}, retry {}/{} in {:.1}s",
            self.reason,
            self.attempt,
            self.max,
            self.delay_ms as f64 / 1000.0
        )
    }

    pub fn parse(text: &str) -> Option<RetryNotice> {
        let rest = text.strip_prefix("API ")?;
        let (reason, rest) = rest.rsplit_once(", retry ")?;
        let (counts, secs) = rest.split_once(" in ")?;
        let (a, m) = counts.split_once('/')?;
        let secs: f64 = secs.strip_suffix('s')?.parse().ok()?;
        Some(RetryNotice {
            reason: reason.to_string(),
            attempt: a.parse().ok()?,
            max: m.parse().ok()?,
            delay_ms: (secs * 1000.0).round() as u64,
        })
    }
}

pub struct Mapper {
    cwd: PathBuf,
    /// Session id from the latest `system/init`. Changes after `/clear`.
    pub session_id: String,
    /// Resolved model id from the latest init, e.g. `claude-haiku-4-5-20251001`.
    pub model: String,
    /// Permission mode from the latest init.
    pub mode: String,
    pub in_turn: bool,
    window: u64,
    cost: Option<f64>,
    cost_offset: f64,
    last: Usage,
    tools: HashMap<String, ToolCall>,
    todos: Vec<(String, Todo)>,
    /// Ids of messages whose text already went out as deltas.
    streamed: HashSet<String>,
    synthetic: Vec<String>,
    /// Agent calls that were launched in the background.
    bg: HashSet<String>,
    rate_seen: HashSet<String>,
}

impl Mapper {
    pub fn new(cwd: PathBuf) -> Self {
        Mapper {
            cwd,
            session_id: String::new(),
            model: String::new(),
            mode: String::new(),
            in_turn: false,
            window: 200_000,
            cost: None,
            cost_offset: 0.0,
            last: Usage::default(),
            tools: HashMap::new(),
            todos: Vec::new(),
            streamed: HashSet::new(),
            synthetic: Vec::new(),
            bg: HashSet::new(),
            rate_seen: HashSet::new(),
        }
    }

    /// How many tool calls are still tracked (running, or waiting for a background report).
    pub fn tracked_tools(&self) -> usize {
        self.tools.len()
    }

    pub fn set_window(&mut self, w: u64) {
        if w > 0 {
            self.window = w;
        }
    }

    /// `total_cost_usd` restarts at zero with every process; carry the earlier total.
    pub fn set_cost_offset(&mut self, c: f64) {
        self.cost_offset = c;
    }

    pub fn cost(&self) -> f64 {
        self.cost.unwrap_or(self.cost_offset)
    }

    /// Forget per-session state (new session, `/clear`). Cost and window stay.
    pub fn reset_session(&mut self) {
        self.tools.clear();
        self.todos.clear();
        self.bg.clear();
        self.streamed.clear();
        self.synthetic.clear();
        self.last = Usage::default();
        self.in_turn = false;
    }

    /// Usage as the footer wants it after a model change: our last numbers with
    /// the CLI's own context estimate and window.
    pub fn usage_with(&mut self, context: u64, window: u64) -> Usage {
        self.set_window(window);
        self.last.context_tokens = context;
        self.snapshot()
    }

    fn snapshot(&self) -> Usage {
        Usage {
            context_window: self.window,
            cost_usd: self
                .cost
                .or_else(|| (self.cost_offset > 0.0).then_some(self.cost_offset)),
            ..self.last.clone()
        }
    }

    /// Call when a prompt is written to the CLI.
    pub fn begin_turn(&mut self) -> Option<Event> {
        if self.in_turn {
            return None;
        }
        self.in_turn = true;
        Some(Event::TurnStart)
    }

    /// The process is gone or was killed mid-turn: close the turn and fail what was running.
    pub fn abort_turn(&mut self, reason: StopReason, why: &str) -> Vec<Event> {
        let mut out = Vec::new();
        let mut ids: Vec<&String> = self
            .tools
            .iter()
            .filter(|(_, t)| t.status == ToolStatus::Running || t.status == ToolStatus::Pending)
            .map(|(k, _)| k)
            .collect();
        ids.sort();
        let ids: Vec<String> = ids.into_iter().cloned().collect();
        for id in ids {
            if let Some(t) = self.tools.get_mut(&id) {
                t.status = ToolStatus::Failed;
                t.output = Some(why.to_string());
                out.push(Event::Tool(t.clone()));
            }
        }
        if self.in_turn {
            self.in_turn = false;
            out.push(Event::TurnEnd(reason));
        }
        out
    }

    fn ensure_turn(&mut self, out: &mut Vec<Event>) {
        if !self.in_turn {
            self.in_turn = true;
            out.push(Event::TurnStart);
        }
    }

    pub fn feed(&mut self, v: &Value) -> Vec<Event> {
        let mut out = Vec::new();
        match s(v, "type") {
            Some("system") => self.system(v, &mut out),
            Some("stream_event") => self.stream_event(v, &mut out),
            Some("assistant") => self.assistant(v, &mut out),
            Some("user") => self.user(v, &mut out),
            Some("result") => self.result(v, &mut out),
            Some("rate_limit_event") => self.rate_limit(v, &mut out),
            _ => {}
        }
        out
    }

    fn system(&mut self, v: &Value, out: &mut Vec<Event>) {
        match s(v, "subtype") {
            Some("init") => {
                if let Some(m) = s(v, "model") {
                    self.model = m.to_string();
                }
                if let Some(m) = s(v, "permissionMode") {
                    self.mode = m.to_string();
                }
                if let Some(id) = s(v, "session_id") {
                    if !self.session_id.is_empty() && self.session_id != id {
                        // `/clear` starts a new conversation inside the same process.
                        self.reset_session();
                        out.push(Event::History {
                            session_id: id.to_string(),
                            items: Vec::new(),
                        });
                    }
                    self.session_id = id.to_string();
                }
                self.ensure_turn(out);
            }
            Some("status") => match s(v, "status") {
                Some("requesting") => self.ensure_turn(out),
                Some("compacting") => out.push(notice(NoticeLevel::Info, "Compacting context...")),
                _ => {
                    if s(v, "compact_result") == Some("failed") {
                        let why = s(v, "compact_error").unwrap_or("unknown error");
                        out.push(notice(
                            NoticeLevel::Error,
                            format!("Compaction failed: {why}"),
                        ));
                    }
                }
            },
            Some("api_retry") => {
                let status = v
                    .get("error_status")
                    .and_then(Value::as_u64)
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "no response".into());
                let err = s(v, "error").unwrap_or("error");
                let reason = format!("{status} ({err})");
                out.push(notice(
                    NoticeLevel::Warn,
                    RetryNotice {
                        reason,
                        attempt: u(v, "attempt") as u32,
                        max: u(v, "max_retries") as u32,
                        delay_ms: u(v, "retry_delay_ms"),
                    }
                    .text(),
                ));
            }
            Some("compact_boundary") => {
                let m = v.get("compact_metadata").cloned().unwrap_or(Value::Null);
                let (pre, post) = (u(&m, "pre_tokens"), u(&m, "post_tokens"));
                out.push(notice(
                    NoticeLevel::Info,
                    format!("Context compacted: {} -> {} tokens", k(pre), k(post)),
                ));
                if post > 0 {
                    self.last.context_tokens = post;
                    out.push(Event::Usage(self.snapshot()));
                }
            }
            Some("model_fallback") => {
                if let Some(c) = s(v, "content") {
                    out.push(notice(NoticeLevel::Warn, c));
                }
            }
            Some("informational") => {
                // "info" lines are transcript-only in the real UI; the rest are meant to be seen.
                let level = match s(v, "level") {
                    Some("warning") => Some(NoticeLevel::Warn),
                    Some("notice") | Some("suggestion") => Some(NoticeLevel::Info),
                    _ => None,
                };
                if let (Some(level), Some(c)) = (level, s(v, "content")) {
                    out.push(notice(level, c));
                }
            }
            Some("notification") => {
                if let Some(t) = s(v, "text") {
                    let level = if matches!(s(v, "priority"), Some("high") | Some("immediate")) {
                        NoticeLevel::Warn
                    } else {
                        NoticeLevel::Info
                    };
                    out.push(notice(level, t));
                }
            }
            Some("task_started") => {
                if let (Some(id), Some(true)) = (
                    s(v, "tool_use_id"),
                    v.get("is_backgrounded").and_then(Value::as_bool),
                ) {
                    self.bg.insert(id.to_string());
                }
            }
            Some("task_progress") => {
                let Some(id) = s(v, "tool_use_id") else {
                    return;
                };
                let Some(t) = self.tools.get_mut(id) else {
                    return;
                };
                if t.status != ToolStatus::Running {
                    return;
                }
                let uses = v.get("usage").map(|x| u(x, "tool_uses")).unwrap_or(0);
                let what = s(v, "description").unwrap_or("working");
                t.output = Some(format!("{what} ({uses} tool uses)"));
                out.push(Event::Tool(t.clone()));
            }
            Some("task_notification") => {
                // A background subagent finishing. The Agent tool_result arrived long
                // before (it only said "launched"), so this is what completes the call.
                let Some(id) = s(v, "tool_use_id") else {
                    return;
                };
                let Some(t) = self.tools.get_mut(id) else {
                    return;
                };
                // A foreground subagent sends this just before its tool_result, which has the full report.
                if !self.bg.contains(id)
                    || matches!(t.status, ToolStatus::Completed | ToolStatus::Failed)
                {
                    return;
                }
                let ok = s(v, "status") == Some("completed");
                t.status = if ok {
                    ToolStatus::Completed
                } else {
                    ToolStatus::Failed
                };
                t.output = Some(cap(
                    s(v, "summary").unwrap_or(s(v, "status").unwrap_or("finished")),
                    tools::OUTPUT_CAP,
                ));
                out.push(Event::Tool(t.clone()));
                // Nothing more can arrive for it.
                self.tools.remove(id);
                self.bg.remove(id);
            }
            // hook_*, thinking_tokens, task_started/updated, background_tasks_changed,
            // session_state_changed, turn_duration, permission_denied (the tool_result
            // already carries the reason) and anything newer.
            _ => {}
        }
    }

    fn stream_event(&mut self, v: &Value, out: &mut Vec<Event>) {
        // Only the main thread streams; subagents arrive as whole messages.
        if !v
            .get("parent_tool_use_id")
            .map(Value::is_null)
            .unwrap_or(true)
        {
            return;
        }
        let Some(e) = v.get("event") else { return };
        match s(e, "type") {
            Some("message_start") => {
                self.ensure_turn(out);
                if let Some(id) = e.get("message").and_then(|m| s(m, "id")) {
                    self.streamed.insert(id.to_string());
                }
            }
            Some("content_block_start") => {
                let Some(b) = e.get("content_block") else {
                    return;
                };
                if s(b, "type") == Some("tool_use") {
                    let (Some(id), Some(name)) = (s(b, "id"), s(b, "name")) else {
                        return;
                    };
                    if !self.tools.contains_key(id) {
                        let mut t = new_call(id, name, Value::Null, &self.cwd, None);
                        t.status = ToolStatus::Pending;
                        self.tools.insert(id.to_string(), t.clone());
                        out.push(Event::Tool(t));
                    }
                }
            }
            Some("content_block_delta") => {
                let Some(d) = e.get("delta") else { return };
                match s(d, "type") {
                    Some("text_delta") => {
                        if let Some(t) = s(d, "text").filter(|t| !t.is_empty()) {
                            out.push(Event::TextDelta(t.to_string()));
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(t) = s(d, "thinking").filter(|t| !t.is_empty()) {
                            out.push(Event::ThoughtDelta(t.to_string()));
                        }
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                // Cumulative for this API call, and the last one of a turn is the real context size.
                if let Some(usage) = e.get("usage") {
                    self.set_message_usage(usage);
                    out.push(Event::Usage(self.snapshot()));
                }
            }
            _ => {}
        }
    }

    fn set_message_usage(&mut self, usage: &Value) {
        let (inp, outp) = (u(usage, "input_tokens"), u(usage, "output_tokens"));
        let (read, create) = (
            u(usage, "cache_read_input_tokens"),
            u(usage, "cache_creation_input_tokens"),
        );
        self.last.input_tokens = inp;
        self.last.output_tokens = outp;
        self.last.cached_tokens = read;
        // Counts come off the wire as u64; a bogus one must not wrap or panic a debug build.
        self.last.context_tokens = inp.saturating_add(read).saturating_add(create);
    }

    fn assistant(&mut self, v: &Value, out: &mut Vec<Event>) {
        let parent = s(v, "parent_tool_use_id").map(str::to_string);
        let Some(msg) = v.get("message") else { return };
        let id = s(msg, "id").unwrap_or("");
        let synthetic = s(msg, "model") == Some("<synthetic>");
        let streamed = self.streamed.contains(id);
        if parent.is_none() && !synthetic {
            self.ensure_turn(out);
        }
        let Some(blocks) = msg.get("content").and_then(Value::as_array) else {
            return;
        };
        for b in blocks {
            match s(b, "type") {
                Some("text") => {
                    let t = s(b, "text").unwrap_or("");
                    if synthetic {
                        self.synthetic.push(t.to_string());
                    } else if parent.is_none() && !streamed && !t.is_empty() {
                        out.push(Event::TextDelta(t.to_string()));
                    }
                }
                Some("thinking") => {
                    let t = s(b, "thinking").unwrap_or("");
                    if parent.is_none() && !streamed && !t.is_empty() {
                        out.push(Event::ThoughtDelta(t.to_string()));
                    }
                }
                Some("tool_use") => {
                    let (Some(tid), Some(name)) = (s(b, "id"), s(b, "name")) else {
                        continue;
                    };
                    let input = b.get("input").cloned().unwrap_or(Value::Null);
                    let call = new_call(tid, name, input, &self.cwd, parent.as_deref());
                    if name == "TodoWrite" {
                        if let Some(list) = call.input.get("todos").and_then(Value::as_array) {
                            self.todos = list
                                .iter()
                                .enumerate()
                                .map(|(i, t)| {
                                    (
                                        i.to_string(),
                                        Todo {
                                            text: s(t, "content").unwrap_or("").to_string(),
                                            status: todo_status(s(t, "status").unwrap_or("")),
                                        },
                                    )
                                })
                                .collect();
                            out.push(self.todos_event());
                        }
                    }
                    self.tools.insert(tid.to_string(), call.clone());
                    out.push(Event::Tool(call));
                }
                _ => {}
            }
        }
        if parent.is_none() && !streamed && !synthetic {
            if let Some(usage) = msg.get("usage") {
                self.set_message_usage(usage);
                out.push(Event::Usage(self.snapshot()));
            }
        }
    }

    fn user(&mut self, v: &Value, out: &mut Vec<Event>) {
        let tur = v.get("tool_use_result").cloned().unwrap_or(Value::Null);
        let content = v.get("message").and_then(|m| m.get("content"));
        match content {
            Some(Value::Array(blocks)) => {
                for b in blocks {
                    match s(b, "type") {
                        Some("tool_result") => self.tool_result(b, &tur, out),
                        Some("text") => self.user_text(s(b, "text").unwrap_or(""), out),
                        _ => {}
                    }
                }
            }
            Some(Value::String(t)) => self.user_text(t, out),
            _ => {}
        }
    }

    /// User-role text the CLI echoes back. Only local command output is worth showing.
    fn user_text(&mut self, text: &str, out: &mut Vec<Event>) {
        if let Some(inner) = text
            .trim()
            .strip_prefix("<local-command-stdout>")
            .and_then(|t| t.strip_suffix("</local-command-stdout>"))
        {
            let inner = inner.trim();
            if !inner.is_empty() {
                out.push(notice(NoticeLevel::Info, inner));
            }
        }
    }

    fn tool_result(&mut self, b: &Value, tur: &Value, out: &mut Vec<Event>) {
        let Some(id) = s(b, "tool_use_id") else {
            return;
        };
        let text = flatten_content(b.get("content").unwrap_or(&Value::Null));
        let is_error = b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
        let Some(call) = self.tools.get_mut(id) else {
            return;
        };
        if tur.get("isAsync").and_then(Value::as_bool) == Some(true) {
            self.bg.insert(id.to_string());
        }
        apply_result(call, &text, is_error, tur);
        let call = call.clone();
        if !is_error {
            match call.name.as_str() {
                "TaskCreate" => {
                    let tid = tur
                        .get("task")
                        .and_then(|t| s(t, "id"))
                        .map(str::to_string)
                        .unwrap_or_else(|| call.id.clone());
                    let subject = tur
                        .get("task")
                        .and_then(|t| s(t, "subject"))
                        .or_else(|| s(&call.input, "subject"))
                        .or_else(|| s(&call.input, "title"))
                        .unwrap_or("")
                        .to_string();
                    if !self.todos.iter().any(|(i, _)| *i == tid) {
                        self.todos.push((
                            tid,
                            Todo {
                                text: subject,
                                status: TodoStatus::Pending,
                            },
                        ));
                        out.push(self.todos_event());
                    }
                }
                "TaskUpdate" => {
                    let tid = s(tur, "taskId")
                        .or_else(|| s(&call.input, "taskId"))
                        .or_else(|| s(&call.input, "id"))
                        .unwrap_or("")
                        .to_string();
                    let status = tur
                        .get("statusChange")
                        .and_then(|c| s(c, "to"))
                        .or_else(|| s(&call.input, "status"))
                        .map(str::to_string);
                    let subject = s(&call.input, "subject").map(str::to_string);
                    if status.as_deref() == Some("deleted") {
                        self.todos.retain(|(i, _)| *i != tid);
                        out.push(self.todos_event());
                    } else if let Some((_, t)) = self.todos.iter_mut().find(|(i, _)| *i == tid) {
                        if let Some(st) = status {
                            t.status = todo_status(&st);
                        }
                        if let Some(sub) = subject {
                            t.text = sub;
                        }
                        out.push(self.todos_event());
                    }
                }
                _ => {}
            }
        }
        // A finished call has told the UI everything. Keeping every one for the life of the
        // session made a long run hold each tool's full output twice; only a call that can
        // still be changed from outside (a background agent's later notification) stays.
        let finished = matches!(call.status, ToolStatus::Completed | ToolStatus::Failed);
        if finished && !self.bg.contains(id) {
            self.tools.remove(id);
        } else {
            self.tools.insert(id.to_string(), call.clone());
        }
        out.push(Event::Tool(call));
    }

    fn todos_event(&self) -> Event {
        Event::Todos(self.todos.iter().map(|(_, t)| t.clone()).collect())
    }

    fn result(&mut self, v: &Value, out: &mut Vec<Event>) {
        self.ensure_turn(out);
        if let Some(c) = v.get("total_cost_usd").and_then(Value::as_f64) {
            self.cost = Some(self.cost_offset + c);
        }
        let usage = v.get("usage").cloned().unwrap_or(Value::Null);
        let model_usage = v
            .get("modelUsage")
            .and_then(Value::as_object)
            .filter(|m| !m.is_empty());
        if let Some(mu) = model_usage {
            let entry = mu.values().find(|e| e.get("contextWindow").is_some());
            if let Some(w) = entry.map(|e| u(e, "contextWindow")) {
                self.set_window(w);
            }
        }
        let text = s(v, "result").unwrap_or("").trim().to_string();
        let terminal = s(v, "terminal_reason").unwrap_or("");
        let subtype = s(v, "subtype").unwrap_or("success");
        let aborted = terminal.starts_with("aborted");
        let limited = LIMIT_PREFIXES.iter().any(|p| text.starts_with(p));
        let failed = !aborted
            && (v.get("is_error").and_then(Value::as_bool).unwrap_or(false)
                || subtype != "success"
                || limited);

        let synthetic = std::mem::take(&mut self.synthetic)
            .join("\n")
            .trim()
            .to_string();
        if failed {
            let msg = if !text.is_empty() {
                text.clone()
            } else if !synthetic.is_empty() {
                synthetic
            } else {
                let errs = v
                    .get("errors")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
                    .unwrap_or_default();
                if errs.is_empty() {
                    format!("turn failed ({subtype})")
                } else {
                    errs
                }
            };
            out.push(notice(NoticeLevel::Error, msg));
        } else if !aborted {
            // Local slash commands answer with a synthetic assistant message (model
            // "<synthetic>", num_turns 0); `result` repeats the same text.
            let msg = if !synthetic.is_empty() {
                synthetic
            } else if u(v, "num_turns") == 0 {
                text
            } else {
                String::new()
            };
            if !msg.is_empty() {
                out.push(notice(NoticeLevel::Info, msg));
            }
        }

        if model_usage.is_some() && u(v, "num_turns") > 0 {
            self.last.input_tokens = u(&usage, "input_tokens");
            self.last.output_tokens = u(&usage, "output_tokens");
            self.last.cached_tokens = u(&usage, "cache_read_input_tokens");
            out.push(Event::Usage(self.snapshot()));
        }

        let reason = if aborted {
            StopReason::Cancelled
        } else if subtype == "error_max_turns" || terminal == "max_turns" {
            StopReason::MaxTurns
        } else if failed {
            StopReason::Error
        } else {
            StopReason::EndTurn
        };
        self.in_turn = false;
        self.streamed.clear();
        out.push(Event::TurnEnd(reason));
    }

    fn rate_limit(&mut self, v: &Value, out: &mut Vec<Event>) {
        let Some(info) = v.get("rate_limit_info") else {
            return;
        };
        let status = s(info, "status").unwrap_or("allowed");
        if status == "allowed" {
            return;
        }
        let kind = s(info, "rateLimitType").unwrap_or("usage");
        if !self.rate_seen.insert(format!("{status}/{kind}")) {
            return;
        }
        let util = info.get("utilization").and_then(Value::as_f64).or_else(|| {
            info.get("unifiedWindows")
                .and_then(|w| w.get(kind))
                .and_then(|w| w.get("utilization"))
                .and_then(Value::as_f64)
        });
        let resets = info.get("resetsAt").and_then(Value::as_u64).map(|t| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let left = t.saturating_sub(now);
            format!(", resets in {}", span_text(left))
        });
        let pct = util
            .map(|x| format!(" at {:.0}%", x * 100.0))
            .unwrap_or_default();
        // Close to a limit is information, not a call for a human: it is a faint line. Only
        // a limit that has been hit is an error.
        let (level, what) = if status == "rejected" {
            (NoticeLevel::Error, "limit reached")
        } else {
            (NoticeLevel::Info, "limit close")
        };
        out.push(notice(
            level,
            format!(
                "{} {what}{pct}{}",
                limit_name(kind),
                resets.unwrap_or_default()
            ),
        ));
    }
}

/// The window's name as a person says it: `five_hour` is `5 hour`, `seven_day` is `weekly`.
fn limit_name(kind: &str) -> String {
    match kind {
        "five_hour" => "5 hour".into(),
        "seven_day" => "weekly".into(),
        other => match other.strip_prefix("seven_day_") {
            Some(m) => format!("weekly {}", m.replace('_', " ")),
            None => other.replace('_', " "),
        },
    }
}

/// `2d 15h` from two days up, `3h 05m` under, `12m` under an hour.
fn span_text(secs: u64) -> String {
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    match (d, h) {
        (2.., _) => format!("{d}d {h}h"),
        (_, 1..) => format!("{}h {m:02}m", secs / 3600),
        _ => format!("{m}m"),
    }
}

fn notice(level: NoticeLevel, text: impl Into<String>) -> Event {
    Event::Notice {
        level,
        text: text.into(),
    }
}

fn todo_status(s: &str) -> TodoStatus {
    match s {
        "in_progress" | "in-progress" => TodoStatus::InProgress,
        "completed" | "done" => TodoStatus::Completed,
        _ => TodoStatus::Pending,
    }
}

fn k(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_notices_use_plain_names_and_are_info_until_the_limit_is_hit() {
        assert_eq!(limit_name("seven_day"), "weekly");
        assert_eq!(limit_name("five_hour"), "5 hour");
        assert_eq!(limit_name("seven_day_opus"), "weekly opus");
        assert_eq!(limit_name("some_new_one"), "some new one");
        assert_eq!(span_text(63 * 3600 + 20 * 60), "2d 15h");
        assert_eq!(span_text(3 * 3600 + 300), "3h 05m");
        assert_eq!(span_text(25 * 60), "25m");
    }

    #[test]
    fn a_retry_notice_round_trips_and_other_text_is_not_one() {
        let r = RetryNotice {
            reason: "529 (overloaded)".into(),
            attempt: 3,
            max: 10,
            delay_ms: 2_100,
        };
        assert_eq!(RetryNotice::parse(&r.text()), Some(r.clone()));
        assert!(r.text().contains("retry 3/10 in 2.1s"));
        assert_eq!(
            RetryNotice::parse("Context compacted: 12k -> 2k tokens"),
            None
        );
    }
}
